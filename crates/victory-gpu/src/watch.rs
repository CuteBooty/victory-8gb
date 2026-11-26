//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! Writes into physical pages through another view are seen too: the
//! runtime tells us the views of the pages it wrote itself ([`writing`],
//! xmem's `write_physical` and DirectStorage's page destinations), and a
//! write by the title through any view of a guarded page faults like one
//! at the guarded address (the guard makes every view of the page
//! read-only, [`open`] gives them all back; not a page of more than
//! [`MANY_VIEWS`] views). A
//! system call writing into a guarded block fails (none is known to).

use std::collections::BTreeMap;
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

mod blocks;
pub(crate) mod light;
mod port;
use light::Cause;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

#[derive(Default)]
struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: AtomicU64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges asked ahead for a submission's refreshes ([`ask_ahead`])
    /// and the asking that did: `writes` answers for a range inside one
    /// from the journal, without another kernel call.
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    /// The ahead asking stands for a whole submission ([`ask_once`]): no
    /// other ahead asking is made, and `ahead` stays, until
    /// [`ask_once_end`].
    kept: bool,
}

static JOURNAL: RwLock<Journal> = RwLock::new(Journal {
    asked: AtomicU64::new(0),
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

type Answer = (u64, Vec<(usize, u64)>);

impl Journal {
    fn next_ask(&self) -> u64 {
        self.asked.fetch_add(1, Relaxed) + 1
    }

    fn answer(&self, first: usize, last: usize, stamp: u64) -> Answer {
        (stamp, self.written.range(first..last).map(|(&p, &w)| (p, w)).collect())
    }

    fn ahead_answer(&self, first: usize, last: usize) -> Option<Answer> {
        covered(&self.ahead, first, last).then(|| {
            TABLE_HITS.fetch_add(1, Relaxed);
            self.answer(first, last, self.ahead_asked)
        })
    }

    fn fallback_ahead(&self, first: usize, last: usize, guarded: bool) -> Option<Answer> {
        let answer = self.ahead_answer(first, last)?;
        light::note(guarded, light::Miss::PrivateAhead);
        Some(answer)
    }

    #[cfg(test)]
    fn record(&mut self, first: usize, last: usize, pages: Vec<usize>) -> Answer {
        self.record_poll(first, last, pages, false)
    }

    fn record_poll(&mut self, first: usize, last: usize, pages: Vec<usize>, from_guards: bool) -> Answer {
        let now = self.next_ask();
        self.written.extend(pages.iter().map(|&page| (page, now)));
        if from_guards {
            light::publish_poll(first, last, &pages, now, &self.written);
        }
        self.answer(first, last, now)
    }
}

fn journal() -> RwLockWriteGuard<'static, Journal> {
    timed(JOURNAL.try_write(), || JOURNAL.write(), 0)
}

fn journal_read() -> RwLockReadGuard<'static, Journal> {
    timed(JOURNAL.try_read(), || JOURNAL.read(), 0)
}

pub(crate) fn split() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        crate::host::prep::on()
            && std::env::var_os("VICTORY_WATCH_SPLIT").is_some_and(|v| v == "1")
            // The separate ResetWriteWatch in quiet_copy_back can erase a
            // racing title store. Only use split polling with atomic resets.
            && std::env::var_os("VICTORY_BACK_QUIET").is_none_or(|v| v != "1")
    })
}

/// Each pair is accumulated waiting time (µs) and contended acquisitions.
static LOCK_WAITS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn timed<G>(attempt: std::sync::TryLockResult<G>, wait: impl FnOnce() -> std::sync::LockResult<G>, at: usize) -> G {
    match attempt {
        Ok(held) => held,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => {
            let started = std::time::Instant::now();
            let held = wait().unwrap_or_else(|e| e.into_inner());
            LOCK_WAITS[at].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
            LOCK_WAITS[at + 1].fetch_add(1, Relaxed);
            held
        }
    }
}

pub(crate) fn lock_waits() -> [u64; 6] {
    LOCK_WAITS.each_ref().map(|v| v.swap(0, Relaxed))
}

// Each 2 MiB section has one of 64 poll locks. Cover every section touched by
// the rounded ask, in stripe order, from the reset/guard check to publication.
// A hash collision costs concurrency only. A submission-wide ask may bridge
// holes between its input ranges and takes all stripes before the journal.
static REGIONS: [RwLock<()>; 64] = [const { RwLock::new(()) }; 64];

fn region_mask(first: usize, last: usize) -> u64 {
    if first >= last {
        return 0;
    }
    const PAGES_PER_SECTION: usize = (2 << 20) / PAGE;
    let start = first / PAGES_PER_SECTION;
    let count = ((last - 1) / PAGES_PER_SECTION - start + 1).min(64);
    (u64::MAX >> (64 - count)).rotate_left((start % 64) as u32)
}

// No allocation for the common single-section range.
type RegionLocks<G> = (Option<G>, Vec<G>);

fn region_locks<'a, G>(
    locks: &'a [RwLock<()>; 64], mut mask: u64, take: impl Fn(&'a RwLock<()>) -> G,
) -> RegionLocks<G> {
    let mut selected = std::iter::from_fn(|| {
        if mask == 0 {
            return None;
        }
        let m = &locks[mask.trailing_zeros() as usize];
        mask &= mask - 1;
        Some(take(m))
    });
    (selected.next(), selected.collect())
}

fn regions(mask: u64) -> RegionLocks<RwLockWriteGuard<'static, ()>> {
    region_locks(&REGIONS, mask, |m| timed(m.try_write(), || m.write(), 4))
}

fn read_regions(mask: u64) -> RegionLocks<RwLockReadGuard<'static, ()>> {
    region_locks(&REGIONS, mask, |m| timed(m.try_read(), || m.read(), 4))
}

static UPFRONT_KERNEL: AtomicU64 = AtomicU64::new(0);
static LIVE_KERNEL: AtomicU64 = AtomicU64::new(0);
static TABLE_HITS: AtomicU64 = AtomicU64::new(0);
static CLEAN_HITS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn poll_counts() -> [u64; 4] {
    [&UPFRONT_KERNEL, &LIVE_KERNEL, &TABLE_HITS, &CLEAN_HITS].map(|v| v.swap(0, Relaxed))
}

/// Ranges of one submission closer than this are asked about together:
/// walking the pages between them costs less than another kernel call (a
/// call that resets written pages costs 30-70 microseconds whatever the
/// range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// Asks the kernel about many ranges `(start, end)` at once, before the
/// [`writes`] of each: ranges close together are one asking, and `writes`
/// then answers from the journal until [`ask_ahead_done`]. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14). Ranges that are not write-watched, or lie alone, are
/// left to `writes`.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    poll_ahead(ranges, false);
}

// Keep only ranges of successful individual kernel calls. Containment is
// safe to fold; joining adjacent successes could span different reservations.
fn index_ahead(ranges: &mut Vec<(usize, usize)>) {
    ranges.sort_unstable_by_key(|&(first, last)| (first, std::cmp::Reverse(last)));
    let mut furthest = 0;
    ranges.retain(|&(_, last)| {
        if last <= furthest {
            return false;
        }
        furthest = last;
        true
    });
}

fn covered(ranges: &[(usize, usize)], first: usize, last: usize) -> bool {
    let n = ranges.partition_point(|&(a, _)| a <= first);
    n > 0 && last <= ranges[n - 1].1
}

#[derive(Default)]
struct AheadPoll {
    found: Vec<usize>,
    covered: Vec<(usize, usize)>,
    written: Vec<usize>,
}

fn poll_ahead(ranges: &[(usize, usize)], keep: bool) {
    // (`VICTORY_VIEW_KNOWN=1`: known view memory is left to `writes`, which
    // goes to the guards; asked here it failed, and with others theirs too,
    // asked again by halves.)
    let known: Vec<(usize, usize)>;
    let ranges = if view_known() {
        known = ranges.iter().copied().filter(|&(start, end)| !known_view_memory(start / PAGE, end.div_ceil(PAGE))).collect();
        VIEW_KNOWN_SKIPS.fetch_add((ranges.len() - known.len()) as u64, Relaxed);
        &known[..]
    } else {
        ranges
    };
    let mut pieces: Vec<_> = ranges.iter().filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE))).collect();
    if !keep && pieces.len() < 2 {
        return;
    }
    if pieces.is_empty() {
        // An empty submission can retire its table without reserving any
        // regions. No dirty bits are consumed.
        let mut j = journal();
        j.ahead_asked = j.next_ask();
        j.ahead.clear();
        j.kept = false;
        return;
    }
    pieces.sort_unstable();
    let split = split();
    let _regions = split.then(|| regions(u64::MAX));
    let mut j = journal();
    if j.kept && !keep {
        return;
    }
    let mut poll = AheadPoll { found: std::mem::take(&mut j.found), ..Default::default() };
    if split {
        // The bulk reset may bridge input ranges. All poll regions are
        // held until the entire standing answer and its stamp are published.
        drop(j);
        poll.scan(&pieces, ranges, keep);
        j = journal();
    } else {
        poll.scan(&pieces, ranges, keep);
    }
    let now = j.next_ask();
    j.ahead_asked = now;
    j.written.extend(poll.written.into_iter().map(|page| (page, now)));
    j.found = poll.found;
    j.ahead = poll.covered;
    j.kept = keep && !j.ahead.is_empty();
}

impl AheadPoll {
    fn scan(&mut self, mut pieces: &[(usize, usize)], ranges: &[(usize, usize)], keep: bool) {
        while let Some(&(first, mut last)) = pieces.first() {
            let mut k = 1;
            while k < pieces.len()
                && pieces[k].0 <= last + AHEAD_GAP
                && last.max(pieces[k].1) - first <= AHEAD_MAX
            {
                last = last.max(pieces[k].1);
                k += 1;
            }
            self.ask_run(&pieces[..k]);
            pieces = &pieces[k..];
        }
        if keep {
            // A submission-wide ask also polls singletons. Keep the original
            // request order for these retries across separate reservations.
            for &(start, end) in ranges {
                let (first, last) = (start / PAGE, end.div_ceil(PAGE));
                if start < end && !self.covered.iter().any(|&(a, b)| a <= first && last <= b) {
                    self.ask_kernel(first, last);
                }
            }
        }
        index_ahead(&mut self.covered);
    }

    fn ask_run(&mut self, pieces: &[(usize, usize)]) {
        if pieces.len() < 2 {
            return;
        }
        let first = pieces[0].0;
        let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
        if !self.ask_kernel(first, last) {
            let (a, b) = pieces.split_at(pieces.len() / 2);
            self.ask_run(a);
            self.ask_run(b);
        }
    }

    fn ask_kernel(&mut self, first: usize, last: usize) -> bool {
        UPFRONT_KERNEL.fetch_add(1, Relaxed);
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let Some(count) = reset_written(&mut self.found, first, last) else {
            return false;
        };
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            let us = started.elapsed().as_micros() as u64;
            add(C::Watches, 1);
            add(C::WatchUs, us);
            slow(us);
        }
        self.covered.push((first, last));
        for &at in &self.found[..count] {
            let page = at / PAGE;
            if self.written.last() != Some(&page) {
                self.written.push(page);
            }
        }
        true
    }
}

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    let mut j = journal();
    if !j.kept {
        j.ahead.clear();
    }
}

