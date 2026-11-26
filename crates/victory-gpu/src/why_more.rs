//! SPDX-License-Identifier: GPL-3.0-or-later
//!
//! More `VICTORY_FRAME_WHY2=1` counters, printed with its five-second lines ([`log`]); nothing is
//! measured without the switch.
//!
//! - The frame's timeline from the present call: the title's
//!   last enqueue before it, the submit worker's last `ExecuteCommandLists`
//!   of the title's lists before the present returns, when the relay saw
//!   that submission's fence and when its landing ended, the present's
//!   return, and the title's first enqueue afterwards. Do the title's lists
//!   wait behind our present, and does the card's tail?
//! - The heap pass: of the slots it looked at again with a view
//!   kept, how many resolved to the same host copy and range; when the pass
//!   ran between two presents, and on which thread.
//! - Fresh descriptor tables, by cause: a constant buffer view
//!   not read in place, a half view, an entry at or past the mirrored
//!   slots. A table may have more than one cause.
//! - The guards lock: how long each kind of holder held it.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use crate::frame_why::enabled;

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Nanoseconds since the first look; never 0.
fn now() -> u64 {
    EPOCH.elapsed().as_nanos() as u64 + 1
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn ms(ns: i128) -> f64 {
    ns as f64 / 1e6
}

// ---------------------------------------------------------------- timeline

/// The timeline's fields, in the line's order.
const FIELDS: [&str; 6] = [
    "title_last_enqueue",
    "worker_last_execute",
    "its_fence_seen",
    "its_landing_end",
    "present_return",
    "title_first_enqueue_after",
];

#[derive(Default)]
struct Timeline {
    /// The open frame's present call and return.
    call: u64,
    ret: u64,
    /// The last enqueue's return before the open frame's present call.
    before: u64,
    first_after: u64,
    /// The last title submission executed (fence value, time), and the
    /// one that was last when the open frame's present returned.
    exec: (u64, u64),
    exec_at_return: (u64, u64),
    last_enqueue: u64,
    /// Fence values the relay saw passed, and the fences of the flights it
    /// landed, with the times: the open and the previous frame's.
    seen: Vec<(u64, u64)>,
    landed: Vec<(u64, u64)>,
    /// The heap pass since the last present return: start, end, thread.
    pass: Option<(u64, u64, usize)>,
    // Interval sums: per field the sum of offsets (ns) and the frames that
    // had it; the worst frame's offsets.
    sums: [i128; FIELDS.len()],
    counts: [u64; FIELDS.len()],
    frames: u64,
    worst: (u64, [Option<i128>; FIELDS.len()]),
    // The heap pass: passes by thread kind, starts and ends after the
    // previous present's return, its end before the next present call.
    passes: [u64; 3],
    pass_sums: [u64; 3],
    pass_max: [u64; 3],
    /// The submit worker's idle ns inside passes (sum, longest).
    pass_idle: (u64, u64),
    /// The enqueue of the pass thread's list after a pass: a pass ended
    /// and its thread has not enqueued yet (`pass_open`), the present call
    /// that came meanwhile (`pass_ref`), an enqueue waiting for the next
    /// present call (`pass_enqueued`); offsets from the present call (ns:
    /// sum, count, the latest and the earliest).
    pass_open: bool,
    pass_ref: u64,
    pass_enqueued: u64,
    pass_enqueue: (i128, u64, i128, i128),
}

impl Timeline {
    fn pass_enqueue_at(&mut self, offset: i128) {
        let e = &mut self.pass_enqueue;
        if e.1 == 0 {
            (e.2, e.3) = (offset, offset);
        }
        e.0 += offset;
        e.1 += 1;
        e.2 = e.2.max(offset);
        e.3 = e.3.min(offset);
    }
}

thread_local! {
    /// This thread ran the heap pass and has not enqueued since.
    static PASS_THREAD: Cell<bool> = const { Cell::new(false) };
}

static TIMELINE: LazyLock<Mutex<Timeline>> = LazyLock::new(Default::default);

impl Timeline {
    /// The open frame is complete: its next present call came at `next`.
    fn close(&mut self, next: u64) {
        if self.call == 0 || self.ret == 0 {
            return;
        }
        let at = |t: u64| (t != 0).then(|| i128::from(t) - i128::from(self.call));
        let (fence, executed) = self.exec_at_return;
        let first = |v: &[(u64, u64)]| {
            v.iter().filter(|&&(f, t)| fence != 0 && f >= fence && t >= executed).map(|&(_, t)| t).min()
        };
        let row = [
            at(self.before),
            at(executed),
            first(&self.seen).and_then(at),
            first(&self.landed).and_then(at),
            at(self.ret),
            at(self.first_after),
        ];
        for (i, v) in row.iter().enumerate() {
            if let Some(v) = v {
                self.sums[i] += v;
                self.counts[i] += 1;
            }
        }
        self.frames += 1;
        let wall = next.saturating_sub(self.call);
        if wall >= self.worst.0 {
            self.worst = (wall, row);
        }
        // What came before this frame's last execute is no longer needed.
        let keep = |v: &mut Vec<(u64, u64)>| {
            v.retain(|&(_, t)| t >= executed);
            if v.len() > 4096 {
                v.drain(..v.len() - 4096);
            }
        };
        keep(&mut self.seen);
        keep(&mut self.landed);
    }
}

/// The title's present begins (`crate::queue`).
pub(crate) fn present_call() {
    if !enabled() {
        return;
    }
    let t = now();
    let mut s = lock(&TIMELINE);
    s.close(t);
    if let Some((start, end, kind)) = s.pass.take() {
        // Measured from the previous present's return.
        let from = s.ret;
        if from != 0 {
            s.passes[kind] += 1;
            let parts = [start.saturating_sub(from), end.saturating_sub(from), t.saturating_sub(end)];
            for (i, p) in parts.into_iter().enumerate() {
                s.pass_sums[i] += p;
                s.pass_max[i] = s.pass_max[i].max(p);
            }
        }
    }
    if s.pass_enqueued != 0 {
        let offset = i128::from(s.pass_enqueued) - i128::from(t);
        s.pass_enqueue_at(offset);
        s.pass_enqueued = 0;
    } else if s.pass_open && s.pass_ref == 0 {
        s.pass_ref = t;
    }
    s.call = t;
    s.ret = 0;
    s.before = s.last_enqueue;
    s.first_after = 0;
}

/// The title's present returns (`crate::queue`).
pub(crate) fn present_return() {
    if !enabled() {
        return;
    }
    let mut s = lock(&TIMELINE);
    s.ret = now();
    s.exec_at_return = s.exec;
}

/// A title thread gives lists to a queue (`crate::queue`'s `submit_cut`);
/// the enqueue's return is noted when this is dropped.
pub(crate) fn enqueue() -> Enqueue {
    if !enabled() {
        return Enqueue(false);
    }
    let t = now();
    let mut s = lock(&TIMELINE);
    if s.ret != 0 && s.first_after == 0 {
        s.first_after = t;
    }
    if PASS_THREAD.replace(false) && s.pass_open {
        s.pass_open = false;
        if s.pass_ref != 0 {
            // After the present call that followed the pass.
            let offset = i128::from(t) - i128::from(s.pass_ref);
            s.pass_enqueue_at(offset);
            s.pass_ref = 0;
        } else {
            s.pass_enqueued = t;
        }
    }
    Enqueue(true)
}

pub(crate) struct Enqueue(bool);

impl Drop for Enqueue {
    fn drop(&mut self) {
        if self.0 {
            lock(&TIMELINE).last_enqueue = now();
        }
    }
}

/// The title's lists went to the card, followed by the host fence's
/// `fence` (`Host::submit`).
pub(crate) fn executed(fence: u64) {
    if enabled() {
        lock(&TIMELINE).exec = (fence, now());
    }
}

/// The relay found the flights up to fence value `fence` run.
pub(crate) fn fence_seen(fence: u64) {
    if enabled() {
        lock(&TIMELINE).seen.push((fence, now()));
    }
}

/// The relay landed the flights up to fence value `fence`.
pub(crate) fn landed(fence: u64) {
    if enabled() {
        lock(&TIMELINE).landed.push((fence, now()));
    }
}

// --------------------------------------------------------------- heap pass

/// Of the slots the heap pass looked at again: those that had a view kept,
/// and of them those that resolved to the same host copy and range.
/// Then: due slots looked at, and of them those whose record is the one
/// kept and whose host slot is made from it already (self-marks and
/// rewrites of the same record: what `VICTORY_HEAP_SAME_SKIP` would skip).
static HEAP_SAME: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// A due slot the heap pass read the record of: `same` whether the record
/// is the kept one and its host slot made from it.
pub(crate) fn heap_record(same: bool) {
    if enabled() {
        HEAP_SAME[3].fetch_add(1, Relaxed);
        HEAP_SAME[4].fetch_add(u64::from(same), Relaxed);
    }
}

/// The heap pass begins: its start and the submit worker's idle clock, if
/// measured.
pub(crate) fn heap_pass_begin() -> Option<(u64, u64)> {
    enabled().then(|| (now(), crate::frame_why::submit_idle_ns()))
}

/// The heap pass that began at `start` ended.
pub(crate) fn heap_pass_end(start: Option<(u64, u64)>) {
    let Some((start, idle)) = start else { return };
    let idle = crate::frame_why::submit_idle_ns().saturating_sub(idle);
    let kind = match thread_kind() {
        Kind::Title => 0,
        Kind::Heap => 1,
        _ => 2,
    };
    let mut s = lock(&TIMELINE);
    s.pass = Some((start, now(), kind));
    s.pass_idle.0 += idle;
    s.pass_idle.1 = s.pass_idle.1.max(idle);
    // The list this thread is recording goes to its queue next.
    PASS_THREAD.set(true);
    (s.pass_open, s.pass_ref, s.pass_enqueued) = (true, 0, 0);
}

/// A slot the heap pass looked at: `kept` whether a view was kept for it
/// (and the copy and range were known), `same` whether it resolved to them.
pub(crate) fn heap_slot(kept: bool, same: bool) {
    if enabled() {
        HEAP_SAME[0].fetch_add(1, Relaxed);
        HEAP_SAME[1].fetch_add(u64::from(kept), Relaxed);
        HEAP_SAME[2].fetch_add(u64::from(kept && same), Relaxed);
    }
}

// ------------------------------------------------------------ fresh tables

/// Why a table is made afresh (bits).
pub(crate) const CBV_COPIED: u8 = 1;
pub(crate) const HALF: u8 = 2;
pub(crate) const PAST_MIRROR: u8 = 4;

/// Fresh tables made, then tables and slots by cause (constant buffer
/// copied, half views, past the mirror), then tables of several causes.
static TABLES: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// A table made afresh, of `slots` slots, for the causes `why`.
pub(crate) fn fresh_table(why: u8, slots: u32) {
    if !enabled() {
        return;
    }
    TABLES[0].fetch_add(1, Relaxed);
    for (i, bit) in [CBV_COPIED, HALF, PAST_MIRROR].into_iter().enumerate() {
        if why & bit != 0 {
            TABLES[1 + 2 * i].fetch_add(1, Relaxed);
            TABLES[2 + 2 * i].fetch_add(u64::from(slots), Relaxed);
        }
    }
    TABLES[7].fetch_add(u64::from(why.count_ones() > 1), Relaxed);
}

// ------------------------------------------------------------- guards lock

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Submit,
    Fault,
    Landing,
    Remap,
    Ours,
    Heap,
    Title,
}

