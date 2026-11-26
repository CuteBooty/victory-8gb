//! A texture-to-buffer copy lands in ordinary title memory. Its destination is
//! independent of both the shader-write declarations and the tiled textures.
use super::{
    FINISHED, HostList, Late, WHY_READ, WHY_TEXTURES, blocked_by, join_ranges, line, relay_wait,
    touches,
};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        super::late_free_present()
            && super::relay()
            && std::env::var_os("VICTORY_FRAME_FEEDBACK").is_none_or(|v| v != "0")
    })
}

/// Whether the present waits only for the flights with feedback targets
/// (`VICTORY_FRAME_FEEDBACK=2`), instead of the `VICTORY_BACK_SETTLE` policy.
/// Off by default: a flight whose only result is a small shader-written
/// buffer has no target, and a title that keeps a mapped pointer across
/// frames could read the old value after the present returned. An explicit legacy present policy
/// (including its byte threshold) wins.
pub(crate) fn at_present() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        enabled()
            && present_policy(
                std::env::var_os("VICTORY_FRAME_FEEDBACK").as_deref(),
                std::env::var_os("VICTORY_BACK_SETTLE").is_some()
                    || std::env::var_os("VICTORY_BACK_SETTLE_BYTES").is_some(),
            )
    })
}

fn present_policy(feedback: Option<&std::ffi::OsStr>, legacy: bool) -> bool {
    feedback.is_some_and(|v| v == "2") && !legacy
}

#[derive(Default)]
pub(super) struct Targets {
    pub(super) number: u64,
    // Sorted, joined, half-open. Row padding is never copied into title memory.
    readbacks: [Vec<(usize, usize)>; 3],
    // 1x1 and 5x1 footprints, other transfers, selected textures.
    counts: [u64; 4],
    bytes: u64,
}

impl Targets {
    fn add_rows(&mut self, at: usize, [row, rows, pitch]: [usize; 3], extent: [u32; 3]) {
        if rows == 0 || row == 0 {
            return;
        }
        // The destination footprint was checked against its backing at recording.
        let kind = footprint_kind(extent);
        let ranges = &mut self.readbacks[kind];
        if pitch <= row || rows == 1 {
            ranges.push((at, at + (rows - 1) * pitch + row));
        } else {
            ranges.extend((0..rows).map(|i| {
                let start = at + i * pitch;
                (start, start + row)
            }));
        }
        self.counts[kind] += 1;
        self.bytes += (row * rows) as u64;
    }

    pub(super) fn from_lists(
        number: u64,
        lists: &[crate::list::Detached],
        order: &[usize],
        textures: &[(usize, usize)],
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let mut targets = Self {
            number,
            ..Self::default()
        };
        for &i in order {
            for (_, at, shape, extent) in &lists[i].host().deferred().backs {
                targets.add_rows(*at, *shape, *extent);
            }
        }
        for ranges in &mut targets.readbacks {
            if ranges.len() > 1 {
                join_ranges(ranges, []);
            }
        }
        if !textures.is_empty() && super::texture_back_settle() {
            targets.counts[3] = textures.len() as u64;
            targets.bytes += textures
                .iter()
                .map(|(from, to)| (to - from) as u64)
                .sum::<u64>();
        }
        (targets.bytes != 0).then_some(targets)
    }
}

#[derive(Default)]
struct Due {
    number: u64,
    counts: [u64; 4],
    bytes: u64,
}

fn due<'a>(
    targets: impl Iterator<Item = &'a Targets>,
    upto: u64,
    wanted: impl Fn(&Targets) -> bool,
) -> Due {
    let mut due = Due::default();
    for t in targets.filter(|t| t.number <= upto && wanted(t)) {
        due.number = due.number.max(t.number);
        for (count, n) in due.counts.iter_mut().zip(t.counts) {
            *count += n;
        }
        due.bytes += t.bytes;
    }
    due
}

