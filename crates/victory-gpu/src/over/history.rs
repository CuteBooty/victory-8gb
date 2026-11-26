//! Scene history at the FidelityFX dispatch, before either present plane is composed.
use super::*;
use crate::host::{HostList, transition};
use crate::list::UpscaleFrame;
use std::sync::{LazyLock, OnceLock};

pub(super) const SOURCE: &str = include_str!("history.hlsl");
/// `VICTORY_UPSCALE_HISTORY_MV=2`: reprojected accumulation.
pub(super) const TEMPORAL: &str = include_str!("temporal.hlsl");
#[cfg(feature = "diagnostics")]
pub(crate) const TIMER: &str = "upscale history pass";

/// The switches, read once.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Settings {
    /// `VICTORY_UPSCALE_HISTORY`: the highest history weight, 0..1.
    pub weight: f32,
    /// `VICTORY_UPSCALE_HISTORY_MV`: 0 plain blend, 1 clamped blend, 2
    /// reprojected accumulation (motion, depth, reactive).
    pub mode: u8,
    /// `VICTORY_UPSCALE_MV_JITTERED=0/1`: overrides the context's
    /// jitter-cancellation flag (FSR default when neither: not jittered).
    pub jittered: Option<bool>,
    /// `VICTORY_UPSCALE_DEPTH_INVERTED=0/1`: overrides the context's flag
    /// (FSR default when neither: not inverted).
    pub depth_inverted: Option<bool>,
    /// `VICTORY_UPSCALE_MV_SCALE=x,y`: multiplies the dispatch's
    /// motionVectorScale (`-1,-1` flips the vectors, `0,0` turns them off).
    pub mv_scale: [f32; 2],
}

fn parse_settings(get: impl Fn(&str) -> Option<String>) -> Option<Settings> {
    let weight: f32 = get("VICTORY_UPSCALE_HISTORY")?.trim().parse().ok()?;
    if !(weight > 0.0 && weight <= 1.0) {
        return None;
    }
    // Level one predates reprojection: the clamp alone.
    let mode = match get("VICTORY_UPSCALE_HISTORY_MV").as_deref().map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    };
    let flag = |name: &str| match get(name).as_deref().map(str::trim) {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    };
    let mv_scale = get("VICTORY_UPSCALE_MV_SCALE")
        .and_then(|v| {
            let mut it = v.split(',').map(|s| s.trim().parse::<f32>());
            match (it.next(), it.next(), it.next()) {
                (Some(Ok(x)), Some(Ok(y)), None) if x.is_finite() && y.is_finite() => Some([x, y]),
                _ => None,
            }
        })
        .unwrap_or([1.0, 1.0]);
    Some(Settings {
        weight,
        mode,
        jittered: flag("VICTORY_UPSCALE_MV_JITTERED"),
        depth_inverted: flag("VICTORY_UPSCALE_DEPTH_INVERTED"),
        mv_scale,
    })
}

pub(crate) fn settings() -> Option<Settings> {
    static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
    *SETTINGS.get_or_init(|| parse_settings(|k| std::env::var(k).ok()))
}

/// Whether the dispatch's depth, motion and reactive inputs are wanted.
pub(crate) fn wants_inputs() -> bool {
    settings().is_some_and(|s| s.mode == 2)
}

// FidelityFX upscale context flags (ffx_upscale.h, FfxApiCreateContextUpscaleFlags).
pub(crate) const FLAG_DISPLAY_RESOLUTION_MOTION_VECTORS: u32 = 1 << 1;
pub(crate) const FLAG_MOTION_VECTORS_JITTER_CANCELLATION: u32 = 1 << 2;
pub(crate) const FLAG_DEPTH_INVERTED: u32 = 1 << 3;

/// Tuning of the per-pixel weight; the shader receives these.
const MOTION_HALF_PX: f32 = 12.0;
const DEPTH_TOLERANCE: f32 = 0.1;
const CHANGE_GAIN: f32 = 4.0;
const MAX_COUNT: f32 = 16.0;

/// The per-pixel history weight and the count stored for the next frame,
/// as temporal.hlsl computes them: `count` frames accumulated so far (0 =
/// rejected), `motion_px` the reprojection length in output pixels,
/// `snap` the colour change seen by the clip (0..1), `reactive` the mask.
/// Still, unchanged pixels ramp up to `cap`; motion, change and the mask
/// pull the weight down at once.
#[cfg_attr(not(test), allow(dead_code))]
fn weight(cap: f32, count: f32, motion_px: f32, snap: f32, reactive: f32) -> (f32, f32) {
    let n = count.max(0.0) * (1.0 - snap.clamp(0.0, 1.0)) * (1.0 - reactive.clamp(0.0, 1.0));
    let w = (n / (n + 1.0)).min(cap) * MOTION_HALF_PX / (MOTION_HALF_PX + motion_px.max(0.0));
    (w, (n + 1.0).min(MAX_COUNT))
}

/// How a raw motion vector becomes (previous - current) in output UV:
/// `uv_offset = raw * scale + bias`, the FSR convention. `motion_scale`
/// (from the dispatch, times the user's) turns the stored value into pixels
/// of the motion texture's space; jittered vectors carry
/// `previous jitter - current jitter`, which the bias takes out.
fn motion_to_uv(
    motion_scale: [f32; 2],
    space: [u32; 2],
    render: [u32; 2],
    jittered: bool,
    jitter: [f32; 2],
    previous_jitter: [f32; 2],
) -> Option<([f32; 2], [f32; 2])> {
    if space.contains(&0) || render.contains(&0) {
        return None;
    }
    let scale = [0, 1].map(|i| motion_scale[i] / space[i] as f32);
    let bias = [0, 1].map(|i| {
        if jittered {
            -(previous_jitter[i] - jitter[i]) / render[i] as f32
        } else {
            0.0
        }
    });
    (scale.iter().chain(&bias).all(|v| v.is_finite())).then_some((scale, bias))
}