const HOLDERS: [&str; 7] = ["submit", "fault", "landing", "remap", "our_other", "heap_worker", "title_other"];

thread_local! {
    /// What this thread is doing that takes the guards lock (`Holder`).
    static TAG: Cell<Option<Kind>> = const { Cell::new(None) };
    /// The thread's kind by its name, found once.
    static KIND: Cell<Option<Kind>> = const { Cell::new(None) };
}

fn thread_kind() -> Kind {
    KIND.with(|k| {
        *k.get().get_or_insert_with(|| {
            let kind = match std::thread::current().name() {
                None => Kind::Title,
                Some("victory-submit") => Kind::Submit,
                Some("victory-relay" | "victory-land" | "victory-late-wait") => Kind::Landing,
                Some("victory-heap") => Kind::Heap,
                Some(_) => Kind::Ours,
            };
            k.set(Some(kind));
            kind
        })
    })
}

/// What takes the guards lock on this thread until dropped: the write
/// fault handler or a remap (`crate::watch`), whatever thread it is on.
pub(crate) enum Holder {
    Fault,
    Remap,
}

pub(crate) struct Tagged(Option<Option<Kind>>);

pub(crate) fn holder(h: Holder) -> Tagged {
    if !enabled() {
        return Tagged(None);
    }
    let kind = match h {
        Holder::Fault => Kind::Fault,
        Holder::Remap => Kind::Remap,
    };
    Tagged(Some(TAG.replace(Some(kind))))
}

