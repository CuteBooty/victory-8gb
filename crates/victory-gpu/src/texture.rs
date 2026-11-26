//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    /// First mip to read from `layout` for host mip zero.
    first_mip: u32,
    /// Full title extent for the indexed upload budget and residency checks.
    title_bytes: usize,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    memory: *const u8,
    last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a submitted list last
    /// uploaded it, and whether one has uploaded it at all: a list records
    /// the upload only then. (Every list that sampled a title texture
    /// uploaded all of it again, changed or not: 5,411 subresource copies
    /// of 1,120 textures in one frame of run 18.)
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
    /// The buffer's bytes as made (at least `bytes`): what it goes back to
    /// the pool as ([`POOL`]).
    capacity: u64,
    /// Whether the buffer came from the pool and has not been detiled into
    /// yet: it holds another texture's texels, which a refresh that does
    /// not detile (a texture all zero, or not there yet) must not upload.
    stale: std::sync::atomic::AtomicBool,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
    /// The frame its hash last found it changed ([`hot`]).
    changed: Option<u64>,
    /// Its hash found it changed while its guard saw no write: the title
    /// writes it through another view, so it is hashed at every check.
    behind_guard: bool,
    /// Checks of it through the guards so far (counted up to a few).
    checks: u8,
    /// The frame it was read in ahead of its submission
    /// ([`Staging::refresh_ahead`]).
    ahead: Option<u64>,
}

/// Whether a texture in view memory whose hash found it changed lately
/// (within [`HOT_FRAMES`]) is hashed at every check, not every eighth frame
/// (`VICTORY_TEXTURE_HOT=1`; off by default until measured).
///
/// What the guards cannot see, a write to the same pages through another
/// mapping, only the hash finds, and it was taken every eighth frame by
/// address. A texture the title rewrites every frame that way showed one
/// frame in eight: the boot's loading video plays from three such textures
/// in turn, and each took a new frame only at its hash turn, so the screen
/// cycled through three stale pictures (sp-probe4: a period of three at
/// 30 fps, a slot changing every eight frames, 1.7 MB of texture uploads a
/// frame). In gameplay at 2-3 frames a second, eight frames are seconds.
fn hot() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_HOT").is_some_and(|v| v == "1"))
}

/// Frames a texture stays [`hot`] after its hash last found a change.
const HOT_FRAMES: u64 = 30;

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// Every staging state, grouped by the address where its texture's title
/// memory starts. [`STAGING`] is keyed by resource, which cannot answer
/// "which textures lie in this memory range?"; this index can, so a remap
/// does not have to scan every staged texture.
///
/// Several textures can start at the same address (for example two
/// formats viewing the same memory), hence the `Vec`. Entries are `Weak`
/// so the index never keeps a staging buffer alive on its own; entries
/// whose staging state is gone are removed by [`prune_address_index`].
static STAGED_BY_ADDRESS: Mutex<[StagedSpans; 4]> = Mutex::new([const { StagedSpans::new() }; 4]);

struct StagedSpans {
    by_start: std::collections::BTreeMap<usize, Vec<Weak<Staging>>>,
    largest: usize,
}

impl StagedSpans {
    const fn new() -> Self {
        Self { by_start: std::collections::BTreeMap::new(), largest: 0 }
    }

    // A large texture widens the remap search only for others of its class.
    fn class(bytes: usize, split: bool) -> usize {
        if split {
            [64 << 10, 1 << 20, 16 << 20].partition_point(|&limit| bytes > limit)
        } else {
            0
        }
    }

    fn candidates(&self, at: usize, end: usize) -> impl Iterator<Item = (&usize, &Vec<Weak<Staging>>)> {
        self.by_start.range(at.saturating_sub(self.largest)..end)
    }

    fn add(&mut self, start: usize, bytes: usize, staging: Weak<Staging>) {
        self.largest = self.largest.max(bytes);
        let entries = self.by_start.entry(start).or_default();
        entries.retain(|entry| entry.strong_count() > 0);
        entries.push(staging);
    }
}

fn remap_by_size() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_REMAP_BY_SIZE").is_some_and(|v| v == "1"))
}

/// Entries examined and microseconds spent finding staged textures on remaps.
#[cfg(feature = "diagnostics")]
pub(crate) static REMAP_SCAN: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Adds a newly made staging state to [`STAGED_BY_ADDRESS`].
fn add_to_address_index(staging: &Arc<Staging>) {
    let (start, bytes) = staging.memory();
    let mut index = STAGED_BY_ADDRESS.lock().unwrap_or_else(|e| e.into_inner());
    index[StagedSpans::class(bytes, remap_by_size())].add(start, bytes, Arc::downgrade(staging));
}

/// Removes entries of [`STAGED_BY_ADDRESS`] whose staging state was let go.
fn prune_address_index() {
    let mut index = STAGED_BY_ADDRESS.lock().unwrap_or_else(|e| e.into_inner());
    for spans in index.iter_mut() {
        spans.by_start.retain(|_, entries| {
            entries.retain(|entry| entry.strong_count() > 0);
            !entries.is_empty()
        });
    }
}

