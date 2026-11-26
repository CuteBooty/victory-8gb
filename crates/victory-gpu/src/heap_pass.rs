//! Bookkeeping of the pass over the view heap's buffer views for shaders
//! that pick them by a computed index (`draw::State::heap_buffers`): which
//! slots it looks at again, and the span of each host copy their views
//! read. Kept apart from D3D12 so that it can be tested alone.
//!
//! The pass used to look again at every buffer view of the heap (81,000 in
//! the world) every 16 passes, and to rebuild the spans of all host copies
//! from all views every pass. Now a slot is looked at again only when
//! - its buffer view was written or went (the heap's own set of changes);
//! - the view table entry its record names was freed (the title released
//!   the resource: `descriptor::forget`), which leaves the record as it was;
//! - its record in title memory is not the one seen at the last look (a
//!   record copied by the title itself, past the API); a compare of 12
//!   bytes per slot, no lock and no lookup (with `VICTORY_DESC_DIRTY`, only
//!   for records on pages written since the last pass: [`DirtyPages`]);
//! - it found no host copy at its last look: every 16 passes, as before;
//! - every [`FULL_EVERY`] passes, all of them, as a safety net.
//!
//! The span of a copy is worked out again only when one of its slots went.
//! `VICTORY_HEAP_PASS_FULL=1` brings back the old pass (all slots every 16
//! passes, all spans every pass) to compare.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `VICTORY_DESC_DIRTY`: the pass compares the records of its kept slots
/// only on title-memory pages written since its last pass ([`DirtyPages`]).
/// Unset or 0: every record, as before. 1: memory the kernel's write watch
/// tracks (a heap elsewhere is compared whole); 2: also view memory, which
/// is then guarded (`crate::watch::writes` with `guarded`). Off by default.
///
/// Ported from a tree from the avatarmanz lineage, whose `sync_heaps`
/// takes the changed pages of placed shader-visible heaps from the write
/// watch; here the journal we keep already answers, and what is skipped is
/// only the record compare.
pub(crate) fn desc_dirty() -> Option<u8> {
    static MODE: std::sync::OnceLock<Option<u8>> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = std::env::var("VICTORY_DESC_DIRTY")
            .ok()
            .and_then(|v| v.trim().parse::<u8>().ok())
            .filter(|&m| m > 0)?;
        tracing::info!(
            mode,
            "the heap pass compares records only on pages written since its last pass (VICTORY_DESC_DIRTY)"
        );
        Some(mode)
    })
}