impl Drop for Tagged {
    fn drop(&mut self) {
        if let Some(was) = self.0 {
            TAG.set(was);
        }
    }
}

/// Per holder: write holds' ns, the longest, read holds' ns.
static HOLDS: [[AtomicU64; 3]; HOLDERS.len()] = [const { [const { AtomicU64::new(0) }; 3] }; HOLDERS.len()];

/// Whether the guards lock's holds are timed for the by-holder line
/// (`VICTORY_FRAME_WHY2=1`). The timing wrapper is `crate::watch`'s `Hold`.
pub(crate) fn guards_timed() -> bool {
    enabled()
}

/// A hold of the guards lock that lasted `ns`, counted to this thread's
/// holder. Atomics only, no lock or allocation: the write fault handler
/// drops one.
pub(crate) fn guards_held(ns: u64, write: bool) {
    let kind = TAG.get().unwrap_or_else(thread_kind);
    let h = &HOLDS[kind as usize];
    if write {
        h[0].fetch_add(ns, Relaxed);
        h[1].fetch_max(ns, Relaxed);
    } else {
        h[2].fetch_add(ns, Relaxed);
    }
}

// -------------------------------------------------------------------- lines

/// With the five-second lines of `VICTORY_FRAME_WHY2`: `frames` since the last.
pub(crate) fn log(frames: f64) {
    let t = {
        let mut s = lock(&TIMELINE);
        let t = (
            s.sums, s.counts, s.frames, s.worst, s.passes, s.pass_sums, s.pass_max, s.pass_idle, s.pass_enqueue,
        );
        (s.sums, s.counts, s.frames, s.worst) = Default::default();
        (s.passes, s.pass_sums, s.pass_max, s.pass_idle, s.pass_enqueue) = Default::default();
        t
    };
    let (sums, counts, timed, worst, passes, pass_sums, pass_max, pass_idle, pass_enqueue) = t;
    let fields = |row: &dyn Fn(usize) -> String| FIELDS.iter().enumerate().map(|(i, n)| format!("{n}={}", row(i))).collect::<Vec<_>>().join(" ");
    let mean = fields(&|i| {
        if counts[i] == 0 { "-".into() } else { format!("{:.1}", ms(sums[i] / i128::from(counts[i]))) }
    });
    let worst_row = fields(&|i| worst.1[i].map_or("-".into(), |v| format!("{:.1}", ms(v))));
    tracing::info!(
        "why2 timeline ms from the present call (VICTORY_FRAME_WHY2=1), mean over {timed} frames: {mean} | worst frame \
         ({:.1} ms to the next present call): {worst_row} | frames with each: {}",
        ms(i128::from(worst.0)),
        counts.iter().map(u64::to_string).collect::<Vec<_>>().join("/"),
    );
    let n = passes.iter().sum::<u64>().max(1) as f64;
    let [looked, kept, same, records, same_records] = HEAP_SAME.each_ref().map(|v| v.swap(0, Relaxed));
    let share = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    let (enqueue_sum, enqueues, latest, earliest) = pass_enqueue;
    tracing::info!(
        "why2 heap pass per frame: {:.1} due slots read, {:.1} with the kept record and a host slot made from it \
         ({:.1}%); {:.1} found a host copy, {:.1} with a view kept, {:.1} of those resolved to the same copy and range \
         ({:.1}%); passes {} on title threads, {} on the heap worker, {} on other threads; \
         start {:.1} ms (longest {:.1}) and end {:.1} ms (longest {:.1}) after the previous present returned, \
         end {:.1} ms (longest {:.1}) before the next present call; submit worker idle inside the pass {:.1} ms \
         (longest {:.1}); the pass thread's next enqueue {:.1} ms from the present call (latest {:.1}, earliest {:.1}, \
         {enqueues} seen)",
        records as f64 / frames,
        same_records as f64 / frames,
        share(same_records, records),
        looked as f64 / frames,
        kept as f64 / frames,
        same as f64 / frames,
        share(same, kept),
        passes[0],
        passes[1],
        passes[2],
        pass_sums[0] as f64 / n / 1e6,
        pass_max[0] as f64 / 1e6,
        pass_sums[1] as f64 / n / 1e6,
        pass_max[1] as f64 / 1e6,
        pass_sums[2] as f64 / n / 1e6,
        pass_max[2] as f64 / 1e6,
        pass_idle.0 as f64 / n / 1e6,
        pass_idle.1 as f64 / 1e6,
        if enqueues == 0 { 0.0 } else { ms(enqueue_sum / i128::from(enqueues)) },
        ms(latest),
        ms(earliest),
    );
    let t = TABLES.each_ref().map(|v| v.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "why2 fresh tables per frame: {:.1} made; by cause (a table may have several, {:.1} did): constant buffer not read \
         in place {:.1} tables {:.0} slots, half views {:.1} tables {:.0} slots, an entry at or past the mirrored slots \
         {:.1} tables {:.0} slots",
        t[0], t[7], t[1], t[2], t[3], t[4], t[5], t[6],
    );
    let holds = HOLDERS
        .iter()
        .zip(&HOLDS)
        .map(|(name, h)| {
            let [w, longest, r] = h.each_ref().map(|v| v.swap(0, Relaxed));
            format!("{name}={:.2}/{:.2}/{:.2}", w as f64 / frames / 1e6, longest as f64 / 1e6, r as f64 / frames / 1e6)
        })
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!("why2 guards lock held ms per frame by holder, write sum/longest write/read sum: {holds}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_measured_from_its_present_call() {
        let mut s = Timeline { call: 100, ret: 160, before: 90, first_after: 170, ..Default::default() };
        s.exec_at_return = (7, 120);
        s.seen = vec![(6, 110), (7, 140), (8, 150)];
        s.landed = vec![(7, 155)];
        s.close(300);
        assert_eq!(s.frames, 1);
        assert_eq!(s.worst, (200, [Some(-10), Some(20), Some(40), Some(55), Some(60), Some(70)]));
        // Only what may still belong to a later frame is kept.
        assert_eq!(s.seen, [(7, 140), (8, 150)]);
    }

    #[test]
    fn a_frame_without_a_return_is_not_counted() {
        let mut s = Timeline { call: 100, ..Default::default() };
        s.close(200);
        assert_eq!(s.frames, 0);
    }
}
