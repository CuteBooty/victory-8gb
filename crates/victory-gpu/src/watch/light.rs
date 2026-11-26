//! Published guard answers, modelled on a tree from the avatarmanz lineage.
//! Keep the clean bit and the journal write it vouches for in ONE atomic
//! word. Two shadows can have copied the same block at different askings.
use super::{BLOCK, BlockRuns, Guards, guards};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::atomic::{
    AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIGHT_REFRESH").is_some_and(|v| v == "1"))
}

// Ring submissions and driver-private pointers may have no bounded buffer list.
// Once enabled in a light-refresh process, fall back to source reads for life.
static OPAQUE_IO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn opaque_io() -> bool { OPAQUE_IO.load(Acquire) }

pub fn quarantine_opaque_io() {
    if !on() { return; }
    let mut g = guards();
    if opaque_io() { return; }
    OPAQUE_IO.store(true, Release);
    super::open_here(&mut g, 0, usize::MAX, Cause::Kernel);
    g.clean = BlockRuns::new();
    QUARANTINED.fetch_add(1, Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Cause {
    Fault = 1,
    Plain,
    Kernel,
    Landing,
    Quiet,
    Aside,
    Alias,
    Remap,
    Physical,
    Journal,
    Check,
}
const NAMES: [&str; 12] = [
    "unknown", "fault", "plain", "kernel", "landing", "quiet", "aside", "alias", "remap", "physical", "journal",
    "check",
];
const PIECE: usize = 1 << 13;
// 128 TiB of user address space; allocate 64 KiB of stamps per touched 512 MiB.
struct Bits<const N: usize>([OnceLock<Box<[AtomicU64]>>; N]);
static BITS: Bits<{ 1 << 18 }> = Bits::new();

impl<const N: usize> Bits<N> {
    const fn new() -> Self {
        Self([const { OnceLock::new() }; N])
    }

    fn entry(&self, page: usize) -> Option<&AtomicU64> {
        Some(&self.0.get(page / PIECE)?.get()?[page % PIECE])
    }

    // Only the guards lock writes the words; readers never acquire it.
    fn set(&self, page: usize, stamp: u64) -> bool {
        self.set_state(page, stamp, 1)
    }

    // State 2 is a quarantined prediction used only by the diagnostics.
    fn set_state(&self, page: usize, stamp: u64, state: u64) -> bool {
        let Some(chunk) = self.0.get(page / PIECE) else {
            return false;
        };
        if stamp == 0 || stamp > u64::MAX >> 6 {
            self.clear(page, Cause::Journal);
            return false;
        }
        let words = chunk.get_or_init(|| (0..PIECE).map(|_| AtomicU64::new(0)).collect());
        let word = &words[page % PIECE];
        // Writers hold GUARDS. Retain the high-water mark even while clear.
        let old = word.load(Relaxed);
        if stamp < old >> 6 { return false; }
        word.store((stamp << 6) | (old & 60) | state, Release);
        true
    }

    fn clear(&self, page: usize, cause: Cause) -> bool {
        self.entry(page).is_some_and(|word| {
            let old = word.load(Relaxed);
            let reason = if cause == Cause::Check { old & 60 } else { (cause as u64) << 2 };
            word.store((old & !63) | reason, Release);
            old & 1 != 0
        })
    }

    fn all(&self, from: usize, to: usize, accepts: impl FnMut(usize, u64) -> bool) -> bool {
        self.all_states(from, to, 1, accepts)
    }

    fn all_states(&self, from: usize, to: usize, states: u64, mut accepts: impl FnMut(usize, u64) -> bool) -> bool {
        from < to
            && (from / BLOCK..to.div_ceil(BLOCK)).all(|p| {
                let word = self.entry(p).map_or(0, |word| word.load(Acquire));
                word & states != 0 && accepts(p, word >> 6)
            })
    }
}

pub(crate) fn published(from: usize, to: usize) -> bool {
    on() && !opaque_io() && !super::view_ask() && BITS.all(from, to, |_, _| true)
}

pub(crate) fn candidate(from: usize, to: usize) -> bool {
    from < to && published(from, from + 1)
}

#[cfg(feature = "diagnostics")]
pub(crate) fn predicted(from: usize, to: usize) -> bool {
    BITS.all_states(from, to, 3, |_, _| true)
}

#[cfg(feature = "diagnostics")]
pub(crate) fn trial_candidate(from: usize, to: usize) -> bool {
    HAD_FAILURE.load(Relaxed) && !super::view_ask() && from < to && BITS.all_states(from, from + 1, 3, |_, _| true)
}

#[cfg(feature = "diagnostics")]
static HAD_FAILURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn current(from: usize, to: usize, base: usize, copied: &[u64], ranged: u64) -> bool {
    current_states(from, to, base, copied, ranged, 1)
}

pub(crate) fn current_states(from: usize, to: usize, base: usize, copied: &[u64], ranged: u64, states: u64) -> bool {
    !opaque_io() && BITS.all_states(from, to, states, |page, write| {
        page.checked_sub(base)
            .and_then(|i| copied.get(i))
            .is_some_and(|&stamp| stamp.max(ranged) >= write)
    })
}

pub(super) fn clear(page: usize, cause: Cause) {
    if on() && BITS.clear(page, cause) {
        CLEARS[cause as usize].fetch_add(1, Relaxed);
    }
}

/// Called after journal insertion while the journal writer still orders
/// overlapping polls. Recheck under GUARDS: a fault may have beaten us.
pub(super) fn publish_poll(first: usize, last: usize, pages: &[usize], stamp: u64, written: &BTreeMap<usize, u64>) {
    if !on() || super::view_ask() { return; }
    if pages.is_empty() { return retry_clean(first, last, written); }
    let mut g = guards();
    for &p in pages { publish_block(&mut g, p, stamp); }
    // Mixed ranges can repeatedly miss on a clean block beside a hot one.
    // Guard results are in address order. Each newly observed page was
    // already considered above, including pages withheld on this poll.
    for (&p, &w) in written.range(first..last) {
        if unpublished(p) && pages.binary_search(&p).is_err() { retry_block(&mut g, p, w); }
    }
}

fn unpublished(p: usize) -> bool {
    BITS.entry(p).is_none_or(|word| word.load(Acquire) & 1 == 0)
}

fn retry_block(g: &mut Guards, p: usize, stamp: u64) {
    if g.clean.contains_range(p, p + 1) && publish_block(g, p, stamp) {
        RETRIED.fetch_add(1, Relaxed);
    }
}

// The caller still orders overlapping polls through the journal or region
// lock. Recheck clean, aliases and writer windows under GUARDS before setting.
pub(super) fn retry_clean(first: usize, last: usize, written: &BTreeMap<usize, u64>) {
    if !on() || super::view_ask() { return; }
    let mut missing = written.range(first..last).filter(|&(&p, _)| unpublished(p)).peekable();
    if missing.peek().is_none() { return; }
    let mut g = guards();
    for (&p, &stamp) in missing {
        retry_block(&mut g, p, stamp);
    }
}

fn publish_block(g: &mut Guards, p: usize, stamp: u64) -> bool {
    let mut busy = false;
    let mut quarantined = false;
    let safe = super::view_guard()
        && g.clean.contains_range(p, p + 1)
        && crate::inplace::publishable_view(p * BLOCK, |section, offset, shown| {
            quarantined = g.windows.checked_pages.get(&section)
                .is_some_and(|r| r.contains_range(offset as usize / BLOCK, offset as usize / BLOCK + 1));
            busy = g.windows.busy(section, offset);
            !busy
                && shown.iter().all(|at| {
                    let b = at / BLOCK;
                    g.protected.contains(&b) && (shown.len() == 1 || g.aliased.contains(&b))
                })
        }).unwrap_or(false);
    if !safe {
        if busy {
            // A bypass writer can change the page repeatedly without a
            // fault. The slow poll must report it dirty again too.
            g.clean.remove(p);
        }
        if BITS.entry(p).is_some_and(|word| word.load(Relaxed) & 3 != 0) {
            clear(p, Cause::Alias);
        }
        WITHHELD.fetch_add(1, Relaxed);
    } else if quarantined || g.windows.failed_block(p) {
        // Continue checking the prediction, but force the slow path to
        // report a write to every real consumer after a mismatch.
        #[cfg(feature = "diagnostics")]
        if let Some(old) = BITS.entry(p).map(|w| w.load(Acquire))
            && old & 2 != 0 && !g.windows.fingerprints.is_empty()
        {
            // Forced slow copies advance the journal even without a guard
            // clear. Preserve this diagnostic baseline across those copies.
            let probe = &mut g.windows.fingerprints[p % PROBE_SLOTS];
            if (probe.0, probe.1) == (p, old >> 6) { probe.1 = stamp; }
        }
        BITS.set_state(p, stamp, 2);
        g.clean.remove(p);
        WITHHELD.fetch_add(1, Relaxed);
    } else if BITS.set(p, stamp) {
        return true;
    } else {
        WITHHELD.fetch_add(1, Relaxed);
    }
    false
}

type Physical = (usize, u64, usize); // section handle, byte offset, length
struct Access {
    address: Option<(usize, usize)>,
    mapping: Option<usize>,
    spans: Vec<Physical>,
    cause: Cause,
}

pub(super) struct Windows {
    #[cfg(feature = "diagnostics")]
    failures: BTreeMap<usize, (usize, u64)>, // anonymous block ID, occurrences
    #[cfg(feature = "diagnostics")]
    fingerprints: Vec<(usize, u64, u64)>, // block, journal write, hash
    checked_pages: BTreeMap<usize, BlockRuns>,
    title_protection: BlockRuns,
    asynchronous_addresses: BlockRuns,
    asynchronous_pages: BTreeMap<usize, BlockRuns>,
    next: u64,
    active: BTreeMap<u64, Access>,
}

impl Windows {
    pub(super) const fn new() -> Self {
        Self {
            #[cfg(feature = "diagnostics")]
            failures: BTreeMap::new(),
            #[cfg(feature = "diagnostics")]
            fingerprints: Vec::new(),
            checked_pages: BTreeMap::new(),
            next: 0,
            active: BTreeMap::new(),
            title_protection: BlockRuns::new(),
            asynchronous_addresses: BlockRuns::new(),
            asynchronous_pages: BTreeMap::new(),
        }
    }

    #[cfg(all(feature = "diagnostics", test))]
    pub(super) fn failure_count(&self, p: usize) -> u64 {
        self.failures.get(&p).map_or(0, |entry| entry.1)
    }

    fn failed_block(&self, _p: usize) -> bool {
        #[cfg(feature = "diagnostics")]
        { return self.failures.contains_key(&_p); }
        #[cfg(not(feature = "diagnostics"))]
        { false }
    }

    fn exclude(&mut self, (section, offset, len): Physical) {
        let (first, last) = (
            offset as usize / BLOCK,
            (offset as usize).saturating_add(len).div_ceil(BLOCK),
        );
        self.asynchronous_pages
            .entry(section)
            .or_insert_with(BlockRuns::new)
            .insert_range(first, last);
    }

    fn enroll(&mut self, at: usize, (section, offset, len): Physical) {
        let end = at.saturating_add(len);
        // An overlapped ReadFile may still target the address when it is remapped.
        if self
            .asynchronous_addresses
            .runs
            .range(..end.div_ceil(BLOCK))
            .next_back()
            .is_some_and(|(_, &past)| past > at / BLOCK)
        {
            self.exclude((section, offset, len));
        }
        for access in self.active.values_mut() {
            if let Some((a, e)) = access.address {
                let (lo, hi) = (a.max(at), e.min(end));
                if lo < hi {
                    let span = (section, offset + (lo - at) as u64, hi - lo);
                    if !access.spans.contains(&span) {
                        access.spans.push(span);
                    }
                }
            }
        }
    }

    pub(super) fn remapped(&mut self, first: usize, last: usize) {
        self.title_protection.remove_range(first, last);
    }

    // Do not replace a protection just requested by the caller, or make a
    // kernel destination read-only before its asynchronous write completes.
    pub(super) fn may_arm(&self, first: usize, last: usize) -> bool {
        let overlaps = |runs: &BlockRuns| runs.runs.range(..last).next_back().is_some_and(|(_, &end)| end > first);
        !overlaps(&self.title_protection) && !overlaps(&self.asynchronous_addresses)
            && !self.active.values().any(|a| matches!(a.cause, Cause::Kernel | Cause::Remap)
                && a.address.is_some_and(|(from, to)| from / BLOCK < last && first < to.div_ceil(BLOCK)))
    }

    fn busy(&self, section: usize, offset: u64) -> bool {
        let page = offset as usize / BLOCK;
        self.asynchronous_pages
            .get(&section)
            .is_some_and(|runs| runs.contains_range(page, page + 1))
            || self.active.values().any(|access| {
                access.spans.iter().any(|&(s, from, len)| {
                    s == section
                        && from < offset.saturating_add(BLOCK as u64)
                        && offset < from.saturating_add(len as u64)
                })
            })
    }
}

pub(super) fn invalidate(g: &mut Guards, spans: &[Physical], cause: Cause) {
    for &(section, offset, len) in spans {
        for (at, bytes) in crate::inplace::views_of_section(section, offset, len) {
            let (first, last) = (at / BLOCK, at.saturating_add(bytes).div_ceil(BLOCK));
            for &b in g.protected.range(first..last) {
                clear(b, cause);
            }
            g.clean.remove_range(first, last);
        }
    }
}

/// Keep affected physical pages unpublished throughout an unguarded
/// writer's lifetime, including partial writes and early error returns.
#[must_use]
pub struct WriteWindow(u64);

impl WriteWindow {
    pub fn quarantine(self) {
        let mut g = guards();
        if let Some(access) = g.windows.active.remove(&self.0) {
            if let Some((from, to)) = access.address {
                let excluded = if access.cause == Cause::Remap {
                    &mut g.windows.title_protection
                } else {
                    &mut g.windows.asynchronous_addresses
                };
                excluded.insert_range(from / BLOCK, to.div_ceil(BLOCK));
            }
            for span in access.spans {
                g.windows.exclude(span);
            }
            QUARANTINED.fetch_add(1, Relaxed);
        }
        // Its active entry is already gone.
        drop(g);
    }

    fn begin(g: &mut Guards, access: Access) -> Self {
        invalidate(g, &access.spans, access.cause);
        if access.cause == Cause::Remap && let Some((at, end)) = access.address {
            // Give up our artificial protection before Windows records the
            // previous protection. A partial change may split a guard block.
            super::open_here(g, at, end, Cause::Remap);
        }
        g.windows.next += 1;
        let id = g.windows.next;
        g.windows.active.insert(id, access);
        Self(id)
    }
}

impl Drop for WriteWindow {
    fn drop(&mut self) {
        let mut g = guards();
        if let Some(access) = g.windows.active.remove(&self.0) {
            // I/O may have started between the OS map and title_memory_view.
            if let Some(at) = access.mapping {
                g.windows.enroll(at, access.spans[0]);
            }
            // A slow poll could have armed the page while bytes were in flight.
            invalidate(&mut g, &access.spans, access.cause);
        }
    }
}

// A synchronous writer may omit guard enrollment only while its empty mapping
// stays empty. A nonblocking pin also works for callers already holding a pin.
#[must_use]
pub struct SynchronousWrite {
    _pin: Option<super::Reading>,
    _window: Option<WriteWindow>,
}

pub(crate) fn address_io(at: usize, len: usize, cause: Cause) -> Option<SynchronousWrite> {
    if !on() || len == 0 { return None; }
    if let Some(pin) = super::title_memory_reading_now(at, len) {
        let end = at.saturating_add(len);
        let spans = crate::inplace::physical_spans(at, end);
        if spans.is_empty() {
            return Some(SynchronousWrite { _pin: Some(pin), _window: None });
        }
        let window = WriteWindow::begin(&mut guards(), Access {
            address: Some((at, end)), mapping: None, spans, cause,
        });
        return Some(SynchronousWrite { _pin: None, _window: Some(window) });
    }
    tracked_address(at, len, cause).map(|window| SynchronousWrite { _pin: None, _window: Some(window) })
}

pub fn synchronous_write_window(at: usize, len: usize) -> Option<SynchronousWrite> {
    address_io(at, len, Cause::Kernel)
}

fn tracked_address(at: usize, len: usize, cause: Cause) -> Option<WriteWindow> {
    if !on() || len == 0 {
        return None;
    }
    let end = at.saturating_add(len);
    let mut g = guards();
    let spans = crate::inplace::physical_spans(at, end);
    Some(WriteWindow::begin(
        &mut g,
        Access {
            address: Some((at, end)),
            mapping: None,
            spans,
            cause,
        },
    ))
}

/// An explicit commit may change the OS protection of an existing view
/// outside guard bookkeeping. Only registered physical memory is affected.
pub fn protection_window(at: usize, len: usize) -> Option<SynchronousWrite> {
    address_io(at, len, Cause::Remap)
}

impl SynchronousWrite {
    pub fn quarantine(self) {
        if let Some(window) = self._window { window.quarantine(); }
    }
}

pub fn kernel_write_window(at: usize, len: usize) -> Option<WriteWindow> {
    tracked_address(at, len, Cause::Kernel)
}

pub fn physical_write_window(section: usize, offset: u64, len: usize) -> Option<WriteWindow> {
    if !on() || len == 0 {
        return None;
    }
    Some(WriteWindow::begin(
        &mut guards(),
        Access {
            address: None,
            mapping: None,
            spans: vec![(section, offset, len)],
            cause: Cause::Physical,
        },
    ))
}

/// Before the OS makes (or removes) a writable view. Also enroll any new
/// physical destination under an outstanding address-based I/O operation.
pub fn mapping_window(at: usize, section: usize, offset: u64, len: usize) -> Option<WriteWindow> {
    if !on() || len == 0 {
        return None;
    }
    let mut g = guards();
    g.windows.enroll(at, (section, offset, len));
    Some(WriteWindow::begin(
        &mut g,
        Access {
            address: None,
            mapping: Some(at),
            spans: vec![(section, offset, len)],
            cause: Cause::Remap,
        },
    ))
}

pub(super) fn dirty_address(g: &mut Guards, at: usize, len: usize, cause: Cause) {
    if on() && len != 0 {
        invalidate(g, &crate::inplace::physical_spans(at, at.saturating_add(len)), cause);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Miss { Clean, Hole, Unknown, Many, Hot, NoView, PrivateAhead, PrivateLive, Dirty, Busy, Other, UnclassifiedView, OpaqueIo }
const MISSES: [&str; 13] = ["unpublished-clean", "hole", "unknown-map", ">64-aliases", "hot",
    "no-view", "private-ahead", "private-live", "dirty", "writer/quarantine", "other", "unsampled-view", "opaque-io"];
static BY_CAUSE: [AtomicU64; MISSES.len()] = [const { AtomicU64::new(0) }; MISSES.len()];
static RETRIED: AtomicU64 = AtomicU64::new(0);
pub(super) static PRIVATE_POLLS: AtomicU64 = AtomicU64::new(0);

pub(super) fn note(guarded: bool, why: Miss) {
    if guarded && on() { BY_CAUSE[why as usize].fetch_add(1, Relaxed); }
}

// Attribute each range to its first unavailable block, before arming it.
pub(super) fn view_miss(g: &Guards, first: usize, last: usize) {
    if !on() { return; }
    // The timing run counts guard polls without another alias-table lookup.
    #[cfg(feature = "diagnostics")]
    let detailed = interval() != 0;
    #[cfg(not(feature = "diagnostics"))]
    let detailed = false;
    if !detailed { return note(true, Miss::UnclassifiedView); }
    let p = (first..last).find(|&p| unpublished(p));
    let why = p.map_or(Miss::Other, |p| {
        if g.absent.contains(&p) { return Miss::Hole; }
        if g.heat.get(&p).is_some_and(|h| h.held_since_arm && crate::queue::presented() < h.until) {
            return Miss::Hot;
        }
        match crate::inplace::publishable_view(p * BLOCK, |s, off, _| {
            !g.windows.busy(s, off) && !g.windows.checked_pages.get(&s)
                .is_some_and(|r| r.contains_range(off as usize / BLOCK, off as usize / BLOCK + 1))
        }) {
            Err(reason) => reason,
            Ok(false) => Miss::Busy,
            Ok(true) if g.clean.contains_range(p, p + 1) => Miss::Clean,
            Ok(true) => Miss::Dirty,
        }
    });
    note(true, why);
}

pub(super) static ANSWERED: AtomicU64 = AtomicU64::new(0);
pub(super) static FALLBACK: AtomicU64 = AtomicU64::new(0);
pub(crate) static SKIPPED: AtomicU64 = AtomicU64::new(0);
static WITHHELD: AtomicU64 = AtomicU64::new(0);
pub(crate) static PENDING_HELD: AtomicU64 = AtomicU64::new(0);
pub(crate) static SHADOW_HELD: AtomicU64 = AtomicU64::new(0);
static QUARANTINED: AtomicU64 = AtomicU64::new(0);
static CLEARS: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];
#[cfg(feature = "diagnostics")]
pub(crate) fn sample_due() -> bool {
    static SHADOW: AtomicU64 = AtomicU64::new(0);
    due(&SHADOW)
}

#[cfg(feature = "diagnostics")]
pub(super) fn answer_sample_due() -> bool {
    static ANSWER: AtomicU64 = AtomicU64::new(0);
    due(&ANSWER)
}

#[cfg(feature = "diagnostics")]
fn due(tick: &AtomicU64) -> bool {
    let every = interval();
    every != 0 && tick.fetch_add(1, Relaxed).is_multiple_of(every)
}

#[cfg(feature = "diagnostics")]
fn interval() -> u64 {
    static CHECK: OnceLock<u64> = OnceLock::new();
    *CHECK.get_or_init(|| match std::env::var("VICTORY_LIGHT_REFRESH_CHECK")
        .unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        // Preserve the original =1 interval. "all" checks every candidate.
        "1" | "true" | "yes" | "on" => 256,
        "all" => 1,
        value => value.parse().unwrap_or(0),
    })
}
static ANSWER_SAMPLES: AtomicU64 = AtomicU64::new(0);
pub(crate) static SAMPLES: AtomicU64 = AtomicU64::new(0);
pub(crate) static MISMATCHES: AtomicU64 = AtomicU64::new(0);
pub(crate) static INCONCLUSIVE: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn mismatch(at: usize, check: &'static str) {
    failed(&mut guards(), at, check);
}

#[cfg(feature = "diagnostics")]
fn failed(g: &mut Guards, at: usize, check: &'static str) {
    HAD_FAILURE.store(true, Relaxed);
    let block = at / BLOCK;
    let next = g.windows.failures.len() + 1;
    let (id, count) = g.windows.failures.entry(block).or_insert((next, 0));
    *count += 1;
    if *count <= 3 {
        let cause = BITS.entry(block).map_or(0, |w| (w.load(Acquire) & 60) as usize >> 2);
        tracing::warn!(check, block_id = *id, occurrence = *count, last_clear = NAMES[cause], "light refresh mismatch");
    }
    MISMATCHES.fetch_add(1, Relaxed);
    clear(block, Cause::Check);
    g.clean.remove(block);
    let spans = crate::inplace::physical_spans(at, at.saturating_add(1));
    invalidate(g, &spans, Cause::Check);
    for (section, offset, _) in spans {
        let page = offset as usize / BLOCK;
        let runs = g.windows.checked_pages.entry(section).or_insert_with(BlockRuns::new);
        if !runs.contains_range(page, page + 1) {
            runs.insert(page);
            QUARANTINED.fetch_add(1, Relaxed);
        }
    }
}

#[cfg(feature = "diagnostics")]
const PROBE_SLOTS: usize = 2048;

#[cfg(feature = "diagnostics")]
pub(crate) fn with_check_buffer<R>(check: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; BLOCK]);
    }
    BUF.with_borrow_mut(check)
}