/// Only the submitting thread starts or ends the kept asking. A flush on
/// that thread can skip constructing its ranges when ask_ahead would
/// discard them. Each refresh still goes through writes as usual.
pub(crate) fn needs_batch_ask() -> bool {
    static SKIP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let skip = *SKIP.get_or_init(|| std::env::var_os("VICTORY_SKIP_HELD_ASK").is_none_or(|v| v != "0"));
    !skip || !journal_read().kept
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (on unless `VICTORY_ASK_ONCE=0`).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `VICTORY_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if once() {
        poll_ahead(ranges, true);
    }
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut j = journal();
    if j.kept {
        j.kept = false;
        j.ahead.clear();
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    let first = at / PAGE;
    let last = at.saturating_add(len).div_ceil(PAGE);
    let _regions = split().then(|| regions(region_mask(first, last)));
    let mut j = journal();
    if light::on() && len != 0 {
        let spans = crate::inplace::physical_spans(at, at.saturating_add(len));
        if !spans.is_empty() {
            light::invalidate(&mut guards(), &spans, Cause::Journal);
        }
    }
    if !j.kept || len == 0 {
        return;
    }
    let now = j.next_ask();
    for page in first..last {
        j.written.insert(page, now);
    }
}

/// Write a merged stretch in one shadow page. Only its originating shadow
/// may advance its copy stamp. Every other reader sees the journal write,
/// including readers served by a standing submission asking.
pub(crate) fn quiet_copy_back(_mapping: &Reading, at: usize, bytes: &[u8], copied: &mut u64) -> bool {
    use windows_sys::Win32::System::Memory::{GetWriteWatch, ResetWriteWatch};
    let page = at / PAGE;
    if *copied == 0 || bytes.is_empty() || bytes.len() > PAGE - at % PAGE {
        return false;
    }
    let first = at & !(KERNEL_PAGE - 1);
    let end = (at + bytes.len()).next_multiple_of(KERNEL_PAGE);
    let _regions = split().then(|| regions(region_mask(page, page + 1)));
    let mut j = journal();
    if j.written.get(&page).is_some_and(|&w| w > *copied) {
        return false;
    }
    let mut found = [std::ptr::null_mut(); PAGE / KERNEL_PAGE];
    let mut count = found.len();
    let mut granularity = 0;
    // A successful non-resetting query identifies private write-watched
    // memory. An existing title write anywhere on the kernel pages stays.
    LIVE_KERNEL.fetch_add(1, Relaxed);
    let query = unsafe {
        GetWriteWatch(0, first as _, end - first, found.as_mut_ptr(), &mut count, &mut granularity)
    };
    if query != 0 || count != 0 || granularity as usize != KERNEL_PAGE {
        return false;
    }
    // Keep the ordinary guard and placed-texture invalidations. Do not
    // call write_texture here: it would take the journal lock again.
    open(&guards(), at, at + bytes.len(), Cause::Quiet);
    let (ok, done) = crate::host::write_bytes_once(at, bytes);
    if done != 0 {
        let now = j.next_ask();
        j.written.insert(page, now);
        if j.kept || !j.ahead.is_empty() {
            j.ahead_asked = now;
        }
        crate::descriptor::host::remapped(at, done);
    }
    if !ok || done != bytes.len() {
        return false; // Ordinary write retries; keep every dirty bit.
    }
    // A concurrent CPU store on these same 4 KiB pages between the query
    // and reset can lose its bit. The journal serializes its pollers, but
    // the mapping hold cannot exclude a title store (fork PR 1188, opt-in).
    if unsafe { ResetWriteWatch(first as _, end - first) } != 0 {
        // Already written and journaled. Keep this shadow stale too.
        return true;
    }
    *copied = j.asked.load(Relaxed);
    true
}

#[cfg(feature = "diagnostics")]
fn note_kernel_ask(elapsed: std::time::Duration) {
    use crate::copystat::{C, add};
    add(C::KernelWatchNs, elapsed.as_nanos() as u64);
    add(C::KernelWatchCalls, 1);
}

/// What [`writes`] would say of `[start, end)` from the journal alone, if
/// the range lies in one asked ahead ([`ask_ahead`], [`ask_once`]): the
/// pages written since tracking began and the asking that last found each
/// written, with the number of the asking that stands (as [`writes`] gives
/// it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let _regions = split().then(|| read_regions(region_mask(first, last)));
    if light::opaque_io() { return None; }
    journal_read().ahead_answer(first, last)
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    writes_for_copy(start, end, guarded, None)
}

/// [`writes`], but never from an asking made ahead for a submission
/// ([`ask_ahead`], [`ask_once`]): the kernel is asked now, so what was
/// written since that asking is in the answer, noted under a new asking
/// (later than the ahead one, as [`wrote`] notes). For a reader that reads
/// right after asking and skips what was not written since its last
/// asking (`crate::heap_pass`, `VICTORY_DESC_DIRTY`; called only then).
/// Memory the kernel does not watch: as [`writes`] with `guarded`.
///
/// Merged with the light refresh and `VICTORY_WATCH_SPLIT`: the
/// reset and its publication follow the same order as every other poller
/// (the region locks with the split, else the journal's write lock across
/// the kernel call), and the stamp comes from [`Journal::next_ask`], so
/// stamps stay monotonic with `wrote`, the light refresh's publications and
/// quiet copy-backs. With the light refresh's opaque I/O quarantine the
/// kernel's answer is not trusted: `None`, so the heap pass compares all.
pub(crate) fn writes_now(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    if first >= last || light::opaque_io() {
        return None;
    }
    if known_view_memory(first, last) {
        // (`VICTORY_VIEW_KNOWN=1`: the kernel would refuse.)
        VIEW_KNOWN_SKIPS.fetch_add(1, Relaxed);
        return if guarded { writes(start, end, true) } else { None };
    }
    let pages = |found: &[usize], count: usize| -> Vec<usize> {
        found[..count].iter().map(|&at| at / PAGE).collect()
    };
    if split() {
        thread_local! {
            static FOUND: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        // Held from the reset until the stamp is published, as split_poll.
        let _regions = regions(region_mask(first, last));
        LIVE_KERNEL.fetch_add(1, Relaxed);
        if let Some(written) =
            FOUND.with_borrow_mut(|found| reset_written(found, first, last).map(|count| pages(found, count)))
        {
            return Some(journal().record_poll(first, last, written, false));
        }
    } else {
        let mut j = journal();
        LIVE_KERNEL.fetch_add(1, Relaxed);
        let mut found = std::mem::take(&mut j.found);
        let answer = reset_written(&mut found, first, last).map(|count| pages(&found, count));
        j.found = found;
        if let Some(written) = answer {
            return Some(j.record_poll(first, last, written, false));
        }
    }
    if guarded { writes(start, end, true) } else { None }
}

// Only normal shadow refreshes opt into a hold. Texture and pending-write
// checks re-arm in the usual way. Diagnostics return held blocks in address
// order and whether each was already held at its preceding check.
pub(crate) fn writes_for_copy(
    start: usize, end: usize, guarded: bool, held: Option<&mut Vec<(usize, bool)>>,
) -> Option<Answer> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    if light::opaque_io() {
        if guarded {
            light::FALLBACK.fetch_add(1, Relaxed);
            light::note(true, light::Miss::OpaqueIo);
        }
        return None;
    }
    if guarded && light::on() {
        let candidate = light::candidate(start, end);
        #[cfg(feature = "diagnostics")]
        let candidate = candidate || light::trial_candidate(start, end);
        if candidate {
            let j = journal_read();
            let published = light::published(start, end);
            #[cfg(feature = "diagnostics")]
            let published = {
                let trial = !published && light::trial_candidate(start, end)
                    && light::predicted(start, end);
                if (published || trial) && light::answer_sample_due() {
                    light::check_answer(first, last, &j.written) && published && light::published(start, end)
                } else { published }
            };
            if published {
                light::ANSWERED.fetch_add(1, Relaxed);
                return Some(j.answer(first, last, j.next_ask()));
            }
        }
        light::FALLBACK.fetch_add(1, Relaxed);
    }
    if split() {
        return split_poll(first, last, guarded, held);
    }
    let mut j = journal();
    if let Some(answer) = j.fallback_ahead(first, last, guarded) {
        return Some(answer);
    }
    let (pages, from_guards) = observe(&mut j.found, first, last, guarded, held, false)?;
    Some(j.record_poll(first, last, pages, from_guards))
}

fn split_poll(
    first: usize, last: usize, guarded: bool, held: Option<&mut Vec<(usize, bool)>>,
) -> Option<Answer> {
    // Every overlapping reset/publisher uses these locks, including asks
    // over gaps, wrote() and quiet copy-back. A later copy can never take
    // its stamp between consuming a dirty bit and publishing that write.
    let mask = region_mask(first, last);
    {
        let _reading = read_regions(mask);
        if let Some(answer) = journal_read().fallback_ahead(first, last, guarded) {
            return Some(answer);
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let clean = guarded && first < last && !view_ask() && {
            let g = guards_read();
            let clean = g.clean.contains_range(first, last);
            if clean { light::view_miss(&g, first, last); }
            clean
        };
        if clean {
            CLEAN_HITS.fetch_add(1, Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                let us = started.elapsed().as_micros() as u64;
                add(C::GuardUs, us);
                add(C::GuardCleanUs, us);
                add(C::WatchKnownView, 1);
                add(C::Watches, 1);
                add(C::WatchUs, us);
                slow(us);
            }
            let j = journal_read();
            light::retry_clean(first, last, &j.written);
            return Some(j.answer(first, last, j.next_ask()));
        }
    }
    // Recheck after upgrading. Another poll could have reset and published
    // a write or a submission could have installed a new standing answer.
    let _regions = regions(mask);
    if let Some(answer) = journal_read().fallback_ahead(first, last, guarded) {
        return Some(answer);
    }
    thread_local! {
        static FOUND: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let (pages, from_guards) = FOUND.with_borrow_mut(|found| observe(found, first, last, guarded, held, true))?;
    if pages.is_empty() {
        let j = journal_read();
        if from_guards { light::retry_clean(first, last, &j.written); }
        Some(j.answer(first, last, j.next_ask()))
    } else {
        Some(journal().record_poll(first, last, pages, from_guards))
    }
}

// An atomic reset-and-report. The caller owns the answer buffer and orders
// overlapping asks through publication. A store after this syscall sets
// the bit again, even while the earlier copy is still reading the bytes.
fn reset_written(found: &mut Vec<usize>, first: usize, last: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    found.resize(count.max(found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let result = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    };
    #[cfg(feature = "diagnostics")]
    note_kernel_ask(started.elapsed());
    if result == 0 && light::on() { light::PRIVATE_POLLS.fetch_add(1, Relaxed); }
    (result == 0).then_some(count)
}

// Read-only checks can overlap in split mode. Recheck after taking the
// exclusive lock: faults and remappings may have changed the protection.
fn known_changes(
    first: usize, last: usize, held: Option<&mut Vec<(usize, bool)>>, shared: bool,
) -> Option<Vec<usize>> {
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let clean = || {
        CLEAN_HITS.fetch_add(1, Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            let us = started.elapsed().as_micros() as u64;
            add(C::GuardUs, us);
            add(C::GuardCleanUs, us);
        }
        Some(Vec::new())
    };
    if shared {
        let g = guards_read();
        if g.clean.contains_range(first, last) {
            light::view_miss(&g, first, last);
            return clean();
        }
        if !g.known_view(first) {
            return None;
        }
    }
    // (Shared with renderer port 6, `port`.)
    let g = port::Held::take();
    if g.clean.contains_range(first, last) {
        light::view_miss(&g, first, last);
        return clean();
    }
    if !g.known_view(first) {
        return None;
    }
    #[cfg(feature = "diagnostics")]
    let guarding = std::time::Instant::now();
    light::view_miss(&g, first, last);
    note_views(first, last);
    let pages = match g {
        port::Held::Alone(g) => guard_chunked(g, first, last, held),
        port::Held::Shared(g) => port::guard_blocks(&g, first, last),
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        let us = guarding.elapsed().as_micros() as u64;
        add(C::GuardUs, us);
        add(C::GuardKnownDirtyUs, us);
    }
    Some(pages)
}

fn observe(
    found: &mut Vec<usize>, first: usize, last: usize, guarded: bool,
    mut held: Option<&mut Vec<(usize, bool)>>, shared: bool,
) -> Option<(Vec<usize>, bool)> {
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    if guarded && first < last && !view_ask()
        && let Some(pages) = known_changes(first, last, held.as_deref_mut(), shared)
    {
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            let us = started.elapsed().as_micros() as u64;
            add(C::WatchKnownView, 1);
            add(C::Watches, 1);
            add(C::WatchUs, us);
            slow(us);
        }
        return Some((pages, true));
    }
    if first < last && known_view_memory(first, last) {
        // (`VICTORY_VIEW_KNOWN=1`: the kernel would refuse; the guards
        // answer, as they do after its refusal.)
        VIEW_KNOWN_SKIPS.fetch_add(1, Relaxed);
        if !guarded {
            return None;
        }
        if let Some(pages) = guard_for_copy(first * PAGE, last * PAGE, held.as_deref_mut()) {
            return Some((pages, true));
        }
        // No view there now: forgotten as one, and the kernel asked.
        VIEWS.remove_range(first, last);
    }
    LIVE_KERNEL.fetch_add(1, Relaxed);
    let count = reset_written(found, first, last);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::WatchFailed, count.is_none() as u64);
    let pages = if let Some(count) = count {
        light::note(guarded, light::Miss::PrivateLive);
        found[..count].iter().map(|&at| at / PAGE).collect()
    } else {
        if !guarded {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let guarding = std::time::Instant::now();
        let written = guard_for_copy(first * PAGE, last * PAGE, held);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            let us = guarding.elapsed().as_micros() as u64;
            add(C::GuardUs, us);
            add(C::GuardFallbackUs, us);
        }
        written?
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        let us = started.elapsed().as_micros() as u64;
        add(C::Watches, 1);
        add(C::WatchUs, us);
        slow(us);
    }
    Some((pages, count.is_none()))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

/// Whether view memory once found (by the guards) is not asked of the
/// kernel's write watch, which refuses views, until it is unmapped
/// (`VICTORY_VIEW_KNOWN=1`; off by default). [`known_view`] already leaves
/// out blocks the guards hold read-only or know absent; a block a write
/// fault opened is neither, and its next asking went to the kernel to fail
/// before the guards answered. After avatarmanz's renderer port 6 (65f9707d and its
/// `VIEWS` in 9d723e88).
///
/// [`known_view`]: Guards::known_view
fn view_known() -> bool {
    #[cfg(test)]
    if VIEW_KNOWN_IN_TEST.get() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_KNOWN").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    /// [`view_known`] for what this thread does.
    static VIEW_KNOWN_IN_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Blocks found to be view memory by the guards ([`view_known`]);
/// forgotten when the runtime tells their mapping changed
/// ([`forget_guards`]). One wrongly here is only asked of the guards, which
/// find it no view and say so, and is then forgotten.
static VIEWS: blocks::Blocks = blocks::Blocks::new();

/// Askings not made of the kernel because the range is known view memory
/// ([`view_known`]), since the last line.
static VIEW_KNOWN_SKIPS: AtomicU64 = AtomicU64::new(0);

/// [`VIEW_KNOWN_SKIPS`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn view_known_skips() -> u64 {
    VIEW_KNOWN_SKIPS.swap(0, Relaxed)
}

/// Whether a block of `[first, last)` is known view memory ([`VIEWS`]): an
/// asking of the kernel about it would fail.
fn known_view_memory(first: usize, last: usize) -> bool {
    view_known() && !view_ask() && VIEWS.any(first, last)
}

/// `[first, last)` was found view memory by the guards ([`VIEWS`]).
fn note_views(first: usize, last: usize) {
    if view_known() && !VIEWS.contains_range(first, last) {
        VIEWS.insert_range(first, last);
    }
}

/// Whether [`writes`] asks the kernel's write watch even about view memory
/// the guards already hold (where it can only fail), as before fork PRs 670
/// and 685: `VICTORY_VIEW_ASK=1` (or `VICTORY_GUARD_ASK=1`), the one
/// switch for both PRs' skips of that asking.
fn view_ask() -> bool {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASK.get_or_init(|| {
        ["VICTORY_VIEW_ASK", "VICTORY_GUARD_ASK"]
            .iter()
            .any(|n| std::env::var_os(n).is_some_and(|v| v == "1"))
    })
}

/// Disjoint, maximally merged half-open intervals `[start, end)` of guarded
/// clean blocks: checking a 174 MiB heap (2,784 blocks) takes one O(log R)
/// lookup with zero allocations instead of collecting every clean block.
struct BlockRuns {
    runs: BTreeMap<usize, usize>,
}

impl BlockRuns {
    const fn new() -> Self {
        Self {
            runs: BTreeMap::new(),
        }
    }

    fn contains_range(&self, first: usize, last: usize) -> bool {
        first >= last
            || self
                .runs
                .range(..=first)
                .next_back()
                .is_some_and(|(_, &end)| end >= last)
    }

    // (Of the guards' clean blocks before renderer port 6; kept for its test.)
    #[cfg(test)]
    fn next_uncovered(&self, block: usize, last: usize) -> usize {
        if block >= last {
            return last;
        }
        match self.runs.range(..=block).next_back() {
            Some((_, &end)) if end > block => end.min(last),
            _ => block,
        }
    }

    #[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
    fn insert(&mut self, block: usize) {
        if let Some(next) = block.checked_add(1) {
            self.insert_range(block, next);
        }
    }

    fn insert_range(&mut self, mut from: usize, mut to: usize) {
        if from >= to {
            return;
        }
        if let Some((&s, &e)) = self.runs.range(..=from).next_back()
            && e >= from
        {
            from = s;
            to = to.max(e);
            self.runs.remove(&s);
        }
        loop {
            let next = self
                .runs
                .range((
                    std::ops::Bound::Included(from),
                    std::ops::Bound::Included(to),
                ))
                .next()
                .map(|(&s, &e)| (s, e));
            let Some((s, e)) = next else {
                break;
            };
            to = to.max(e);
            self.runs.remove(&s);
        }
        self.runs.insert(from, to);
    }

    #[cfg(test)]
    fn remove(&mut self, block: usize) {
        if let Some(next) = block.checked_add(1) {
            self.remove_range(block, next);
        }
    }

    fn remove_range(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        if let Some((&s, &e)) = self.runs.range(..=from).next_back()
            && e > from
        {
            self.runs.remove(&s);
            if s < from {
                self.runs.insert(s, from);
            }
            if e > to {
                self.runs.insert(to, e);
                return;
            }
        }
        let hit: Vec<(usize, usize)> = self
            .runs
            .range(from..to)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in hit {
            self.runs.remove(&s);
            if e > to {
                self.runs.insert(to, e);
            }
        }
    }
}

struct Guards {
    windows: light::Windows,
    // Per-address fault history while the mapping lives. Only a registered,
    // unaliased single-block view can be held open by a shadow refresh.
    heat: BTreeMap<usize, Heat>,
    /// Blocks we made read-only. (The four sets are of atomic words, for
    /// renderer port 6 ([`port_guard`]): a thread holding the lock shared
    /// changes the bits of the blocks it holds the claims of. Who holds it
    /// alone sees and changes them as before.)
    protected: blocks::Blocks,
    /// Of those, the ones not written since a copy took them.
    clean: blocks::Blocks,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    absent: blocks::Blocks,
    /// Blocks whose physical page was guarded together with its other views
    /// (those views' blocks are in it too): a write to one opens them all.
    /// A write range can be huge (`fread`), so only these are looked up.
    aliased: blocks::Blocks,
}

static GUARDS: RwLock<Guards> = RwLock::new(Guards {
    windows: light::Windows::new(),
    heat: BTreeMap::new(),
    protected: blocks::Blocks::new(),
    clean: blocks::Blocks::new(),
    absent: blocks::Blocks::new(),
    aliased: blocks::Blocks::new(),
});

impl Guards {
    fn known_view(&self, first: usize) -> bool {
        self.protected.contains(first) || self.absent.contains(first)
            || (hot_guards() && self.heat.get(&first).is_some_and(|h| h.held_since_arm)
                && crate::inplace::single_block_view(first * BLOCK))
    }
}

/// The guards, alone. (Diagnostics: how long each place holds them, [`Hold`].)
#[track_caller]
fn guards() -> Hold {
    if port::on() {
        port::exclusive_taken();
    }
    #[cfg(feature = "diagnostics")]
    let at = std::panic::Location::caller();
    let g = timed(GUARDS.try_write(), || GUARDS.write(), 2);
    Hold::new(
        g,
        true,
        #[cfg(feature = "diagnostics")]
        at,
    )
}

/// The guards, shared ([`Hold`], timed for the why2 by-holder line only).
#[track_caller]
fn guards_read() -> Hold<RwLockReadGuard<'static, Guards>> {
    #[cfg(feature = "diagnostics")]
    let at = std::panic::Location::caller();
    let g = timed(GUARDS.try_read(), || GUARDS.read(), 2);
    Hold::new(
        g,
        false,
        #[cfg(feature = "diagnostics")]
        at,
    )
}

/// With `VICTORY_FRAME_WHY2=1`, time shared and exclusive holds by holder
/// ([`crate::why_more`]). Diagnostics builds also count exclusive holds by
/// caller ([`hold_line`]) using the same elapsed time.
struct Hold<G = RwLockWriteGuard<'static, Guards>> {
    g: G,
    since: Option<std::time::Instant>,
    write: bool,
    #[cfg(feature = "diagnostics")]
    at: &'static std::panic::Location<'static>,
}

impl<G> Hold<G> {
    fn new(g: G, write: bool, #[cfg(feature = "diagnostics")] at: &'static std::panic::Location<'static>) -> Hold<G> {
        Hold {
            g,
            since: crate::why_more::guards_timed().then(std::time::Instant::now),
            write,
            #[cfg(feature = "diagnostics")]
            at,
        }
    }
}

impl<G: std::ops::Deref<Target = Guards>> std::ops::Deref for Hold<G> {
    type Target = Guards;
    fn deref(&self) -> &Guards {
        &self.g
    }
}

impl<G: std::ops::DerefMut<Target = Guards>> std::ops::DerefMut for Hold<G> {
    fn deref_mut(&mut self) -> &mut Guards {
        &mut self.g
    }
}

impl<G> Drop for Hold<G> {
    fn drop(&mut self) {
        let Some(since) = self.since else { return };
        let ns = since.elapsed().as_nanos() as u64;
        crate::why_more::guards_held(ns, self.write);
        #[cfg(feature = "diagnostics")]
        if self.write {
            places::add(self.at, ns);
        }
    }
}

/// Since the last line, by the place that took the guards alone: holds,
/// nanoseconds held, the longest hold. A fixed table of atomics (the
/// places are few and fixed), so a hold's end takes no lock.
#[cfg(feature = "diagnostics")]
mod places {
    use std::panic::Location;
    use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering::{AcqRel, Acquire, Relaxed}};

    const N: usize = 128;
    static AT: [AtomicPtr<Location<'static>>; N] = [const { AtomicPtr::new(std::ptr::null_mut()) }; N];
    /// Per place: holds, nanoseconds, the longest.
    static BY: [[AtomicU64; 3]; N] = [const { [const { AtomicU64::new(0) }; 3] }; N];
    /// Holds of places past the table's room.
    static LOST: AtomicU64 = AtomicU64::new(0);

    pub(super) fn add(at: &'static Location<'static>, ns: u64) {
        let p = at as *const Location<'static> as *mut Location<'static>;
        for (slot, by) in AT.iter().zip(&BY) {
            let mut here = slot.load(Acquire);
            if here.is_null() {
                here = match slot.compare_exchange(std::ptr::null_mut(), p, AcqRel, Acquire) {
                    Ok(_) => p,
                    Err(other) => other,
                };
            }
            if here == p {
                by[0].fetch_add(1, Relaxed);
                by[1].fetch_add(ns, Relaxed);
                by[2].fetch_max(ns, Relaxed);
                return;
            }
        }
        LOST.fetch_add(1, Relaxed);
    }

    /// The places held since the last call: (place, holds, ns, longest).
    pub(super) fn take() -> Vec<(&'static Location<'static>, u64, u64, u64)> {
        let mut out = Vec::new();
        for (slot, by) in AT.iter().zip(&BY) {
            let p = slot.load(Acquire);
            if p.is_null() {
                break;
            }
            let [n, ns, most] = by.each_ref().map(|v| v.swap(0, Relaxed));
            if n > 0 {
                // SAFETY: only `&'static Location`s are stored.
                out.push((unsafe { &*p }, n, ns, most));
            }
        }
        if LOST.swap(0, Relaxed) > 0 {
            tracing::debug!("the guards' holds by place: more places than {N}, some not counted");
        }
        out
    }
}

