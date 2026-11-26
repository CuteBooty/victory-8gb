//! Observe publication without moving it. Only shape, generation and timing
//! counts leave this module. A native load from a retained pointer is invisible.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

type Range = (usize, usize);
// feedback::footprint_kind returns 2 for unrelated buffer copies.
pub(super) const TILED: usize = 3;
const MAX_TARGETS: usize = 256;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BACK_LATCH").is_some_and(|v| v == "1"))
}

#[derive(Default)]
struct Counts {
    frames: u64,
    // Per destination requested in the frame: 0, 1, 2, 3+, unknown.
    ages: [[u64; 5]; 3],
    // For frames requesting both tiny shapes: current, uniformly late,
    // mixed, unknown (no successful write, or a copy in progress).
    pair: [u64; 4],
    uniform_transitions: u64,
    changed_age: u64,
    pair_us: [u64; 3], // Both preceding-frame shapes: uniform, mixed, unknown.
    // Copy windows wholly outside / inside present, or crossing an edge.
    phases: [[u64; 3]; 3],
    bytes: [u64; 3],
    late_outside: [u64; 3],
    // Overlapping Map intents after normal synchronization: current, stale, unknown.
    maps: [u64; 3],
    misses: u64,
    failed: u64,
    unstamped: u64,
}

struct Target {
    kind: usize,
    range: Range,
    submitted: u64,
    visible: u64,
    writing: usize,
    observe: bool,
}

#[derive(Default)]
struct Probe {
    targets: Vec<Target>,
    counts: Counts,
    previous: Option<u64>,
    phase: u64,
    presents: usize,
    overflow_frame: u64,
    observed_pair: bool,
    sampled: Option<(Instant, usize)>,
}

static PROBE: LazyLock<Mutex<Probe>> = LazyLock::new(Default::default);

fn probe() -> MutexGuard<'static, Probe> {
    PROBE.lock().unwrap_or_else(|e| e.into_inner())
}

impl Probe {
    fn request(&mut self, kind: usize, range: Range, frame: u64) {
        if range.0 >= range.1 {
            return;
        }
        if let Some(t) = self
            .targets
            .iter_mut()
            .find(|t| t.kind == kind && t.range == range)
        {
            t.submitted = frame;
        } else if self.targets.len() < MAX_TARGETS {
            self.targets.push(Target {
                kind,
                range,
                submitted: frame,
                visible: 0,
                writing: 0,
                observe: false,
            });
        } else {
            self.counts.misses += 1;
            if kind < 2 {
                self.overflow_frame = frame;
            }
        }
    }

    fn boundary(&mut self, frame: u64) {
        self.counts.frames += 1;
        let mut shapes = 0u8;
        let mut first = None;
        let mut mixed = false;
        let mut unknown = self.overflow_frame == frame;
        for t in &mut self.targets {
            t.observe = t.kind < 2 && t.submitted == frame;
            if t.submitted != frame {
                continue;
            }
            let age = if t.visible == 0 || t.writing != 0 {
                None
            } else {
                frame.checked_sub(t.visible)
            };
            self.counts.ages[t.kind.min(2)][age.map_or(4, |a| a.min(3) as usize)] += 1;
            if t.kind < 2 {
                shapes |= 1 << t.kind;
                if let Some(a) = age {
                    mixed |= *first.get_or_insert(a) != a;
                } else {
                    unknown = true;
                }
            }
        }
        let mut uniform = None;
        self.observed_pair = shapes == 3 && self.overflow_frame != frame;
        if shapes == 3 || self.overflow_frame == frame {
            let bin = if unknown {
                3
            } else if mixed {
                2
            } else {
                usize::from(first != Some(0))
            };
            self.counts.pair[bin] += 1;
            if bin < 2 {
                uniform = first;
            }
        }
        if let (Some(old), Some(new)) = (self.previous, uniform) {
            self.counts.uniform_transitions += 1;
            self.counts.changed_age += u64::from(old != new);
        }
        self.previous = uniform;
        // Limit history for addresses recycled over a long session. Missing
        // history is unknown. Never evict a target with an active copy window.
        self.targets
            .retain(|t| t.writing > 0 || frame.saturating_sub(t.submitted) < 8);
    }

    fn map_ready(&mut self, range: Range) {
        for t in &self.targets {
            if t.range.0 < range.1 && range.0 < t.range.1 {
                let bin = if t.visible == 0 || t.writing > 0 {
                    2
                } else {
                    usize::from(t.visible != t.submitted)
                };
                self.counts.maps[bin] += 1;
            }
        }
    }

    // Follow the same destinations until the next present begins, even if
    // submissions meanwhile select a different staging slot.
    fn sample(&mut self, now: Instant) {
        if let Some((since, bin)) = self.sampled.take() {
            self.counts.pair_us[bin] += (now - since).as_micros() as u64;
        }
        if self.presents != 0 || !self.observed_pair {
            return;
        }
        let mut first = None;
        let mut bin = 0;
        for t in self.targets.iter().filter(|t| t.observe) {
            if t.writing != 0 || t.visible == 0 {
                bin = 2;
                break;
            }
            if *first.get_or_insert(t.visible) != t.visible {
                bin = 1;
            }
        }
        self.sampled = Some((now, bin));
    }
}

pub(super) fn register(list: &mut super::Deferred) {
    let mut p = probe();
    let frame = crate::queue::presents_done() + 1;
    list.submitted_at = frame;
    for (_, at, [row, _, _], extent) in &list.backs {
        let kind = super::feedback::footprint_kind(*extent);
        if kind < 2 {
            p.request(kind, (*at, at.saturating_add(*row)), frame);
        }
    }
    for back in &list.texture_backs {
        p.request(TILED, back.span(), frame);
    }
}