/// Called when the runtime maps or unmaps title memory `[at, at + len)`.
///
/// A texture in that range may now show completely different bytes, but
/// its staging state still says "uploaded and unchanged". In that state
/// [`upload`] skips the copy and only schedules a check, so the first list
/// that samples the texture after the remap reads the old texels, and the
/// new ones only arrive one list later (#574).
///
/// Marking the texture dirty makes its next [`upload`] record the copy.
/// The check that runs when that list is submitted reads the new memory
/// into the staging buffer before the copy executes, so the very first
/// list after the remap sees the new texels.
pub(crate) fn remapped(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let index = STAGED_BY_ADDRESS.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(feature = "diagnostics")]
    let mut examined = 0;
    for (_, entries) in index.iter().filter(|s| !s.by_start.is_empty()).flat_map(|s| s.candidates(at, end)) {
        #[cfg(feature = "diagnostics")]
        {
            examined += entries.len() as u64;
        }
        for staging in entries.iter().filter_map(Weak::upgrade) {
            let (start, bytes) = staging.memory();
            if start.saturating_add(bytes) > at {
                staging.dirty.store(true, Relaxed);
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    {
        drop(index);
        REMAP_SCAN[0].fetch_add(examined, Relaxed);
        REMAP_SCAN[1].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    }
}

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    // Host copies with other levels than each other's: filled from title
    // memory instead (`crate::resource::drop_levels`).
    if crate::resource::dropped(placed) != crate::resource::dropped(source) {
        return false;
    }
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let through = {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return false;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return false;
                }
                let Some(buffer) = host.uav_buffer(bytes[0]) else {
                    return false;
                };
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    let first = list.first_use(placed as usize);
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list.transition(source, COPY_SOURCE).map(|(texture, before)| {
        let state = crate::host::legal_state(texture, COPY_SOURCE);
        let barrier = crate::host::transition(
            texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(state as i32),
        );
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        before
    });
    if !first && before.is_none() {
        return true;
    }
    crate::list::ensure_state(list, placed, COPY_DEST);
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While the world streams in the runtime remaps all the time,
/// and each of the frame's 150-360 refreshes waited for it: runs 99-101
/// have 4, 15 and 16 frame lines with over 300 ms of "textures", up to
/// 3,708 ms, and on every one of them the wait for this lock is the whole
/// of it (1,345 of 1,381 ms; 3,630 of 3,708; the write-watch askings 3-12
/// ms, the texture's own lock 0).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. It never shows partly read.
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, with [`no_wait`]: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there.
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While the world streams in, the frame line's "textures" is seconds a
/// frame in some runs and tenths in others for the same megabytes (2,566
/// ms for 110 MB in run 93, 314 ms for 132 MB in run 91; before these
/// counters, runs 81, 83, 85 and 94 each had lines of 1.5-2.9 s), and the
/// submitting thread is not running while it lasts (run 93: 240-310 ms of
/// its own time a frame beside 1,100-1,800 ms of "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    let us = started.elapsed().as_micros() as u64;
    REFRESH_WAITS[slot].fetch_add(us, std::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    if slot == 2 {
        crate::copystat::mapping_wait(us);
    }
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`VICTORY_TEXTURE_COPY=1`; off by
/// default). Round 3 of the memory work read every such texture that way
/// to be safe from pages taken away under the read; it cost 0.6 ms per MB
/// against 0.3 in place (90-105 ms a frame while the world streams in, runs
/// 72 and 73, against 20-40), and both runs with it lost the graphics
/// device, for a reason not found. The read in place is safe now for
/// another reason (`crate::watch::title_memory_reading`); this switch is
/// left to tell whether the copy was what lost the device.
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

/// Adapted from Alexis, fork PR #1078 (4f83d40d). Off by default.
fn short_hold() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SHORT_HOLD").is_some_and(|v| v == "1"))
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// With a short hold, the callback sees only the thread's private snapshot.
///
/// # Safety
/// The guard must cover the source range. Unless `mapped_copy` is set, the
/// caller must also have checked that all its bytes are readable under it.
unsafe fn with_refresh_bytes<R>(
    memory: *const u8,
    size: usize,
    mapping: crate::watch::Reading,
    mapped_copy: bool,
    short: bool,
    f: impl FnOnce(&[u8]) -> R,
) -> R {
    if mapped_copy {
        return with_title(memory, size, |bytes| {
            if short {
                drop(mapping);
            }
            f(bytes)
        });
    }
    if short {
        return with_read(size, |snapshot| {
            // SAFETY: the readable range is held throughout the copy, and
            // the thread's private buffer is disjoint from title memory.
            unsafe { std::ptr::copy_nonoverlapping(memory, snapshot.as_mut_ptr(), size) };
            drop(mapping);
            f(snapshot)
        });
    }
    // SAFETY: the guard outlives the callback's borrow of the readable range.
    f(unsafe { std::slice::from_raw_parts(memory, size) })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
        // The lists that recorded a copy from it held it until they were
        // reset, after the card ran them: the card is done with it.
        give_back(self.buffer.clone(), self.data, self.capacity);
    }
}

/// An upload buffer of a staging buffer let go, mapped, kept for another.
struct Pooled {
    buffer: ID3D12Resource,
    data: *mut u8,
}

// SAFETY: a buffer no list holds and no thread writes while it is pooled.
unsafe impl Send for Pooled {}

/// Upload buffers of staging buffers let go, by size class, oldest first
/// in each, and their bytes.
///
/// A staging buffer was an upload buffer made for its texture and released
/// with it. On the RTX 5070 making and mapping one costs 1.9 ms a megabyte
/// and releasing it 1.2 more, whatever the heap flags (the system zeroes
/// the pages), against 0.36 to write one already there (`upload_bench`:
/// 1,000 buffers, 350 MB). The pass over the heap's textures makes up to
/// 192 MB of them a frame (576 MB while it catches up), on the thread that
/// records the frame: most of its 250-750 ms. Kept here, the next texture
/// of the same size class takes one ([`class`]).
///
/// Most staging buffers live a frame (the pass lets go of what it uploaded,
/// and a texture found changed is staged again), so the buffers given back
/// last are the likeliest to be asked for: with the pool full, the oldest
/// of the class holding the most bytes goes to make room. (Refusing the
/// new one instead left the pool full of sizes no longer asked for: 5 % of
/// buffers taken from it, 93 % released with it full.)
struct Pool {
    free: std::collections::BTreeMap<u64, std::collections::VecDeque<Pooled>>,
    bytes: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    free: std::collections::BTreeMap::new(),
    bytes: 0,
});

/// The bytes a staging buffer of `size` is made with, so that buffers of
/// nearby sizes are one class: whole 64 KiB up to 1 MiB (the system gives
/// no less), then eight steps per power of two (at most an eighth unused).
fn class(size: u64) -> u64 {
    const LINE: u64 = 64 << 10;
    let size = size.max(1);
    if size <= 1 << 20 {
        return size.next_multiple_of(LINE);
    }
    let step = (1u64 << (63 - size.leading_zeros())) / 8;
    size.next_multiple_of(step)
}

/// Since the start: staging buffers made anew, taken from the pool, given
/// back to it, and released because the pool was full.
static POOL_COUNTS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The most bytes the pool keeps: `VICTORY_STAGING_POOL_MB` (256; 0 keeps
/// none, and every staging buffer is made and released as before).
fn pool_limit() -> u64 {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_STAGING_POOL_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(256) << 20
    })
}

