//! Ordered present boundary for explicit stage 3. Submission owns the frame
//! inventory; the relay stamps the same tickets even after a present returns.
use super::{HostList, Step, join_ranges, touches};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::Instant;

type Range = (usize, usize);
const PENDING: u64 = u64::MAX;

pub(crate) fn cut() -> bool {
    static CUT: OnceLock<bool> = OnceLock::new();
    *CUT.get_or_init(|| {
        super::late_stage() == 3 && std::env::var_os("VICTORY_LATE_WAIT").is_some_and(|v| v == "3")
    })
}

pub(crate) fn why() -> bool {
    static WHY: OnceLock<bool> = OnceLock::new();
    *WHY.get_or_init(|| {
        (super::late_stage() == 2 || cut())
            && std::env::var_os("VICTORY_FRAME_END_WHY").is_some_and(|v| v == "1")
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Set {
    Exposure,
    Water,
    All,
}

fn selection() -> Set {
    static SET: OnceLock<Set> = OnceLock::new();
    *SET.get_or_init(|| match std::env::var("VICTORY_LATE_SET").as_deref() {
        Ok("exposure") => Set::Exposure,
        Ok("all") => Set::All,
        Ok("exposure+water") | Err(_) => Set::Water,
        Ok(_) => Set::All, // Unknown spelling: use the widest set.
    })
}

fn now() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Default)]
struct Inventory {
    measure: bool,
    ranges: Vec<Range>,
    tiny: HashSet<Range>,
    // Explicit texture-to-buffer and tiled texture copies. UAV declarations
    // describe possible writes; they do not specify how many bytes landed.
    counts: [u64; 4],       // tagged rows, total rows, tagged bytes, total bytes
    shader_bytes: [u64; 2], // matched/all declarations; includes repeated draws
}

impl Inventory {
    fn writer(&mut self, r: Range, select: bool, affected: &mut Vec<Range>) -> bool {
        if r.0 >= r.1 {
            return false;
        }
        if select {
            // Repeated draw declarations commonly name the same buffer. Only
            // edit the sorted union when this write extends it.
            let first = self.ranges.partition_point(|&(_, end)| end < r.0);
            if let Some(&(a, b)) = self.ranges.get(first)
                && a <= r.0
                && b >= r.1
            {
                if affected.last() != Some(&r) {
                    affected.push(r);
                }
                return true;
            }
            let past = first + self.ranges[first..].partition_point(|&(start, _)| start <= r.1);
            let joined = if first == past {
                r
            } else {
                (
                    r.0.min(self.ranges[first].0),
                    r.1.max(self.ranges[past - 1].1),
                )
            };
            self.ranges.splice(first..past, [joined]);
            affected.push(r);
            return true;
        }
        if !touches(&self.ranges, r.0, r.1) {
            return false;
        }
        let first = self.ranges.partition_point(|&(_, end)| end <= r.0);
        affected.extend(
            self.ranges[first..]
                .iter()
                .take_while(|&&(a, _)| a < r.1)
                .filter_map(|&(a, b)| {
                    let intersection = (a.max(r.0), b.min(r.1));
                    (intersection.0 < intersection.1).then_some(intersection)
                }),
        );
        true
    }

    fn count(&mut self, rows: u64, bytes: u64, tagged: bool) {
        if !self.measure {
            return;
        }
        self.counts[1] += rows;
        self.counts[3] += bytes;
        if tagged {
            self.counts[0] += rows;
            self.counts[2] += bytes;
        }
    }

    fn buffer_rows(
        &mut self,
        at: usize,
        [row, rows, pitch]: [usize; 3],
        selected: bool,
        affected: &mut Vec<Range>,
    ) -> bool {
        if rows == 0 || row == 0 {
            return false;
        }
        let end = at + (rows - 1) * pitch + row;
        if !selected && !touches(&self.ranges, at, end) {
            self.count(rows as u64, (rows * row) as u64, false);
            return false;
        }
        if rows == 1 || (selected && pitch == row) {
            let hit = self.writer((at, end), selected, affected);
            self.count(rows as u64, (rows * row) as u64, hit);
            return hit;
        }
        let mut matched = false;
        for y in 0..rows {
            let start = at + y * pitch;
            let hit = self.writer((start, start + row), selected, affected);
            self.count(1, row as u64, hit);
            matched |= hit;
        }
        matched
    }

    fn inspect(
        &mut self,
        list: &HostList,
        previous: &HashSet<Range>,
        set: Set,
        affected: &mut Vec<Range>,
    ) -> (bool, bool) {
        let d = list.deferred();
        let mut tag = false;
        for (_, at, shape, extent) in &d.backs {
            let selected = set == Set::All || super::feedback::footprint_kind(*extent) < 2;
            tag |= self.buffer_rows(*at, *shape, selected, affected);
        }
        for back in &d.texture_backs {
            let span = back.span();
            let hit = self.writer(span, set != Set::Exposure, affected);
            if self.measure {
                self.count(back.rows(), (span.1 - span.0) as u64, hit);
            }
            tag |= hit;
        }
        for step in &d.copies {
            if let Step::GpuWrite(_, at, bytes) = step {
                let r = (*at, at.saturating_add(*bytes));
                let tiny = matches!(*bytes, 4 | 20);
                if tiny {
                    self.tiny.insert(r);
                }
                let hit = self.writer(
                    r,
                    set == Set::All || (tiny && previous.contains(&r)),
                    affected,
                );
                if self.measure {
                    if hit {
                        self.shader_bytes[0] += *bytes as u64;
                    }
                    self.shader_bytes[1] += *bytes as u64;
                }
                tag |= hit;
            }
        }
        let mut pipe = false;
        for &(at, _, bytes) in &d.writes {
            pipe |= self.writer((at, at.saturating_add(bytes)), false, affected);
        }
        (tag || pipe, pipe)
    }
}

pub(crate) struct Ticket {
    number: u64,
    last_list: Option<usize>,
    has_pipe: bool,
    // Zero denotes shader merges; tiled copies then buffer copies are 1-based
    // in their actual CPU publication order.
    last_copy: usize,
    affected: Vec<Range>,
    fence: AtomicU64,
    published: AtomicU64,
    released: AtomicU64,
    map_intents: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

#[derive(Default)]
struct Frame {
    data: Inventory,
    producer: Option<Arc<Ticket>>,
    last: Option<Arc<Ticket>>,
    tail: [u64; 2],
    splits: u64,
    map_intents: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    current: Frame,
    previous_tiny: HashSet<Range>,
    in_flight: VecDeque<Arc<Ticket>>,
    reports: Vec<Report>,
}

struct Report {
    frame: Frame,
    start: u64,
    barrier: u64,
    returned: u64,
    waited: u64,
    fallback: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);
static MOVED: Condvar = Condvar::new();
fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn split(runs: &[&HostList]) -> Option<usize> {
    let s = state();
    let mut preview = Inventory {
        ranges: s.current.data.ranges.clone(),
        ..Default::default()
    };
    let previous = s.previous_tiny.clone();
    drop(s);
    let mut affected = Vec::new();
    let mut last = None;
    for (i, list) in runs.iter().enumerate() {
        affected.clear();
        if preview
            .inspect(list, &previous, selection(), &mut affected)
            .0
        {
            last = Some(i + 1);
        }
    }
    let n = last.filter(|&n| n < runs.len())?;
    // Splitting moves prefix publication ahead of suffix CPU preparation.
    // Preserve a single batch when that preparation could observe the move.
    let mut pipe = Vec::new();
    for list in &runs[..n] {
        pipe.extend(
            list.deferred()
                .writes
                .iter()
                .map(|&(at, _, len)| (at, at.saturating_add(len))),
        );
    }
    if !pipe.is_empty() {
        join_ranges(&mut pipe, []);
        for list in &runs[n..] {
            let d = list.deferred();
            if d.texture_backs.iter().any(|b| {
                let (a, z) = b.span();
                touches(&pipe, a, z)
            }) || d.backs.iter().any(|(_, a, [row, rows, pitch], _)| {
                touches(
                    &pipe,
                    *a,
                    a.saturating_add(rows.saturating_sub(1) * pitch)
                        .saturating_add(*row),
                )
            }) {
                return None;
            }
        }
        join_ranges(&mut preview.ranges, pipe);
    }
    super::blocked_by(&[], &preview.ranges, &runs[n..], false)
        .is_none()
        .then_some(n)
}

pub(super) fn register(number: u64, runs: &[&HostList]) -> Option<Arc<Ticket>> {
    let mut s = state();
    let State {
        current: frame,
        previous_tiny,
        ..
    } = &mut *s;
    frame.data.measure = why();
    let mut affected = Vec::new();
    let mut last = None;
    let mut has_pipe = false;
    for (i, list) in runs.iter().enumerate() {
        let (tag, pipe) = frame
            .data
            .inspect(list, previous_tiny, selection(), &mut affected);
        if tag {
            last = Some(i);
        }
        has_pipe |= pipe;
    }
    if last.is_none() && !why() {
        return None;
    }
    // Pipe writes run after *all* lists' read-backs. A pipe write on an
    // earlier list can overwrite a destination first tagged on a later list.
    if last.is_some() && !has_pipe {
        has_pipe = runs.iter().any(|list| {
            list.deferred()
                .writes
                .iter()
                .any(|&(at, _, len)| touches(&frame.data.ranges, at, at.saturating_add(len)))
        });
    }
    if affected.len() > 1 {
        join_ranges(&mut affected, []);
    }
    let last_copy = last.map_or(0, |i| {
        let d = runs[i].deferred();
        let textures = d.texture_backs.iter().map(|b| b.span());
        let buffers = d.backs.iter().map(|(_, at, [row, rows, pitch], _)| {
            (
                *at,
                at.saturating_add(rows.saturating_sub(1) * pitch)
                    .saturating_add(*row),
            )
        });
        textures
            .chain(buffers)
            .enumerate()
            .filter_map(|(op, (a, b))| touches(&affected, a, b).then_some(op + 1))
            .last()
            .unwrap_or(0)
    });
    let t = Arc::new(Ticket {
        number,
        last_list: last,
        has_pipe,
        last_copy,
        affected,
        fence: PENDING.into(),
        published: PENDING.into(),
        released: PENDING.into(),
        map_intents: frame.map_intents.clone(),
        failed: frame.failed.clone(),
    });
    if let Some(i) = last {
        frame.producer = Some(t.clone());
        frame.tail = [0, (runs.len() - i - 1) as u64];
    } else if frame.producer.is_some() {
        frame.tail[0] += 1;
        frame.tail[1] += runs.len() as u64;
    }
    if why() {
        frame.last = Some(t.clone());
        s.in_flight.push_back(t.clone());
    }
    Some(t)
}

/// Entries without a flight are ordered writes/signals belonging to the
/// preceding flight. Return the count up to the next flight after a producer.
pub(super) fn landing_prefix(ready: impl Iterator<Item = Option<bool>>) -> usize {
    let mut tagged = false;
    let mut count = 0;
    for entry in ready {
        if let Some(producer) = entry {
            if tagged {
                break;
            }
            tagged = producer;
        }
        count += 1;
    }
    count
}

impl Ticket {
    pub(super) fn tagged(&self) -> bool {
        self.last_list.is_some()
    }

    pub(crate) fn number(&self) -> u64 {
        self.number
    }

    // Observe tagged fences independently of the relay: it can spend many ms
    // copying older flights. A relay observation is also a valid upper bound.
    pub(super) fn observe(t: &Arc<Self>, value: u64) {
        if t.last_list.is_none() || !why() {
            return;
        }
        type Job = (Arc<Ticket>, u64);
        static WORK: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
        if let Some(tx) = WORK.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("victory-frame-fence".into())
                .spawn(move || {
                    crate::cores::worker("victory-frame-fence");
                    let Some(host) = super::host() else { return };
                    for (t, value) in rx {
                        // SAFETY: a null event makes this call wait on the host fence.
                        if unsafe {
                            host.fence
                                .SetEventOnCompletion(value, super::HANDLE::default())
                        }
                        .is_ok()
                        {
                            t.fence.fetch_min(now(), Ordering::Relaxed);
                        }
                    }
                })
                .ok()
                .map(|_| tx)
        }) {
            let _ = tx.send((t.clone(), value));
        }
    }

    pub(super) fn fenced(&self) {
        if why() && self.last_list.is_some() {
            self.fence.fetch_min(now(), Ordering::Relaxed);
        }
    }

    pub(super) fn copy_done(&self, i: usize, op: usize, failure: Option<Range>) {
        if failure.is_some_and(|(a, b)| touches(&self.affected, a, b)) {
            self.failed.store(true, Ordering::Release);
        }
        if self.last_list == Some(i) && op == self.last_copy && !self.has_pipe {
            self.publish();
        }
    }

    pub(crate) fn wait(&self) -> bool {
        if self.published.load(Ordering::Acquire) == PENDING {
            let start = why().then(now);
            let _s = MOVED
                .wait_while(state(), |_| {
                    self.published.load(Ordering::Acquire) == PENDING
                })
                .unwrap_or_else(|e| e.into_inner());
            if let Some(start) = start {
                waited(now() - start);
            }
        }
        !self.failed.load(Ordering::Acquire)
    }

    fn publish(&self) {
        let _s = state(); // Same lock as the wait predicate. No lost wakeup.
        self.published
            .store(if why() { now() } else { 0 }, Ordering::Release);
        MOVED.notify_all();
    }

    pub(super) fn after_writes(&self) {
        if self.has_pipe {
            self.publish();
        }
    }
}