/// `VICTORY_DESC_DIRTY_CHECK=<n>`: every n-th record skipped as clean is
/// compared all the same, a mismatch counted (the first few logged) and
/// the slot looked at. 0 or unset: none.
pub(crate) fn desc_dirty_check() -> u64 {
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("VICTORY_DESC_DIRTY_CHECK")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// The journal's page (`crate::watch::PAGE`, which is Windows only).
const PAGE: u64 = 64 << 10;

/// Since the last line ([`dirty_line`]): records skipped as clean, records
/// compared, dirty pages found, skipped records compared all the same,
/// and of those the ones whose record had changed.
static DIRTY_STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Mismatches logged so far (the first few only).
static DIRTY_LOGGED: AtomicU64 = AtomicU64::new(0);

/// Told the frames of each frame-rate line; while [`desc_dirty`] is on, a
/// line every five seconds over the frames told since the last.
pub(crate) fn dirty_line(frames: f64) {
    if desc_dirty().is_none() {
        return;
    }
    /// Frames told since the last line, and when it was.
    static SINCE: std::sync::Mutex<Option<(f64, std::time::Instant)>> = std::sync::Mutex::new(None);
    let mut since = SINCE.lock().unwrap_or_else(|e| e.into_inner());
    let (told, last) = since.get_or_insert_with(|| (0.0, std::time::Instant::now()));
    *told += frames;
    if last.elapsed() < std::time::Duration::from_secs(5) || *told <= 0.0 {
        return;
    }
    let frames = std::mem::take(told);
    *last = std::time::Instant::now();
    drop(since);
    let [skipped, checked, pages, samples, mismatches] = [0, 1, 2, 3, 4].map(|i| DIRTY_STATS[i].swap(0, Relaxed));
    let [looks, first, changed, untracked, remapped, inplace, spanless] =
        [0, 1, 2, 3, 4, 5, 6].map(|i| DIRTY_WHY[i].swap(0, Relaxed));
    tracing::info!(
        "descriptor dirty per frame: {:.0} records skipped, {:.0} records checked, {:.1} pages dirty, {:.1} check samples, \
         {mismatches} mismatches in all over {frames:.0} frames (VICTORY_DESC_DIRTY_CHECK={}); of {looks} passes, could not \
         skip: {first} first look, {changed} heap or span changed, {untracked} memory untracked (mode 1: not write-watched, \
         e.g. physical-page views; mode 2 guards those), {remapped} heap's span remapped since the last look, {inplace} \
         VICTORY_INPLACE on, {spanless} heap without a span",
        skipped as f64 / frames,
        checked as f64 / frames,
        pages as f64 / frames,
        samples as f64 / frames,
        desc_dirty_check()
    );
}

/// Since the last line: passes that looked (or would have), and of them
/// those that could not skip, by cause: a first look, another heap object
/// or span, memory the journal does not track, the heap's span remapped
/// since the last look, title memory in place, a heap with no span.
static DIRTY_WHY: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Causes of [`DIRTY_WHY`] the caller finds before asking the journal.
pub(crate) enum NoLook {
    InPlace,
    NoSpan,
}

/// The pass could not look at the journal at all ([`NoLook`]).
pub(crate) fn dirty_no_look(why: NoLook) {
    DIRTY_WHY[0].fetch_add(1, Relaxed);
    DIRTY_WHY[match why {
        NoLook::InPlace => 5,
        NoLook::NoSpan => 6,
    }]
    .fetch_add(1, Relaxed);
}

/// What the journal said of a heap's memory: the asking's number and the
/// pages written since tracking began with the asking that last found each
/// (`crate::watch::writes`).
pub(crate) type Journal = (u64, Vec<(usize, u64)>);

/// The title-memory pages of one heap written since the pass last looked
/// ([`Picked::observe`]), from the write journal (`crate::watch`). A page
/// is dirty when its stamp (the asking that last found it written) is not
/// the one seen at the last look.
///
/// Sound only if the journal hears of every write into the heap's memory
/// made after an asking: the kernel's write watch sees every CPU write of
/// the process (the title's, ours in `descriptor::record` and
/// CopyDescriptors), the guards those into view memory. Not so the card's
/// own writes into title memory in place (`crate::inplace`,
/// `VICTORY_INPLACE`): the caller does not observe then. Memory mapped
/// anew since the last look: nothing is skipped.
pub(crate) struct DirtyPages {
    /// The heap object, its first byte, its end and its step, as last seen;
    /// another of any of them is a first use.
    owner: usize,
    start: u64,
    end: u64,
    step: u64,
    /// The remap epoch taken at the last look.
    epoch: u64,
    /// Stamps of the heap's pages as last seen.
    stamps: HashMap<usize, u64>,
    /// Pages written between the last two looks.
    dirty: HashSet<usize>,
    /// Whether the last look can say what is clean.
    known: bool,
    /// Whether the journal answered at the last look (a first use if not).
    looked: bool,
    /// Records skipped since the start, for the self-check.
    skips: u64,
}

impl DirtyPages {
    fn new() -> Self {
        DirtyPages {
            owner: 0,
            start: 0,
            end: 0,
            step: 0,
            epoch: 0,
            stamps: HashMap::new(),
            dirty: HashSet::new(),
            known: false,
            looked: false,
            skips: 0,
        }
    }

    /// Whether slot `slot`'s record (its first 12 bytes) lies wholly on
    /// pages not written between the last two looks.
    fn clean(&self, slot: u32) -> bool {
        if !self.known || self.step == 0 {
            return false;
        }
        let from = self.start + u64::from(slot) * self.step;
        let to = from + 12;
        if to > self.end {
            return false;
        }
        (from / PAGE..=(to - 1) / PAGE).all(|p| !self.dirty.contains(&(p as usize)))
    }
}

/// Passes between two looks at every slot, besides those marked.
pub(crate) const FULL_EVERY: u64 = 256;
/// Passes between two looks at slots that found no host copy (and between
/// two looks at every slot with the old pass).
pub(crate) const ORPHANS_EVERY: u64 = 16;

/// Whether `VICTORY_HEAP_PASS_FULL=1` asks for the old pass.
pub(crate) fn full_pass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_PASS_FULL").is_some_and(|v| v == "1"))
}