/// An upload buffer of at least `size` bytes, its mapping, its bytes, and
/// whether it was used before: from the pool, the last given back of the
/// size's class, else made with the class's bytes.
fn take(size: u64) -> Option<(ID3D12Resource, *mut u8, u64, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    // With the pool off a buffer has its own size, as before.
    let capacity = if pool_limit() > 0 { class(size) } else { size };
    if pool_limit() > 0 {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = pool.free.get_mut(&capacity)
            && let Some(p) = list.pop_back()
        {
            if list.is_empty() {
                pool.free.remove(&capacity);
            }
            pool.bytes -= capacity;
            POOL_COUNTS[1].fetch_add(1, Relaxed);
            return Some((p.buffer, p.data, capacity, true));
        }
    }
    let (buffer, data) = host()?.upload_buffer(capacity)?;
    POOL_COUNTS[0].fetch_add(1, Relaxed);
    Some((buffer, data, capacity, false))
}

/// A staging buffer's upload buffer, let go: kept in the pool, the oldest
/// of the fullest class released while the pool is over its bytes. A
/// buffer not of a class's size (or bigger than the whole pool) is released.
fn give_back(buffer: ID3D12Resource, data: *mut u8, capacity: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let limit = pool_limit();
    if capacity > limit || class(capacity) != capacity {
        POOL_COUNTS[3].fetch_add(1, Relaxed);
        return;
    }
    let mut released = Vec::new();
    {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        while pool.bytes + capacity > limit {
            let Some((&fullest, _)) = pool
                .free
                .iter()
                .max_by_key(|&(&class, list)| class * list.len() as u64)
            else {
                break;
            };
            let list = pool.free.get_mut(&fullest).expect("the class just found");
            if let Some(oldest) = list.pop_front() {
                released.push(oldest);
            }
            if list.is_empty() {
                pool.free.remove(&fullest);
            }
            pool.bytes -= fullest;
        }
        pool.bytes += capacity;
        pool.free.entry(capacity).or_default().push_back(Pooled { buffer, data });
    }
    POOL_COUNTS[2].fetch_add(1, Relaxed);
    POOL_COUNTS[3].fetch_add(released.len() as u64, Relaxed);
    // Released outside the lock: a release costs about 1.2 ms a megabyte.
    drop(released);
}

/// For the memory line: staging buffers made, taken from the pool, given
/// back, released with the pool full; then the pool's buffers and bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn pool_stats() -> [u64; 6] {
    use std::sync::atomic::Ordering::Relaxed;
    let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let kept = pool.free.values().map(|l| l.len() as u64).sum();
    let [made, taken, given, released] = [0, 1, 2, 3].map(|i| POOL_COUNTS[i].load(Relaxed));
    [made, taken, given, released, kept, pool.bytes]
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (2,246 of
/// them, 2.1 GB, at the end of run 24). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|_, s| {
        let keep = Arc::strong_count(s) > 1
            || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
        if !keep {
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
        keep
    });
    if went > 0 {
        prune_address_index();
        crate::host::trail(|| format!("sweep: {went} staging buffers of textures let go ({} KiB), held by no list", bytes >> 10));
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default).insert(resource as usize)
    };
    // Diagnostics (run 177): where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at as u64, (bytes, words[4], words[6], words[7], words[8], resource as usize));
        }
    }
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, d| d.5 != resource);
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    // Asked about already in this list: nothing to do whatever the global
    // set says, so its lock is not taken. Nothing is
    // noted here: the order of the look and the note below is as before.
    if list.used(resource as usize) {
        return;
    }
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    staging.used.store(crate::host::moment(), Relaxed);
    // The host texture has the staging buffer's contents: only the check
    // at submission is left, and what it finds changed the next list that
    // samples the texture uploads (a frame late where every list of the
    // frame was recorded before that check).
    if staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // When the list is submitted, its copy takes what the staging buffer
    // holds by then: noted before the check below, which sets `dirty`
    // again if it detiles (the next list then uploads once more).
    // A step of its own (was a host closure doing `mark_uploaded`), so that
    // `VICTORY_PREP_THREADS` can keep it after a detile running elsewhere.
    list.defer_uploaded(staging.clone());
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging);
}

impl Staging {
    /// A submitted list's upload takes what the buffer holds by then
    /// ([`upload`]'s step at submission).
    pub(crate) fn mark_uploaded(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.uploaded.store(true, Relaxed);
        self.dirty.store(false, Relaxed);
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.layout.size as usize)
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        crate::cores::prep_job();
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(whole.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        read
    }

    fn refresh_timed(&self) -> usize {
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let read = self.refresh_held(&mut last);
        // A buffer from the pool that this refresh did not detile into
        // holds another texture's texels: zeros instead, as a buffer made
        // anew has. Still under `last`'s lock: a refresh on another thread
        // must not detile between the swap and the zeros, which would wipe
        // its texels.
        if self.stale.swap(false, std::sync::atomic::Ordering::Relaxed) {
            // SAFETY: the mapped staging buffer, `bytes` long; written only
            // here and in `detile`, both under `last`'s lock.
            unsafe { self.data.write_bytes(0, self.bytes as usize) };
        }
        read
    }

