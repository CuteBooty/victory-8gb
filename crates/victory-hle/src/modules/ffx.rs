//! `amd_fidelityfx_loader_xbox.dll`: the upscale dispatch.
//!
//! The title's Series S mode renders below its output size and calls the
//! console's FidelityFX upscaler to make the output. The loader and the
//! upscaler are the title's own DLLs and run as they are, but the
//! upscaler's passes are compute pipelines in a container we do not
//! translate: its output stayed black. `ffxDispatch` alone is taken here:
//! an upscale dispatch becomes a plain stretch of the colour input
//! (`victory_gpu::list::upscale_stretch`); anything else, and an upscale we
//! could not stretch, goes to the title's DLL.
//!
//! The structures are the public FidelityFX API's (`ffx_api.h`,
//! `ffx_upscale.h`): a header of a type and a next pointer, and resources of
//! a pointer, a 32-byte description and a state.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};


/// The DLL whose `ffxDispatch` is taken.
pub const LOADER: &str = "amd_fidelityfx_loader_xbox.dll";

static REAL: AtomicUsize = AtomicUsize::new(0);

/// The title's own `ffxDispatch`, for what is not stretched here.
pub fn set_real_dispatch(address: usize) {
    REAL.store(address, Ordering::Relaxed);
}

#[repr(C)]
struct Header {
    kind: u64,
    next: *const Header,
}

#[repr(C)]
struct Resource {
    resource: *const c_void,
    description: [u32; 8],
    state: u32,
}

#[repr(C)]
struct Upscale {
    header: Header,
    list: *const c_void,
    colour: Resource,
    depth: Resource,
    motion: Resource,
    exposure: Resource,
    reactive: Resource,
    composition: Resource,
    output: Resource,
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    render: [u32; 2],
    upscale: [u32; 2],
}

// Continuation of ffxDispatchDescUpscale. Read the tail only for temporal
// stretching. Use the low byte of the API boolean on this little-endian ABI.
#[cfg(any(windows, test))]
#[repr(C)]
struct TemporalUpscale {
    base: Upscale,
    enable_sharpening: u8,
    sharpness: f32,
    frame_time_delta: f32,
    pre_exposure: f32,
    reset: u8,
}

const _: () = assert!(size_of::<Resource>() == 48);
const _: () = assert!(std::mem::offset_of!(Upscale, output) == 0x138);

/// `FFX_API_RETURN_ERROR`: what a dispatch nothing could answer returns.
const ERROR: u32 = 1;

/// `FFX_API_DISPATCH_DESC_TYPE_UPSCALE`.
const UPSCALE: u64 = 0x0001_0001;

type Dispatch = extern "system" fn(*mut *mut c_void, *const Header) -> u32;