/// `VICTORY_HEAP_SAME_SKIP=1` (off by default): a due slot whose record is the one kept, whose host
/// slot is made from it with its view still there
/// (`HostHeap::made_from`), and whose kept host copy the tables still give
/// for that copy's span (`shadow::still_found`) with the same table epoch is
/// not looked at again: the look would find the same view, copy and
/// range. (Self-marks and rewrites of the same record, some 15k slots a
/// frame.) What the copy's bytes need is not the look's: every pass
/// refreshes each kept copy's span (`Picked::spans`, then
/// `Shadow::refresh`), looked at or not. Not with the old pass.
pub(crate) fn same_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SAME_SKIP").is_some_and(|v| v == "1"))
}

/// `VICTORY_HEAP_SAME_SKIP_CHECK=<n>`: every n-th slot the skip would
/// pass by is looked at all the same, and what the look finds (view's
/// host copy and range) compared with what was kept; a difference is a
/// mismatch, counted for the interval. 0 or unset: none.
pub(crate) fn same_skip_check() -> u64 {
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("VICTORY_HEAP_SAME_SKIP_CHECK").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    })
}

/// Since the last line: slots skipped, due slots looked at, copies not
/// still found (their slots not skipped), checks, check mismatches.
pub(crate) static SAME_SKIP: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// With the frame-rate line, while [`same_skip`] is on.
pub(crate) fn same_skip_line(frames: f64) {
    if !same_skip() {
        return;
    }
    let [skipped, due, moved, checks, mismatches] = SAME_SKIP.each_ref().map(|v| v.swap(0, Relaxed));
    tracing::info!(
        "heap same skip per frame (VICTORY_HEAP_SAME_SKIP=1): {:.1} due slots passed by of {:.1}, {:.1} kept copies \
         not found again (their slots looked at); checks {checks}, check mismatches {mismatches} (interval totals)",
        skipped as f64 / frames,
        (skipped + due) as f64 / frames,
        moved as f64 / frames,
    );
}

/// A slot the skip passed by was looked at (`VICTORY_HEAP_SAME_SKIP_CHECK`):
/// `kept` the copy key and range kept, `found` what the look found.
pub(crate) fn same_skip_checked(kept: (usize, u64, u64), found: Option<(usize, u64, u64)>) {
    SAME_SKIP[3].fetch_add(1, Relaxed);
    if found != Some(kept) {
        SAME_SKIP[4].fetch_add(1, Relaxed);
    }
}

/// A slot's record in title memory: the tag and two words
/// (`descriptor::record_at`).
pub(crate) type Record = [u32; 3];

struct Slot {
    copy: usize,
    from: u64,
    to: u64,
    record: Record,
    table_epoch: u64,
}

struct Copy<C> {
    copy: C,
    slots: HashSet<u32>,
    from: u64,
    to: u64,
    /// A slot went: the span is worked out again from those left.
    stale: bool,
}

/// What a pass looks at (see the module).
pub(crate) struct Picked<C> {
    passes: u64,
    slots: HashMap<u32, Slot>,
    copies: HashMap<usize, Copy<C>>,
    /// Slots that found no host copy, with the record seen.
    orphans: HashMap<u32, Record>,
    /// View table entry -> slots whose record named it at their last look
    /// (some may name another by now: checked when used).
    by_entry: HashMap<u32, HashSet<u32>>,
    /// Table entries freed since the last pass; `None`: not known.
    forgotten: Option<Vec<u32>>,
    /// The old pass (`VICTORY_HEAP_PASS_FULL=1`).
    full: bool,
    /// Pages of the heap written since the last look (`VICTORY_DESC_DIRTY`;
    /// never known unless [`Picked::observe`] is called).
    dirty: DirtyPages,
}

impl<C: Clone> Picked<C> {
    pub(crate) fn new(full: bool) -> Self {
        Picked {
            passes: 0,
            slots: HashMap::new(),
            copies: HashMap::new(),
            orphans: HashMap::new(),
            by_entry: HashMap::new(),
            forgotten: None,
            full,
            dirty: DirtyPages::new(),
        }
    }

