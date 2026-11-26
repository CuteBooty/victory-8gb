// Reprojected temporal accumulation for the FidelityFX upscale dispatch
// (VICTORY_UPSCALE_HISTORY_MV=2). One compute pass at output resolution:
// history is moved by the dispatch's motion vectors (dilated by the nearest
// depth in 3x3), rejected where depth says it was something nearer that has
// moved away, clipped to the current 3x3 colour variance in YCoCg, and blended
// with a per-pixel weight that grows while a pixel stays put and drops with
// motion, colour change and the reactive mask. The weight rule mirrors
// `history::weight` in history.rs, which the unit tests cover.
cbuffer Frame : register(b0) {
    float2 scale, outputTexel;   // rendered / backing of the colour input; 1 / output size
    float2 jitterUV;             // jitter / backing
    float weight, exposureRatio; // highest history weight (0 reseeds); pre-exposure ratio
    float2 renderSize, jitterPx; // rendered pixels; jitter in render pixels
    float2 mvScale, mvBias;      // raw vector -> (previous - current) in output UV
    float motionHalf;            // motion in output pixels that halves the weight
    float depthTolerance;        // relative nearness step that rejects history
    float changeGain;            // clip distance (luma, compressed) that drops the count
    float maxCount;              // frames a pixel can accumulate
    uint inputs;                 // bits: 1 depth, 2 motion, 4 reactive, 8 depth inverted,
                                 // 16 display-resolution motion, 32 previous depth valid
    uint stats;                  // 1: this pass adds to the counters
};
Texture2D<float4> source : register(t0);
Texture2D<float4> previous : register(t1);
Texture2D<float> depth : register(t2);
Texture2D<float4> motion : register(t3);
Texture2D<float> reactive : register(t4);
Texture2D<float> previousNear : register(t5);
RWTexture2D<float4> target : register(u0);
RWTexture2D<float4> nextHistory : register(u1);
RWTexture2D<float> nextNear : register(u2);
RWByteAddressBuffer counters : register(u3);
SamplerState bilinear : register(s0);

float3 toYCoCg(float3 c) {
    return float3(dot(c, float3(0.25, 0.5, 0.25)), dot(c, float3(0.5, 0.0, -0.5)), dot(c, float3(-0.25, 0.5, -0.25)));
}
float3 fromYCoCg(float3 c) {
    return float3(c.x + c.y - c.z, c.x + c.z, c.x - c.y - c.z);
}
// Reversible tone map: bright texels do not dominate a blend (Karis).
float3 compress(float3 c) {
    c = max(c, 0.0);
    return c / (1.0 + max(c.r, max(c.g, c.b)));
}
float3 expand(float3 c) {
    c = max(c, 0.0);
    return c / max(1.0 - max(c.r, max(c.g, c.b)), 1.0 / 1024.0);
}
// Larger is nearer, proportional to 1 / view depth for either convention.
float nearness(float z) {
    return ((inputs & 8u) != 0) ? z : 1.0 - z;
}
float3 clipToBox(float3 c, float3 lo, float3 hi) {
    float3 centre = 0.5 * (lo + hi);
    float3 extent = max(0.5 * (hi - lo), 1e-5);
    float3 v = c - centre;
    float3 a = abs(v / extent);
    float m = max(a.x, max(a.y, a.z));
    return m > 1.0 ? centre + v / m : c;
}
// Catmull-Rom from five bilinear taps: resampling history every frame stays sharp.
float3 historyAt(float2 uv, float2 size) {
    float2 p = uv * size;
    float2 c = floor(p - 0.5) + 0.5;
    float2 f = p - c;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 t0 = (c - 1.0) / size;
    float2 t3 = (c + 2.0) / size;
    float2 t12 = (c + w2 / w12) / size;
    float a = w12.x * w0.y, b = w0.x * w12.y, m = w12.x * w12.y, d = w3.x * w12.y, e = w12.x * w3.y;
    float3 r = previous.SampleLevel(bilinear, float2(t12.x, t0.y), 0).rgb * a
             + previous.SampleLevel(bilinear, float2(t0.x, t12.y), 0).rgb * b
             + previous.SampleLevel(bilinear, t12, 0).rgb * m
             + previous.SampleLevel(bilinear, float2(t3.x, t12.y), 0).rgb * d
             + previous.SampleLevel(bilinear, float2(t12.x, t3.y), 0).rgb * e;
    return max(r / (a + b + m + d + e), 0.0);
}