/// The guards' holds per frame by holder, the longest first (diagnostics).
fn hold_line(_frames: f64) {
    #[cfg(feature = "diagnostics")]
    {
        if !crate::why_more::guards_timed() {
            return;
        }
        let mut by = places::take();
        if by.is_empty() {
            return;
        }
        by.sort_by_key(|h| std::cmp::Reverse(h.2));
        let all: u64 = by.iter().map(|h| h.2).sum();
        let text: Vec<String> = by
            .iter()
            .take(8)
            .map(|(at, n, ns, most)| {
                format!("{}:{} {:.1} ms in {:.0} (longest {:.2} ms)", at.file().rsplit(['/', '\\']).next().unwrap_or(""), at.line(), *ns as f64 / 1e6 / _frames, *n as f64 / _frames, *most as f64 / 1e6)
            })
            .collect();
        tracing::info!(
            "the guards' lock held alone per frame: {:.1} ms (VICTORY_GUARD_CHUNK={}); by holder: {}",
            all as f64 / 1e6 / _frames,
            guard_chunk(),
            text.join(", ")
        );
    }
}

/// The frame line's lines of the guards: renderer port 6's ([`port`]) and
/// the holds of the lock ([`hold_line`]).
pub(crate) fn port_line(frames: f64) {
    port::line(frames);
    hold_line(frames);
}

fn hot_guards() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOT").is_some_and(|v| v == "1"))
}

fn reuse_arm() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GUARD_REUSE_ARM").is_some_and(|v| v == "1"))
}

#[derive(Default)]
struct Heat {
    last_fault: Option<u64>,
    streak: u8,
    until: u64,
    last_counted: Option<u64>,
    held_since_arm: bool,
}

impl Heat {
    fn fault(&mut self, frame: u64) {
        if frame < self.until || self.last_fault == Some(frame) {
            return;
        }
        self.streak = if frame.checked_sub(1).is_some_and(|f| self.last_fault == Some(f)) {
            self.streak.saturating_add(1)
        } else {
            1
        };
        self.last_fault = Some(frame);
        if self.streak == 3 {
            self.until = frame.saturating_add(8);
            self.streak = 0;
        }
    }
}

fn guard_ask() -> bool {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1"))
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
#[cfg(test)]
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    guard_for_copy(start, end, None)
}

fn guard_for_copy(start: usize, end: usize, held: Option<&mut Vec<(usize, bool)>>) -> Option<Vec<usize>> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let known = !guard_ask() && guards_read().known_view(first);
    if !known && !is_view(start, end) {
        light::note(true, light::Miss::NoView);
        return None;
    }
    install();
    note_views(first, last);
    let g = port::Held::take();
    light::view_miss(&g, first, last);
    Some(match g {
        port::Held::Alone(g) => guard_chunked(g, first, last, held),
        port::Held::Shared(g) => port::guard_blocks(&g, first, last),
    })
}