/// One optional input of the dispatch: a host texture and the format its
/// view reads it as.
pub(crate) struct Input<'a> {
    resource: &'a ID3D12Resource,
    format: DXGI_FORMAT,
    size: [u32; 2],
}

impl<'a> Input<'a> {
    /// `title` is the title's own format when the host copy is typeless.
    pub(crate) fn new(resource: &'a ID3D12Resource, title: Option<u32>) -> Option<Self> {
        // SAFETY: a live resource.
        let desc = unsafe { resource.GetDesc() };
        let ok = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && desc.SampleDesc.Count == 1
            && desc.DepthOrArraySize == 1
            && !desc.Flags.contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE);
        ok.then(|| Input {
            resource,
            format: DXGI_FORMAT(read_format(desc.Format.0 as u32, title) as i32),
            size: [desc.Width as u32, desc.Height],
        })
    }
}

/// The dispatch's optional inputs, those the host holds.
#[derive(Default)]
pub(crate) struct Inputs<'a> {
    pub depth: Option<Input<'a>>,
    pub motion: Option<Input<'a>>,
    pub reactive: Option<Input<'a>>,
}

/// The format a shader reads an input as: depth formats as their red
/// channel, typeless ones as the title's format or the usual typed one.
fn read_format(host: u32, title: Option<u32>) -> u32 {
    fn depth(f: u32) -> Option<u32> {
        Some(match f {
            19..=21 => 21,
            39..=41 => 41,
            44..=46 => 46,
            53 | 55 => 56,
            _ => return None,
        })
    }
    depth(host)
        .or_else(|| title.map(|t| depth(t).unwrap_or(t)))
        .unwrap_or(match host {
            1 => 2,
            9 => 10,
            15 => 16,
            23 => 24,
            27 => 28,
            33 => 34,
            60 => 61,
            f => f,
        })
}

// FFX jitter is a projection displacement in render pixels. Source texel
// center P represents the unjittered position P - jitter, hence the lookup
// for U is U + jitter. Padded textures normalize by the backing dimensions.
fn jitter_uv(jitter: [f32; 2], backing: [u32; 2]) -> Option<[f32; 2]> {
    (jitter.iter().all(|v| v.is_finite()) && !backing.contains(&0))
        .then(|| [0, 1].map(|i| jitter[i] / backing[i] as f32))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Geometry {
    context: usize,
    backing: [u32; 2],
    rendered: [u32; 2],
    output: [u32; 2],
    upscale_size: [u32; 2],
    formats: [DXGI_FORMAT; 2],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    First,
    Resize,
    Context,
    Cut,
    Reseed,
    Blend,
}

#[derive(Default)]
struct Sequence {
    geometry: Option<Geometry>,
    previous: Option<usize>,
    exposure: f32,
    /// The jitter of the recorded frame, for jittered motion vectors.
    jitter: [f32; 2],
    /// Whether the recorded frame wrote a nearness history (it had depth).
    nearness: bool,
}

impl Sequence {
    fn plan(&self, key: Geometry, cut: bool) -> (usize, Action) {
        let action = match self.geometry {
            None => Action::First,
            Some(old) if old.context != key.context => Action::Context,
            Some(old) if old != key => Action::Resize,
            _ if cut => Action::Cut,
            _ if self.previous.is_none() => Action::Reseed,
            _ => Action::Blend,
        };
        (self.previous.map_or(0, |i| 1 - i), action)
    }

    fn recorded(&mut self, key: Geometry, index: usize, exposure: f32) {
        self.geometry = Some(key);
        self.previous = Some(index);
        self.exposure = exposure;
    }

    fn failed(&mut self) {
        self.previous = None;
    }
}

struct Targets {
    size: [u32; 2],
    format: DXGI_FORMAT,
    pair: [ID3D12Resource; 2],
    bytes: u64,
}

fn targets(
    device: &ID3D12Device,
    size: [u32; 2],
    format: DXGI_FORMAT,
) -> windows::core::Result<Targets> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(size[0]),
        Height: size[1],
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let make = || -> windows::core::Result<ID3D12Resource> {
        let mut resource = None;
        // SAFETY: one ordinary, default-heap texture with a typed SRV/UAV.
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                None,
                &mut resource,
            )?;
        }
        resource.ok_or_else(windows::core::Error::empty)
    };
    let pair = [make()?, make()?];
    let bytes = unsafe { device.GetResourceAllocationInfo(0, &[desc]).SizeInBytes } * 2;
    Ok(Targets {
        size,
        format,
        pair,
        bytes,
    })
}

/// R16G16B16A16_FLOAT: the colour history of level two (alpha holds the
/// frames accumulated), independent of the output's format.
const HISTORY_FORMAT: DXGI_FORMAT = DXGI_FORMAT(10);
/// R32_FLOAT: the nearest-depth history of level two.
const NEARNESS_FORMAT: DXGI_FORMAT = DXGI_FORMAT(41);
/// Counters slots copied back: the newest finished one is read.
const COUNTER_SLOTS: u64 = 8;