[numthreads(8, 8, 1)]
void stretch(uint3 id : SV_DispatchThreadID) {
    uint w, h, sw, sh;
    target.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;
    source.GetDimensions(sw, sh);
    float2 size = float2(sw, sh);
    float2 outSize = float2(w, h);
    // Where this output pixel is in the rendered area, unjittered, 0..1.
    float2 here = (float2(id.xy) + 0.5) * outputTexel;
    float2 uv = clamp(here * scale + jitterUV, 0.5 / size, scale - 0.5 / size);
    float4 scene = source.SampleLevel(bilinear, uv, 0);
    // The same place on the jittered render grid, in render pixels.
    float2 pixel = here * renderSize + jitterPx;
    int2 last = int2(renderSize) - 1;
    int2 center = clamp(int2(floor(pixel)), int2(0, 0), last);

    // 3x3: colour moments in YCoCg and the nearest depth.
    float3 m1 = 0.0, m2 = 0.0;
    float3 lo = 1e9, hi = -1e9;
    bool hasDepth = (inputs & 1u) != 0;
    float nearest = hasDepth ? nearness(depth.Load(int3(center, 0))) : 0.0;
    int2 nearestAt = center;
    [unroll] for (int y = -1; y <= 1; ++y) {
        [unroll] for (int x = -1; x <= 1; ++x) {
            int2 p = clamp(center + int2(x, y), int2(0, 0), last);
            float3 c = toYCoCg(compress(source.Load(int3(p, 0)).rgb));
            m1 += c;
            m2 += c * c;
            lo = min(lo, c);
            hi = max(hi, c);
            [branch] if (hasDepth) {
                float n = nearness(depth.Load(int3(p, 0)));
                if (n > nearest) {
                    nearest = n;
                    nearestAt = p;
                }
            }
        }
    }

    // Motion of the nearest texel: (previous - current) in output UV.
    float2 mv = 0.0;
    [branch] if ((inputs & 2u) != 0) {
        float2 raw;
        [branch] if ((inputs & 16u) != 0) {
            float2 shift = (float2(nearestAt) + 0.5 - pixel) * outSize / renderSize;
            int2 o = clamp(int2(id.xy) + int2(round(shift)), int2(0, 0), int2(w, h) - 1);
            raw = motion.Load(int3(o, 0)).xy;
        } else {
            raw = motion.Load(int3(nearestAt, 0)).xy;
        }
        mv = raw * mvScale + mvBias;
        // A NaN or a huge vector is no vector.
        if (!(abs(mv.x) < 4.0 && abs(mv.y) < 4.0)) mv = 0.0;
    }
    float2 there = here + mv;
    float motionPx = length(mv * outSize);
    float react = ((inputs & 4u) != 0) ? saturate(reactive.Load(int3(center, 0))) : 0.0;

    float3 cur = toYCoCg(compress(scene.rgb));
    float3 result = cur;
    float count = 1.0;
    bool rejected = true;
    float snap = 0.0;
    [branch] if (weight > 0.0 && all(there > 0.0) && all(there < 1.0)) {
        float n = previous.Load(int3(int2(there * outSize), 0)).a;
        [branch] if ((inputs & 32u) != 0) {
            float4 g = previousNear.Gather(bilinear, there);
            float before = max(max(g.x, g.y), max(g.z, g.w));
            // Something nearer was here and has gone: what history holds is it.
            if (before - nearest > depthTolerance * before + 1e-6) n = 0.0;
        }
        [branch] if (n > 0.0) {
            rejected = false;
            float3 old = toYCoCg(compress(historyAt(there, outSize) * exposureRatio));
            float3 mean = m1 / 9.0;
            float3 sigma = sqrt(max(m2 / 9.0 - mean * mean, 0.0));
            // Wide while still (thin wires survive), tight in motion.
            float gamma = lerp(1.5, 0.75, saturate(motionPx / max(motionHalf, 1e-3)));
            float3 boxLo = max(mean - gamma * sigma, lo);
            float3 boxHi = min(mean + gamma * sigma, hi);
            boxLo = min(boxLo, cur);
            boxHi = max(boxHi, cur);
            float3 clipped = clipToBox(old, boxLo, boxHi);
            snap = saturate(abs(clipped.x - old.x) * changeGain);
            // The weight rule of `history::weight`.
            n = n * (1.0 - snap) * (1.0 - react);
            float wh = min(weight, n / (n + 1.0)) * motionHalf / (motionHalf + motionPx);
            // Still pixels take the nearest jittered sample, weighted by how
            // close its centre is: real sub-pixel detail accumulates. Moving
            // ones keep the bilinear sample.
            float sharp = saturate(wh / max(weight, 1e-3));
            float2 off = float2(center) + 0.5 - pixel;
            float conf = lerp(1.0, exp(-2.0 * dot(off, off)), sharp);
            float3 nearestColour = toYCoCg(compress(source.Load(int3(center, 0)).rgb));
            float3 now = lerp(cur, nearestColour, sharp);
            float a = (1.0 - wh) * conf / max((1.0 - wh) * conf + wh, 1e-6);
            result = lerp(clipped, now, a);
            count = min(n + 1.0, maxCount);
        }
    }
    float3 colour = expand(fromYCoCg(result));
    target[id.xy] = float4(colour, scene.a);
    nextHistory[id.xy] = float4(colour, count);
    nextNear[id.xy] = nearest;
    [branch] if (stats != 0 && (id.x & 7u) == 0 && (id.y & 7u) == 0) {
        counters.InterlockedAdd(0, 1u);
        if (rejected) counters.InterlockedAdd(4, 1u);
        counters.InterlockedAdd(8, uint(min(motionPx, 64.0) * 4.0 + 0.5));
        if (snap > 0.5) counters.InterlockedAdd(12, 1u);
    }
}