struct Stats {
    waits: AtomicU64,
    us: AtomicU64,
    counts: [AtomicU64; 4],
    bytes: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Self {
            waits: AtomicU64::new(0),
            us: AtomicU64::new(0),
            counts: [const { AtomicU64::new(0) }; 4],
            bytes: AtomicU64::new(0),
        }
    }

    fn wait(&self, due: Due, why: usize) {
        if due.number == 0 {
            return;
        }
        let us = relay_wait(why, |_| FINISHED.load(Ordering::Acquire) >= due.number);
        if us != 0 {
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.us.fetch_add(us, Ordering::Relaxed);
            self.bytes.fetch_add(due.bytes, Ordering::Relaxed);
            for (counter, n) in self.counts.iter().zip(due.counts) {
                counter.fetch_add(n, Ordering::Relaxed);
            }
        }
    }
}

static PREPARE: Stats = Stats::new();
static VALUE: Stats = Stats::new();
static MAP: Stats = Stats::new();
static PRESENT: Stats = Stats::new();
static PREPARE_HITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static MAP_HITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

static LANDED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LANDED_BYTES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LATE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static FAILED: AtomicU64 = AtomicU64::new(0);

pub(super) fn readback_result(extent: [u32; 3], bytes: usize, mapped: bool, submitted_at: u64) {
    if !mapped {
        FAILED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let kind = footprint_kind(extent);
    LANDED[kind].fetch_add(1, Ordering::Relaxed);
    LANDED_BYTES[kind].fetch_add(bytes as u64, Ordering::Relaxed);
    if submitted_at != 0 && crate::queue::presents_done() >= submitted_at {
        LATE[kind].fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn footprint_kind(extent: [u32; 3]) -> usize {
    match extent {
        [1, 1, 1] => 0,
        [5, 1, 1] => 1,
        _ => 2,
    }
}

pub(super) fn before_prepare(runs: &[&HostList]) {
    // Do not hold the relay's lock while walking the lists' preparation steps.
    let pending: Vec<_> = line()
        .feedback
        .iter()
        .filter(|t| t.readbacks.iter().any(|r| !r.is_empty()))
        .cloned()
        .collect();
    let due = due(pending.iter().map(AsRef::as_ref), u64::MAX, |t| {
        let mut dependency = false;
        for (ranges, hits) in t.readbacks.iter().zip(&PREPARE_HITS) {
            if let Some(why) = blocked_by(&[], ranges, runs, false) {
                dependency = true;
                if why == WHY_READ {
                    hits.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        dependency
    });
    PREPARE.wait(due, WHY_READ);
}

/// A native reader can keep its pointer across frames. Cover that case at
/// the boundary, after the queue released runnable work and before the
/// present's own blit. No arbitrary UAV declaration qualifies a flight.
pub(crate) fn before_present(upto: u64) {
    if at_present() {
        let due = due(line().feedback.iter().map(AsRef::as_ref), upto, |_| true);
        PRESENT.wait(due, WHY_TEXTURES);
    }
}

/// A queue can test the old bytes before it asks for a predicted pipe write.
pub(crate) fn before_value(wait: &crate::list::ValueWait) {
    if enabled() {
        let (from, bytes) = wait.place();
        let due = due(line().feedback.iter().map(AsRef::as_ref), u64::MAX, |t| {
            t.readbacks
                .iter()
                .any(|ranges| touches(ranges, from, from.saturating_add(bytes)))
        });
        VALUE.wait(due, WHY_READ);
    }
}

static MAP_SHADER: AtomicU64 = AtomicU64::new(0);
static MAP_TEXTURE: AtomicU64 = AtomicU64::new(0);
static MAP_PIPE: AtomicU64 = AtomicU64::new(0);
static MAP_READ_BYTES: AtomicU64 = AtomicU64::new(0);
static MAP_DRAIN_US: AtomicU64 = AtomicU64::new(0);

// A resolved query uses the same ordered writes as a bottom-of-pipe value.
fn pipe_writer(items: &[Late], from: usize, to: usize) -> Option<u64> {
    let mut matched = false;
    for item in items.iter().rev() {
        match item {
            Late::Write(at, _, n) if *at < to && from < at.saturating_add(*n) => matched = true,
            Late::Run(f) if matched => return Some(f.number),
            _ => {}
        }
    }
    None
}

/// A buffer Map with read intent can precede present. First publish the
/// runnable prefix on the async submitter, then snapshot the last overlapping
/// writer. A later submission cannot extend the wait.
pub(crate) fn before_map(from: usize, to: usize) {
    if from >= to || !enabled() {
        return;
    }
    if crate::queue::behind() {
        let start = std::time::Instant::now();
        crate::queue::before_sync();
        MAP_DRAIN_US.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
    let line = line();
    let mut due = due(line.feedback.iter().map(AsRef::as_ref), u64::MAX, |t| {
        let mut overlap = false;
        for (ranges, hits) in t.readbacks.iter().zip(&MAP_HITS) {
            if touches(ranges, from, to) {
                hits.fetch_add(1, Ordering::Relaxed);
                overlap = true;
            }
        }
        overlap
    });
    let shader = touches(&line.written, from, to);
    let texture = touches(&line.texture_ranges, from, to);
    if shader || texture {
        // The union includes the batch out of the queue in relay_land. If
        // no queued flight overlaps, that batch owns the dependency.
        let mark = line
            .line
            .iter()
            .rev()
            .find_map(|item| {
                let Late::Run(f) = item else { return None };
                (f.texture_ranges.iter().any(|&(a, b)| a < to && from < b)
                    || f.back
                        .iter()
                        .any(|(_, ranges)| ranges.iter().any(|&(a, b)| a < to && from < b)))
                .then_some(f.number)
            })
            .unwrap_or(line.landing_through);
        due.number = due.number.max(mark);
        MAP_SHADER.fetch_add(u64::from(shader), Ordering::Relaxed);
        MAP_TEXTURE.fetch_add(u64::from(texture), Ordering::Relaxed);
    }
    let pipe = pipe_writer(&line.line, from, to).or_else(|| {
        line.in_hand
            .iter()
            .any(|&(at, _, n)| at < to && from < at.saturating_add(n))
            .then_some(line.landing_through)
    });
    if let Some(n) = pipe {
        due.number = due.number.max(n);
        MAP_PIPE.fetch_add(1, Ordering::Relaxed);
    }
    drop(line);
    if due.number != 0 {
        MAP_READ_BYTES.fetch_add((to - from) as u64, Ordering::Relaxed);
        MAP.wait(due, WHY_READ);
    }
}

pub(super) fn log_interval() {
    if !enabled() {
        return;
    }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::queue::presents_done();
    let frames = now.saturating_sub(LAST.swap(now, Ordering::Relaxed)).max(1) as f64;
    let each = |v: &AtomicU64| v.swap(0, Ordering::Relaxed) as f64 / frames;
    for (kind, stats) in [
        ("prepare", &PREPARE),
        ("queue value", &VALUE),
        ("buffer Map", &MAP),
        ("present", &PRESENT),
    ] {
        let [one, five, other, textures] = stats.counts.each_ref().map(each);
        tracing::info!(target: "victory_gpu::host",
            "frame feedback per frame ({kind}): {:.2} waits, {:.2} ms; targets on matching flights: \
             {one:.2} 1x1, {five:.2} 5x1, {other:.2} other read-backs, \
             {textures:.2} selected textures, {:.0} bytes in all",
            each(&stats.waits), each(&stats.us) / 1000.0, each(&stats.bytes)
        );
    }
    tracing::info!(target: "victory_gpu::host",
        "buffer Map read ranges per frame: {:.2} overlapping shader writes, {:.2} overlapping selected textures, {:.2} overlapping pipe writes, \
         {:.0} requested bytes across overlaps, {:.2} ms releasing queued work",
        each(&MAP_SHADER), each(&MAP_TEXTURE), each(&MAP_PIPE), each(&MAP_READ_BYTES), each(&MAP_DRAIN_US) / 1000.0
    );
    let landed = LANDED.each_ref().map(each);
    let bytes = LANDED_BYTES.each_ref().map(each);
    let late = LATE.each_ref().map(each);
    let prep = PREPARE_HITS.each_ref().map(each);
    let map = MAP_HITS.each_ref().map(each);
    tracing::info!(target: "victory_gpu::host",
        "texture-to-buffer read-backs per frame (1x1 / 5x1 / other): landed {:.2}/{:.2}/{:.2}, bytes {:.0}/{:.0}/{:.0}, \
         after the following present returned {:.2}/{:.2}/{:.2}, failed maps {:.2}; overlapping flight ranges \
         at preparation {:.2}/{:.2}/{:.2}, at buffer Map {:.2}/{:.2}/{:.2}",
        landed[0], landed[1], landed[2], bytes[0], bytes[1], bytes[2], late[0], late[1], late[2], each(&FAILED),
        prep[0], prep[1], prep[2], map[0], map[1], map[2]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The present keeps the `VICTORY_BACK_SETTLE` policy, which waits for
    /// small shader-written buffers too, unless asked for with `=2` and no
    /// legacy setting.
    #[test]
    fn the_present_keeps_the_back_settle_policy_by_default() {
        use std::ffi::OsStr;
        assert!(!present_policy(None, false));
        assert!(!present_policy(Some(OsStr::new("1")), false));
        assert!(present_policy(Some(OsStr::new("2")), false));
        assert!(!present_policy(Some(OsStr::new("2")), true));
    }

    #[test]
    fn only_written_row_bytes_overlap() {
        let mut t = Targets::default();
        t.add_rows(100, [4, 2, 16], [1, 2, 1]);
        let ranges = &t.readbacks[2];
        assert!(touches(ranges, 103, 104));
        assert!(!touches(ranges, 104, 116));
        assert!(touches(ranges, 116, 117));
        assert!(!touches(ranges, 120, 121));
        assert_eq!(t.bytes, 8);
    }

    #[test]
    fn consumer_wait_ends_at_its_last_writer_and_present_at_its_cutoff() {
        let mut targets = [2, 5, 7, 9].map(|number| Targets {
            number,
            ..Targets::default()
        });
        for (t, at) in targets.iter_mut().zip([100, 100, 120, 100]) {
            t.add_rows(at, [20, 1, 256], [5, 1, 1]);
        }
        let read = due(targets.iter(), 8, |t| touches(&t.readbacks[1], 119, 120));
        assert_eq!(
            (read.number, read.counts, read.bytes),
            (5, [0, 2, 0, 0], 40)
        );
        assert_eq!(due(targets.iter(), 8, |_| true).number, 7);
        assert_eq!(due(targets.iter(), 1, |_| true).number, 0);
    }

    #[test]
    fn a_query_map_waits_for_the_flight_that_owns_the_write() {
        let flight = |number| {
            Late::Run(super::super::Flight {
                frame_end: None,
                number,
                fence: number,
                lists: Vec::new(),
                order: Vec::new(),
                card: None,
                back: Vec::new(),
                texture_ranges: Vec::new(),
                texture_start: 0,
                settle: false,
            })
        };
        let write = |at| Late::Write(at, crate::queue::Written::Value(1), 8);
        let items = [flight(2), write(100), flight(4), write(108), flight(6)];
        assert_eq!(pipe_writer(&items, 107, 108), Some(2));
        assert_eq!(pipe_writer(&items, 107, 109), Some(4));
        assert_eq!(pipe_writer(&items, 116, 120), None);
    }
}