    /// `VICTORY_DESC_DIRTY`: what the journal says of the heap's memory now,
    /// asked before any record of this pass is read, so that a write after
    /// the asking is in the next one. `owner`: the heap object; `start`,
    /// `end`, `step`: its memory and bytes per slot; `answer`: the journal
    /// (`None`: the memory is not tracked, nothing is skipped); `epoch`: the
    /// remap epoch taken before the asking (`crate::watch::remap_epoch`);
    /// `remapped_since`: whether the memory was mapped anew after an epoch
    /// (then nothing is skipped). The first look at a heap only sets the
    /// stamps. Returns the dirty pages. Not called: the pass compares every
    /// record, as before.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn observe(
        &mut self,
        owner: usize,
        start: u64,
        end: u64,
        step: u64,
        answer: Option<Journal>,
        epoch: u64,
        remapped_since: impl FnOnce(u64) -> bool,
    ) -> usize {
        let d = &mut self.dirty;
        let changed = d.looked && (d.owner != owner || d.start != start || d.end != end || d.step != step);
        let first = !d.looked || changed;
        // Only the heap's own span: remaps of other title memory (hundreds
        // a frame) do not count (`crate::watch::remapped_since`).
        let remapped = !first && remapped_since(d.epoch);
        d.epoch = epoch;
        (d.owner, d.start, d.end, d.step) = (owner, start, end, step);
        d.dirty.clear();
        DIRTY_WHY[0].fetch_add(1, Relaxed);
        let Some((_, pages)) = answer else {
            DIRTY_WHY[3].fetch_add(1, Relaxed);
            d.known = false;
            d.looked = false;
            d.stamps.clear();
            return 0;
        };
        if first {
            DIRTY_WHY[if changed { 2 } else { 1 }].fetch_add(1, Relaxed);
        } else if remapped {
            DIRTY_WHY[4].fetch_add(1, Relaxed);
        }
        d.looked = true;
        let (from, to) = ((start / PAGE) as usize, end.div_ceil(PAGE) as usize);
        let fresh: HashMap<usize, u64> = pages.into_iter().filter(|&(p, _)| p >= from && p < to).collect();
        if !first {
            for (&p, &w) in &fresh {
                if d.stamps.get(&p) != Some(&w) {
                    d.dirty.insert(p);
                }
            }
            // The journal forgets nothing: a page gone from it is in doubt.
            for &p in d.stamps.keys() {
                if !fresh.contains_key(&p) {
                    d.dirty.insert(p);
                }
            }
        }
        d.stamps = fresh;
        d.known = !first && !remapped;
        DIRTY_STATS[2].fetch_add(d.dirty.len() as u64, Relaxed);
        d.dirty.len()
    }

    /// View table entries were freed (`None`: some were, which is not
    /// known): their slots are looked at next pass. Every heap's pass is
    /// told, though one of them asks.
    pub(crate) fn note_forgotten(&mut self, entries: Option<&[u32]>) {
        match (entries, &mut self.forgotten) {
            (Some(new), Some(kept)) => kept.extend_from_slice(new),
            _ => self.forgotten = None,
        }
    }

    /// The slots this pass looks at again, in order. `changed`: the slots
    /// whose buffer view was written or went since the last pass (and the
    /// entries [`Picked::note_forgotten`] was told of: if it was told it
    /// does not know, all slots are looked at); `all`: every slot that holds a
    /// buffer view; `record`: a slot's record now; `entry`: the table
    /// entry a record names, if it names one.
    pub(crate) fn plan(
        &mut self,
        changed: Vec<u32>,
        all: impl FnOnce() -> Vec<u32>,
        record: impl Fn(u32) -> Option<Record>,
        entry: impl Fn(Record) -> Option<u32>,
    ) -> Vec<u32> {
        let pass = self.passes;
        self.passes += 1;
        let forgotten = self.forgotten.replace(Vec::new());
        if self.full {
            if pass.is_multiple_of(ORPHANS_EVERY) {
                self.clear();
                return all();
            }
            return changed;
        }
        let Some(forgotten) = forgotten else {
            self.clear();
            return all();
        };
        if pass.is_multiple_of(FULL_EVERY) {
            self.clear();
            return all();
        }
        let mut due: HashSet<u32> = changed.into_iter().collect();
        for e in forgotten {
            if let Some(slots) = self.by_entry.remove(&e) {
                // Only those whose record still names the entry.
                due.extend(
                    slots
                        .into_iter()
                        .filter(|s| self.record_of(*s).is_some_and(|r| entry(r) == Some(e))),
                );
            }
        }
        // A record on a page not written since the last look is as it was
        // then (`VICTORY_DESC_DIRTY`; without it `skipping` is false and
        // every record is compared, as before).
        let skipping = self.dirty.known;
        let every = if skipping { desc_dirty_check() } else { 0 };
        let (mut skipped, mut checked) = (0u64, 0u64);
        // For VICTORY_FRAME_WHY2: records read (sampled checks included) and
        // records found different.
        let (mut sampled, mut different) = (0u64, 0u64);
        let dirty = &mut self.dirty;
        let mut look = |slot: u32, kept: Record, due: &mut HashSet<u32>| {
            if skipping && dirty.clean(slot) {
                skipped += 1;
                dirty.skips += 1;
                if every > 0 && dirty.skips.is_multiple_of(every) {
                    DIRTY_STATS[3].fetch_add(1, Relaxed);
                    sampled += 1;
                    if record(slot) != Some(kept) {
                        DIRTY_STATS[4].fetch_add(1, Relaxed);
                        different += 1;
                        if DIRTY_LOGGED.fetch_add(1, Relaxed) < 8 {
                            tracing::warn!(
                                heap = format_args!("{:#x}", dirty.start),
                                slot,
                                "VICTORY_DESC_DIRTY_CHECK: a record skipped as clean had changed"
                            );
                        }
                        due.insert(slot);
                    }
                }
                return;
            }
            checked += 1;
            if record(slot) != Some(kept) {
                different += 1;
                due.insert(slot);
            }
        };
        for (&slot, kept) in &self.slots {
            look(slot, kept.record, &mut due);
        }
        let orphans_too = pass.is_multiple_of(ORPHANS_EVERY);
        for (&slot, &seen) in &self.orphans {
            if orphans_too {
                due.insert(slot);
            } else {
                look(slot, seen, &mut due);
            }
        }
        crate::frame_why::count(crate::frame_why::Count::Checked, checked + sampled);
        crate::frame_why::count(crate::frame_why::Count::Changed, different);
        if desc_dirty().is_some() {
            DIRTY_STATS[0].fetch_add(skipped, Relaxed);
            DIRTY_STATS[1].fetch_add(checked, Relaxed);
        }
        let mut due: Vec<u32> = due.into_iter().collect();
        due.sort_unstable();
        due
    }

    fn record_of(&self, slot: u32) -> Option<Record> {
        self.slots
            .get(&slot)
            .map(|s| s.record)
            .or_else(|| self.orphans.get(&slot).copied())
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.copies.clear();
        self.orphans.clear();
        self.by_entry.clear();
    }

    /// What slot `slot` held is let go (it is looked at now).
    pub(crate) fn forget(&mut self, slot: u32) {
        self.orphans.remove(&slot);
        let Some(old) = self.slots.remove(&slot) else {
            return;
        };
        if let Some(copy) = self.copies.get_mut(&old.copy) {
            copy.slots.remove(&slot);
            if copy.slots.is_empty() {
                self.copies.remove(&old.copy);
            } else if old.from <= copy.from || old.to >= copy.to {
                copy.stale = true;
            }
        }
    }

    /// Slot `slot` (with `record`, naming table entry `entry`) reads title
    /// memory `[from, to)` through host copy `copy` (`key`: its identity).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn keep(&mut self, slot: u32, key: usize, copy: &C, from: u64, to: u64, record: Record, entry: Option<u32>, table_epoch: u64) {
        self.forget(slot);
        self.note_entry(slot, entry);
        self.slots.insert(slot, Slot { copy: key, from, to, record, table_epoch });
        let c = self.copies.entry(key).or_insert_with(|| Copy {
            copy: copy.clone(),
            slots: HashSet::new(),
            from,
            to,
            stale: false,
        });
        c.slots.insert(slot);
        c.from = c.from.min(from);
        c.to = c.to.max(to);
    }

    /// Slot `slot` holds a buffer view whose memory has no host copy yet.
    pub(crate) fn orphan(&mut self, slot: u32, record: Record, entry: Option<u32>) {
        self.forget(slot);
        self.note_entry(slot, entry);
        self.orphans.insert(slot, record);
    }

    fn note_entry(&mut self, slot: u32, entry: Option<u32>) {
        // Only kept with the incremental pass (the old one does not use it).
        if let Some(e) = entry
            && !self.full
        {
            // Once: a slot looked at again every pass with the same entry
            // was pushed again each time.
            self.by_entry.entry(e).or_default().insert(slot);
        }
    }

    /// The host copy (its key) and range kept for slot `slot`, if a view
    /// is kept for it (`VICTORY_FRAME_WHY2`'s count of slots looked at
    /// again that resolve to the same).
    pub(crate) fn kept(&self, slot: u32) -> Option<(usize, u64, u64, Record, u64)> {
        self.slots.get(&slot).map(|s| (s.copy, s.from, s.to, s.record, s.table_epoch))
    }

    /// Each kept host copy: its key, the copy and its span so far.
    pub(crate) fn kept_copies(&self) -> impl Iterator<Item = (usize, &C, u64, u64)> {
        self.copies.iter().map(|(&key, c)| (key, &c.copy, c.from, c.to))
    }

    /// Whether this is the old pass (`VICTORY_HEAP_PASS_FULL=1`).
    pub(crate) fn full(&self) -> bool {
        self.full
    }

    /// The views kept.
    pub(crate) fn views(&self) -> usize {
        self.slots.len()
    }

    /// Per host copy, the span its views read: `(copy, from, to)`.
    pub(crate) fn spans(&mut self) -> Vec<(C, u64, u64)> {
        if self.full {
            // As the old pass did: from every view, every pass.
            let mut spans: HashMap<usize, (u64, u64)> = HashMap::new();
            for s in self.slots.values() {
                let span = spans.entry(s.copy).or_insert((s.from, s.to));
                span.0 = span.0.min(s.from);
                span.1 = span.1.max(s.to);
            }
            return spans
                .into_iter()
                .filter_map(|(key, (from, to))| Some((self.copies.get(&key)?.copy.clone(), from, to)))
                .collect();
        }
        let slots = &self.slots;
        self.copies
            .values_mut()
            .map(|c| {
                if c.stale {
                    c.stale = false;
                    let (mut from, mut to) = (u64::MAX, 0);
                    for s in c.slots.iter().filter_map(|s| slots.get(s)) {
                        from = from.min(s.from);
                        to = to.max(s.to);
                    }
                    (c.from, c.to) = (from, to);
                }
                (c.copy.clone(), c.from, c.to)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap of slots with records, as title memory holds them.
    struct Heap {
        records: HashMap<u32, Record>,
    }

    impl Heap {
        fn record(&self, slot: u32) -> Option<Record> {
            self.records.get(&slot).copied()
        }
    }

    fn entry(r: Record) -> Option<u32> {
        Some(r[1])
    }

    /// One pass: plans, then "looks" at each due slot: a slot reads 16
    /// bytes from `slot * 16` of copy `slot / 4`.
    fn pass(p: &mut Picked<u32>, heap: &Heap, changed: Vec<u32>, forgotten: Vec<u32>) -> Vec<u32> {
        p.note_forgotten(Some(&forgotten));
        let due = p.plan(
            changed,
            || {
                let mut all: Vec<u32> = heap.records.keys().copied().collect();
                all.sort_unstable();
                all
            },
            |s| heap.record(s),
            entry,
        );
        for &slot in &due {
            let Some(record) = heap.record(slot) else {
                p.forget(slot);
                continue;
            };
            let copy = slot / 4;
            let from = u64::from(slot) * 16;
            p.keep(slot, copy as usize, &copy, from, from + 16, record, entry(record), 0);
        }
        due
    }

    fn heap(n: u32) -> Heap {
        Heap {
            records: (0..n).map(|s| (s, [1, 100 + s, 1])).collect(),
        }
    }

    #[test]
    fn a_slot_looked_at_again_is_noted_once_under_its_entry() {
        let heap = heap(8);
        let mut p = Picked::new(false);
        pass(&mut p, &heap, vec![], vec![]);
        for _ in 0..5 {
            assert_eq!(pass(&mut p, &heap, vec![3], vec![]), vec![3]);
        }
        assert_eq!(p.by_entry[&103].len(), 1);
        // The entry freed: the slot is looked at once.
        assert_eq!(pass(&mut p, &heap, vec![], vec![103]), vec![3]);
    }

    #[test]
    fn only_slots_changed_since_the_last_pass_are_looked_at_again() {
        let mut heap = heap(64);
        let mut p = Picked::new(false);
        // The first pass looks at all of them.
        assert_eq!(pass(&mut p, &heap, vec![], vec![]).len(), 64);
        assert_eq!(p.views(), 64);
        // Nothing changed: nothing is looked at.
        assert!(pass(&mut p, &heap, vec![], vec![]).is_empty());
        // A view written through the API.
        heap.records.insert(5, [1, 500, 1]);
        assert_eq!(pass(&mut p, &heap, vec![5], vec![]), vec![5]);
        // A record copied past the API: found by its words alone.
        heap.records.insert(9, [1, 900, 1]);
        assert_eq!(pass(&mut p, &heap, vec![], vec![]), vec![9]);
        // The resource of entry 117 (slot 17) released: the record is as
        // it was, the slot is looked at all the same; slot 5 no longer
        // names its old entry 105 and is not.
        assert_eq!(pass(&mut p, &heap, vec![], vec![117, 105]), vec![17]);
        // And the next pass leaves them alone again.
        assert!(pass(&mut p, &heap, vec![], vec![]).is_empty());
    }

    #[test]
    fn a_slot_that_went_shrinks_its_copy_span() {
        let mut heap = heap(8);
        let mut p = Picked::new(false);
        pass(&mut p, &heap, vec![], vec![]);
        let span = |p: &mut Picked<u32>, copy: u32| p.spans().into_iter().find(|s| s.0 == copy).map(|s| (s.1, s.2));
        assert_eq!(span(&mut p, 1), Some((64, 128)));
        heap.records.remove(&7);
        pass(&mut p, &heap, vec![7], vec![]);
        assert_eq!(span(&mut p, 1), Some((64, 112)));
        for s in 4..7 {
            heap.records.remove(&s);
        }
        pass(&mut p, &heap, vec![4, 5, 6], vec![]);
        assert_eq!(span(&mut p, 1), None);
        assert_eq!(span(&mut p, 0), Some((0, 64)));
    }

    #[test]
    fn orphans_are_tried_again_every_16_passes_and_all_slots_now_and_then() {
        let heap = heap(4);
        let mut p: Picked<u32> = Picked::new(false);
        let _ = p.plan(vec![], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
        for s in 0..3 {
            p.keep(s, 0, &0, 0, 16, heap.record(s).unwrap(), None, 0);
        }
        p.orphan(3, heap.record(3).unwrap(), None);
        for n in 1..FULL_EVERY {
            let due = p.plan(vec![], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
            let want: Vec<u32> = if n.is_multiple_of(ORPHANS_EVERY) { vec![3] } else { vec![] };
            assert_eq!(due, want, "pass {n}");
        }
        let due = p.plan(vec![], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
        assert_eq!(due, vec![0, 1, 2, 3]);
    }

    #[test]
    fn the_old_pass_looks_at_all_every_16_passes() {
        let heap = heap(4);
        let mut p: Picked<u32> = Picked::new(true);
        for n in 0..40u64 {
            let due = p.plan(vec![2], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
            let want: Vec<u32> = if n.is_multiple_of(ORPHANS_EVERY) { vec![0, 1, 2, 3] } else { vec![2] };
            assert_eq!(due, want, "pass {n}");
        }
    }

    // `VICTORY_DESC_DIRTY`: a heap of 4,096 view slots (32 bytes each) on
    // two journal pages, 16 and 17.
    const SLOTS: u32 = 4096;
    const START: u64 = 16 * PAGE;
    const STEP: u64 = 32;

    /// The journal with pages 16 and 17 last found written by askings `a`
    /// and `b`.
    fn journal(a: u64, b: u64) -> Option<Journal> {
        Some((a.max(b), vec![(16, a), (17, b)]))
    }

    fn observe(p: &mut Picked<u32>, owner: usize, answer: Option<Journal>, remapped: bool) -> usize {
        p.observe(owner, START, START + u64::from(SLOTS) * STEP, STEP, answer, 0, |_| remapped)
    }

    /// The plan alone, and how many records it read; then the due slots
    /// looked at, as `pass` does.
    fn plan_and_look(p: &mut Picked<u32>, heap: &Heap) -> (Vec<u32>, u64) {
        p.note_forgotten(Some(&[]));
        let reads = std::cell::Cell::new(0u64);
        let due = p.plan(
            vec![],
            || {
                let mut all: Vec<u32> = heap.records.keys().copied().collect();
                all.sort_unstable();
                all
            },
            |s| {
                reads.set(reads.get() + 1);
                heap.record(s)
            },
            entry,
        );
        for &slot in &due {
            let Some(record) = heap.record(slot) else {
                p.forget(slot);
                continue;
            };
            let from = u64::from(slot) * 16;
            p.keep(slot, (slot / 4) as usize, &(slot / 4), from, from + 16, record, entry(record), 0);
        }
        (due, reads.get())
    }

    /// A heap whose first pass has been made with the journal at (1, 1).
    fn looked_once() -> (Heap, Picked<u32>) {
        let heap = heap(SLOTS);
        let mut p = Picked::new(false);
        assert_eq!(observe(&mut p, 1, journal(1, 1), false), 0);
        assert_eq!(plan_and_look(&mut p, &heap).0.len(), SLOTS as usize);
        (heap, p)
    }

    #[test]
    fn records_on_clean_pages_are_not_read() {
        let (mut heap, mut p) = looked_once();
        // Nothing written: no record read at all.
        assert_eq!(observe(&mut p, 1, journal(1, 1), false), 0);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![], 0));
        // Slot 3000 (page 17) written: only page 17's records are read.
        heap.records.insert(3000, [1, 9000, 1]);
        assert_eq!(observe(&mut p, 1, journal(1, 2), false), 1);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![3000], 2048));
    }

    #[test]
    fn a_write_after_the_asking_is_found_next_pass() {
        let (mut heap, mut p) = looked_once();
        // The asking finds nothing; the title writes slot 7 before the
        // plan: skipped now, as it was written after the asking...
        observe(&mut p, 1, journal(1, 1), false);
        heap.records.insert(7, [1, 700, 1]);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![], 0));
        // ...and the next asking has found page 16 written.
        assert_eq!(observe(&mut p, 1, journal(3, 1), false), 1);
        assert_eq!(plan_and_look(&mut p, &heap).0, vec![7]);
        // A page gone from the journal is in doubt.
        heap.records.insert(9, [1, 900, 1]);
        assert_eq!(observe(&mut p, 1, Some((3, vec![(17, 1)])), false), 1);
        assert_eq!(plan_and_look(&mut p, &heap).0, vec![9]);
    }

    #[test]
    fn a_heap_made_anew_at_the_same_address_is_compared_whole() {
        let (mut heap, mut p) = looked_once();
        // Another heap object over the same memory, the pages' stamps as
        // they were: a first use, every record compared.
        heap.records.insert(11, [1, 1100, 1]);
        observe(&mut p, 2, journal(1, 1), false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![11], u64::from(SLOTS)));
        // Then known again.
        observe(&mut p, 2, journal(1, 1), false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![], 0));
        // The memory mapped anew: every record compared.
        heap.records.insert(12, [1, 1200, 1]);
        observe(&mut p, 2, journal(1, 1), true);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![12], u64::from(SLOTS)));
        // Another span: a first use.
        heap.records.insert(13, [1, 1300, 1]);
        p.observe(2, START + PAGE, START + PAGE + u64::from(SLOTS) * STEP, STEP, journal(1, 1), 0, |_| false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![13], u64::from(SLOTS)));
    }

    #[test]
    fn first_use_and_untracked_memory_compare_every_record() {
        // Passes without the switch: every record compared, as before.
        let mut heap = heap(SLOTS);
        let mut p = Picked::new(false);
        assert_eq!(plan_and_look(&mut p, &heap).0.len(), SLOTS as usize);
        heap.records.insert(5, [1, 500, 1]);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![5], u64::from(SLOTS)));
        // The first look sets the stamps only.
        heap.records.insert(6, [1, 600, 1]);
        observe(&mut p, 1, journal(4, 4), false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![6], u64::from(SLOTS)));
        // Memory the journal does not track: no skip, and the next answer
        // is a first use again.
        heap.records.insert(8, [1, 800, 1]);
        observe(&mut p, 1, None, false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![8], u64::from(SLOTS)));
        observe(&mut p, 1, journal(4, 4), false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![], u64::from(SLOTS)));
        observe(&mut p, 1, journal(4, 4), false);
        assert_eq!(plan_and_look(&mut p, &heap), (vec![], 0));
    }

    #[test]
    fn a_record_across_a_page_end_needs_both_pages_clean() {
        let mut d = DirtyPages::new();
        (d.start, d.end, d.step, d.known) = (START - 8, START + PAGE, STEP, true);
        // Slot 0 holds bytes START-8..START+4: pages 15 and 16.
        assert!(d.clean(0));
        d.dirty.insert(16);
        assert!(!d.clean(0));
        d.dirty.clear();
        d.dirty.insert(15);
        assert!(!d.clean(0));
        assert!(d.clean(1));
    }
}