/// How many blocks an asking guards per hold of the lock alone
/// (`VICTORY_GUARD_CHUNK=<blocks>`; unset or 0: the whole range in one
/// hold, as before; 64 suggested). A refresh of a big range (the title's
/// 174 MB heap is 2,784 blocks) held the lock across every protection
/// change of it, and the title's write faults waited for it (81.5 ms a
/// frame over 667 faults in two reads of the same run).
fn guard_chunk() -> usize {
    #[cfg(test)]
    if CHUNK_IN_TEST.get() != 0 {
        return CHUNK_IN_TEST.get();
    }
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_GUARD_CHUNK").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

#[cfg(test)]
thread_local! {
    /// [`guard_chunk`] for what this thread does (0: as set).
    static CHUNK_IN_TEST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Times [`guard_chunked`] let the lock go between chunks, since the start.
static CHUNK_RELEASES: AtomicU64 = AtomicU64::new(0);

/// [`guard_locked`] of `[first, last)` with the lock alone, let go and
/// taken again between chunks of [`guard_chunk`] blocks. Each chunk is
/// what an asking of that part alone does: its blocks are made read-only
/// and noted clean under the lock, so a write between chunks either lands
/// before its block is guarded (and is read by the caller's copy, which
/// comes after) or faults and opens it (told at the next asking). The
/// light refresh publishes after the whole asking and checks each block
/// again under the lock; the heat of `VICTORY_GUARD_HOT` changes only
/// under it.
fn guard_chunked(mut g: Hold, first: usize, last: usize, mut held: Option<&mut Vec<(usize, bool)>>) -> Vec<usize> {
    let chunk = guard_chunk();
    if chunk == 0 || last.saturating_sub(first) <= chunk {
        return guard_locked(&mut g, first, last, held);
    }
    let mut written = Vec::new();
    let mut from = first;
    loop {
        let to = from.saturating_add(chunk).min(last);
        written.extend(guard_locked(&mut g, from, to, held.as_deref_mut()));
        if to >= last {
            return written;
        }
        drop(g);
        CHUNK_RELEASES.fetch_add(1, Relaxed);
        g = guards();
        from = to;
    }
}

fn guard_locked(g: &mut Guards, first: usize, last: usize, held: Option<&mut Vec<(usize, bool)>>) -> Vec<usize> {
    let frame = held.as_ref().filter(|_| hot_guards()).map(|_| crate::queue::presented());
    guard_blocks(g, first, last, held, frame)
}

fn guard_blocks(
    g: &mut Guards, first: usize, last: usize, mut held: Option<&mut Vec<(usize, bool)>>, frame: Option<u64>,
) -> Vec<usize> {
    if light::opaque_io() { return (first..last).collect(); }
    let mut written = Vec::new();
    if g.clean.contains_range(first, last) {
        // Nothing written, mapped or new since the range was last taken.
        return written;
    }
    let mut run: Option<usize> = None;
    let mut uncommitted_until = first;
    let mut block = g.clean.next_uncovered(first, last);
    while block < last {
        let mut open = true;
        if g.absent.contains(block) {
            // Ordinary commits need no runtime mapping notification. Reuse
            // this asking's query for the following blocks in the same region.
            if block >= uncommitted_until {
                uncommitted_until = uncommitted_end(block);
            }
            open = block >= uncommitted_until;
            if open {
                g.absent.remove(block);
            }
        }
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            let aliased = g.aliased.contains(block);
            // With `alias_lazy`, a guarded block whose pages another view
            // wrote is still read-only here: noted clean, and the other
            // views guarded again, with no call for this one.
            let hold = frame.is_some_and(|f| {
                !aliased && !g.protected.contains(block)
                    && g.heat.get(&block).is_some_and(|h| f < h.until)
                    && crate::inplace::single_block_view(block * BLOCK)
            });
            let check_aliases = aliased && (alias_lazy() || reuse_arm());
            if hold {
                if let Some(from) = run.take() {
                    protect(g, from, block);
                }
                // Already in written. Never mark a writable block clean.
                // Flushing the preceding run may discover a newly mapped
                // alias and arm this block too. It is still reported dirty.
                if !g.protected.contains(block) && let Some(heat) = g.heat.get_mut(&block) {
                    if let Some(pages) = held.as_deref_mut().filter(|_| cfg!(feature = "diagnostics")) {
                        pages.push((block, heat.held_since_arm));
                    }
                    heat.held_since_arm = true;
                    if heat.last_counted != frame {
                        #[cfg(feature = "diagnostics")]
                        crate::copystat::add(crate::copystat::C::HotViews, 1);
                        heat.last_counted = frame;
                    }
                }
            } else if g.protected.contains(block) && (!aliased || check_aliases) {
                if let Some(from) = run.take() {
                    protect(g, from, block);
                }
                // Written through the runtime, still read-only; or through
                // another view of its pages ([`alias_lazy`]).
                g.clean.insert(block);
                if check_aliases {
                    #[cfg(feature = "diagnostics")]
                    if !alias_lazy() {
                        crate::copystat::add(crate::copystat::C::ReuseArmBlocks, 1);
                    }
                    guard_aliases(g, block);
                }
            } else {
                run.get_or_insert(block);
            }
        } else if let Some(from) = run.take() {
            protect(g, from, block);
        }
        let next = block + 1;
        let next_dirty = g.clean.next_uncovered(next, last);
        if next_dirty != next
            && let Some(from) = run.take()
        {
            protect(g, from, next);
        }
        block = next_dirty;
    }
    if let Some(from) = run.take() {
        protect(g, from, last);
    }
    written
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `VICTORY_GUARD_HOLES=0` looks at the first address only, as before.
fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes = *HOLES.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent. The same physical pages
/// at every other view of them are made read-only too: a write through
/// one of those is this memory written, and only a fault can say so.
/// `VICTORY_VIEW_GUARD=0` leaves the other views as they are, as before.
fn protect(g: &mut Guards, from: usize, to: usize) {
    protect_here(g, from, to);
    if !view_guard() {
        return;
    }
    static BATCH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let batch = to - from > 1 && *BATCH.get_or_init(|| {
        std::env::var_os("VICTORY_GUARD_BATCH_ALIAS").is_some_and(|v| v == "1")
    });
    let mut groups = Vec::new();
    let mut pending = Vec::new();
    for (base, bytes) in crate::inplace::views_between(from.saturating_mul(BLOCK), to.saturating_mul(BLOCK)) {
        let first = (base / BLOCK).max(from);
        let last = (base.saturating_add(bytes).div_ceil(BLOCK)).min(to);
        for block in first..last {
            let shown = crate::inplace::guard_views_of(block * BLOCK);
            // One view (nothing else to guard), or the page the title shows
            // wherever it has nothing (thousands of views): each view of
            // that one is guarded by itself.
            if shown.len() < 2 || shown.len() > MANY_VIEWS {
                continue;
            }
            #[cfg(feature = "diagnostics")]
            ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for &alias in &shown {
                let alias = alias / BLOCK;
                g.aliased.insert(alias);
                if alias != block {
                    if batch {
                        if !g.protected.contains(alias) {
                            pending.push(alias);
                        }
                    } else {
                        protect_alias(g, alias);
                    }
                }
            }
            if batch {
                groups.push(shown);
            } else {
                check_group(g, &shown);
            }
        }
    }
    if batch {
        arm_alias_runs(g, &mut pending);
        for group in groups {
            check_group(g, &group);
        }
    }
}

// An alias is never marked clean by arming it. A syscall spanning separate
// Windows mappings is retried one block at a time if Windows refuses it.
fn arm_alias_runs(g: &mut Guards, pending: &mut Vec<usize>) {
    pending.sort_unstable();
    pending.dedup();
    let mut start = 0;
    while start < pending.len() {
        let mut end = start + 1;
        while end < pending.len() && pending[end] == pending[end - 1] + 1 {
            end += 1;
        }
        let run = &pending[start..end];
        if run.len() > 1 && arm(g, run[0], run[run.len() - 1] + 1) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::AliasBatchSaved, (run.len() - 1) as u64);
            g.protected.extend(run.iter().copied());
        } else {
            #[cfg(feature = "diagnostics")]
            if run.len() > 1 {
                crate::copystat::add(crate::copystat::C::AliasBatchFailed, 1);
            }
            for &block in run {
                protect_alias(g, block);
            }
        }
        start = end;
    }
}

fn check_group(g: &mut Guards, shown: &[usize]) {
    if shown.iter().any(|at| !g.protected.contains(at / BLOCK)) {
        for &at in shown {
            light::clear(at / BLOCK, Cause::Alias);
            g.clean.remove(at / BLOCK);
        }
    }
}

/// [`alias_lazy`]: `block` is guarded and was just noted clean; the other
/// views of its pages are made read-only again (one written through since
/// was opened by the fault), not noted clean, as [`protect`] does. A view
/// that cannot be guarded keeps every view of the pages changed.
fn guard_aliases(g: &mut Guards, block: usize) {
    let shown = crate::inplace::guard_views_of(block * BLOCK);
    if shown.len() < 2 || shown.len() > MANY_VIEWS {
        return;
    }
    for &alias in &shown {
        let alias = alias / BLOCK;
        g.aliased.insert(alias);
        if alias != block {
            protect_alias(g, alias);
        }
    }
    check_group(g, &shown);
}

/// Whether a write through one view of a guarded page leaves its other
/// views read-only, only noted written (`VICTORY_ALIAS_LAZY=1`; off by
/// default: they are all opened, and guarded again at the next asking).
/// Unit tests have it on unless the variable says otherwise.
///
/// Port of triippywhale 7e88d165 (Claude, authored in that fork; there a
/// frame had some 350 guard faults, 5,300 blocks opened and 5,200 made
/// read-only again, 15 ms).
fn alias_lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_ALIAS_LAZY") {
        Some(v) => v == "1",
        None => cfg!(test),
    })
}

/// Views of one page past which its views are not guarded together.
pub(crate) const MANY_VIEWS: usize = 64;

/// Whether all the views of a guarded page are guarded together
/// (`VICTORY_VIEW_GUARD`).
fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_GUARD").is_none_or(|v| v != "0"))
}

/// Diagnostics: blocks guarded together with their other views, blocks
/// opened because another view of theirs was written, and blocks opened
/// because a view of theirs was mapped; since the start.
#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Makes `block` read-only: another view of pages a copy has just taken
/// through its own view, so that a write through this one faults too. It is
/// not noted clean. The copies of title memory at this view have not taken
/// the pages: noted clean, the block would not be reported written at their
/// next check and they would keep the bytes from before the write (`guard`
/// reports a block that is read-only and not clean as written, then notes it
/// clean). A block that is clean stays so: had the pages been written since
/// its copies took them, `open` would have opened it.
fn protect_alias(g: &mut Guards, block: usize) {
    if hot_guards() {
        g.heat.remove(&block);
    }
    if g.protected.contains(block) {
        return;
    }
    if arm(g, block, block + 1) {
        g.protected.insert(block);
    }
}

fn arm(g: &Guards, from: usize, to: usize) -> bool {
    (!light::on() || g.windows.may_arm(from, to)) && set_guard_protection(from, to, false)
}

fn set_guard_protection(from: usize, to: usize, writable: bool) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    let flags = if writable { PAGE_READWRITE } else { PAGE_READONLY };
    let mut old = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: only the protection of title view memory changes.
    let ok = unsafe { VirtualProtect((from * BLOCK) as *const _, (to - from) * BLOCK, flags, &mut old) } != 0;
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        let elapsed = started.elapsed().as_nanos() as u64;
        add(if writable { C::GuardOpenNs } else { C::GuardArmNs }, elapsed);
        add(if writable { C::GuardOpenCalls } else { C::GuardArmCalls }, 1);
        if !writable && ok {
            add(C::GuardArmPages, ((to - from) * (BLOCK / KERNEL_PAGE)) as u64);
        }
    }
    ok
}