    /// [`Staging::refresh_timed`] with `last`'s lock held by the caller.
    fn refresh_held(&self, last: &mut Last) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        if let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false)) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        // View memory has no write-watch, and a texture there was hashed
        // whole at every check: 229 MB and 51 ms a frame in a gameplay view
        // where two textures change (run 58). It is guarded now, as buffers
        // in view memory are: a write of the title's to it faults once and
        // is noted, a mapping is announced by the runtime. While neither
        // happened the texture is as it was. What the guard cannot see (a
        // write to the same pages through another view) is still found by
        // the hash, which is taken every eighth frame only, by the
        // texture's address, so the textures take turns.
        // `VICTORY_TEXTURE_GUARD=0` hashes at every check, as before.
        // A texture whose hash found it changed while the guard saw nothing
        // is hashed at every check from then on (`Last::behind_guard`): the
        // title's video player decodes into a ring of three frames written
        // through another view, and on its eighth-frame turn each frame of
        // the ring was refreshed about every 24 frames, so two presented
        // frames in three showed video from up to two seconds before (the
        // intro). Off unless `VICTORY_TEXTURE_BEHIND_GUARD=1` (it hashes
        // every frame and was not tried in the open world).
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static BEHIND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let behind_on = *BEHIND.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_BEHIND_GUARD").is_some_and(|v| v == "1"));
        let mut unseen = false;
        if *GUARD.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, true))
        {
            const HASH_EVERY: u64 = 8;
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let frame = crate::frame::current();
            // Not in the frames right after a read ahead of the submission
            // (`refresh_ahead`; None without `VICTORY_HEAP_WORKER`).
            let turn = frame.wrapping_add((start >> 16) as u64).is_multiple_of(HASH_EVERY)
                && last.ahead.is_none_or(|at| frame > at + 1);
            let changing = hot() && last.changed.is_some_and(|at| frame < at + HOT_FRAMES);
            // A texture's first checks hash it whatever the turn, so one
            // written behind its guard is found at its second use, not up
            // to a second or two later (the intro's opening star flashed
            // with old frames until each set of its ring had had its turn;
            // the ring's memory moves from run to run, so it cannot be
            // remembered by address).
            const FIRST_CHECKS: u8 = 3;
            let young = behind_on && last.checks < FIRST_CHECKS;
            last.checks = last.checks.saturating_add(1);
            if unwritten && !turn && !changing && !young && !(behind_on && last.behind_guard) {
                return 0;
            }
            unseen = unwritten && behind_on;
        }
        let hash_before = last.hash;
        let read = self.refresh_hashed(last, size);
        // changed with no write the guard saw: hashed at every check now
        if unseen && hash_before.is_some() && last.hash != hash_before && !last.behind_guard {
            crate::watch::light::unseen_write(start, size);
            last.behind_guard = true;
            tracing::info!(
                memory = ?self.memory,
                bytes = size,
                "a texture changed with no write its guard saw (written through another view): hashed at every check from now on"
            );
        }
        read
    }

    /// [`Staging::refresh_timed`] past the write watch and the guards: the
    /// texture's memory read, hashed and detiled if it changed.
    fn refresh_hashed(&self, last: &mut Last, size: usize) -> usize {
        // From here on the texture's memory may be read in place. Held until
        // that read is done: the runtime changes what title memory shows
        // only with this taken alone, so what `committed` finds mapped
        // stays mapped (see `crate::watch::title_memory_reading`; without
        // it a page went between the asking and the read in runs 71 and
        // 74).
        let mapped = if no_wait() {
            // Not waited for: while the runtime remaps title memory the
            // texture is read through the kernel instead, whole or not at
            // all (`no_wait`).
            let Some(mapped) = crate::watch::title_memory_reading_now(self.memory as usize, size) else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            // Port of c79c751c: the bytes held are exactly those read below
            // (`self.memory`, `size` = the kept levels' bytes after
            // `staging_layout`).
            waited(2, || crate::watch::title_memory_reading(self.memory as usize, size))
        };
        let partial = !committed(self.memory, size);
        if partial {
            // Partly mapped: a streamed texture with only some mips' pages
            // there, the usual state while it streams in. Read through the
            // copy, which stands unmapped pages (zeros), and upload what
            // is there; nothing at all mapped changes nothing.
            // shortcut: read again at most every 30 frames (reading around
            // unmapped pages is slow, and nothing tells us when pages
            // arrive): a mip that streams in shows up to 30 frames late.
            let frame = crate::frame::current();
            if last.partial.is_some_and(|at| frame < at + 30) {
                return 0;
            }
            last.partial = Some(frame);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        let refresh = |tiled: &[u8]| {
            let hash = blake3::hash(tiled);
            if last.hash == Some(hash) {
                return;
            }
            last.hash = Some(hash);
            last.changed = Some(crate::frame::current());
            if partial && tiled.iter().all(|&b| b == 0) {
                return;
            }
            self.detile(tiled);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        };
        // SAFETY: `mapped` covers the same bytes tested by `committed`
        // above. A partial texture always goes through the mapped copy.
        unsafe { with_refresh_bytes(self.memory, size, mapped, partial || through_copy(), short_hold(), refresh) };
        size
    }

    /// Detiles the texture memory into the staging buffer ahead of the
    /// submission, on the thread of `crate::heap_worker`: read through the
    /// kernel, so that the runtime's remapping never waits for it, whole or
    /// not at all (the submission then does it, as ever). False if it was
    /// not read. What the title writes after this is found at submission.
    /// (fxceless, victory-fork d4a528ba.)
    pub(crate) fn refresh_ahead(&self) -> bool {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if last.hash.is_some() || last.asked != 0 {
            // Detiled before: the submission's check is cheap.
            return true;
        }
        let asked =
            crate::watch::writes(start, start + size, false).or_else(|| crate::watch::writes(start, start + size, true));
        let Some((now, _)) = asked else {
            return false;
        };
        with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            last.hash = Some(blake3::hash(read));
            self.detile(read);
            last.asked = now;
            last.ahead = Some(crate::frame::current());
            true
        })
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]):
    /// the texture is read through the kernel, which fails if a page of it
    /// is not there at this moment. Then nothing is taken and the texture
    /// is looked at again at its next refresh (what was learnt about writes
    /// to it is forgotten, so that it is not passed over as unwritten).
    /// Returns the bytes read.
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = blake3::hash(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * rows as usize * f.Depth as usize;
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if let Err(e) =
                self.layout
                    .detile(tiled, mip + self.first_mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
        self.stale.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes, and the
/// ranges of it (offsets from the address, bytes) that [`fingerprint`]
/// reads besides its three windows: the start of every mip of every slice
/// ([`victory_formats::texture::Layout::sample_ranges`]). None with
/// `VICTORY_HEAP_RECHECK_MIPS=0`: the three windows alone, as before.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64, Option<std::sync::Arc<[(u64, u64)]>>)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    let (at, len) = staging.memory();
    let mips = recheck_mips().then(|| staging.layout.sample_ranges(MIP_WINDOW, MIP_RANGES).into());
    Some((at as u64, len as u64, mips))
}

/// Bytes [`fingerprint`] reads at the start of each mip, and the most mips
/// it reads of one texture (spread over its slices and mips when it has
/// more). A 4096-square texture with all its mips: 10 reads, 12 KiB; a cube
/// map with 10 mips: 40 reads, 52 KiB.
const MIP_WINDOW: u64 = 1 << 10;
const MIP_RANGES: usize = 64;

/// Whether [`fingerprint`] also reads the start of every mip of every
/// slice (`VICTORY_HEAP_RECHECK_MIPS=0`: no).
fn recheck_mips() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK_MIPS").is_none_or(|v| v != "0"))
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of 16 KiB at its start, its middle and its end,
/// and of the ranges `mips` (offsets from `at`, bytes; see
/// [`staged_memory`]). A texture streams in whole mips, and the three
/// windows alone see only the smallest mips and mip 0 of a plain texture,
/// and only two slices of an array or cube map: a mip or slice that
/// arrived after the upload elsewhere was not seen. Read through the
/// kernel, so pages not there count as zeros.
pub(crate) fn fingerprint(at: u64, len: u64, mips: &[(u64, u64)]) -> u64 {
    const WINDOW: usize = 16 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    let mut hasher = blake3::Hasher::new();
    READ.with(|read| {
        let mut read = read.borrow_mut();
        let longest = mips.iter().map(|&(_, n)| n as usize).max().unwrap_or(0);
        read.resize((3 * WINDOW).max(longest), 0);
        let mut take = |from: usize, bytes: usize| {
            read[..bytes].fill(0);
            crate::host::copy_mapped(read.as_mut_ptr(), (at as usize + from) as *const u8, bytes, false);
            hasher.update(&read[..bytes]);
        };
        if len <= 3 * WINDOW {
            take(0, len);
        } else {
            take(0, WINDOW);
            take((len / 2) & !0xfff, WINDOW);
            take(len - WINDOW, WINDOW);
            for &(from, bytes) in mips {
                let from = from as usize;
                if from < len {
                    take(from, (bytes as usize).min(len - from));
                }
            }
        }
    });
    let hash = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first) | 1
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
/// For the frame listing: whether a list has uploaded `resource`'s staging
/// buffer, and whether it was detiled since. None: it has none.
#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref().is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    let size = s.title_bytes;
    Some((size as u64, committed(s.memory, size)))
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`VICTORY_COMMITTED_QUERY=1`). `VirtualQuery` answers with the
/// length of the run of pages in the same state, which it finds by walking
/// the page tables to the run's end: 8.8 ms for one question in the title's
/// 1.1 GB block (round 6). The pass over the heap's views asks three or
/// more of them for every texture slot that is due, 3,000 slots a frame
/// when the card has no room (run 75): that pass then ran 208-264 ms, and
/// while it ran the submitting thread's write-watch askings waited behind
/// the walks (run 76: 8-22 askings a frame of 6-21 ms each, 46-172 ms).
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot. One question to the kernel for up to 256 pages at once
/// (`QueryWorkingSetEx`: the state of each page asked about, no walk); a
/// page not in the working set may still be there (never touched, or
/// trimmed), and a one-byte read through the kernel says.
/// A page that is committed and may not be read counts as not there, which
/// is what the callers ask for: they read or write the memory next.
fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. With neither the first nor the last page committed the
/// answer is none without a look at the pages between (two queries for the
/// slots whose texture is away, which are asked again and again), unless
/// [`mapped_middle`].
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    mapped_bytes_with(at, len, mapped_middle())
}