extern "system" fn dispatch(context: *mut *mut c_void, desc: *const Header) -> u32 {
    // SAFETY: the title passes a dispatch description, whose header says
    // what follows it.
    if let Some(header) = unsafe { desc.as_ref() } {
        static SEEN: AtomicU32 = AtomicU32::new(0);
        let first = SEEN.fetch_add(1, Ordering::Relaxed) < 4;
        if header.kind == UPSCALE {
            // SAFETY: an upscale description, by its type.
            let d = unsafe { &*desc.cast::<Upscale>() };
            if first {
                tracing::info!(
                    list = ?d.list,
                    colour = ?d.colour.resource,
                    colour_size = ?(d.colour.description[2], d.colour.description[3]),
                    output = ?d.output.resource,
                    output_size = ?(d.output.description[2], d.output.description[3]),
                    render = ?d.render,
                    upscale = ?d.upscale,
                    "upscale dispatch"
                );
            }
            #[cfg(windows)]
            {
                let frame = victory_gpu::list::upscale_history_enabled().then(|| {
                    // SAFETY: this is the public upscale dispatch, selected by its header.
                    let t = unsafe { &*desc.cast::<TemporalUpscale>() };
                    victory_gpu::list::UpscaleFrame {
                        // ffxContext is an opaque handle passed by address.
                        context: unsafe { context.as_ref() }.copied().unwrap_or_default() as usize,
                        upscale_size: d.upscale,
                        jitter: d.jitter,
                        reset: t.reset != 0,
                        pre_exposure: t.pre_exposure,
                        depth: d.depth.resource as usize,
                        motion: d.motion.resource as usize,
                        reactive: d.reactive.resource as usize,
                        motion_scale: d.motion_scale,
                        flags: unsafe { context.as_ref() }
                            .and_then(|c| context_flags(*c as usize)),
                    }
                });
                if victory_gpu::list::upscale_stretch_with_history(
                    d.list, d.colour.resource, d.output.resource, d.render, frame,
                ) {
                    return 0;
                }
            }
            if first {
                tracing::warn!("upscale dispatch not stretched: left to the title's upscaler");
            }
        } else if first {
            tracing::info!(
                kind = format_args!("{:#x}", header.kind),
                "another FidelityFX dispatch"
            );
        }
    }
    match REAL.load(Ordering::Relaxed) {
        // Neither stretched nor passed on: the output was not written, and
        // the title is told so instead of presenting it as it is.
        0 => {
            static TOLD: std::sync::Once = std::sync::Once::new();
            TOLD.call_once(|| {
                tracing::error!(
                    "a FidelityFX dispatch was not answered: no stretch was recorded and the title's own ffxDispatch was not found"
                );
            });
            ERROR
        }
        // SAFETY: the title's own `ffxDispatch`, with its own arguments.
        real => unsafe { std::mem::transmute::<usize, Dispatch>(real)(context, desc) },
    }
}

/// What the title's import of `ffxDispatch` is bound to. The DLL is not an
/// HLE module: the title's own must load, for its other entry points and
/// for the dispatches passed on.
pub fn dispatch_address() -> usize {
    dispatch as *const () as usize
}

// ---------------------------------------------------------------------------
// Context creation, watched for the upscale context's flags
// ---------------------------------------------------------------------------
//
// `VICTORY_UPSCALE_HISTORY_MV=2` reprojects history with the dispatch's
// motion vectors. Whether those include jitter, whether depth is inverted and
// whether the vectors are at display resolution are flags of the upscale
// context (`ffxCreateContextDescUpscale.flags`, public `ffx_upscale.h`), given
// once at `ffxCreateContext`. That call is passed to the title's own and its
// flags are noted against the context it made. Bound only with level two on.

static REAL_CREATE: AtomicUsize = AtomicUsize::new(0);

/// `FFX_API_CREATE_CONTEXT_DESC_TYPE_UPSCALE`.
const CREATE_UPSCALE: u64 = 0x0001_0000;

/// `ffxCreateContextDescUpscale`: the header, then the flags.
#[allow(dead_code)] // the sizes are the API's, kept for the layout
#[repr(C)]
struct CreateUpscale {
    header: Header,
    flags: u32,
    max_render: [u32; 2],
    max_upscale: [u32; 2],
}

type Create = extern "system" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;

/// Upscale contexts made and their flags (few: one or two per run).
static FLAGS: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());

/// The title's own `ffxCreateContext`.
pub fn set_real_create(address: usize) {
    REAL_CREATE.store(address, Ordering::Relaxed);
}

/// What the title's import of `ffxCreateContext` is bound to: ours when the
/// title's own was found and level two is on, else the title's own.
pub fn create_address() -> Option<usize> {
    let wanted = std::env::var("VICTORY_UPSCALE_HISTORY_MV").is_ok_and(|v| v.trim() == "2");
    (wanted && REAL_CREATE.load(Ordering::Relaxed) != 0).then_some(create as *const () as usize)
}

/// The creation flags of upscale context `context`, if its creation was seen.
pub fn context_flags(context: usize) -> Option<u32> {
    let flags = FLAGS.lock().unwrap_or_else(|e| e.into_inner());
    flags.iter().rev().find(|(c, _)| *c == context).map(|&(_, f)| f)
}

