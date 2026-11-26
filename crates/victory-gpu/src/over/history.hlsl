cbuffer Frame : register(b0) {
    float2 scale, outputTexel;
    float2 jitterUV;
    float weight, exposureRatio;
    float clampHistory;
};
Texture2D<float4> source : register(t0);
Texture2D<float4> previous : register(t1);
RWTexture2D<float4> target : register(u0);
RWTexture2D<float4> nextHistory : register(u1);
SamplerState bilinear : register(s0);

[numthreads(8, 8, 1)]
void stretch(uint3 id : SV_DispatchThreadID) {
    uint w, h, sw, sh;
    target.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;
    source.GetDimensions(sw, sh);
    float2 size = float2(sw, sh);
    float2 uv = (float2(id.xy) + 0.5) * outputTexel * scale + jitterUV;
    uv = clamp(uv, 0.5 / size, scale - 0.5 / size);
    float4 scene = source.SampleLevel(bilinear, uv, 0);
    [branch] if (weight > 0) {
        float3 old = previous.Load(int3(id.xy, 0)).rgb * exposureRatio;
        [branch] if (clampHistory > 0) {
            int2 center = int2(uv * size);
            int2 last = int2(round(scale * size)) - 1;
            float3 lo = scene.rgb, hi = scene.rgb;
            [unroll] for (int y = -1; y <= 1; ++y) {
                [unroll] for (int x = -1; x <= 1; ++x) {
                    int2 p = clamp(center + int2(x, y), int2(0, 0), last);
                    float3 c = source.Load(int3(p, 0)).rgb;
                    lo = min(lo, c);
                    hi = max(hi, c);
                }
            }
            old = clamp(old, lo, hi);
        }
        scene.rgb = lerp(scene.rgb, old, weight);
    }
    target[id.xy] = scene;
    nextHistory[id.xy] = scene;
}