// Validate a range answer against independent guard, OS protection and
// journal state. Hold JOURNAL then GUARDS so a racing clear is inconclusive.
#[cfg(feature = "diagnostics")]
pub(super) fn check_answer(first: usize, last: usize, written: &BTreeMap<usize, u64>) -> bool {
    let mut g = guards();
    if !BITS.all_states(first * BLOCK, (last - 1) * BLOCK + 1, 3, |_, _| true) {
        INCONCLUSIVE.fetch_add(1, Relaxed);
        return false;
    }
    let counted = || {
        SAMPLES.fetch_add(1, Relaxed);
        ANSWER_SAMPLES.fetch_add(1, Relaxed);
    };
    for p in first..last {
        let word = BITS.entry(p).unwrap().load(Acquire);
        let safe = (word & 2 != 0 || g.clean.contains_range(p, p + 1))
            && written.get(&p) == Some(&(word >> 6))
            && crate::inplace::publishable_view(p * BLOCK, |s, off, shown| {
                !g.windows.busy(s, off) && shown.iter().all(|&at| {
                    g.protected.contains(&(at / BLOCK)) && (shown.len() == 1 || g.aliased.contains(&(at / BLOCK)))
                        && readonly_block(at)
                })
            }) == Ok(true);
        if !safe {
            counted();
            failed(&mut g, p * BLOCK, "range answer");
            return false;
        }
    }
    // A guard and its journal can agree while an unobserved writer changes
    // the bytes. Compare one full rotating block to its last sample at this
    // write stamp. Keep only bounded hashes and release GUARDS for the read.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let p = first + NEXT.fetch_add(1, Relaxed) as usize % (last - first);
    let word = BITS.entry(p).unwrap();
    let before = word.load(Acquire);
    drop(g);
    let hash = with_check_buffer(|buf| {
        use std::hash::Hasher;
        if !crate::host::read_into((p * BLOCK) as _, buf) { return None; }
        let mut hasher = std::hash::DefaultHasher::new();
        hasher.write(buf);
        Some(hasher.finish())
    });
    let mut g = guards();
    let Some(hash) = hash.filter(|_| word.load(Acquire) == before) else {
        INCONCLUSIVE.fetch_add(1, Relaxed);
        return false;
    };
    counted();
    let probes = &mut g.windows.fingerprints;
    if probes.is_empty() { probes.resize(PROBE_SLOTS, (0, 0, 0)); }
    let stamp = before >> 6;
    let old = std::mem::replace(&mut probes[p % PROBE_SLOTS], (p, stamp, hash));
    if (old.0, old.1) == (p, stamp) && old.2 != hash {
        failed(&mut g, p * BLOCK, "range bytes");
        return false;
    }
    true
}