/// What level two keeps besides the colour pair: the nearness pair and
/// monotonic GPU counters (samples, rejected, motion in quarter pixels,
/// snapped) with a read-back ring.
struct Temporal {
    nearness: Targets,
    counters: ID3D12Resource,
    readback: ID3D12Resource,
    slot: u64,
    last: [u32; 4],
}

fn buffer(
    device: &ID3D12Device,
    bytes: u64,
    heap: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut resource = None;
    // SAFETY: one plain buffer.
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: heap,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut resource,
        )?;
    }
    resource.ok_or_else(windows::core::Error::empty)
}

fn temporal(device: &ID3D12Device, size: [u32; 2]) -> windows::core::Result<Temporal> {
    Ok(Temporal {
        nearness: targets(device, size, NEARNESS_FORMAT)?,
        counters: buffer(
            device,
            16,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        )?,
        readback: buffer(
            device,
            16 * COUNTER_SLOTS,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )?,
        slot: 0,
        last: [0; 4],
    })
}

/// Shader resource views of level two, in table order.
const TEMPORAL_SRVS: u32 = 6;
/// Unordered access views of level two, after the shader resource views.
const TEMPORAL_UAVS: u32 = 4;
/// Root constants of temporal.hlsl's `Frame`.
const TEMPORAL_WORDS: usize = 22;

fn make_temporal(device: &ID3D12Device) -> windows::core::Result<Stretch> {
    use windows::core::s;
    let cs = crate::host::compile(TEMPORAL, s!("stretch"), s!("cs_5_1"))?;
    let views = [
        D3D12_DESCRIPTOR_RANGE {
            NumDescriptors: TEMPORAL_SRVS,
            ..range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0)
        },
        D3D12_DESCRIPTOR_RANGE {
            NumDescriptors: TEMPORAL_UAVS,
            ..range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, TEMPORAL_SRVS)
        },
    ];
    let words = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: 0,
                RegisterSpace: 0,
                Num32BitValues: TEMPORAL_WORDS as u32,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let root = root_signature(
        device,
        &[words, table(&views, D3D12_SHADER_VISIBILITY_ALL)],
        D3D12_SHADER_VISIBILITY_ALL,
        D3D12_ROOT_SIGNATURE_FLAG_NONE,
    )?;
    // SAFETY: the description points at live data and never releases its
    // pointer.
    unsafe {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.as_ptr().cast(),
                BytecodeLength: cs.len(),
            },
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = device.CreateComputePipelineState(&desc)?;
        Ok(Stretch {
            root,
            pipeline,
            step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            views: stretch_heap(device, TEMPORAL_SRVS + TEMPORAL_UAVS)?,
            next: 0,
        })
    }
}

/// A 2D texture view of `resource` (or a null view) as `format`.
fn srv(format: DXGI_FORMAT) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                ..Default::default()
            },
        },
    }
}

fn uav(format: DXGI_FORMAT) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    }
}

#[derive(Default)]
struct Window {
    blends: u32,
    first: u32,
    resize: u32,
    context: u32,
    cut: u32,
    failed: u32,
    jitter: Option<([f32; 2], [f32; 2])>,
    timed: u32,
    ms: f64,
    /// Level two: dispatches that brought depth, motion, reactive.
    depth: u32,
    motion: u32,
    reactive: u32,
    /// Level two: the last motion conversion and context flags used.
    motion_uv: Option<([f32; 2], [f32; 2])>,
    raw_motion_scale: Option<[f32; 2]>,
    flags: Option<u32>,
    conventions: Option<(bool, bool, bool)>,
}

impl Window {
    fn observe(&mut self, j: [f32; 2]) {
        if j.iter().all(|v| v.is_finite()) {
            let (lo, hi) = self.jitter.get_or_insert((j, j));
            for i in 0..2 {
                lo[i] = lo[i].min(j[i]);
                hi[i] = hi[i].max(j[i]);
            }
        }
    }
}

#[derive(Default)]
struct History {
    pass: Option<Stretch>,
    targets: Option<Targets>,
    sequence: Sequence,
    window: Window,
    temporal: Option<Temporal>,
}

static HISTORY: LazyLock<Mutex<History>> = LazyLock::new(|| Mutex::new(History::default()));

impl History {
    fn failed(&mut self, frame: UpscaleFrame) {
        self.sequence.failed();
        self.window.failed += 1;
        self.window.observe(frame.jitter);
    }

    fn prepare(&mut self, device: &ID3D12Device, key: Geometry, level_two: bool) -> windows::core::Result<()> {
        if self.pass.is_none() {
            self.pass = Some(if level_two { make_temporal(device)? } else { make_stretch(device, true)? });
        }
        let format = if level_two { HISTORY_FORMAT } else { key.formats[1] };
        if self
            .targets
            .as_ref()
            .is_none_or(|b| b.size != key.output || b.format != format)
        {
            self.targets = Some(targets(device, key.output, format)?);
        }
        if level_two && self.temporal.as_ref().is_none_or(|t| t.nearness.size != key.output) {
            self.temporal = Some(temporal(device, key.output)?);
            // A new nearness pair holds nothing of the previous frame.
            self.sequence.nearness = false;
        }
        if let Some(p) = self.pass.as_mut()
            && p.next == STRETCH_RING
        {
            // Recorded lists retain the old descriptor slab until completion.
            let views = if level_two { TEMPORAL_SRVS + TEMPORAL_UAVS } else { 4 };
            p.views = stretch_heap(device, views)?;
            p.next = 0;
        }
        Ok(())
    }
}

pub(crate) fn failed(frame: UpscaleFrame) {
    HISTORY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .failed(frame);
}