pub(crate) fn begin_present() {
    if enabled() {
        let mut p = probe();
        p.presents += 1;
        p.phase += 1;
        p.sample(Instant::now());
    }
}

pub(crate) fn end_present(done: &AtomicU64) {
    let mut p = probe();
    let frame = done.fetch_add(1, Ordering::Release) + 1;
    p.boundary(frame);
    p.presents -= 1;
    p.phase += 1;
    p.sample(Instant::now());
}

pub(crate) fn map_ready(range: Range) {
    if enabled() {
        probe().map_ready(range);
    }
}

pub(super) struct Window {
    kind: usize,
    range: Range,
    submitted: u64,
    phase: u64,
    tracked: bool,
}

pub(super) fn begin_copy(kind: usize, range: Range, submitted: u64) -> Option<Window> {
    if kind == 2 || !enabled() {
        return None;
    }
    let mut p = probe();
    let tracked = if let Some(t) = p
        .targets
        .iter_mut()
        .find(|t| t.kind == kind && t.range == range)
    {
        t.writing += 1;
        true
    } else {
        false
    };
    if kind < 2 {
        p.sample(Instant::now());
    }
    Some(Window {
        kind,
        range,
        submitted,
        phase: p.phase,
        tracked,
    })
}

impl Window {
    pub(super) fn finish(self, ok: bool) {
        let mut p = probe();
        let kind = self.kind.min(2);
        let phase = if self.phase != p.phase {
            2
        } else {
            usize::from(p.presents != 0)
        };
        if ok {
            p.counts.phases[kind][phase] += 1;
            p.counts.bytes[kind] += self.range.1.saturating_sub(self.range.0) as u64;
            if phase == 0 && self.submitted != 0 && crate::queue::presents_done() >= self.submitted
            {
                p.counts.late_outside[kind] += 1;
            }
        } else {
            p.counts.failed += 1;
        }
        p.counts.unstamped += u64::from(self.submitted == 0);
        if let Some(t) = p
            .targets
            .iter_mut()
            .find(|t| t.kind == self.kind && t.range == self.range)
        {
            if self.tracked {
                t.writing -= 1;
            }
            t.visible = if ok { self.submitted } else { 0 };
        }
        if self.kind < 2 {
            p.sample(Instant::now());
        }
    }
}

pub(super) fn log_interval() {
    if !enabled() {
        return;
    }
    let c = {
        let mut p = probe();
        p.sample(Instant::now());
        std::mem::take(&mut p.counts)
    };
    tracing::info!(target: "victory_gpu::host",
        stage = super::late_stage(), relay = super::relay(), frames = c.frames,
        ages = ?c.ages, paired = ?c.pair, age_changes = c.changed_age,
        comparable_transitions = c.uniform_transitions, pair_ms_between_presents = ?c.pair_us.map(|us| us as f64 / 1000.0),
        copy_windows = ?c.phases, published_mb = ?c.bytes.map(|n| n as f64 / 1e6),
        late_between_presents = ?c.late_outside, map_overlaps = ?c.maps,
        untracked_requests = c.misses, failed_copies = c.failed, unstamped = c.unstamped,
        "copy-back age probe only (1x1 / 5x1 / selected tiled): return ages 0/1/2/3+/unknown; \
         paired return current/late/mixed/unknown; paired time uniform/mixed/unknown; successful copies outside/inside/crossing present; \
         Map destination overlaps current/stale/unknown");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_destinations_report_the_generation_in_each_requested_slot() {
        let mut p = Probe::default();
        for f in 1..=4 {
            let a = 8 * (f as usize % 2);
            p.request(0, (a, a + 4), f);
            p.request(1, (20, 40), f);
            p.targets.iter_mut().find(|t| t.kind == 1).unwrap().visible = f;
            if f <= 2 {
                p.targets
                    .iter_mut()
                    .find(|t| t.range.0 == a)
                    .unwrap()
                    .visible = f;
            }
            p.boundary(f);
        }
        assert_eq!(p.counts.pair, [2, 0, 2, 0]);
        assert_eq!(p.counts.ages[0], [2, 0, 2, 0, 0]);
    }

    #[test]
    fn equally_stale_pairs_can_still_change_age_and_active_writes_are_unknown() {
        let mut p = Probe::default();
        for f in 1..=3 {
            p.request(0, (0, 4), f);
            p.request(1, (8, 28), f);
            for t in &mut p.targets {
                t.visible = 1;
            }
            if f == 3 {
                p.targets[1].writing = 1;
            }
            p.boundary(f);
        }
        assert_eq!(p.counts.pair, [1, 1, 0, 1]);
        assert_eq!((p.counts.changed_age, p.counts.uniform_transitions), (1, 1));
        p.map_ready((3, 9));
        p.map_ready((4, 8));
        assert_eq!(p.counts.maps, [0, 1, 1]);
    }

    #[test]
    fn two_separate_landings_expose_a_mixed_pair_between_coherent_boundaries() {
        use std::time::Duration;
        let mut p = Probe::default();
        for kind in 0..2 {
            p.request(kind, (kind * 8, kind * 8 + 4), 1);
            p.targets[kind].visible = 1;
        }
        p.boundary(1);
        let now = Instant::now();
        let at = |us| now + Duration::from_micros(us);
        p.sample(at(0));
        p.targets[0].visible = 2;
        p.sample(at(30));
        p.targets[1].writing = 1;
        p.sample(at(40));
        p.targets[1].writing = 0;
        p.targets[1].visible = 2;
        p.sample(at(45));
        p.presents += 1;
        p.sample(at(55));
        p.sample(at(100));
        assert_eq!(p.counts.pair_us, [40, 10, 5]);
    }
}