#[cfg(feature = "diagnostics")]
fn readonly_block(mut at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualQuery};
    let end = at + BLOCK;
    while at < end {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { VirtualQuery(at as _, &mut info, size_of_val(&info)) } == 0 ||
            info.State != MEM_COMMIT || info.Type != MEM_MAPPED || info.Protect != PAGE_READONLY {
            return false;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at { return false; }
        at = next;
    }
    true
}

fn exclude_unseen(g: &mut Guards, at: usize, len: usize) {
    let spans = crate::inplace::physical_spans(at, at.saturating_add(len));
    invalidate(g, &spans, Cause::Check);
    for span in spans {
        g.windows.exclude(span);
    }
    QUARANTINED.fetch_add(1, Relaxed);
}

pub(crate) fn unseen_write(at: usize, len: usize) {
    if on() {
        exclude_unseen(&mut guards(), at, len);
    }
}

pub(crate) fn summary(frames: f64) -> String {
    if !on() {
        return String::new();
    }
    let per = |c: &AtomicU64| c.swap(0, Relaxed) as f64 / frames;
    let reasons = MISSES.iter().zip(&BY_CAUSE).map(|(name, n)| format!("{name}={:.1}", per(n)))
        .collect::<Vec<_>>().join(" ");
    let pending = per(&PENDING_HELD);
    let gpu_history = per(&SHADOW_HELD);
    let answer_samples = ANSWER_SAMPLES.swap(0, Relaxed);
    let retried = per(&RETRIED);
    let private = per(&PRIVATE_POLLS);
    let clears = CLEARS
        .iter()
        .zip(NAMES)
        .skip(1)
        .map(|(n, label)| format!("{label}={:.1}", per(n)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "; light refresh per frame: {:.1} refreshes skipped by bits, {:.1} view ranges answered by bits, \
        {:.1} fallback asks ({reasons}), {:.1} block publications withheld, {retried:.1} blocks republished, \
        {private:.1} successful private watches; bits cleared by cause: {clears}; \
        {:.1} GPU-written ranges withheld (pending={pending:.1} history={gpu_history:.1}); \
        check samples {}, mismatches {}, inconclusive {}, quarantines {} (interval totals, {answer_samples} range samples)",
        per(&SKIPPED),
        per(&ANSWERED),
        per(&FALLBACK),
        per(&WITHHELD),
        pending + gpu_history,
        SAMPLES.swap(0, Relaxed),
        MISMATCHES.swap(0, Relaxed),
        INCONCLUSIVE.swap(0, Relaxed),
        QUARANTINED.swap(0, Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_require_every_block_and_its_own_write_stamp() {
        let bits = Bits::<2>::new();
        let edge = PIECE * BLOCK;
        assert!(bits.set(PIECE - 1, 7));
        assert!(bits.all(edge - 1, edge, |_, write| write == 7));
        assert!(!bits.all(edge, edge, |_, _| true));
        assert!(!bits.all(edge - 1, edge + 1, |_, _| true));
        assert!(bits.set(PIECE, 9));
        assert!(bits.all(edge - 1, edge + 1, |p, w| [7, 9][p - (PIECE - 1)] == w));
        assert!(!bits.all(edge - 1, edge + 1, |_, w| w <= 8));
        assert!(bits.clear(PIECE, Cause::Remap));
        assert!(!bits.clear(PIECE, Cause::Remap));
        assert!(!bits.all(edge - 1, edge + 1, |_, _| true));
        assert!(!bits.set(2 * PIECE, 9));
        assert!(!bits.all(usize::MAX - 1, usize::MAX, |_, _| true));
    }

    #[test]
    fn an_older_stamp_never_replaces_a_published_or_cleared_newer_one() {
        let bits = Bits::<1>::new();
        assert!(bits.set(0, 9));
        assert!(!bits.set(0, 4));
        assert!(bits.all(0, 1, |_, w| w == 9));
        bits.clear(0, Cause::Fault);
        assert!(!bits.set(0, 4));
        assert!(!bits.all(0, 1, |_, _| true));
    }

    #[test]
    fn readers_never_accept_an_old_shadow_after_clear_and_republication() {
        let bits = Bits::<1>::new();
        bits.set(0, 4);
        std::thread::scope(|scope| {
            let (opened, read) = std::sync::mpsc::sync_channel(0);
            let (checked, resume) = std::sync::mpsc::sync_channel(0);
            let bits = &bits;
            let writer = scope.spawn(move || {
                bits.clear(0, Cause::Fault);
                opened.send(()).unwrap();
                resume.recv().unwrap();
                bits.set(0, 5);
            });
            read.recv().unwrap();
            assert!(!bits.all(0, 1, |_, _| true));
            checked.send(()).unwrap();
            writer.join().unwrap();
            assert!(!bits.all(0, 1, |_, w| w <= 4));
            assert!(bits.all(0, 1, |_, w| w <= 5));
        });
    }

    #[test]
    fn io_follows_remaps_and_quarantine_is_scoped_to_physical_pages() {
        let mut w = Windows::new();
        let at = 40 * BLOCK;
        let section = 123;
        w.active.insert(
            1,
            Access {
                address: Some((at + 31 * BLOCK, at + 32 * BLOCK)),
                mapping: None,
                spans: vec![],
                cause: Cause::Kernel,
            },
        );
        w.enroll(at, (section, 0, 2 << 20));
        assert!(!w.busy(section, 30 * BLOCK as u64));
        assert!(w.busy(section, 31 * BLOCK as u64));
        assert!(!w.busy(section + 1, 31 * BLOCK as u64));
        w.enroll(at + 31 * BLOCK, (section + 1, 0, BLOCK));
        assert!(w.busy(section + 1, 0));
        let span = w.active.remove(&1).unwrap().spans[0];
        w.exclude(span);
        assert!(!w.busy(section + 1, 0));
        assert!(w.busy(section, 31 * BLOCK as u64));
    }
}