pub(crate) fn record(
    device: &ID3D12Device,
    list: &HostList,
    target: &ID3D12Resource,
    source: &ID3D12Resource,
    render: [u32; 2],
    frame: UpscaleFrame,
    inputs: Inputs<'_>,
) -> bool {
    let Some(Settings { weight, mode, .. }) = settings() else {
        return false;
    };
    let clamp = mode == 1;
    let level_two = mode == 2;
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let (to, from) = unsafe { (target.GetDesc(), source.GetDesc()) };
    let key = Geometry {
        context: frame.context,
        backing: [from.Width as u32, from.Height],
        rendered: rendered_size(render, &from),
        output: [to.Width as u32, to.Height],
        upscale_size: frame.upscale_size,
        formats: [typed(from.Format), typed(to.Format)],
    };
    let Some(offset) = jitter_uv(frame.jitter, key.backing) else {
        h.failed(frame);
        return false;
    };
    if key.output.contains(&0)
        || !to
            .Flags
            .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
        || !frame.pre_exposure.is_finite()
        || frame.pre_exposure < 0.0
    {
        h.failed(frame);
        return false;
    }
    if let Err(error) = h.prepare(device, key, level_two) {
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(
            || tracing::warn!(%error, "upscale history unavailable, using plain stretch"),
        );
        h.failed(frame);
        return false;
    }
    let (write, action) = h.sequence.plan(key, frame.reset);
    let exposure = if frame.pre_exposure == 0.0 {
        1.0
    } else {
        frame.pre_exposure
    };
    let ratio = if action == Action::Blend {
        exposure / h.sequence.exposure
    } else {
        1.0
    };
    if !ratio.is_finite() {
        h.failed(frame);
        return false;
    }
    if level_two {
        let blend = action == Action::Blend;
        record_temporal(
            &mut h, device, list, (target, &to), (source, &from), key, frame, &inputs,
            if blend { weight } else { 0.0 }, ratio, write,
        );
        h.sequence.recorded(key, write, exposure);
        h.sequence.jitter = frame.jitter;
        h.sequence.nearness = inputs.depth.is_some();
        h.window.observe(frame.jitter);
        h.window.count(action);
        return true;
    }
    let History {
        pass,
        targets,
        sequence,
        window,
        ..
    } = &mut *h;
    let p = pass.as_mut().expect("prepared stretch pipeline");
    let b = targets.as_ref().expect("prepared history textures");
    let [sx, sy, tx, ty] = stretch_sizes(key.rendered, &to, &from);
    let constants = [
        sx,
        sy,
        tx,
        ty,
        offset[0],
        offset[1],
        if action == Action::Blend { weight } else { 0.0 },
        ratio,
        u8::from(clamp) as f32,
    ];
    let (previous, next) = (&b.pair[1 - write], &b.pair[write]);
    list.keep(previous);
    list.keep(next);
    list.keep(&p.views);
    let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    #[cfg(feature = "diagnostics")]
    list.note_op(|| TIMER.to_owned());
    // Both textures start and finish each list readable, including on a reseed.
    unsafe { list.list.ResourceBarrier(&[transition(next, read, uav)]) };
    dispatch_stretch(
        p,
        device,
        &list.list,
        (target, &to),
        (source, &from),
        &constants,
        Some((previous, next)),
    );
    unsafe { list.list.ResourceBarrier(&[transition(next, uav, read)]) };
    #[cfg(feature = "diagnostics")]
    list.note_op(|| "end upscale history".into());
    sequence.recorded(key, write, exposure);
    window.observe(frame.jitter);
    window.count(action);
    true
}

impl Window {
    fn count(&mut self, action: Action) {
        match action {
            Action::Blend => self.blends += 1,
            Action::First => self.first += 1,
            Action::Cut => self.cut += 1,
            Action::Resize => self.resize += 1,
            Action::Context => self.context += 1,
            Action::Reseed => {} // counted when the preceding dispatch failed
        }
    }
}

/// The conventions of this context: (jittered vectors, inverted depth,
/// display-resolution vectors). The user's switches first, then the flags
/// seen at context creation, then FSR's defaults; display resolution is
/// guessed from the motion texture's size when no flags were seen.
fn conventions(s: &Settings, flags: Option<u32>, motion_size: Option<[u32; 2]>, render: [u32; 2], upscale: [u32; 2]) -> (bool, bool, bool) {
    let flag = |bit: u32| flags.map(|f| f & bit != 0);
    let jittered = s.jittered.or(flag(FLAG_MOTION_VECTORS_JITTER_CANCELLATION)).unwrap_or(false);
    let inverted = s.depth_inverted.or(flag(FLAG_DEPTH_INVERTED)).unwrap_or(false);
    let display = flag(FLAG_DISPLAY_RESOLUTION_MOTION_VECTORS).unwrap_or_else(|| {
        motion_size.is_some_and(|m| {
            m[0] >= upscale[0] && m[1] >= upscale[1] && (m[0] > render[0] || m[1] > render[1])
        })
    });
    (jittered, inverted, display)
}