/// Whether a range with neither end committed is looked at between its
/// ends too (`VICTORY_MAPPED_MIDDLE=1`; off by default): fork PR 1023
/// (4c29c4db), a streamed texture with mip data mapped between two
/// unmapped ends was taken for one not there. The look costs a probe per
/// 64 KiB block (or the `VirtualQuery` walk with `VICTORY_COMMITTED_QUERY=1`)
/// for every slot whose texture is away, each time it is asked again; the
/// PR walked with `VirtualQuery` there, which this tree stopped doing
/// for its cost (runs 75-76).
fn mapped_middle() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MAPPED_MIDDLE").is_some_and(|v| v == "1"))
}

/// [`mapped_bytes`], with the look between two absent ends or without.
fn mapped_bytes_with(at: *const u8, len: usize, middle: bool) -> u64 {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let start = at as usize;
    let end = start.saturating_add(len);
    if start >= end {
        return 0;
    }
    if !middle && !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
        return 0;
    }
    if !by_query() {
        static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BY_PAGE.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_PAGE").is_some_and(|v| v == "1")) {
            // By page, without the walk: whole pages, cut to the range.
            let (there, _) = readable(start, len, false);
            return (there as u64 * 4096).min(len as u64);
        }
        // By 64 KiB block, one page asked of each: the title's memory is
        // mapped in blocks of that size (the console's page), and the
        // answer is a measure of how much of a texture has arrived, not a
        // promise that every byte can be read (the upload reads through the
        // kernel). By page, a texture half there cost a failed kernel read
        // for each of its missing pages, every frame it stayed due: 1,663
        // textures "uploaded in part" kept the pass over the heap's views at
        // 90-160 ms a frame (run 81). `VICTORY_MAPPED_BY_PAGE=1`: by page.
        const BLOCK: usize = 64 << 10;
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        while block < end {
            let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
            if readable(lo, 1, true).0 == 1 {
                there += (hi - lo) as u64;
            }
            block = block.saturating_add(BLOCK);
        }
        return there;
    }
    let (mut p, mut there) = (start, 0u64);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        if n == 0 || next <= p {
            break;
        }
        if info.State == MEM_COMMIT && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}