pub(super) fn released(through: u64) {
    let mut s = state();
    let stamp = if why() { now() } else { 0 };
    while s.in_flight.front().is_some_and(|t| t.number <= through) {
        let t = s.in_flight.pop_front().unwrap();
        t.released.store(stamp, Ordering::Release);
    }
}

pub(crate) fn map_intent(from: usize, to: usize) {
    let s = state();
    if let Some(t) =
        s.in_flight.iter().rev().find(|t| {
            t.published.load(Ordering::Acquire) == PENDING && touches(&t.affected, from, to)
        })
    {
        t.map_intents.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn did_split() {
    if why() {
        state().current.splits += 1;
    }
}

pub(crate) fn last_producer() -> Option<Arc<Ticket>> {
    state().current.producer.clone()
}

pub(crate) struct Boundary {
    frame: Frame,
    start: u64,
}

thread_local! { static WAIT_US: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

pub(super) fn waited(us: u64) {
    WAIT_US.set(WAIT_US.get() + us);
}

pub(crate) fn begin() -> Option<u64> {
    why().then(|| {
        WAIT_US.set(0);
        now()
    })
}

pub(crate) fn boundary(start: Option<u64>) -> Option<Boundary> {
    if !cut() && start.is_none() {
        return None;
    }
    let host = super::host()?;
    // A submit that obtained its number before the boundary must finish
    // registering and enqueuing its flight before we freeze this frame.
    let _submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = state();
    let mut frame = std::mem::take(&mut s.current);
    s.previous_tiny = std::mem::take(&mut frame.data.tiny);
    frame.data.ranges = Vec::new(); // Only aggregate counts survive the boundary.
    if frame.last.is_none() {
        frame.last = s.in_flight.back().cloned();
    }
    Some(Boundary {
        frame,
        start: start.unwrap_or(0),
    })
}

impl Boundary {
    pub(crate) fn wait_tagged(&self) -> bool {
        let Some(t) = &self.frame.producer else {
            return false;
        };
        t.wait()
    }

    pub(crate) fn report(self, barrier_end: u64, fallback: bool) {
        if why() {
            state().reports.push(Report {
                frame: self.frame,
                start: self.start,
                barrier: barrier_end,
                returned: now(),
                waited: WAIT_US.get(),
                fallback,
            });
        }
    }
}
pub(crate) fn end_wait() -> u64 {
    if why() { now() } else { 0 }
}

pub(super) fn log_interval() {
    if !why() {
        return;
    }
    let mut s = state();
    let mut sums = [0.0; 14];
    let (mut frames, mut missing, mut failed, mut fallbacks) = (0, 0, 0, 0);
    let (mut maps, mut splits, mut wait_ms) = (0u64, 0u64, 0.0);
    s.reports.retain(
        |Report {
             frame: f,
             start,
             barrier,
             returned,
             waited,
             fallback,
         }| {
            let release = f
                .last
                .as_ref()
                .map_or(*start, |t| t.released.load(Ordering::Acquire));
            if release == PENDING {
                return true;
            } // Stage 3 can return first.
            frames += 1;
            maps += f.map_intents.load(Ordering::Relaxed);
            splits += f.splits;
            wait_ms += *waited as f64 / 1000.0;
            if *fallback {
                fallbacks += 1;
            }
            let ms = |t: u64| t.saturating_sub(*start) as f64 / 1000.0;
            let full = ms(release);
            sums[0] += ms(*barrier);
            sums[1] += full;
            sums[2] += ms(*returned);
            if f.failed.load(Ordering::Acquire) {
                failed += 1;
            } else if let Some(t) = &f.producer {
                let published = ms(t.published.load(Ordering::Acquire));
                sums[3] += ms(t.fence.load(Ordering::Relaxed));
                sums[4] += published;
                if !*fallback {
                    sums[5] += (full - published).max(0.0);
                }
            } else {
                missing += 1;
            }
            for (sum, n) in sums[6..].iter_mut().zip(
                f.tail
                    .into_iter()
                    .chain(f.data.counts)
                    .chain(f.data.shader_bytes),
            ) {
                *sum += n as f64;
            }
            false
        },
    );
    drop(s);
    if frames == 0 {
        return;
    }
    let valid = frames - missing - failed;
    let valid_avg = |v: f64| {
        if valid == 0 {
            f64::NAN
        } else {
            v / valid as f64
        }
    };
    let avg = |v: f64| v / frames as f64;
    tracing::info!(
        "frame-end per frame ({frames} frames, {missing} without tag, {failed} copy failures, {fallbacks} full fallbacks): GPU/publication wait {:.2} ms, boundary wall {:.2} ms, present call {:.2} ms, full release {:.2} ms; tagged frames fence observed {:.2} ms, published {:.2} ms; recoverable full minus published {:.2} ms/frame (fallback counts zero); after producer {:.2} submissions / {:.2} lists; {:.2} submission splits; explicit read-back rows tagged/all {:.2}/{:.2}, MB tagged/all {:.3}/{:.3}; shader-declared MB matched/all {:.3}/{:.3} (repeats included); overlapping Map read intents before flight publication {:.2}/frame (persistent pointer reads unknown)",
        avg(wait_ms),
        avg(sums[0]),
        avg(sums[2]),
        avg(sums[1]),
        valid_avg(sums[3]),
        valid_avg(sums[4]),
        avg(sums[5]),
        avg(sums[6]),
        avg(sums[7]),
        avg(splits as f64),
        avg(sums[8]),
        avg(sums[9]),
        avg(sums[10]) / 1e6,
        avg(sums[11]) / 1e6,
        avg(sums[12]) / 1e6,
        avg(sums[13]) / 1e6,
        avg(maps as f64)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_writes_to_part_of_a_tag_are_producers() {
        let mut inv = Inventory::default();
        let mut hits = Vec::new();
        assert!(inv.writer((16, 36), true, &mut hits));
        assert!(!inv.writer((36, 40), false, &mut hits));
        assert!(inv.writer((32, 40), false, &mut hits));
        assert_eq!(hits, [(16, 36), (32, 36)]);
    }
    #[test]
    fn ready_tail_stays_queued_but_pipe_writes_follow_the_producer() {
        let items = [Some(false), Some(true), None, None, Some(false)];
        assert_eq!(landing_prefix(items.into_iter()), 4);
        assert_eq!(landing_prefix([None, Some(false), None].into_iter()), 3);
    }

    #[test]
    fn pitched_rows_count_payload_and_exclude_padding_from_selection() {
        let mut inv = Inventory {
            measure: true,
            ..Default::default()
        };
        let mut hits = Vec::new();
        assert!(inv.buffer_rows(0x100, [4, 2, 16], true, &mut hits));
        assert!(!inv.buffer_rows(0x108, [4, 2, 16], false, &mut hits));
        assert!(inv.buffer_rows(0x110, [4, 2, 16], false, &mut hits));
        assert_eq!(inv.counts, [3, 6, 12, 24]);
    }
    #[test]
    fn touching_tags_join_without_tagging_row_padding() {
        let mut inv = Inventory::default();
        let mut hits = Vec::new();
        for r in [(32, 36), (8, 12), (12, 16), (10, 14)] {
            assert!(inv.writer(r, true, &mut hits));
        }
        assert_eq!(inv.ranges, [(8, 16), (32, 36)]);
        assert!(!inv.writer((16, 32), false, &mut hits));
        assert!(inv.writer((12, 34), false, &mut hits));
        assert!(hits.ends_with(&[(12, 16), (32, 34)]));
    }

    #[test]
    fn fence_completion_does_not_publish_and_pipe_overwrites_extend_the_wait() {
        let t = Ticket {
            number: 1,
            last_list: Some(2),
            last_copy: 1,
            has_pipe: true,
            affected: vec![(8, 12)],
            fence: 1.into(),
            published: PENDING.into(),
            released: PENDING.into(),
            map_intents: Default::default(),
            failed: Default::default(),
        };
        t.copy_done(2, 1, None);
        assert_eq!(t.published.load(Ordering::Acquire), PENDING);
        t.after_writes();
        assert_ne!(t.published.load(Ordering::Acquire), PENDING);
        assert_eq!(t.released.load(Ordering::Acquire), PENDING);
    }

    #[test]
    fn wait_requires_last_copy_and_a_failed_target_requests_full_settle() {
        let t = Ticket {
            number: 1,
            last_list: Some(2),
            last_copy: 1,
            has_pipe: false,
            affected: vec![(8, 12)],
            fence: 1.into(),
            published: PENDING.into(),
            released: PENDING.into(),
            map_intents: Default::default(),
            failed: Default::default(),
        };
        t.copy_done(1, 1, Some((12, 16)));
        assert!(!t.failed.load(Ordering::Acquire));
        t.copy_done(2, 0, None);
        assert_eq!(t.published.load(Ordering::Acquire), PENDING);
        t.copy_done(2, 1, Some((10, 14)));
        assert!(!t.wait());
        assert_eq!(t.released.load(Ordering::Acquire), PENDING);
    }

    #[test]
    fn empty_frame_requests_full_wait() {
        assert!(
            !Boundary {
                frame: Frame::default(),
                start: 0
            }
            .wait_tagged()
        );
    }
}