#[allow(clippy::too_many_arguments)]
fn record_temporal(
    h: &mut History,
    device: &ID3D12Device,
    list: &HostList,
    (target, to): (&ID3D12Resource, &D3D12_RESOURCE_DESC),
    (source, from): (&ID3D12Resource, &D3D12_RESOURCE_DESC),
    key: Geometry,
    frame: UpscaleFrame,
    inputs: &Inputs<'_>,
    weight: f32,
    ratio: f32,
    write: usize,
) {
    let s = settings().expect("level two is set");
    let History {
        pass,
        targets,
        sequence,
        window,
        temporal,
    } = h;
    let p = pass.as_mut().expect("prepared temporal pipeline");
    let b = targets.as_ref().expect("prepared history textures");
    let t = temporal.as_mut().expect("prepared nearness and counters");
    let upscale = if frame.upscale_size.contains(&0) { key.output } else { frame.upscale_size };
    let (jittered, inverted, display) = conventions(
        &s, frame.flags, inputs.motion.as_ref().map(|m| m.size), key.rendered, upscale,
    );
    let motion_scale = [0, 1].map(|i| frame.motion_scale[i] * s.mv_scale[i]);
    let space = if display { upscale } else { key.rendered };
    let motion = inputs.motion.as_ref().and_then(|_| {
        motion_to_uv(motion_scale, space, key.rendered, jittered, frame.jitter, sequence.jitter)
    });
    let previous_nearness = weight > 0.0 && sequence.nearness && inputs.depth.is_some();
    let bits = u32::from(inputs.depth.is_some())
        | u32::from(motion.is_some()) << 1
        | u32::from(inputs.reactive.is_some()) << 2
        | u32::from(inverted) << 3
        | u32::from(display) << 4
        | u32::from(previous_nearness) << 5;
    let (mv_scale, mv_bias) = motion.unwrap_or(([0.0; 2], [0.0; 2]));
    let [sx, sy, tx, ty] = stretch_sizes(key.rendered, to, from);
    let jitter_uv = [0, 1].map(|i| frame.jitter[i] / key.backing[i] as f32);
    let constants: [f32; TEMPORAL_WORDS] = [
        sx,
        sy,
        tx,
        ty,
        jitter_uv[0],
        jitter_uv[1],
        weight,
        ratio,
        key.rendered[0] as f32,
        key.rendered[1] as f32,
        frame.jitter[0],
        frame.jitter[1],
        mv_scale[0],
        mv_scale[1],
        mv_bias[0],
        mv_bias[1],
        MOTION_HALF_PX,
        DEPTH_TOLERANCE,
        CHANGE_GAIN,
        MAX_COUNT,
        f32::from_bits(bits),
        f32::from_bits(u32::from(weight > 0.0)),
    ];
    window.depth += u32::from(inputs.depth.is_some());
    window.motion += u32::from(inputs.motion.is_some());
    window.reactive += u32::from(inputs.reactive.is_some());
    window.motion_uv = motion.or(window.motion_uv);
    window.raw_motion_scale = Some(frame.motion_scale);
    window.flags = frame.flags.or(window.flags);
    window.conventions = Some((jittered, inverted, display));

    let (previous, next) = (&b.pair[1 - write], &b.pair[write]);
    let (near_before, near_next) = (&t.nearness.pair[1 - write], &t.nearness.pair[write]);
    for r in [previous, next, near_before, near_next, &t.counters, &t.readback] {
        list.keep(r);
    }
    list.keep(&p.views);
    let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let unordered = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    #[cfg(feature = "diagnostics")]
    list.note_op(|| TIMER.to_owned());
    let slot = (p.next % STRETCH_RING) * (TEMPORAL_SRVS + TEMPORAL_UAVS);
    p.next = p.next.wrapping_add(1);
    let null_formats = [DXGI_FORMAT(41), DXGI_FORMAT(34), DXGI_FORMAT(61)];
    // SAFETY: an open list; slots of our heap, not reused before the lists
    // holding them ran (see `prepare`); live resources.
    unsafe {
        // The history textures start and finish each list readable; the
        // counters buffer is promoted from common by the dispatch.
        list.list.ResourceBarrier(&[transition(next, read, unordered), transition(near_next, read, unordered)]);
        let cpu = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetCPUDescriptorHandleForHeapStart().ptr + ((slot + i) * p.step) as usize,
        };
        let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetGPUDescriptorHandleForHeapStart().ptr + u64::from(slot * p.step),
        };
        device.CreateShaderResourceView(source, Some(&srv(typed(from.Format))), cpu(0));
        device.CreateShaderResourceView(previous, Some(&srv(HISTORY_FORMAT)), cpu(1));
        for (i, input) in [&inputs.depth, &inputs.motion, &inputs.reactive].into_iter().enumerate() {
            match input {
                Some(input) => {
                    device.CreateShaderResourceView(input.resource, Some(&srv(input.format)), cpu(2 + i as u32))
                }
                None => device.CreateShaderResourceView(
                    None::<&ID3D12Resource>,
                    Some(&srv(null_formats[i])),
                    cpu(2 + i as u32),
                ),
            }
        }
        device.CreateShaderResourceView(near_before, Some(&srv(NEARNESS_FORMAT)), cpu(5));
        device.CreateUnorderedAccessView(target, None, Some(&uav(typed(to.Format))), cpu(6));
        device.CreateUnorderedAccessView(next, None, Some(&uav(HISTORY_FORMAT)), cpu(7));
        device.CreateUnorderedAccessView(near_next, None, Some(&uav(NEARNESS_FORMAT)), cpu(8));
        let raw = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT(39), // R32_TYPELESS
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: 4,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        device.CreateUnorderedAccessView(&t.counters, None, Some(&raw), cpu(9));
        list.list.SetDescriptorHeaps(&[Some(p.views.clone())]);
        list.list.SetComputeRootSignature(&p.root);
        list.list.SetPipelineState(&p.pipeline);
        list.list.SetComputeRoot32BitConstants(0, TEMPORAL_WORDS as u32, constants.as_ptr().cast(), 0);
        list.list.SetComputeRootDescriptorTable(1, gpu);
        list.list.Dispatch(key.output[0].div_ceil(8), key.output[1].div_ceil(8), 1);
        list.list.ResourceBarrier(&[
            transition(next, unordered, read),
            transition(near_next, unordered, read),
            transition(&t.counters, unordered, copy),
        ]);
        list.list.CopyBufferRegion(&t.readback, 16 * (t.slot % COUNTER_SLOTS), &t.counters, 0, 16);
        list.list.ResourceBarrier(&[transition(&t.counters, copy, unordered)]);
    }
    t.slot = t.slot.wrapping_add(1);
    #[cfg(feature = "diagnostics")]
    list.note_op(|| "end upscale history".into());
}