/// For a single 2D slice the smaller mips form a prefix of the title
/// allocation. Keep their original tiled offsets and tail placement while
/// shortening the source that refreshes watch, copy and hash. Array slices
/// have the original stride between their prefixes.
fn staging_layout(
    mut layout: victory_formats::texture::Layout,
    drop: u32,
) -> (victory_formats::texture::Layout, u32) {
    use victory_formats::texture::Dimension;
    if drop == 0 || layout.desc.dimension != Dimension::Tex2d || layout.slices != 1 {
        return (layout, drop);
    }
    // drop_levels always leaves a mip. A cutoff within the packed tail
    // keeps its entire block, including any bytes shared with a dropped mip.
    let last_dropped = &layout.mips[drop as usize - 1];
    let bytes = if last_dropped.tail.is_some() {
        layout.alignment
    } else {
        last_dropped.offset
    };
    layout.mips.drain(..drop as usize);
    layout.first_tail_mip = layout.first_tail_mip.saturating_sub(drop);
    layout.desc.width = (layout.desc.width >> drop).max(1);
    layout.desc.height = (layout.desc.height >> drop).max(1);
    layout.desc.mips -= drop;
    layout.size = bytes;
    layout.slice_size = bytes;
    (layout, 0)
}

/// The bytes of title memory the host copy of a texture placed with the
/// description `words` reads: all of [`crate::resource::placed_bytes`], or
/// with levels left out (`VICTORY_TEXTURE_DROP_MIPS`) the prefix that holds
/// the levels kept ([`staging_layout`]). Asked how much of a texture is
/// mapped, the pass over the heap's views measures this: a texture whose
/// left-out largest level is not mapped is whole for the host copy, and
/// one whose kept levels are not mapped is not there (it was "in part",
/// and its kept levels were read as zeros and uploaded). Added in this tree
/// with fork PR 1023.
pub(crate) fn retained_bytes(words: &[u32; 14]) -> Option<u64> {
    let full = crate::resource::placed_bytes(words)?;
    // SAFETY: 56 bytes in the description's layout.
    let desc = unsafe { words.as_ptr().cast::<crate::resource::ResourceDesc>().read_unaligned() };
    let drop = crate::resource::drop_levels(&desc);
    if drop == 0 {
        return Some(full);
    }
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone()) else {
        return Some(full);
    };
    match staging_layout(layout, drop) {
        (layout, 0) => Some(layout.size.clamp(1, full)),
        _ => Some(full),
    }
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data, capacity, reused) = take(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let title_bytes = layout.size as usize;
    let (layout, first_mip) = staging_layout(layout, crate::resource::drop_levels(&desc));
    let s = Arc::new(Staging {
        buffer,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        first_mip,
        title_bytes,
        bytes: total,
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        capacity,
        stale: std::sync::atomic::AtomicBool::new(reused),
    });
    map.insert(resource as usize, s.clone());
    add_to_address_index(&s);
    Some(s)
}

#[cfg(test)]
mod remap_tests {
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn a_long_texture_does_not_widen_the_search_for_small_ones() {
        use super::StagedSpans;
        let at = 32 << 20;
        let spans = [(0, at + 1), (1, 64 << 10), (at - 8, 16), (at, 1 << 20), (at + 1, 1)];
        for split in [false, true] {
            let mut index = [const { StagedSpans::new() }; 4];
            for (start, bytes) in spans {
                index[StagedSpans::class(bytes, split)].add(start, bytes, super::Weak::new());
            }
            let found: Vec<_> = index.iter().flat_map(|s| s.candidates(at, at + 1)).map(|(&start, _)| start).collect();
            assert!(found.contains(&0) && found.contains(&(at - 8)) && found.contains(&at));
            assert!(!found.contains(&(at + 1)));
            assert_eq!(found.contains(&1), !split);
        }
    }

    /// After a remap of the memory under a texture that was already
    /// uploaded, the texture must be uploaded again (#574). A remap of
    /// unrelated memory, or of an empty range, must leave it alone.
    #[test]
    fn a_remap_under_a_staged_texture_makes_it_upload_again() {
        use windows_sys::Win32::System::Memory::*;
        const PAGE_RUN: usize = 0x1_0000;
        // Two 64 KiB runs: the texture lives in the first, the second is
        // memory next to it that no texture uses.
        // SAFETY: a private allocation of our own, released at the end.
        let base = unsafe {
            VirtualAlloc(std::ptr::null(), 2 * PAGE_RUN, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        };
        assert!(!base.is_null());
        let texture_start = base as usize;
        // Resource description words of a linear 4x4 RGBA8 texture with one
        // mip level, the shape the issue used to reproduce the bug.
        let description = [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 1, 0, 0];
        let staged = crate::resource::texture_at(texture_start as u64, &description).and_then(|resource| {
            let host_texture = crate::resource::host_resource(resource)?;
            super::staging(resource, host_texture)
        });
        // `None` means there is no GPU to make the host texture on: nothing
        // to check then.
        if let Some(staging) = staged {
            // The state after a submitted list uploaded the texture.
            staging.uploaded.store(true, Relaxed);
            staging.dirty.store(false, Relaxed);

            super::remapped(texture_start + PAGE_RUN, PAGE_RUN);
            super::remapped(texture_start, 0);
            assert!(!staging.dirty.load(Relaxed), "a remap elsewhere marked the texture");

            super::remapped(texture_start + 0x10, 0x10);
            assert!(staging.dirty.load(Relaxed), "a remap under the texture did not mark it");
        }
        // SAFETY: the allocation above, released once.
        assert_ne!(unsafe { VirtualFree(base, 0, MEM_RELEASE) }, 0);
    }
}

#[cfg(test)]
mod pool_tests {
    use super::{class, give_back, pool_limit, take};
    use windows::core::Interface;