/// [`protect`] for one range, without the other views of its pages.
fn protect_here(g: &mut Guards, from: usize, to: usize) {
    let note = |g: &mut Guards, from: usize, to: usize| {
        if hot_guards() {
            for (_, heat) in g.heat.range_mut(from..to) {
                heat.held_since_arm = false;
            }
        }
        g.protected.insert_range(from, to);
        g.clean.insert_range(from, to);
    };
    if arm(g, from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if arm(g, block, block + 1) {
            note(g, block, block + 1);
        } else if block < uncommitted_end(block) {
            g.absent.insert(block);
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded. The same physical pages at
/// every other view of them are opened too: the write that faulted (or is
/// ours, `crate::host::copy_mapped`) goes through one view, and every
/// guard over the pages must copy them again.
fn open(g: &Guards, start: usize, end: usize, cause: Cause) -> (bool, usize) {
    // Another core can store as soon as the first view is writable.
    if light::on() && start < end {
        for block in g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)) {
            for at in crate::inplace::guard_views_of(block * BLOCK) {
                light::clear(at / BLOCK, cause);
            }
        }
    }
    // And their clean bits, before any view is writable: with renderer
    // port 6 (`port`) an asking looks at clean bits without the claims a
    // fault holds, and a store through the view opened first, told to
    // that asking as nothing at another view still noted clean, was lost
    // (2 runs in 20 of the stress test). Under the lock alone this only
    // moves the clearing earlier: the same bits end cleared.
    if port::on() && start < end && !g.aliased.is_empty() {
        for block in g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)) {
            for at in crate::inplace::pages_of(block * BLOCK) {
                g.clean.remove(at / BLOCK);
            }
        }
    }
    let (any, mut opened) = open_here(g, start, end, cause);
    if start >= end || g.aliased.is_empty() {
        return (any, opened);
    }
    let aliased: Vec<usize> = g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)).collect();
    if alias_lazy() {
        // The other views stay read-only and are only noted written: a
        // write through one of them faults as before, and the next check
        // of a view of the pages guards the others again (`guard_aliases`,
        // or `protect` for the view opened here).
        for block in aliased {
            for alias in crate::inplace::pages_of(block * BLOCK) {
                let alias = alias / BLOCK;
                if alias != block && g.protected.contains(alias) {
                    light::clear(alias, cause);
                    g.clean.remove(alias);
                    #[cfg(feature = "diagnostics")]
                    ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        return (any, opened);
    }
    for block in aliased {
        g.aliased.remove(block);
        for alias in crate::inplace::pages_of(block * BLOCK) {
            if alias / BLOCK != block {
                let (_guarded, count) = open_here(g, alias, alias.saturating_add(BLOCK), cause);
                opened += count;
                #[cfg(feature = "diagnostics")]
                if _guarded {
                    ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            g.aliased.remove(alias / BLOCK);
        }
    }
    (any, opened)
}

fn direct_open() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GUARD_DIRECT_OPEN").is_some_and(|v| v == "1"))
}

/// Whether the range had guards and how many blocks became writable.
fn open_here(g: &Guards, start: usize, end: usize, cause: Cause) -> (bool, usize) {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    if last == first + 1 && direct_open() {
        return open_block(g, first, cause);
    }
    let blocks: Vec<usize> = g
        .protected
        .range(first..last)
        .collect();
    let mut opened = 0;
    for &block in &blocks {
        opened += open_block(g, block, cause).1;
    }
    (!blocks.is_empty(), opened)
}

fn open_block(g: &Guards, block: usize, cause: Cause) -> (bool, usize) {
    if !g.protected.remove(block) {
        return (false, 0);
    }
    light::clear(block, cause);
    g.clean.remove(block);
    (true, usize::from(set_guard_protection(block, block + 1, true)))
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    open(&guards(), at, at.saturating_add(len), Cause::Kernel);
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// Whether our own writes into guarded title memory may go through a view
/// of our own, the title's views staying read-only ([`write_aside_now`];
/// `VICTORY_WRITE_ASIDE=1`; off by default: the guards are opened and
/// guarded again at the next asking, as before). Unit tests have it on
/// unless the variable says otherwise.
///
/// Port of triippywhale 2b64a9de and b9fe06d6 (Claude, authored in that
/// fork: there about 1,000 of 1,500 blocks opened a frame were opened for
/// our own writes back of what shaders wrote).
fn write_aside() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_WRITE_ASIDE") {
        Some(v) => v == "1",
        None => cfg!(test),
    })
}

/// Our own write of `len` bytes from `src` into title memory at `at`,
/// through a view of our own of the same pages, if the switch is on
/// ([`write_aside`]), every block of the range is read-only by our hand
/// (so its pages are there) and the mapping of the range can be held at
/// once ([`title_memory_reading_now`]: no change of mapping while the
/// bytes go in, so the view found is the one the title shows). Then the
/// blocks, and the other views of their pages, are noted written (they
/// stay read-only), as [`writing`] would have left them, and the journal
/// is told ([`wrote`]). False: nothing written; the caller opens the
/// guards ([`writing`]) and writes as before.
pub(crate) fn write_aside_now(at: usize, src: *const u8, len: usize) -> bool {
    if len == 0 || !write_aside() {
        return false;
    }
    // Only the guards' and the views' locks (leaves) are taken while the
    // mapping is held, and it is let go before anything else.
    let Some(held) = title_memory_reading_now(at, len) else {
        return false;
    };
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    if guards_read().protected.range(first..last).count() != last - first {
        return false;
    }
    let Some(to) = crate::inplace::own_view(at, len) else {
        return false;
    };
    // SAFETY: our own view of the range's memory, all of it committed
    // (guarded by us) and mapped as the title shows it while `held` lives;
    // `src` holds `len` bytes of ours.
    let window = light::address_io(at, len, Cause::Aside);
    unsafe { std::ptr::copy_nonoverlapping(src, to, len) };
    drop(window);
    drop(held);
    wrote_aside(at, len);
    #[cfg(feature = "diagnostics")]
    ASIDE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// Diagnostics: our own writes into title memory that went through a view
/// of our own ([`write_aside_now`]), since the start.
#[cfg(feature = "diagnostics")]
pub(crate) static ASIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// After our own write of `[at, at + len)` through a view of our own: its
/// blocks, and the other views of their pages, are noted written; they
/// stay read-only.
fn wrote_aside(at: usize, len: usize) {
    {
        let mut g = guards();
        let g = &mut *g;
        for block in at / BLOCK..at.saturating_add(len).div_ceil(BLOCK) {
            light::clear(block, Cause::Aside);
            g.clean.remove(block);
            if !g.aliased.contains(block) {
                continue;
            }
            for alias in crate::inplace::pages_of(block * BLOCK) {
                light::clear(alias / BLOCK, Cause::Aside);
                g.clean.remove(alias / BLOCK);
            }
        }
    }
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
    // With the submission's one asking: the journal learns it.
    wrote(at, len);
}

/// The runtime mapped a view of the physical bytes `[offset, offset + len)`
/// of `section` (`crate::inplace::title_memory_view`): the view is
/// writable, so the same bytes guarded at another view must be checked
/// again — and are, the pages noted written, which also has the next check
/// guard the new view (`protect`).
pub(crate) fn views_mapped(section: usize, offset: u64, len: usize, register: impl FnOnce()) {
    register_mapping((light::on() && view_guard()).then(|| guards()), section, offset, len, register);
}

fn register_mapping(mut held: Option<Hold>, section: usize, offset: u64,
    len: usize, register: impl FnOnce()) {
    // Before registration: the 65th view takes the page out of the
    // bounded lookup. Revoke the previous group's publications first.
    if view_guard() && let Some(g) = &mut held {
        light::invalidate(g, &[(section, offset, len)], Cause::Remap);
    }
    register();
    if !view_guard() { return; }
    let ranges = crate::inplace::views_of_section(section, offset, len);
    if ranges.is_empty() { return; }
    let g = held.unwrap_or_else(|| guards());
    for &(at, n) in &ranges {
        if open_here(&g, at, at.saturating_add(n), Cause::Remap).0 {
            #[cfg(feature = "diagnostics")]
            ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
///
/// Unless `VICTORY_MAPPING_RANGES=1` ([`by_range`]); with it reads and
/// changes hold the bytes they touch ([`SPANS`]).
static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Whether a read in place and a change of mapping keep apart only when
/// their bytes meet ([`SPANS`]), not whatever memory each touches
/// ([`MAPPING`]).
///
/// Ported from triippywhale c79c751c (Last Thylacine). Off
/// unless `VICTORY_MAPPING_RANGES=1` (upstream has it on by default); off,
/// every read and change holds all of title memory, as before. Unit tests
/// of this crate have it on unless the variable says otherwise, so that
/// the range test runs.
///
/// With one lock over all of title memory each texture refresh waited for
/// every change of mapping anywhere, and each change for the refresh under
/// way. While the world streams in the title changes mappings 650-1,100
/// times a frame, many a page at a time (`XMemMapPhysicalPagesScatter`
/// maps each page on its own). With the one lock the refreshes waited 203
/// ms of an 801 ms frame for title memory to hold still; holding only the
/// bytes touched, 0 ms of a 486 ms frame (medians, a pair of runs).
pub(crate) fn by_range() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_MAPPING_RANGES") {
        Some(v) => v == "1",
        None => cfg!(test),
    })
}

/// [`by_range`], for the runtime: whether a hold of title memory's mapping
/// per page is worth taking (with one lock over all of title memory a
/// batch keeps its one hold, as before). Port of c79c751c (local, not for
/// publishing).
pub fn title_memory_by_range() -> bool {
    by_range()
}

/// Reads of title memory in place under way, and changes of its mapping,
/// by the bytes they touch ([`by_range`]).
struct Spans {
    /// To tell them apart.
    next: u64,
    /// Reads under way: number, start and end.
    reads: Vec<(u64, usize, usize)>,
    /// Changes: number, start, end, and whether under way (else waiting for
    /// the reads over its bytes to end; no read begins over them meanwhile,
    /// so that one read after another cannot keep a change waiting).
    changes: Vec<(u64, usize, usize, bool)>,
    /// Threads waiting on [`SPANS_LEFT`].
    waiting: usize,
}

static SPANS: Mutex<Spans> = Mutex::new(Spans {
    next: 0,
    reads: Vec::new(),
    changes: Vec::new(),
    waiting: 0,
});

/// Told when a read or a change ends while a thread waits.
static SPANS_LEFT: std::sync::Condvar = std::sync::Condvar::new();

fn spans() -> std::sync::MutexGuard<'static, Spans> {
    SPANS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until a read or a change ends.
fn spans_wait(mut s: std::sync::MutexGuard<'static, Spans>) -> std::sync::MutexGuard<'static, Spans> {
    let _wait = crate::frame_why::span(crate::frame_why::Phase::Mapping);
    s.waiting += 1;
    let mut s = SPANS_LEFT.wait(s).unwrap_or_else(|e| e.into_inner());
    s.waiting -= 1;
    s
}

/// Whether `[a, b)` and `[c, d)` share a byte.
fn meet(a: usize, b: usize, c: usize, d: usize) -> bool {
    a.max(c) < b.min(d)
}

impl Spans {
    fn read(&mut self, at: usize, end: usize) -> Span {
        self.next += 1;
        self.reads.push((self.next, at, end));
        Span(self.next, false)
    }
}

/// A read's or a change's place in [`SPANS`] (its number, and whether a
/// change), given up when dropped.
struct Span(u64, bool);

impl Drop for Span {
    fn drop(&mut self) {
        let mut s = spans();
        if self.1 {
            s.changes.retain(|c| c.0 != self.0);
        } else if let Some(i) = s.reads.iter().position(|r| r.0 == self.0) {
            s.reads.swap_remove(i);
        }
        if s.waiting > 0 {
            SPANS_LEFT.notify_all();
        }
    }
}

/// What a read or a change of mapping holds: its bytes ([`by_range`]) or
/// the lock over all of title memory.
enum Held<G> {
    All(#[allow(dead_code)] G),
    Bytes(#[allow(dead_code)] Span),
}

/// Since the last frame-rate line: changes of title memory's mapping, the
/// microseconds they waited for reads in place, and those they were under
/// way.
pub(crate) static REMAPS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping {
    _held: Held<std::sync::RwLockWriteGuard<'static, ()>>,
    since: std::time::Instant,
}

impl Drop for Remapping {
    fn drop(&mut self) {
        REMAPS[2].fetch_add(self.since.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The runtime is about to map, unmap or move physical pages of title
/// memory `[at, at + len)`: until what it returns is dropped, no read of
/// those bytes in place is under way and none begins
/// ([`title_memory_reading`]). Taken around the whole change, the unmapping
/// and the mapping again, over all the bytes whose mapping it changes.
pub fn title_memory_remapping(at: usize, len: usize) -> Remapping {
    use std::sync::atomic::Ordering::Relaxed;
    let started = std::time::Instant::now();
    let held = if by_range() {
        let end = at.saturating_add(len);
        let mut s = spans();
        s.next += 1;
        let id = s.next;
        s.changes.push((id, at, end, false));
        while s.reads.iter().any(|r| meet(r.1, r.2, at, end))
            || s.changes.iter().any(|c| c.3 && meet(c.1, c.2, at, end))
        {
            s = spans_wait(s);
        }
        if let Some(c) = s.changes.iter_mut().find(|c| c.0 == id) {
            c.3 = true;
        }
        Held::Bytes(Span(id, true))
    } else {
        let _wait = crate::frame_why::span(crate::frame_why::Phase::Mapping);
        Held::All(MAPPING.write().unwrap_or_else(|e| e.into_inner()))
    };
    REMAPS[0].fetch_add(1, Relaxed);
    REMAPS[1].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    Remapping {
        _held: held,
        since: std::time::Instant::now(),
    }
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] Held<std::sync::RwLockReadGuard<'static, ()>>);

/// For reading title memory `[at, at + len)` in place: until what it
/// returns is dropped, the runtime changes no mapping of those bytes, so
/// memory found mapped stays mapped. Not to be taken twice by one thread,
/// nor held across anything that maps title memory.
pub fn title_memory_reading(at: usize, len: usize) -> Reading {
    if !by_range() {
        let _wait = crate::frame_why::span(crate::frame_why::Phase::Mapping);
        return Reading(Held::All(MAPPING.read().unwrap_or_else(|e| e.into_inner())));
    }
    let end = at.saturating_add(len);
    let mut s = spans();
    while s.changes.iter().any(|c| meet(c.1, c.2, at, end)) {
        s = spans_wait(s);
    }
    Reading(Held::Bytes(s.read(at, end)))
}

/// [`title_memory_reading`] if it can be had at once: `None` while the
/// runtime changes a mapping of those bytes (or waits to), for a caller
/// that has another way to read and would rather not wait.
pub fn title_memory_reading_now(at: usize, len: usize) -> Option<Reading> {
    if !by_range() {
        return match MAPPING.try_read() {
            Ok(guard) => Some(Reading(Held::All(guard))),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(Reading(Held::All(e.into_inner()))),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
    }
    let end = at.saturating_add(len);
    let mut s = match SPANS.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    if s.changes.iter().any(|c| meet(c.1, c.2, at, end)) {
        return None;
    }
    Some(Reading(Held::Bytes(s.read(at, end))))
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    remapped(at, len, None);
}

/// A replacement whose new physical view is already known to xmem.
pub fn title_memory_mapped(at: usize, len: usize, section: usize, offset: u64) {
    remapped(at, len, Some((section, offset)));
}

fn remapped(at: usize, len: usize, new: Option<(usize, u64)>) {
    let _holder = crate::why_more::holder(crate::why_more::Holder::Remap);
    if light::on() {
        // Revoke aliases while the old mapping is still identifiable. Serialize
        // forgetting it with both guard cleanup and new guard publications.
        let mut g = guards();
        light::dirty_address(&mut g, at, len, Cause::Remap);
        crate::inplace::views_gone(at, len);
        forget_guards(&mut g, at, len);
        if let Some((section, offset)) = new {
            register_mapping(Some(g), section, offset, len, || {
                crate::inplace::register_view(at, len, section, offset)
            });
        }
    }
    // For writes that land late (`texback`): which bytes changed since when.
    lock_remaps().note(at, at.saturating_add(len));
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // Textures already uploaded from there must be uploaded again, or the
    // next list that samples them reads their old contents.
    crate::texture::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    if !light::on() {
        crate::inplace::views_gone(at, len);
    }
    crate::inplace::title_memory_freeing(at, len);
    if !light::on() {
        forget_guards(&mut guards(), at, len);
        if let Some((section, offset)) = new {
            crate::inplace::title_memory_view(at, len, section, offset);
        }
    }
}

fn forget_guards(g: &mut Guards, at: usize, len: usize) {
    let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
    if view_known() {
        VIEWS.remove_range(range.start, range.end);
    }
    if light::on() { g.windows.remapped(range.start, range.end); }
    if hot_guards() {
        let stale: Vec<_> = g.heat.range(range.clone()).map(|(&b, _)| b).collect();
        for block in stale {
            g.heat.remove(&block);
        }
    }
    let absent: Vec<usize> = g.absent.range(range.clone()).collect();
    for block in absent {
        g.absent.remove(block);
    }
    let blocks: Vec<usize> = g.protected.range(range.clone()).collect();
    for block in blocks {
        light::clear(block, Cause::Remap);
        g.protected.remove(block);
    }
    g.clean.remove_range(range.start, range.end);
    let aliased: Vec<usize> = g.aliased.range(range).collect();
    for block in aliased {
        g.aliased.remove(block);
    }
}

/// The last changes of mapping ([`title_memory_remapped`]), numbered, so a
/// write prepared earlier can ask whether its bytes changed since. Some
/// hundreds to 1,100 a frame while the world streams in: a few frames' worth.
const REMAP_RING: usize = 8192;

struct Remaps {
    /// The number of the last change (0: none yet).
    next: u64,
    /// By number modulo the ring: number, start and end.
    ring: [(u64, usize, usize); REMAP_RING],
}

static REMAPS_SEEN: Mutex<Remaps> = Mutex::new(Remaps::new());

fn lock_remaps() -> std::sync::MutexGuard<'static, Remaps> {
    REMAPS_SEEN.lock().unwrap_or_else(|e| e.into_inner())
}

impl Remaps {
    const fn new() -> Self {
        Remaps {
            next: 0,
            ring: [(0, 0, 0); REMAP_RING],
        }
    }

    fn note(&mut self, at: usize, end: usize) {
        self.next += 1;
        self.ring[self.next as usize % REMAP_RING] = (self.next, at, end);
    }

    /// Yes if in doubt: more changes since `epoch` than the ring keeps.
    fn since(&self, at: usize, end: usize, epoch: u64) -> bool {
        let newer = self.next.saturating_sub(epoch);
        if newer > REMAP_RING as u64 {
            return true;
        }
        (0..newer).any(|k| {
            let (n, a, b) = self.ring[(self.next - k) as usize % REMAP_RING];
            n > epoch && a < end && at < b
        })
    }
}

/// Now, for [`remapped_since`].
pub(crate) fn remap_epoch() -> u64 {
    lock_remaps().next
}

/// Whether the mapping of title memory `[at, at + len)` changed after
/// `epoch` ([`remap_epoch`]); true when that cannot be told. Ask under
/// [`title_memory_reading`] of the range, so no change is under way.
pub(crate) fn remapped_since(at: usize, len: usize, epoch: u64) -> bool {
    lock_remaps().since(at, at.saturating_add(len), epoch)
}

/// A write fault at `at` inside our own in-place write into title memory
/// (`crate::plaincopy::write`, `VICTORY_COPY_PLAIN=1`): true if the block
/// was guarded by us and is now opened (and noted written, its other views
/// too), as [`install`]'s handler does for the title's writes; the write
/// then goes on. False leaves the fault to the plain write, which falls
/// back to the kernel's write. Whichever of the two handlers runs first,
/// a guard of ours is never swallowed without being opened.
pub(crate) fn take_plain_write_fault(at: usize) -> bool {
    handle_write_fault(at, false)
}

fn handle_write_fault(at: usize, retry_writable: bool) -> bool {
    let _holder = crate::why_more::holder(crate::why_more::Holder::Fault);
    if port::on() {
        // Renderer port 6: the guards shared, the block's claim waited for.
        return port::handle_write_fault(at, retry_writable);
    }
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let mut g = guards();
    #[cfg(feature = "diagnostics")]
    let wait = started.elapsed();
    let track_heat = hot_guards() && !g.aliased.contains(at / BLOCK);
    let cause = if retry_writable { Cause::Fault } else { Cause::Plain };
    let (guarded, _opened) = open(&g, at, at.saturating_add(1), cause);
    if guarded && track_heat {
        g.heat.entry(at / BLOCK).or_default().fault(crate::queue::presented());
    }
    // A second writer may fault before the first opens the block. Recheck
    // under the same lock so a concurrent arm cannot slip between checks.
    let resume = guarded || (retry_writable && writable(at));
    drop(g);
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::GuardHandlerNs, started.elapsed().as_nanos() as u64);
        add(C::GuardHandlerWaitNs, wait.as_nanos() as u64);
        add(C::GuardHandlerCalls, 1);
        if guarded {
            add(C::GuardFaults, 1);
            add(C::FaultOpenedPages, (_opened * (BLOCK / KERNEL_PAGE)) as u64);
        }
    }
    resume
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        if handle_write_fault(at, true) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Resolve switches before the first guarded write can fault.
        hot_guards();
        light::on();
        direct_open();
        alias_lazy();
        // SAFETY: the handler only touches our guard state and protections.
        unsafe { AddVectoredExceptionHandler(1, Some(on_write)) };
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[test]
    fn ahead_index_preserves_single_success_coverage() {
        let probes = [(8, 11), (2, 4), (3, 7), (2, 6), (2, 6), (7, 8), (12, 14)];
        let mut index = probes.to_vec();
        index_ahead(&mut index);
        assert_eq!(index, [(2, 6), (3, 7), (7, 8), (8, 11), (12, 14)]);
        for first in 0..15 {
            for last in first + 1..=15 {
                assert_eq!(covered(&index, first, last),
                    probes.iter().any(|&(a, b)| a <= first && last <= b));
            }
        }
    }

    #[test]
    fn one_merged_reset_keeps_writes_in_the_gap_and_leaves_later_stores_pending() {
        // SAFETY: a fresh private write-watched reservation owned by this test.
        let ptr = unsafe {
            VirtualAlloc(std::ptr::null(), 3 * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
        };
        assert!(!ptr.is_null());
        let first = ptr as usize / PAGE;
        let pieces = [(first, first + 1), (first + 2, first + 3)];
        let bytes = pieces.map(|(a, b)| (a * PAGE, b * PAGE));
        let gap = (ptr as *mut u8).wrapping_add(PAGE);
        // SAFETY: the middle page of the committed reservation.
        unsafe { gap.write_volatile(1) };
        let mut poll = AheadPoll::default();
        poll.scan(&pieces, &bytes, false);
        assert_eq!(poll.covered, [(first, first + 3)]);
        assert_eq!(poll.written, [first + 1]);
        // SAFETY: stands for the title storing again before the copy.
        unsafe { gap.write_volatile(2) };
        let mut next = AheadPoll::default();
        assert!(next.ask_kernel(first + 1, first + 2));
        assert_eq!(next.written, [first + 1]);
        // SAFETY: release the test reservation.
        unsafe { VirtualFree(ptr, 0, MEM_RELEASE) };
    }

    #[test]
    fn overlapping_polls_share_a_region_even_across_the_stripe_wrap() {
        let section = (2 << 20) / PAGE;
        let boundary = 64 * section;
        let mask = region_mask(boundary - 1, boundary + 1);
        assert_eq!(mask, (1u64 << 63) | 1);
        assert_eq!(mask & region_mask(boundary, boundary + section), 1);
        assert_eq!(mask & region_mask(boundary + section, boundary + 2 * section), 0);
        assert_eq!(region_mask(0, boundary + section), u64::MAX);
        assert_eq!(region_mask(boundary, boundary), 0);
    }

    #[test]
    fn an_overlapping_reader_waits_for_publication_while_a_disjoint_reader_runs() {
        let locks = [const { RwLock::new(()) }; 64];
        let j = RwLock::new(Journal::default());
        let section = (2 << 20) / PAGE;
        let page = 64 * section;
        let held = region_locks(&locks, region_mask(page - 1, page + 1), |m| m.write().unwrap());
        let (checked, waiting) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let (locks, j) = (&locks, &j);
            let reader = scope.spawn(move || {
                assert!(matches!(locks[0].try_read(), Err(TryLockError::WouldBlock)));
                {
                    let _other = region_locks(locks, region_mask(page + section, page + section + 1),
                        |m| m.try_read().expect("disjoint"));
                }
                checked.send(()).unwrap();
                let _same = region_locks(locks, region_mask(page, page + 1), |m| m.read().unwrap());
                let j = j.read().unwrap();
                j.answer(page, page + 1, j.next_ask())
            });
            waiting.recv().unwrap();
            let (stamp, _) = j.write().unwrap().record(page - 1, page + 1, vec![page]);
            drop(held);
            let (later, seen) = reader.join().unwrap();
            assert!(later > stamp);
            assert_eq!(seen, [(page, stamp)]);
        });
    }

    #[test]
    fn a_store_between_atomic_reset_and_copy_survives_for_overlapping_readers() {
        let mut j = Journal::default();
        let mut kernel_dirty = BTreeSet::from([10]);
        let reset = |dirty: &mut BTreeSet<usize>| std::mem::take(dirty).into_iter().collect();
        let first_pages = reset(&mut kernel_dirty);
        kernel_dirty.insert(10); // Same page stored again before publication or copy.
        kernel_dirty.insert(11);
        let (copied, before) = j.record(10, 12, first_pages);
        assert_eq!(before, [(10, copied)]);
        assert_eq!(j.answer(10, 11, j.next_ask()).1, before);
        let (later, after) = j.record(10, 12, reset(&mut kernel_dirty));
        assert!(later > copied);
        assert_eq!(after, [(10, later), (11, later)]);
        j.ahead = vec![(10, 12)];
        j.ahead_asked = later;
        assert_eq!(j.ahead_answer(11, 12), Some((later, vec![(11, later)])));
    }

    #[test]
    fn split_polls_publish_an_alias_write_to_both_copies() {
        let (section, one, two) = two_views(BLOCK);
        let poll = |at: usize| split_poll(at / PAGE, at / PAGE + 1, true, None).unwrap();
        let (old_one, _) = poll(one);
        let (old_two, _) = poll(two);
        // SAFETY: a test-owned writable section. Its guard faults and reopens it.
        unsafe { (two as *mut u8).write_volatile(1) };
        for (at, old) in [(one, old_one), (two, old_two)] {
            let (stamp, writes) = poll(at);
            assert!(stamp > old);
            assert_eq!(writes, [(at / PAGE, stamp)]);
            assert_eq!(poll(at).1, writes);
        }
        drop_views(section, &[one, two], BLOCK);
    }

    #[test]
    fn a_hold_requires_three_distinct_consecutive_fault_frames() {
        let mut h = Heat::default();
        for frame in [1, 1, 1, 3, 4, 4] {
            h.fault(frame);
            assert_eq!(h.until, 0);
        }
        h.fault(5);
        for frame in [6, 7, 8] {
            h.fault(frame);
        }
        assert_eq!(h.until, 13);
        for frame in [13, 14] {
            h.fault(frame);
            assert_eq!(h.until, 13);
        }
        h.fault(15);
        assert_eq!(h.until, 23);
    }

    #[test]
    fn a_held_view_is_dirty_at_every_check_and_rearmed_at_expiry() {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        let (section, one, two) = two_views(BLOCK);
        assert!(!crate::inplace::single_block_view(one));
        title_memory_remapped(two, BLOCK);
        // SAFETY: the test owns both mappings and retains the first.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: two as *mut _ }) };
        assert!(crate::inplace::single_block_view(one));
        let b = one / BLOCK;
        install();
        let mut held = Vec::new();
        {
            let mut g = guards();
            g.heat.insert(b, Heat { until: 12, ..Heat::default() });
            for frame in [4, 4, 5, 11] {
                assert_eq!(guard_blocks(&mut g, b, b + 1, Some(&mut held), Some(frame)), [b]);
                assert!(!g.clean.contains_range(b, b + 1));
                assert!(writable(one));
                // SAFETY: our whole writable mapping. Each write below has
                // no exception to announce it to the following checker.
                unsafe { (one as *mut u8).write_volatile(frame as u8) };
            }
            #[cfg(feature = "diagnostics")]
            assert_eq!(held, [(b, false), (b, true), (b, true), (b, true)]);
            assert_eq!(guard_blocks(&mut g, b, b + 1, Some(&mut held), Some(12)), [b]);
            assert!(!writable(one));
            g.heat.remove(&b);
        }
        assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
        // SAFETY: our write now faults and is retried by the installed handler.
        unsafe { (one as *mut u8).write_volatile(99) };
        assert_eq!(guard(one, one + BLOCK), Some(vec![b]));
        drop_views(section, &[one], BLOCK);
    }

    #[test]
    fn batched_aliases_stay_dirty_and_a_failed_span_retries_its_mapped_blocks() {
        // SAFETY: one test-owned reservation. Its third block stays reserved.
        let base = unsafe { VirtualAlloc(std::ptr::null(), 4 * BLOCK, MEM_RESERVE, PAGE_READWRITE) } as usize;
        assert_ne!(base, 0);
        for (at, n) in [(base, 2 * BLOCK), (base + 3 * BLOCK, BLOCK)] {
            // SAFETY: both ranges lie inside the reservation.
            assert_eq!(unsafe { VirtualAlloc(at as *const _, n, MEM_COMMIT, PAGE_READWRITE) } as usize, at);
        }
        let b = base / BLOCK;
        {
            let mut g = guards();
            arm_alias_runs(&mut g, &mut vec![b + 1, b, b + 1]);
            arm_alias_runs(&mut g, &mut vec![b + 3, b + 2]);
            for i in [0, 1, 3] {
                assert!(g.protected.contains(b + i));
                assert!(!writable((b + i) * BLOCK));
                assert!(!g.clean.contains_range(b + i, b + i + 1));
            }
            assert!(!g.protected.contains(b + 2));
            assert_eq!(open_block(&g, b + 1, Cause::Kernel), (true, 1));
            assert!(writable(base + BLOCK));
            assert_eq!(open_block(&g, b + 1, Cause::Kernel), (false, 0));
            assert!(!writable(base));
            g.clean.insert(b);
            check_group(&mut g, &[base, base + 2 * BLOCK]);
            assert!(!g.clean.contains_range(b, b + 1));
        }
        title_memory_remapped(base, 4 * BLOCK);
        // SAFETY: release the reservation and all its committed pages.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn remaps_since_an_epoch_hit_miss_and_overflow() {
        let mut r = Box::new(Remaps::new());
        r.note(0x1000, 0x2000);
        let epoch = r.next;
        assert!(!r.since(0x1000, 0x2000, epoch), "a change before the epoch");
        r.note(0x8000, 0x9000);
        assert!(r.since(0x8800, 0x8900, epoch), "a change inside");
        assert!(r.since(0x7000, 0x8001, epoch), "a change meeting one byte");
        assert!(!r.since(0x9000, 0xa000, epoch), "a change next to it");
        assert!(!r.since(0x1000, 0x2000, epoch), "the old change is older");
        for _ in 0..REMAP_RING {
            r.note(0x100000, 0x101000);
        }
        assert!(r.since(0x1000, 0x2000, epoch), "overflow: in doubt");
        assert!(!r.since(0x1000, 0x2000, r.next), "nothing since now");
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        // The journal is process-wide and keeps pages written in memory
        // other tests freed: the system may hand those addresses out again
        // (a page is one allocation granule). Such entries carry askings
        // older than this test's first; nothing in the fresh allocation may
        // be reported under an asking of its own.
        let (asked, before) = writes(start, end, false).expect("watched");
        assert!(before.iter().all(|&(_, w)| w < asked), "{before:?} at {asked}");
        let since = |list: &[(usize, u64)], stamp: u64| -> Vec<(usize, u64)> {
            list.iter().copied().filter(|&(_, w)| w > stamp).collect()
        };
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(since(&after, asked), [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        {
            let at = p + PAGE;
            let mapping = title_memory_reading(at, PAGE);
            let mut stamp = later;
            assert!(quiet_copy_back(&mapping, at + 8, &[2], &mut stamp));
            assert!(stamp > later);
            assert_eq!(writes(at, at + PAGE, false).unwrap().1, [(at / PAGE, stamp)]);
            // A title store outside the merged bytes on the same kernel
            // page must survive both the probe and the next journal poll.
            unsafe { ((at + 9) as *mut u8).write(3) };
            assert!(!quiet_copy_back(&mapping, at + 8, &[4], &mut stamp));
            assert_eq!(unsafe { ((at + 8) as *const u8).read() }, 2);
            let (_, after) = writes(at, at + PAGE, false).unwrap();
            assert!(after[0].1 > stamp);
            assert!(!quiet_copy_back(&mapping, at + 8, &[4], &mut stamp));
        }
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        // The last block is still entirely reserved.
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
        for at in [early, late] {
            assert!(writable(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    /// A range of view memory whose first block is not mapped is a view
    /// all the same: its blocks are reported once, then the ones written
    /// or mapped since.
    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        // Nothing mapped at all: a view with every block absent.
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        // The range begins in the placeholder, its middle block is a view.
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()), "nothing written since");
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // Memory of ours that is no view stays none.
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// A pagefile section mapped twice: two views of the same physical
    /// pages, the second left for the test; the section handle, the two
    /// addresses.
    fn two_views(bytes: usize) -> (usize, usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        // SAFETY: a fresh pagefile section, mapped twice; the views keep
        // the section alive and the handle is closed by the caller.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::title_memory_view(one, bytes, section as usize, 0);
            crate::inplace::title_memory_view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    /// The section of [`two_views`] is closed and its views unmapped; the
    /// guard is told they are gone first.
    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, bytes);
            // SAFETY: a view of the section mapped by `two_views`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                })
            };
        }
        // SAFETY: the section's last reference.
        unsafe { CloseHandle(section as *mut std::ffi::c_void) };
    }

    /// The title wrote the pages of a guarded texture through another view
    /// of them: the fault at that view is the guard's to see, and the
    /// guarded view's next check reports the pages written.
    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        // The first check takes it in (and every view of its pages).
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        // Writing through the other view faults into the handler, which
        // opens that view and the guarded one: the next check sees it.
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// `VICTORY_VIEW_KNOWN=1`: a view block a fault opened is answered by
    /// the guards without a kernel asking, its write seen; an asking that
    /// does not go to the guards is not made; a remap forgets it.
    #[test]
    fn known_view_memory_is_not_asked_of_the_kernel() {
        VIEW_KNOWN_IN_TEST.set(true);
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let b = one / BLOCK;
        let (_, first) = writes(one, one + SIZE, true).expect("a view");
        assert_eq!(first.iter().map(|w| w.0).collect::<Vec<_>>(), [b, b + 1]);
        assert!(VIEWS.contains_range(b, b + 2));
        // SAFETY: our view, opened by the handler.
        unsafe { ((one + BLOCK + 8) as *mut u8).write_volatile(5) };
        assert!(!guards_read().protected.contains(b + 1), "opened by the fault");
        let skipped = VIEW_KNOWN_SKIPS.load(Relaxed);
        let (now, written) = writes(one + BLOCK, one + SIZE, true).expect("a view");
        assert_eq!(written, [(b + 1, now)], "the write is seen");
        assert!(VIEW_KNOWN_SKIPS.load(Relaxed) > skipped, "no kernel asking");
        assert!(writes(one, one + BLOCK, false).is_none());
        title_memory_remapped(one, SIZE);
        assert!(!VIEWS.any(b, b + 2), "forgotten at the remap");
        VIEW_KNOWN_IN_TEST.set(false);
        // (The views stay mapped: the journal keeps their pages written,
        // and memory of another test at their addresses would be told so.)
        let _ = (section, two);
    }

    // Exercise the actual handlers, publication ordering and environment
    // switches in isolated test processes. No adapter or retail input.
    #[test]
    fn light_aliases_fault_publication_and_section_remap() {
        const CHILD: &str = "VICTORY_TEST_LIGHT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let (_, module) = module_path!().split_once("::").unwrap();
            let name = format!("{module}::light_aliases_fault_publication_and_section_remap");
            // (The last three with renderer port 6, `port`, which stays off
            // with VICTORY_GUARD_HOT=1.)
            let runs = [("0", "0", "0"), ("0", "1", "0"), ("4", "0", "0"), ("4", "1", "0"), ("0", "0", "1"), ("4", "0", "1"), ("4", "1", "1")];
            for (threads, split, port) in runs {
                let hot = if port == "1" { "0" } else { split };
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", &name]).env(CHILD, "1").env("VICTORY_LIGHT_REFRESH", "1")
                    .env("VICTORY_PREP_THREADS", threads).env("VICTORY_WATCH_SPLIT", split)
                    .env("VICTORY_VIEW_GUARD", "1").env("VICTORY_ALIAS_LAZY", split)
                    .env("VICTORY_WRITE_ASIDE", "1").env("VICTORY_GUARD_HOT", hot).env("VICTORY_PORT_GUARD", port)
                    // (Chunked holds of the lock alone, with the light refresh and,
                    // with the split, `VICTORY_GUARD_HOT`.)
                    .env("VICTORY_GUARD_CHUNK", if port == "0" && threads == "4" { "1" } else { "0" })
                    .env("VICTORY_GUARD_BATCH_ALIAS", split).env("VICTORY_GUARD_REUSE_ARM", split)
                    .env("VICTORY_GUARD_DIRECT_OPEN", split).env("VICTORY_VIEW_ASK", "0")
                    .env("VICTORY_GUARD_ASK", "0").env("VICTORY_BACK_QUIET", "0")
                    .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
                    .spawn().unwrap();
                // (A deadlock in the child fails here instead of hanging the run.)
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                let mut child = output;
                while child.try_wait().unwrap().is_none() {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        panic!("prep={threads} split={split} port={port}: no end after 120 s, a deadlock");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                    "prep={threads} split={split} port={port}: {} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            }
            return;
        }
        const SIZE: usize = 2 << 20;
        if std::env::var("VICTORY_PORT_GUARD").is_ok_and(|v| v == "1") {
            assert!(port::on(), "renderer port 6 on in this child");
        }
        let (section, one, two) = two_views(SIZE);
        let poll = |at| writes(at, at + SIZE, true).unwrap().0;
        let first = poll(one);
        poll(two);
        assert!(light::published(one, one + SIZE) && light::published(two, two + SIZE));
        // Withhold publication while a sibling lacks verified alias metadata.
        // Restoring that proof without another write must publish the existing
        // journal write, not the newer number of the clean retry.
        let p = one / BLOCK;
        let original = journal_read().written[&p];
        {
            let g = guards();
            assert!(g.aliased.remove(two / BLOCK));
        }
        {
            let j = journal_read();
            light::publish_poll(p, p + 1, &[p], original, &j.written);
        }
        assert!(!light::published(one, one + BLOCK));
        guards().aliased.insert(two / BLOCK);
        writes(one, one + BLOCK, true).unwrap();
        assert!(light::current(one, one + BLOCK, p, &[original], 0));
        assert!(!light::current(one, one + BLOCK, p, &[original - 1], 0));
        let tail = SIZE - BLOCK;
        {
            let g = guards();
            let p = (one + tail) / BLOCK;
            // Both the withheld word and the following fault encode the same
            // clearing cause. A stamp-only CAS could not distinguish them.
            light::clear(p, Cause::Fault);
            assert!(g.clean.contains_range(p, p + 1));
        }
        // Race a delayed publisher with a completed store on the other
        // view. An old journal entry must never resurrect the clean bit.
        std::thread::scope(|s| {
            s.spawn(|| unsafe { ((two + tail) as *mut u8).write_volatile(17) }).join().unwrap();
            let p = (one + tail) / BLOCK;
            let j = journal_read();
            light::retry_clean(p, p + 1, &j.written);
        });
        assert!(!light::published(one + tail, one + SIZE));
        assert!(!light::published(two + tail, two + SIZE));
        assert!(light::published(one, one + tail));
        let later = poll(one);
        assert!(later > first);
        assert!(!light::current(one + tail, one + SIZE, (one + tail) / BLOCK, &[first], 0));
        poll(two);
        // A physical writer bypasses both protections. An intervening
        // slow poll must not republish until its last overlapping hold ends.
        let outer = light::physical_write_window(section, tail as u64, BLOCK).unwrap();
        let inner = light::physical_write_window(section, (SIZE - 1) as u64, 1).unwrap();
        drop(outer);
        poll(one);
        assert!(!light::published(one + tail, one + SIZE));
        drop(inner);
        poll(one);
        assert!(light::published(one, one + SIZE));
        let changing = light::mapping_window(two, section, 0, SIZE).unwrap();
        assert!(!light::published(one, one + 1));
        title_memory_remapped(two, SIZE);
        crate::inplace::title_memory_view(two, SIZE, section, 0);
        poll(one);
        assert!(!light::published(one, one + SIZE));
        drop(changing);
        poll(one);
        assert!(light::published(one, one + SIZE));
        assert!(!light::published(two, two + SIZE));
        poll(two);
        let byte = 19u8;
        assert!(write_aside_now(two + BLOCK - 1, &byte, 1));
        assert!(!light::published(one, one + 1));
        assert!(!light::published(two, two + 1));
        assert!(light::published(one + BLOCK, one + SIZE));
        crate::inplace::title_memory_section_closed(section);

        // Crossing the guard's bounded lookup used to turn "many" into
        // "no siblings". Check that the last published group is revoked.
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile};
        let mut extra = Vec::new();
        for count in 3..=MANY_VIEWS + 1 {
            // SAFETY: another view of the test's first physical page.
            let view = unsafe { MapViewOfFile(section as _, FILE_MAP_WRITE, 0, 0, BLOCK) };
            assert!(!view.Value.is_null());
            crate::inplace::title_memory_view(view.Value as usize, BLOCK, section, 0);
            extra.push(view);
            if count == MANY_VIEWS || count == MANY_VIEWS + 1 {
                writes(one, one + BLOCK, true).unwrap();
                assert_eq!(light::published(one, one + BLOCK), count == MANY_VIEWS);
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            // Use a different block from the over-limit alias group.
            let p = (one + BLOCK) / BLOCK;
            let at = p * BLOCK;
            writes(at, at + BLOCK, true).unwrap();
            assert!(light::published(at, at + BLOCK));
            let wrong = BTreeMap::new();
            assert!(!light::check_answer(p, p + 1, &wrong));
            assert!(!light::published(at, at + BLOCK));
            writes(at, at + BLOCK, true).unwrap();
            assert!(light::predicted(at, at + BLOCK));
            assert!(!light::published(at, at + BLOCK));
            assert!(!light::check_answer(p, p + 1, &wrong));
            assert_eq!(guards_read().windows.failure_count(p), 2);
        }
        // Explicitly protected memory must keep the caller's protection
        // even if a poll occurs before the syscall returns or after it lands.
        let at = one + 2 * BLOCK;
        let change = light::protection_window(at + 16, 32).unwrap();
        writes(at, at + BLOCK, true).unwrap();
        assert!(!guards_read().protected.contains(at / BLOCK));
        change.quarantine();
        writes(at, at + BLOCK, true).unwrap();
        assert!(!guards_read().protected.contains(at / BLOCK));
        assert!(!light::published(at, at + BLOCK));
        for view in extra {
            title_memory_remapped(view.Value as usize, BLOCK);
            unsafe { UnmapViewOfFile(view) };
        }
        drop_views(section, &[one, two], SIZE);
    }

    /// A pagefile section of `pages` blocks mapped `N` times, each view
    /// told to the runtime's table; the section and the views. (Left
    /// mapped by the tests: the journal keeps their pages written, and
    /// another test's memory at their addresses would be told so.)
    fn views<const N: usize>(pages: usize) -> (usize, [usize; N]) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a fresh pagefile section, mapped `N` times.
        unsafe {
            let section =
                CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, (pages * BLOCK) as u32, std::ptr::null());
            assert!(!section.is_null());
            let all = std::array::from_fn(|_| {
                let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, pages * BLOCK).Value as usize;
                assert!(at != 0 && at.is_multiple_of(BLOCK));
                crate::inplace::title_memory_view(at, pages * BLOCK, section as usize, 0);
                at
            });
            (section as usize, all)
        }
    }

    /// Renderer port 6: threads write pages of a section through one of its
    /// views and ask about them at another (`writes`, the guards); every
    /// write is told at the next asking after it, whoever asked in between.
    /// `port`: 1, every thread under the shared lock; 2, every other thread
    /// (the rest under the lock alone, as without the switch).
    fn writes_to_views_are_not_lost<const VIEWS: usize>(port: u8) {
        writes_to_views_are_not_lost_in::<VIEWS>(port, 0);
    }

    /// [`writes_to_views_are_not_lost`]; `chunk`: the askings guard that
    /// many blocks per hold of the lock ([`guard_chunk`]; 0: all at once).
    fn writes_to_views_are_not_lost_in<const VIEWS: usize>(port: u8, chunk: usize) {
        const PAGES: usize = 12;
        const THREADS: usize = 8;
        let (_section, all) = views::<VIEWS>(PAGES);
        let faults = port::count_of_faults();
        std::thread::scope(|s| {
            for t in 0..THREADS {
                s.spawn(move || {
                    port::IN_TEST.set(port == 1 || (port == 2 && t % 2 == 1));
                    CHUNK_IN_TEST.set(chunk);
                    let mut last = [[0u64; PAGES]; VIEWS];
                    let mut seed = t as u64 * 7919 + 1;
                    let mut next = |n: usize| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        (seed >> 33) as usize % n
                    };
                    for round in 0..400 {
                        let page = next(PAGES);
                        let from = page - next(page.min(3) + 1);
                        let to = page + 1 + next((PAGES - page).min(3));
                        let (through, asked) = (all[next(VIEWS)], next(VIEWS));
                        let at = all[asked];
                        let wrote = next(4) != 0;
                        if wrote {
                            let word = through + page * BLOCK + t * 8;
                            if next(4) == 0 {
                                // The runtime's own write, told (opens alone).
                                writing(word, 8);
                            }
                            // SAFETY: a view of the test's section; a guarded
                            // block faults and is opened by the handler.
                            unsafe { (word as *mut u64).write_volatile(round as u64) };
                        }
                        let (now, written) = writes(at + from * BLOCK, at + to * BLOCK, true).expect("a view");
                        if wrote {
                            let number = written.iter().find(|w| w.0 == at / PAGE + page).map_or(0, |w| w.1);
                            assert!(
                                number > last[asked][page],
                                "{VIEWS} views, port {port}, thread {t}, page {page} through {through:x}, asked at {at:x}: {number} is not after {}",
                                last[asked][page]
                            );
                        }
                        for l in &mut last[asked][from..to] {
                            *l = now;
                        }
                    }
                });
            }
        });
        if port != 0 {
            assert!(port::count_of_faults() > faults, "no fault was opened under the shared lock");
        }
    }

    /// Runs `test` on a thread of its own and fails if it takes longer than
    /// `secs` (a deadlock fails the test instead of hanging the run; the
    /// thread is left behind).
    fn within(secs: u64, test: impl FnOnce() + Send + 'static) {
        let (done, finished) = std::sync::mpsc::channel();
        let runner = std::thread::spawn(move || {
            test();
            let _ = done.send(());
        });
        match finished.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(()) => runner.join().unwrap(),
            // Panicked: its message is the test's.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(panic) = runner.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("no end after {secs} s: a deadlock"),
        }
    }

    /// `VICTORY_GUARD_CHUNK`: askings of up to seven blocks guarding one
    /// or two per hold of the lock alone, writers racing them through
    /// every view; no write is missed.
    #[test]
    fn no_write_is_lost_with_chunked_guard_holds() {
        within(60, || {
            let before = CHUNK_RELEASES.load(Relaxed);
            writes_to_views_are_not_lost_in::<1>(0, 1);
            writes_to_views_are_not_lost_in::<3>(0, 2);
            assert!(CHUNK_RELEASES.load(Relaxed) > before, "no asking was guarded in chunks");
        });
    }

    /// The same with the port off: the guards alone, as before.
    #[test]
    fn no_write_through_another_view_is_lost_without_port_6() {
        within(60, || writes_to_views_are_not_lost::<3>(0));
    }

    #[test]
    fn no_write_to_views_is_lost_with_port_6() {
        within(60, || {
            writes_to_views_are_not_lost::<1>(1);
            writes_to_views_are_not_lost::<1>(2);
        });
    }

    #[test]
    fn no_write_through_another_view_is_lost_with_port_6() {
        within(60, || {
            writes_to_views_are_not_lost::<3>(1);
            writes_to_views_are_not_lost::<3>(2);
        });
    }

    /// Renderer port 6: a write fault on a block whose claim a refresh holds
    /// waits for it (no deadlock: the refresh waits for nothing), then opens
    /// the block, and the write is told at the next asking. And a refresh
    /// that cannot claim a block a fault holds leaves it written, told
    /// again until it can.
    ///
    /// The refresh is a thread of its own holding the guards shared and the
    /// claim, as `port::guard_blocks` does, until told to let go: a thread
    /// that held them while it waited for the faulting thread (the first
    /// form of this test) hung whenever a third thread was queued for the
    /// guards alone, for the queued writer of the lock keeps the fault's
    /// shared taking out. No code of the port waits for another thread
    /// while holding the guards.
    #[test]
    fn a_fault_during_a_refresh_of_its_block_waits_and_is_seen() {
        within(60, a_fault_during_a_refresh);
    }

    fn a_fault_during_a_refresh() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        use std::sync::mpsc::channel;
        port::IN_TEST.set(true);
        let (_section, [one, two]) = views::<2>(2);
        let b = one / BLOCK;
        let pages = |view: usize| view / PAGE..view / PAGE + 2;
        assert_eq!(guard(one, one + 2 * BLOCK), Some(pages(one).collect()));
        assert_eq!(guard(one, one + 2 * BLOCK), Some(Vec::new()));
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            let (holding, held) = channel();
            let (release, let_go) = channel::<()>();
            // A refresh under way, holding the guards shared and the claim.
            s.spawn(move || {
                let g = port::guards_shared();
                let mut claims = port::Claims::none();
                assert!(claims.try_one(b));
                holding.send(()).unwrap();
                let_go.recv().unwrap();
                drop(claims);
                drop(g);
            });
            held.recv().unwrap();
            let writer = s.spawn(|| {
                port::IN_TEST.set(true);
                // SAFETY: our view; it faults, and the handler opens it.
                unsafe { ((two + 8) as *mut u8).write_volatile(7) };
                done.store(true, SeqCst);
            });
            std::thread::sleep(std::time::Duration::from_millis(150));
            assert!(!done.load(SeqCst), "the fault waited for the claim");
            assert!(!writable(two));
            release.send(()).unwrap();
            writer.join().unwrap();
        });
        assert!(done.load(SeqCst));
        assert!(!port::claimed(b) && !port::claimed(two / BLOCK), "the claims are let go");
        // SAFETY: as written above.
        assert_eq!(unsafe { (one as *const u8).add(8).read_volatile() }, 7);
        assert_eq!(guard(one, one + 2 * BLOCK), Some(vec![b]), "the write is told");
        assert_eq!(guard(one, one + 2 * BLOCK), Some(Vec::new()));

        // The other way: a fault (another thread) holds the claim; the
        // refresh leaves the block as it is, written, until it can claim it.
        // (That thread holds the claim alone, not the guards: main takes them
        // meanwhile, and a holder of the guards that waits for main would
        // hang whenever a third thread is queued for them alone.)
        writing(one + 8, 1);
        let (holding, held) = channel();
        let (release, let_go) = channel::<()>();
        std::thread::scope(|s| {
            s.spawn(move || {
                let mut claims = port::Claims::none();
                assert!(claims.try_one(b));
                holding.send(()).unwrap();
                let_go.recv().unwrap();
                drop(claims);
            });
            held.recv().unwrap();
            for _ in 0..2 {
                assert_eq!(guard(one, one + BLOCK), Some(vec![b]), "left written");
                assert!(!guards_read().protected.contains(b) && writable(one));
            }
            release.send(()).unwrap();
        });
        assert_eq!(guard(one, one + BLOCK), Some(vec![b]));
        assert!(!writable(one), "guarded once claimed");
        assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
        port::IN_TEST.set(false);
    }

    /// A view of guarded pages is mapped anew: it is writable, so the
    /// guarded view checks its pages again (and guards the new view).
    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        title_memory_remapped(other, SIZE);
        crate::inplace::title_memory_view(other, SIZE, section, 0);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A write of the runtime's own, told at one view of the pages
    /// (`writing`): the other views' next checks report the pages written.
    #[test]
    fn a_write_told_at_one_view_is_reported_at_the_others() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        writing(other + BLOCK, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        // Half a page and the next: both blocks, not just the first's.
        writing(other + BLOCK / 2, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// Both views of the pages are checked, each for a copy of its own
    /// (the title shows one physical page at several addresses, and each
    /// address has its host copy): a write through either is reported at
    /// both, whichever is checked first. Guarding the pages for the first
    /// view's check must not note the second view clean, or its copy would
    /// keep the bytes from before the write.
    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0), pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0), pages(two, 1)]), "the second view's first check");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()), "nothing written yet");
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()), "nothing written yet");
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 1)]), "written through this view, checked second");
        // SAFETY: the first view, as above.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0)]), "written through this view, checked second");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        drop_views(section, &[one, two], SIZE);
    }

    /// A write through another view leaves that view open; the guarded
    /// view's next check makes it read-only again, so a second write
    /// through it is seen too (`alias_lazy`, on in tests). From
    /// triippywhale 7e88d165.
    #[test]
    fn a_second_write_through_another_view_is_reported_too() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        for round in 0..3u8 {
            // SAFETY: the second view, read-write once the handler has opened it.
            unsafe { ((other + BLOCK + 8) as *mut u8).write(round) };
            // SAFETY: as above; the view is open after the first write.
            unsafe { ((other + BLOCK + 16) as *mut u8).write(round) };
            if alias_lazy() {
                assert!(!writable(guarded + BLOCK), "the guarded view stays read-only");
            }
            assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]), "round {round}");
            assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "round {round}");
        }
        drop_views(section, &[guarded, other], SIZE);
    }

    /// Our own write through a view of our own (`write_aside_now`, on in
    /// tests): the title's views stay read-only, the bytes show in them,
    /// and both views' next checks report the pages written; a write of
    /// the title's afterwards still faults and is reported. From
    /// triippywhale 2b64a9de.
    #[test]
    fn a_write_of_ours_aside_is_reported_at_every_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        let bytes = [0x5au8; 16];
        assert!(!write_aside_now(one + BLOCK + 8, bytes.as_ptr(), 16), "not guarded yet");
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0), pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0), pages(two, 1)]));
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        assert!(write_aside_now(one + BLOCK + 8, bytes.as_ptr(), 16), "guarded, in one view");
        // SAFETY: the title's views, readable.
        unsafe {
            assert_eq!(((one + BLOCK + 8) as *const u8).read(), 0x5a);
            assert_eq!(((two + BLOCK + 23) as *const u8).read(), 0x5a);
        }
        assert!(!writable(one + BLOCK), "the title's view stays read-only");
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 1)]));
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((two + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
        // Across the end of the view: not ours to write aside.
        assert!(!write_aside_now(one + BLOCK, [0u8; 2 * BLOCK].as_ptr(), 2 * BLOCK));
        // A remap of the range: no longer guarded, so not written aside.
        title_memory_remapped(one, SIZE);
        assert!(!write_aside_now(one + 8, bytes.as_ptr(), 16));
        crate::inplace::title_memory_section_closed(section);
        drop_views(section, &[one, two], SIZE);
    }

    #[test]
    fn an_unguardable_alias_keeps_every_view_dirty() {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (section, one, two) = two_views(BLOCK);
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
        // SAFETY: map one writable page of the same section. Protecting
        // the entire 64 KiB block at this address must fail.
        let alias = unsafe { MapViewOfFile(section as _, FILE_MAP_WRITE, 0, 0, KERNEL_PAGE).Value } as usize;
        assert_ne!(alias, 0);
        crate::inplace::title_memory_view(alias, KERNEL_PAGE, section, 0);
        for (i, view) in [one, two, one, two].into_iter().enumerate() {
            assert_eq!(guard(view, view + BLOCK), Some(vec![view / PAGE]));
            assert!(writable(alias));
            // SAFETY: different mappings of our section's first byte.
            unsafe {
                (alias as *mut u8).write_volatile(i as u8 + 1);
                assert_eq!((view as *const u8).read_volatile(), i as u8 + 1);
            }
        }
        // Unmap the short alias before checking the remaining views.
        // SAFETY: this test owns the view.
        unsafe {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: alias as *mut _ });
        }
        title_memory_remapped(alias, BLOCK);
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
        drop_views(section, &[one, two], BLOCK);
    }

    /// A page shown in more than `MANY_VIEWS` views is guarded at each by
    /// itself: the others stay writable, and nothing looks through them all.
    #[test]
    fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
        let (section, one, two) = two_views(BLOCK);
        let filler = one + (1 << 45);
        for n in 0..MANY_VIEWS {
            crate::inplace::title_memory_view(filler + n * BLOCK, BLOCK, section, 0);
        }
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert!(writable(two), "another view of it is left alone");
        // SAFETY: the guarded view, opened by the handler.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        title_memory_remapped(filler, MANY_VIEWS * BLOCK);
        drop_views(section, &[one, two], BLOCK);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    #[test]
    fn block_runs_merge_split_and_skip_clean_ranges() {
        let mut r = BlockRuns::new();
        assert!(!r.contains_range(10, 20));
        assert!(r.contains_range(10, 10));
        r.insert_range(10, 20);
        r.insert_range(20, 30); // adjacent -> merges into [10, 30)
        assert_eq!(r.runs.len(), 1);
        assert!(r.contains_range(10, 30));
        assert!(r.contains_range(12, 28));
        assert!(!r.contains_range(9, 30));
        assert!(!r.contains_range(10, 31));
        assert_eq!(r.next_uncovered(10, 35), 30);
        assert_eq!(r.next_uncovered(15, 25), 25);

        // Remove single block in middle -> splits into [10, 15) and [16, 30)
        r.remove(15);
        assert_eq!(r.runs.len(), 2);
        assert!(!r.contains_range(10, 30));
        assert!(r.contains_range(10, 15));
        assert!(r.contains_range(16, 30));
        assert_eq!(r.next_uncovered(10, 30), 15);
        assert_eq!(r.next_uncovered(15, 30), 15);
        assert_eq!(r.next_uncovered(16, 30), 30);

        // Re-insert single block -> merges back into [10, 30)
        r.insert(15);
        assert_eq!(r.runs.len(), 1);
        assert!(r.contains_range(10, 30));

        // Remove across multiple runs + edge boundaries
        r.insert_range(40, 50);
        r.insert_range(60, 70);
        r.remove_range(25, 65);
        assert!(r.contains_range(10, 25));
        assert!(!r.contains_range(25, 26));
        assert!(!r.contains_range(40, 50));
        assert!(r.contains_range(65, 70));
        assert_eq!(r.next_uncovered(10, 70), 25);

        // Near usize::MAX without overflow
        r.insert_range(usize::MAX - 2, usize::MAX);
        assert!(r.contains_range(usize::MAX - 2, usize::MAX));
        r.remove(usize::MAX - 1);
        assert!(r.contains_range(usize::MAX - 2, usize::MAX - 1));
        assert!(!r.contains_range(usize::MAX - 1, usize::MAX));
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{PAGE, by_range, title_memory_reading, title_memory_remapping};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    /// Addresses only these tests hold (nothing is read there).
    const HERE: usize = 0x7e00_0000_0000;

    /// A change of mapping waits for a read in place of its bytes that is
    /// under way, and such a read waits for the change.
    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        // A reader holds; the change does not begin until it lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading(HERE + PAGE / 2, PAGE);
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping(HERE + PAGE, PAGE);
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!changed.load(Ordering::SeqCst), "the mapping changed under a read in place");
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        // A change holds; the read does not begin until it is done.
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping(HERE, 2 * PAGE);
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading(HERE + PAGE + 16, 16);
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!read.load(Ordering::SeqCst), "a read in place began during a change of mapping");
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }

    /// Reads and changes whose bytes do not meet do not wait for each other,
    /// and a change waiting for a read keeps new reads of its bytes out (but
    /// not of others).
    #[test]
    fn reads_and_remappings_of_other_bytes_go_on() {
        if !by_range() {
            return; // VICTORY_MAPPING_RANGES=0: one lock over all of it
        }
        let at = HERE + 16 * PAGE;
        assert!(!super::meet(at, at + PAGE, at + 1, at + 1));
        assert!(!super::meet(at + 1, at + 1, at, at + PAGE));
        let reading = title_memory_reading(at, PAGE);
        // Another page changes while the read is under way, and another
        // read goes on while that change is.
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let changing = title_memory_remapping(at + PAGE, PAGE);
            let (read, was_read) = mpsc::channel();
            std::thread::spawn(move || {
                let _reading = title_memory_reading(at + 2 * PAGE, PAGE);
                read.send(()).unwrap();
            });
            let other = was_read.recv_timeout(Duration::from_secs(10)).is_ok();
            drop(changing);
            done.send(other).unwrap();
        });
        assert_eq!(finished.recv_timeout(Duration::from_secs(10)), Ok(true), "kept apart though their bytes do not meet");
        // A change of the read's page waits, and keeps a new read of its
        // bytes out meanwhile; a read elsewhere is not kept out.
        let changed = Arc::new(AtomicBool::new(false));
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                let _changing = title_memory_remapping(at, PAGE);
                changed.store(true, Ordering::SeqCst);
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !super::spans().changes.iter().any(|c| c.1 == at) {
            assert!(!change.is_finished(), "the writer finished under the read");
            assert!(std::time::Instant::now() < deadline, "the writer did not register");
            std::thread::yield_now();
        }
        assert!(super::title_memory_reading_now(at + 8, 8).is_none(), "a read began over a change that waits");
        while super::title_memory_reading_now(at + 3 * PAGE, 8).is_none() {
            assert!(std::time::Instant::now() < deadline, "a disjoint read stayed blocked");
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(!changed.load(Ordering::SeqCst), "the mapping changed under a read in place");
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
    }
}