/// The newest counters in the read-back ring: the slot with the most
/// samples (the counters only grow; a slot the GPU has not reached yet holds
/// an older value).
fn newest(slots: &[[u32; 4]]) -> Option<[u32; 4]> {
    slots.iter().copied().max_by_key(|s| s[0])
}

/// Counters over a log window: (samples, rejected fraction, mean motion in
/// output pixels, snapped fraction).
fn window_stats(before: [u32; 4], now: [u32; 4]) -> Option<(u32, f64, f64, f64)> {
    let d = [0, 1, 2, 3].map(|i| now[i].wrapping_sub(before[i]));
    (d[0] > 0).then(|| {
        let n = f64::from(d[0]);
        (d[0], f64::from(d[1]) / n, f64::from(d[2]) / 4.0 / n, f64::from(d[3]) / n)
    })
}

fn read_counters(t: &mut Temporal) -> Option<(u32, f64, f64, f64)> {
    let mut data = std::ptr::null_mut();
    // SAFETY: a read-back buffer of ours, 16 bytes a slot, mapped for
    // reading (diagnostics: a slot the GPU is writing reads stale or torn).
    let slots: Vec<[u32; 4]> = unsafe {
        t.readback.Map(0, None, Some(&mut data)).ok()?;
        if data.is_null() {
            return None;
        }
        let words = std::slice::from_raw_parts(data.cast::<u32>(), 4 * COUNTER_SLOTS as usize);
        let slots = words.chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();
        t.readback.Unmap(0, None);
        slots
    };
    let now = newest(&slots)?;
    let stats = window_stats(t.last, now);
    t.last = now;
    stats
}

#[cfg(feature = "diagnostics")]
pub(crate) fn timed(ms: f64) {
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    h.window.timed += 1;
    h.window.ms += ms;
}