    /// Classes are whole 64 KiB up to 1 MiB, then eighths of a power of
    /// two, and hold the size asked.
    #[test]
    fn classes_hold_the_size_with_at_most_an_eighth_unused_past_a_megabyte() {
        assert_eq!(class(1), 64 << 10);
        assert_eq!(class(64 << 10), 64 << 10);
        assert_eq!(class((64 << 10) + 1), 128 << 10);
        assert_eq!(class(1 << 20), 1 << 20);
        assert_eq!(class((1 << 20) + 1), (1 << 20) + (128 << 10));
        for size in [(3 << 20) + 7, 37 << 20, (100 << 20) - 1, 1 << 30] {
            let c = class(size);
            assert!(c >= size && c - size <= size / 8, "{size} -> {c}");
            assert_eq!(class(c), c, "a class is its own class");
        }
    }

    /// A buffer given back is taken again for a size of its class, and not
    /// for one of another class.
    #[test]
    fn a_buffer_given_back_is_taken_again_for_its_class() {
        if pool_limit() == 0 {
            return;
        }
        // A size no other test asks for, so that none takes it meanwhile.
        const SIZE: u64 = (37 << 20) + 12_345;
        let Some((buffer, data, capacity, reused)) = take(SIZE) else {
            eprintln!("no device; skipped");
            return;
        };
        assert!(!reused, "nothing of that size was given back");
        assert_eq!(capacity, class(SIZE));
        let raw = buffer.as_raw();
        give_back(buffer, data, capacity);
        // The next class up, and far smaller: made anew.
        let (bigger, data_b, cap_b, reused_b) = take(capacity + 1).unwrap();
        assert!(!reused_b && bigger.as_raw() != raw);
        let (small, data_s, cap_s, reused_s) = take(SIZE / 2).unwrap();
        assert!(!reused_s && small.as_raw() != raw);
        // Another size of its class: that buffer, mapped as it was.
        let (again, data_a, cap_a, reused_a) = take(capacity - 1).unwrap();
        assert!(reused_a);
        assert_eq!((again.as_raw(), data_a, cap_a), (raw, data, capacity));
        // SAFETY: the mapping of a buffer of `cap_a` bytes.
        unsafe { data_a.write_bytes(0xa5, cap_a as usize) };
        for (b, d, c) in [(bigger, data_b, cap_b), (small, data_s, cap_s), (again, data_a, cap_a)] {
            give_back(b, d, c);
        }
    }
}

#[cfg(test)]
mod title_memory_tests {
    use super::{mapped_bytes_with, with_title};

    #[test]
    fn short_hold_releases_the_mapping_before_processing_the_snapshot() {
        use std::{sync::mpsc, thread, time::Duration};
        for mapped_copy in [false, true] {
            let mut title = vec![0x5au8; 4096];
            let (src, len) = (title.as_mut_ptr(), title.len());
            let start = src as usize;
            thread::scope(|scope| {
                let reading = crate::watch::title_memory_reading(start, len);
                let process = |snapshot: &[u8]| {
                    assert_ne!(snapshot.as_ptr(), src.cast_const());
                    let (sent, done) = mpsc::channel();
                    scope.spawn(move || {
                        let _changing = crate::watch::title_memory_remapping(start, len);
                        title.fill(0xa5);
                        let _ = sent.send(());
                    });
                    done.recv_timeout(Duration::from_secs(3)).expect("remap during processing");
                    assert!(snapshot.iter().all(|&b| b == 0x5a));
                };
                // SAFETY: the allocation is live until the remapper runs,
                // and its entire initialized range is held during the copy.
                unsafe { super::with_refresh_bytes(src, len, reading, mapped_copy, true, process) };
            });
        }
    }

    /// [`mapped_bytes_with`] with the look between the ends
    /// (`VICTORY_MAPPED_MIDDLE=1`), as the PR had it.
    fn mapped_bytes(at: *const u8, len: usize) -> u64 {
        mapped_bytes_with(at, len, true)
    }