/// The upscale flags in a creation description chain, if it has an upscale
/// description (at most 16 links are followed).
fn upscale_flags(desc: *const Header) -> Option<u32> {
    let mut at = desc;
    for _ in 0..16 {
        // SAFETY: the title's description chain, each link a header.
        let header = unsafe { at.as_ref() }?;
        if header.kind == CREATE_UPSCALE {
            // SAFETY: an upscale creation description, by its type.
            return Some(unsafe { &*at.cast::<CreateUpscale>() }.flags);
        }
        at = header.next;
    }
    None
}

extern "system" fn create(context: *mut *mut c_void, desc: *mut Header, memory: *const c_void) -> u32 {
    let real = REAL_CREATE.load(Ordering::Relaxed);
    if real == 0 {
        return ERROR;
    }
    // SAFETY: the title's own `ffxCreateContext`, with its own arguments.
    let code = unsafe { std::mem::transmute::<usize, Create>(real)(context, desc, memory) };
    if code == 0
        && let Some(flags) = upscale_flags(desc)
    {
        // SAFETY: the context the call just wrote.
        let made = unsafe { context.as_ref() }.map_or(0, |c| *c as usize);
        tracing::info!(context = ?made, flags = format_args!("{flags:#x}"), "upscale context made");
        let mut all = FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|(c, _)| *c != made);
        if all.len() >= 16 {
            all.remove(0);
        }
        all.push((made, flags));
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// The upscale description as the public API lays it out: what the
    /// stretch reads sits where the title wrote it.
    #[test]
    fn the_upscale_description_is_laid_out_as_the_public_api() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(offset_of!(Upscale, list), 0x10);
        assert_eq!(offset_of!(Upscale, colour), 0x18);
        assert_eq!(offset_of!(Upscale, depth), 0x48);
        assert_eq!(offset_of!(Upscale, motion), 0x78);
        assert_eq!(offset_of!(Upscale, output), 0x138);
        assert_eq!(offset_of!(Upscale, jitter), 0x168);
        assert_eq!(offset_of!(Upscale, motion_scale), 0x170);
        assert_eq!(offset_of!(Upscale, render), 0x178);
        assert_eq!(offset_of!(Upscale, upscale), 0x180);
        assert_eq!(offset_of!(TemporalUpscale, pre_exposure), 0x194);
        assert_eq!(offset_of!(TemporalUpscale, reset), 0x198);
        assert_eq!(offset_of!(Resource, description), 8);
        assert_eq!(offset_of!(Resource, state), 40);
    }

    /// The upscale creation description's flags sit after the header, and
    /// the chain is walked to the upscale link.
    #[test]
    fn the_upscale_flags_are_found_in_the_creation_chain() {
        assert_eq!(offset_of!(CreateUpscale, flags), 0x10);
        assert_eq!(offset_of!(CreateUpscale, max_render), 0x14);
        let upscale = CreateUpscale {
            header: Header { kind: CREATE_UPSCALE, next: std::ptr::null() },
            flags: 0b1100,
            max_render: [1280, 720],
            max_upscale: [1920, 1080],
        };
        let backend = Header {
            kind: 0x0000_0002,
            next: (&upscale as *const CreateUpscale).cast(),
        };
        assert_eq!(upscale_flags(&backend), Some(0b1100));
        let alone = Header { kind: 0x0000_0002, next: std::ptr::null() };
        assert_eq!(upscale_flags(&alone), None);
        assert_eq!(upscale_flags(std::ptr::null()), None);
        assert_eq!(context_flags(0x1234_5678), None);
    }

    /// A dispatch that is not an upscale, with no DLL of the title's to
    /// pass it to, is an error and not a silent success.
    #[test]
    fn a_dispatch_nothing_answers_is_an_error() {
        let before = REAL.swap(0, Ordering::Relaxed);
        let other = Header {
            kind: 0x0002_0001,
            next: std::ptr::null(),
        };
        assert_eq!(dispatch(std::ptr::null_mut(), &other), ERROR);
        assert_eq!(dispatch(std::ptr::null_mut(), std::ptr::null()), ERROR);
        REAL.store(before, Ordering::Relaxed);
    }
}