pub(crate) fn log_interval(frames: u64) {
    let Some(s) = settings() else {
        return;
    };
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let w = std::mem::take(&mut h.window);
    let mib = h.targets.as_ref().map_or(0, |b| b.bytes)
        + h.temporal.as_ref().map_or(0, |t| t.nearness.bytes);
    tracing::info!(
        weight = s.weight, clamp = (s.mode == 1), mode = s.mode,
        blends_per_frame = w.blends as f64 / frames.max(1) as f64,
        first = w.first, resize = w.resize, context = w.context, cut = w.cut, failure = w.failed,
        jitter_min_max_px = ?w.jitter, gpu_ms_per_pass = ?(w.timed > 0).then(|| w.ms / f64::from(w.timed)),
        history_mib = mib as f64 / 1048576.0,
        "upscale scene history"
    );
    if s.mode == 2 {
        let stats = h.temporal.as_mut().and_then(read_counters);
        let dispatches = w.blends + w.first + w.resize + w.context + w.cut;
        tracing::info!(
            dispatches,
            with_depth = w.depth, with_motion = w.motion, with_reactive = w.reactive,
            context_flags = ?w.flags.map(|f| format!("{f:#x}")),
            jittered_inverted_display = ?w.conventions,
            motion_vector_scale = ?w.raw_motion_scale, user_mv_scale = ?s.mv_scale,
            mv_uv_scale_bias = ?w.motion_uv,
            sampled_px = ?stats.map(|x| x.0),
            rejected_fraction = ?stats.map(|x| x.1),
            mean_motion_px = ?stats.map(|x| x.2),
            snapped_fraction = ?stats.map(|x| x.3),
            "upscale temporal accumulation"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Geometry {
        Geometry {
            context: 1,
            backing: [1600, 800],
            rendered: [1280, 720],
            output: [1920, 1080],
            upscale_size: [1920, 1080],
            formats: [DXGI_FORMAT(10), DXGI_FORMAT(24)],
        }
    }

    #[test]
    fn opposite_projection_jitters_recover_the_same_feature_on_a_padded_texture() {
        let key = scene();
        let [sx, sy] = key.backing.map(|n| n as f32);
        let feature = [211.5, 93.5];
        for j in [[0.25, -0.375], [-0.25, 0.375]] {
            let [dx, dy] = jitter_uv(j, key.backing).unwrap();
            // Where the jittered projection of the stationary feature landed.
            let [w, h] = key.rendered.map(|n| n as f32);
            let clip = [2.0 * j[0] / w, -2.0 * j[1] / h];
            let image = [
                feature[0] + clip[0] * w * 0.5,
                feature[1] - clip[1] * h * 0.5,
            ];
            assert!(((feature[0] / sx + dx) * sx - image[0]).abs() < 0.0001);
            assert!(((feature[1] / sy + dy) * sy - image[1]).abs() < 0.0001);
        }
        assert!(jitter_uv([f32::NAN, 0.0], key.backing).is_none());
        assert!(jitter_uv([0.0; 2], [0, 720]).is_none());
    }

    #[test]
    fn only_matching_uninterrupted_frames_blend() {
        let key = scene();
        let mut s = Sequence::default();
        assert_eq!(s.plan(key, false).1, Action::First);
        s.recorded(key, 0, 1.0);
        assert_eq!(s.plan(key, false).1, Action::Blend);
        assert_eq!(s.plan(key, true).1, Action::Cut);
        for resized in [
            Geometry {
                rendered: [1200, 700],
                ..key
            },
            Geometry {
                backing: [1400, 800],
                ..key
            },
            Geometry {
                output: [1280, 720],
                ..key
            },
            Geometry {
                upscale_size: [1600, 900],
                ..key
            },
            Geometry {
                formats: [DXGI_FORMAT(10); 2],
                ..key
            },
        ] {
            assert_eq!(s.plan(resized, false).1, Action::Resize);
        }
        assert_eq!(
            s.plan(Geometry { context: 2, ..key }, false).1,
            Action::Context
        );
        s.failed();
        s.failed();
        assert_eq!(s.plan(key, false).1, Action::Reseed);
        s.recorded(key, 0, 1.0);
        assert_eq!(s.plan(key, false).1, Action::Blend);
    }

    #[test]
    fn history_alternates_only_after_recording() {
        let key = scene();
        let mut s = Sequence::default();
        for (expected, cut) in [(0, false), (1, false), (0, true), (1, false)] {
            let (slot, _) = s.plan(key, cut);
            assert_eq!(slot, expected);
            assert_ne!(Some(slot), s.previous);
            assert_eq!(s.plan(key, cut).0, slot);
            s.recorded(key, slot, 1.0);
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Option<Settings> {
        let pairs: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        parse_settings(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    #[test]
    fn levels_zero_and_one_stay_as_they_were_and_two_is_new() {
        assert!(env(&[]).is_none());
        assert!(env(&[("VICTORY_UPSCALE_HISTORY", "0")]).is_none());
        assert!(env(&[("VICTORY_UPSCALE_HISTORY", "1.5")]).is_none());
        let base = env(&[("VICTORY_UPSCALE_HISTORY", "0.85")]).unwrap();
        assert_eq!((base.weight, base.mode), (0.85, 0));
        assert_eq!(base.mv_scale, [1.0, 1.0]);
        assert_eq!((base.jittered, base.depth_inverted), (None, None));
        for (v, mode) in [("1", 1), ("2", 2), ("3", 0), ("yes", 0)] {
            let s = env(&[("VICTORY_UPSCALE_HISTORY", "0.9"), ("VICTORY_UPSCALE_HISTORY_MV", v)]).unwrap();
            assert_eq!(s.mode, mode, "{v}");
        }
        let s = env(&[
            ("VICTORY_UPSCALE_HISTORY", "0.9"),
            ("VICTORY_UPSCALE_HISTORY_MV", "2"),
            ("VICTORY_UPSCALE_MV_JITTERED", "1"),
            ("VICTORY_UPSCALE_DEPTH_INVERTED", "0"),
            ("VICTORY_UPSCALE_MV_SCALE", "-1, 0.5"),
        ])
        .unwrap();
        assert_eq!((s.jittered, s.depth_inverted, s.mv_scale), (Some(true), Some(false), [-1.0, 0.5]));
        for bad in ["1", "1,2,3", "x,1", "nan,1"] {
            let s = env(&[("VICTORY_UPSCALE_HISTORY", "0.9"), ("VICTORY_UPSCALE_MV_SCALE", bad)]).unwrap();
            assert_eq!(s.mv_scale, [1.0, 1.0], "{bad}");
        }
    }

    #[test]
    fn a_still_pixel_converges_to_the_cap_and_motion_change_and_mask_pull_it_down() {
        // Still and unchanged: 1/2, 2/3, 3/4 ... up to the cap.
        let mut count = 1.0;
        let mut last = 0.0;
        for _ in 0..40 {
            let (w, next) = weight(0.9, count, 0.0, 0.0, 0.0);
            assert!(w >= last && w <= 0.9);
            last = w;
            count = next;
        }
        assert!((last - 0.9).abs() < 1e-6);
        assert_eq!(count, MAX_COUNT);
        // Motion halves it at MOTION_HALF_PX and keeps falling.
        let (still, _) = weight(0.9, MAX_COUNT, 0.0, 0.0, 0.0);
        let (half, _) = weight(0.9, MAX_COUNT, MOTION_HALF_PX, 0.0, 0.0);
        let (fast, _) = weight(0.9, MAX_COUNT, 8.0 * MOTION_HALF_PX, 0.0, 0.0);
        assert!((half - still / 2.0).abs() < 1e-6);
        assert!(fast < half / 4.0);
        // A full snap or a fully reactive pixel takes the current frame and
        // starts counting again.
        assert_eq!(weight(0.9, MAX_COUNT, 0.0, 1.0, 0.0), (0.0, 1.0));
        assert_eq!(weight(0.9, MAX_COUNT, 0.0, 0.0, 1.0), (0.0, 1.0));
        // Rejected history (count 0) has no weight.
        assert_eq!(weight(0.9, 0.0, 0.0, 0.0, 0.0).0, 0.0);
    }

    #[test]
    fn jittered_vectors_of_a_still_scene_cancel_and_motion_maps_to_output_uv() {
        let render = [1280, 720];
        let (previous_jitter, jitter) = ([0.25, -0.375], [-0.125, 0.3125]);
        // The jittered image of a point is at true + jitter (the convention
        // `jitter_uv` uses), so a still point's jittered vector, in render
        // pixels, is previous jitter - current jitter.
        let raw = [0, 1].map(|i| previous_jitter[i] - jitter[i]);
        let (scale, bias) = motion_to_uv([1.0, 1.0], render, render, true, jitter, previous_jitter).unwrap();
        for i in 0..2 {
            assert!((raw[i] * scale[i] + bias[i]).abs() < 1e-7);
        }
        // Without jitter cancellation a still point's vector is zero.
        let (scale, bias) = motion_to_uv([1.0, 1.0], render, render, false, jitter, previous_jitter).unwrap();
        assert_eq!(bias, [0.0, 0.0]);
        // A point that was 6.4 render pixels left and 3.6 up: previous -
        // current is (-6.4, -3.6) pixels, (-0.005, -0.005) of the frame.
        let uv = [0, 1].map(|i| [-6.4, -3.6][i] * scale[i] + bias[i]);
        assert!((uv[0] + 0.005).abs() < 1e-6 && (uv[1] + 0.005).abs() < 1e-6);
        // Vectors in NDC halves (a common engine encoding) with the scale the
        // title passes, (w, -h): the y sign flips into texture space.
        let (scale, _) = motion_to_uv([1280.0, -720.0], render, render, false, jitter, previous_jitter).unwrap();
        let uv = [-0.005 * scale[0], 0.005 * scale[1]];
        assert!((uv[0] + 0.005).abs() < 1e-6 && (uv[1] + 0.005).abs() < 1e-6);
        // Display-resolution vectors divide by the output size.
        let (scale, _) = motion_to_uv([1.0, 1.0], [1920, 1080], render, false, jitter, previous_jitter).unwrap();
        assert!((scale[0] - 1.0 / 1920.0).abs() < 1e-9);
        assert!(motion_to_uv([1.0, 1.0], [0, 720], render, false, jitter, previous_jitter).is_none());
        assert!(motion_to_uv([f32::NAN, 1.0], render, render, false, jitter, previous_jitter).is_none());
    }

    #[test]
    fn conventions_come_from_the_switches_then_the_flags_then_fsr_defaults() {
        let s = env(&[("VICTORY_UPSCALE_HISTORY", "0.9"), ("VICTORY_UPSCALE_HISTORY_MV", "2")]).unwrap();
        let (render, upscale) = ([1280, 720], [1920, 1080]);
        assert_eq!(conventions(&s, None, Some(render), render, upscale), (false, false, false));
        assert_eq!(conventions(&s, None, Some(upscale), render, upscale), (false, false, true));
        let all = FLAG_MOTION_VECTORS_JITTER_CANCELLATION | FLAG_DEPTH_INVERTED;
        assert_eq!(conventions(&s, Some(all), Some(upscale), render, upscale), (true, true, false));
        let s = Settings { jittered: Some(false), depth_inverted: Some(false), ..s };
        assert_eq!(conventions(&s, Some(all), Some(render), render, upscale), (false, false, false));
    }

    #[test]
    fn inputs_are_read_as_typed_and_depth_formats_as_their_red_channel() {
        assert_eq!(read_format(39, Some(40)), 41); // R32_TYPELESS, title D32_FLOAT
        assert_eq!(read_format(44, None), 46); // R24G8_TYPELESS
        assert_eq!(read_format(19, Some(20)), 21);
        assert_eq!(read_format(33, Some(34)), 34); // R16G16_TYPELESS, title R16G16_FLOAT
        assert_eq!(read_format(33, None), 34);
        assert_eq!(read_format(34, None), 34);
        assert_eq!(read_format(60, None), 61);
        assert_eq!(read_format(1, Some(40)), 41);
    }

    #[test]
    fn counters_are_read_from_the_newest_slot_across_a_wrap() {
        assert_eq!(newest(&[[5, 0, 0, 0], [9, 1, 2, 3], [7, 0, 0, 0]]), Some([9, 1, 2, 3]));
        let before = [u32::MAX - 9, 5, u32::MAX, 0];
        let now = [90, 25, 4 * 300 - 1, 10];
        let (n, rejected, motion, snapped) = window_stats(before, now).unwrap();
        assert_eq!(n, 100);
        assert!((rejected - 0.2).abs() < 1e-9);
        assert!((motion - 3.0).abs() < 1e-9);
        assert!((snapped - 0.1).abs() < 1e-9);
        assert!(window_stats(now, now).is_none());
    }

    #[test]
    fn the_temporal_source_guards_its_writes_and_has_no_root_signature() {
        assert!(TEMPORAL.contains("numthreads(8, 8, 1)"));
        assert!(TEMPORAL.contains("if (id.x >= w || id.y >= h) return;"));
        assert!(!TEMPORAL.contains("RootSignature"));
        // The constants the shader declares are the words record_temporal sends.
        let frame = &TEMPORAL[TEMPORAL.find("cbuffer").unwrap()..TEMPORAL.find("};").unwrap()];
        let words: usize = frame
            .lines()
            .map(|l| l.split("//").next().unwrap().trim())
            .filter(|l| !l.is_empty() && !l.starts_with("cbuffer"))
            .map(|l| {
                let per = if l.starts_with("float2") { 2 } else { 1 };
                per * l.matches(',').count().saturating_add(1)
            })
            .sum();
        assert_eq!(words, TEMPORAL_WORDS);
    }
}