    #[test]
    fn a_streamed_mip_can_be_mapped_between_two_unmapped_ends() {
        use windows_sys::Win32::System::Memory::*;
        const MIP: usize = 64 << 10;
        const SIZE: usize = 4 * MIP;
        // SAFETY: a private reservation; only the committed middle is written.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(mapped_bytes(base, SIZE), 0);
            let middle = base.add(MIP);
            assert_eq!(VirtualAlloc(middle.cast(), MIP, MEM_COMMIT, PAGE_READWRITE), middle.cast());
            let hidden = base.add(2 * MIP);
            assert_eq!(VirtualAlloc(hidden.cast(), MIP, MEM_COMMIT, PAGE_NOACCESS), hidden.cast());
            middle.write_bytes(0xb6, MIP);
            let available = mapped_bytes(base, SIZE);
            // Without the switch: not there, as before.
            assert_eq!(mapped_bytes_with(base, SIZE, false), 0);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..MIP].iter().all(|&b| b == 0));
                assert!(bytes[MIP..2 * MIP].iter().all(|&b| b == 0xb6));
                assert!(bytes[2 * MIP..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            assert_eq!(available, MIP as u64);
        }
    }

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| assert!(bytes.iter().all(|&b| b == 0x77)));
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT), 0);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn retained_source_detiles_every_mip_from_its_original_offset() {
        use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_S] {
            let title = Layout::new(&Desc {
                dimension: Dimension::Tex2d,
                width: 512,
                height: 512,
                depth: 1,
                mips: 10,
                element_bytes: 16,
                block: 4,
                samples: 1,
                swizzle,
            })
            .unwrap();
            let bytes: Vec<_> = (0..title.size)
                .map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8)
                .collect();
            for drop in [1, 2, 4] {
                let (source, first) = super::staging_layout(title.clone(), drop);
                assert_eq!(first, 0);
                assert!(source.size < title.size);
                let read = &bytes[..source.size as usize];
                for mip in drop..title.desc.mips {
                    let m = &title.mips[mip as usize];
                    let pitch = (m.width * title.desc.element_bytes) as usize;
                    let len = pitch * m.height as usize;
                    let mut expected = vec![0; len];
                    let mut actual = vec![0; len];
                    title
                        .detile(&bytes, mip, 0, &mut expected, pitch, len)
                        .unwrap();
                    source
                        .detile(read, mip - drop, 0, &mut actual, pitch, len)
                        .unwrap();
                    assert!(
                        actual == expected,
                        "{swizzle:?}, dropped {drop}, title mip {mip}"
                    );
                }
            }
        }
    }

    #[test]
    fn texture_refresh_reads_retained_mips_and_keeps_array_slice_stride() {
        use super::{Last, fingerprint, staged, staged_memory, staging};
        use crate::{object, resource};
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
        use windows::core::Interface;

        let Some(host) = crate::host::host() else {
            eprintln!("no D3D12 device; skipping texture refresh");
            return;
        };
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4,
        };
        // SAFETY: the LUID comes from the device that creates the staging buffer.
        let adapter = unsafe {
            CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
                .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid()))
                .and_then(|a| a.GetDesc1())
        };
        if let Ok(adapter) = adapter {
            let n = adapter
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(adapter.Description.len());
            eprintln!(
                "texture refresh D3D12 adapter: {}",
                String::from_utf16_lossy(&adapter.Description[..n])
            );
        }
        for slices in [1u32, 2] {
            // Four RGBA8 mips, 512x512 down to 64x64. The last row fills
            // its entire 256-byte host pitch. The backing is ours.
            let words = [
                3u32,
                0,
                0,
                0,
                512,
                0,
                512,
                (4 << 16) | slices,
                28,
                1,
                0,
                0,
                0,
                0,
            ];
            // SAFETY: the 56-byte title resource description is plain numbers.
            let desc: resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let title = &resource::texture_layout(&desc).unwrap().planes[0].layout;
            let drop = resource::drop_levels(&desc);
            if std::env::var("VICTORY_TEXTURE_DROP_MIPS").as_deref() == Ok("1") {
                assert_eq!(drop, 1);
            }
            let mut bytes = vec![1u8; title.size as usize];
            let address = bytes.as_ptr() as u64;
            let mut image = std::ptr::null();
            assert_eq!(
                resource::create_at_address(
                    std::ptr::null(),
                    address,
                    &desc,
                    0,
                    std::ptr::null(),
                    std::ptr::from_ref(&ID3D12Resource::IID).cast(),
                    &mut image,
                ),
                object::S_OK
            );
            let upload = staging(image, resource::host_resource(image).unwrap()).unwrap();
            let (at, size, mips) = staged_memory(image).unwrap();
            // Adapted: our staged_memory also returns the mip ranges to sample.
            let mips = mips.as_deref().unwrap_or(&[]);
            assert_eq!(at, address);
            let cropped = drop > 0 && slices == 1;
            let expected = if cropped {
                title.mips[drop as usize - 1].offset
            } else {
                title.size
            };
            eprintln!(
                "texture refresh: drop={drop} slices={slices} title_bytes={} source_bytes={size} expected_bytes={expected}",
                title.size
            );
            assert_eq!(size, expected, "refresh source bytes with {slices} slices");
            assert_eq!(
                staged(image).unwrap().0,
                title.size,
                "original title budget"
            );
            let mut last = Last::default();
            let refresh = |last: &mut Last| {
                assert_eq!(upload.refresh_unlocked(last, size as usize), size as usize);
            };
            refresh(&mut last);
            let original = last.hash;
            let print = fingerprint(at, size, mips);
            upload.dirty.store(false, Relaxed);
            // The final byte lies in the largest mip in the final slice.
            *bytes.last_mut().unwrap() ^= 0x55;
            refresh(&mut last);
            assert_eq!(last.hash == original, cropped);
            assert_eq!(fingerprint(at, size, mips) == print, cropped);
            assert_eq!(upload.dirty.load(Relaxed), !cropped);

            upload.dirty.store(false, Relaxed);
            let slice = slices - 1;
            let source = title.offset(drop, 0, 0, slice).unwrap() as usize;
            bytes[0] ^= 0x11; // the first window sampled by the recheck
            bytes[source] = 0xab;
            refresh(&mut last);
            assert_ne!(last.hash, original);
            assert_ne!(fingerprint(at, size, mips), print);
            assert!(upload.dirty.load(Relaxed));
            let host_mip = (slice * (4 - drop)) as usize;
            let offset = upload.footprints[host_mip].Offset as usize;
            // SAFETY: first byte of this live, mapped staging subresource.
            assert_eq!(unsafe { upload.data.add(offset).read() }, 0xab);
            // The same texel's other channels retain their original bytes.
            // SAFETY: the next three bytes are in the same RGBA8 texel.
            assert_eq!(
                unsafe { std::slice::from_raw_parts(upload.data.add(offset + 1), 3) },
                &[1, 1, 1]
            );
            std::mem::drop(upload);
            assert_eq!(
                object::slot::<extern "system" fn(object::This) -> u32>(image, 0x10)(image),
                0
            );
        }
    }

    /// The texture both device-loss runs named last (16 texels wide, 4 or 8
    /// high, format 10, layout 0x100, console flags 0x300000; its memory
    /// began 0x300 into a page in run 73 and 0x700 in run 72, and here also
    /// 0xf00, so that a page line runs through it): what the detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(size >= row * height as usize, "the layout holds every texel");
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(std::ptr::null(), (pages + 1) * 4096, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE), base.cast());
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(direct[y * pitch..][..row].iter().any(|&b| b != 0xee), "row {y} written");
                    assert!(direct[y * pitch + row..(y + 1) * pitch].iter().all(|&b| b == 0xee), "nothing past row {y}");
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}
