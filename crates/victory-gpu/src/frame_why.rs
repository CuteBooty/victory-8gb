//! Wall clocks for the five-second INFO window. Each thread has an exclusive
//! current phase. A present samples open phases too (notably idle workers).
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FRAME_WHY2").is_some_and(|v| v == "1"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Unknown,
    Caller,
    Submit,
    Relay,
    Helper,
    /// The heap worker (`VICTORY_HEAP_WORKER`): a lane of its own.
    Heap,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Record,
    Ack,
    Memory,
    Mapping,
    Fence,
    Event,
    NativeEvent,
    Drain,
    Display,
    RelayWait,
    Prepare,
    Gpu,
    Idle,
    Land,
    CardMerge,
    Merge,
    Publish,
    Writes,
    SubmitCall,
    SubmitLock,
    Other,
}
use Phase::*;
const N: usize = Other as usize + 1;
const LABELS: [&str; N] = [
    "record",
    "ack",
    "copy_memory_wait",
    "mapping_lock",
    "fence",
    "frame_event",
    "win32_wait",
    "present_drain",
    "display",
    "relay_wait",
    "prepare",
    "gpu_fence_wait",
    "idle",
    "land_other",
    "card_merge",
    "shadow_merge",
    "publish",
    "queued_writes",
    "submit_other",
    "submit_lock",
    "remainder",
];

const CALLER_PHASES: &[Phase] = &[
    Record,
    Ack,
    Memory,
    Mapping,
    Fence,
    Event,
    NativeEvent,
    Drain,
    Display,
    RelayWait,
    SubmitCall,
    SubmitLock,
    Other,
];
const SUBMIT_PHASES: &[Phase] = &[
    Prepare, Gpu, RelayWait, Memory, Mapping, SubmitLock, Idle, Other,
];
const RELAY_PHASES: &[Phase] = &[
    Gpu, Idle, Land, CardMerge, Merge, Publish, Writes, Memory, Mapping, Other,
];
// The heap worker: its pass ("prepare"), waiting for
// one ("idle"), the watch's locks.
const HEAP_PHASES: &[Phase] = &[Prepare, Idle, Memory, Mapping, Other];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Track {
    role: Role,
    since: Instant,
    phase: Phase,
    ns: [u64; N],
    baseline: [u64; N],
}
impl Track {
    fn new(now: Instant) -> Self {
        Self {
            role: Role::Unknown,
            since: now,
            phase: Other,
            ns: [0; N],
            baseline: [0; N],
        }
    }
    fn charge(&mut self, now: Instant) {
        self.ns[self.phase as usize] += (now - self.since).as_nanos() as u64;
        self.since = now;
    }
    fn switch(&mut self, phase: Phase, now: Instant) -> Phase {
        self.charge(now);
        std::mem::replace(&mut self.phase, phase)
    }
    fn sample(&mut self, now: Instant) -> [u64; N] {
        self.charge(now);
        let prior = std::mem::replace(&mut self.baseline, self.ns);
        std::array::from_fn(|i| self.ns[i] - prior[i])
    }
}

static TRACKS: Mutex<Vec<Arc<Mutex<Track>>>> = Mutex::new(Vec::new());
thread_local! {
    static TRACK: Arc<Mutex<Track>> = {
        let mut tracks = lock(&TRACKS);
        let t = Arc::new(Mutex::new(Track::new(Instant::now())));
        tracks.push(t.clone());
        t
    };
}

pub(crate) fn role(role: Role) {
    if enabled() {
        TRACK.with(|t| {
            lock(t).role = role;
            match role {
                Role::Submit => {
                    let _ = SUBMIT.set(t.clone());
                }
                Role::Relay => {
                    let _ = RELAY.set(t.clone());
                }
                _ => {}
            }
        });
    }
}

static SUBMIT: OnceLock<Arc<Mutex<Track>>> = OnceLock::new();
static RELAY: OnceLock<Arc<Mutex<Track>>> = OnceLock::new();
static OVERLAPS: Mutex<[u64; 7]> = Mutex::new([0; 7]);

// Sample cumulative worker clocks on both edges of the present drain. This
// measures intersection directly; separate lane totals alone cannot show it.
fn worker_clocks() -> [u64; 7] {
    fn snapshot(worker: &OnceLock<Arc<Mutex<Track>>>) -> [u64; N] {
        worker.get().map_or([0; N], |t| {
            let mut t = lock(t);
            t.charge(Instant::now());
            t.ns
        })
    }
    let s = snapshot(&SUBMIT);
    let r = snapshot(&RELAY);
    [
        s[Prepare as usize],
        r[Gpu as usize],
        r[CardMerge as usize],
        r[Merge as usize],
        r[Publish as usize],
        r[Writes as usize],
        // The submit worker's idle time inside the present's settle.
        s[Idle as usize],
    ]
}

/// The submit worker's idle clock so far (ns; its open phase included):
/// the heap pass's overlap with it (`crate::why_more`).
pub(crate) fn submit_idle_ns() -> u64 {
    SUBMIT.get().map_or(0, |t| {
        let mut t = lock(t);
        t.charge(Instant::now());
        t.ns[Idle as usize]
    })
}

pub(crate) struct PresentDrain {
    _phase: Span,
    clocks: Option<[u64; 7]>,
}
pub(crate) fn drain() -> PresentDrain {
    PresentDrain {
        _phase: span(Phase::Drain),
        clocks: enabled().then(worker_clocks),
    }
}
impl Drop for PresentDrain {
    fn drop(&mut self) {
        if let Some(start) = self.clocks {
            let end = worker_clocks();
            let mut sums = lock(&OVERLAPS);
            for i in 0..sums.len() {
                sums[i] += end[i] - start[i];
            }
        }
    }
}

pub(crate) struct Span(Option<Phase>);
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(prev) = self.0 {
            TRACK.with(|t| {
                lock(t).switch(prev, Instant::now());
            });
        }
    }
}

pub(crate) fn span(phase: Phase) -> Span {
    if !enabled() {
        return Span(None);
    }
    TRACK.with(|t| {
        let mut t = lock(t);
        // A mapping lock alone can come from a helper. Only attribute a thread
        // to the title after observing it enter a title-facing API.
        if t.role == Role::Unknown
            && matches!(
                phase,
                Record | SubmitCall | Drain | Fence | Event | NativeEvent
            )
        {
            t.role = Role::Caller;
        }
        // Attribute present and blocking fence APIs inclusively. Their inline
        // submits, including any relay waits, are already on this thread.
        if t.role == Role::Caller
            && (matches!(t.phase, Drain | Display | Fence)
                || matches!(
                    phase,
                    Prepare | Gpu | Land | Idle | Merge | CardMerge | Publish | Writes
                ))
        {
            return Span(None);
        }
        Span(Some(t.switch(phase, Instant::now())))
    })
}

// Independent workers each cover the same wall interval. Time before a worker
// is registered and uninstrumented time both belong to its remainder.
fn complete(ns: &mut [u64; N], wall: u64, phases: &[Phase]) {
    let measured = phases
        .iter()
        .filter(|&&p| p != Other)
        .map(|&p| ns[p as usize])
        .sum();
    ns[Other as usize] = wall.saturating_sub(measured);
}

#[derive(Default)]
struct Summary {
    sum: [u64; N],
    peak: [u64; N],
}
impl Summary {
    fn push(&mut self, ns: [u64; N]) {
        for (i, v) in ns.into_iter().enumerate() {
            self.sum[i] += v;
            self.peak[i] = self.peak[i].max(v);
        }
    }
    fn fields(&self, frames: f64, phases: &[Phase]) -> String {
        phases
            .iter()
            .map(|&p| {
                let i = p as usize;
                format!(
                    "{}={:.3}/{:.3}",
                    LABELS[i],
                    self.sum[i] as f64 / frames / 1e6,
                    self.peak[i] as f64 / 1e6
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Default)]
struct Window {
    last: Option<Instant>,
    frames: u64,
    wall: u64,
    worst: u64,
    worst_lanes: [[u64; N]; 3],
    lanes: [Summary; 5],
    overlaps: [u64; 7],
    overlap_peak: [u64; 7],
    ack: [u64; 2],
    ack_peak: [u64; 2],
    flight: u64,
    flight_peak: u64,
    flights: u64,
}
static WINDOW: LazyLock<Mutex<Window>> = LazyLock::new(Default::default);
static PRINT: AtomicBool = AtomicBool::new(false);
pub(crate) fn due() {
    if enabled() {
        PRINT.store(true, Relaxed);
    }
}

pub(crate) fn frame() {
    if !enabled() {
        return;
    }
    let current = TRACK.with(Arc::clone);
    let mut w = lock(&WINDOW);
    let mut tracks = lock(&TRACKS);
    let mut lanes = [[0; N]; 5]; // presenter, submit, relay, other callers, heap worker
    let mut heap_seen = false;
    let mut guards: Vec<_> = tracks.iter().map(|t| lock(t)).collect();
    let now = Instant::now();
    for (track, t) in tracks.iter().zip(&mut guards) {
        let ns = t.sample(now);
        let lane = match t.role {
            Role::Caller if Arc::ptr_eq(track, &current) => 0,
            Role::Submit => 1,
            Role::Relay => 2,
            Role::Caller => 3,
            Role::Heap => {
                heap_seen = true;
                4
            }
            Role::Unknown | Role::Helper => continue,
        };
        for (out, v) in lanes[lane].iter_mut().zip(ns) {
            *out += v;
        }
    }
    drop(guards);
    tracks.retain(|t| Arc::strong_count(t) > 1);
    drop(tracks);
    let ack = ACK.each_ref().map(|a| a.swap(0, Relaxed));
    let (flight, flights) = std::mem::take(&mut *lock(&FLIGHTS));
    let overlaps = std::mem::take(&mut *lock(&OVERLAPS));
    let last = w.last.replace(now);
    let report = last.is_some() && PRINT.swap(false, Relaxed);
    let queries = crate::query_why::frame(last.is_some(), report);
    let Some(last) = last else {
        let _ = take_counts();
        return;
    };
    let wall = (now - last).as_nanos() as u64;
    for (ns, phases) in lanes
        .iter_mut()
        .zip([CALLER_PHASES, SUBMIT_PHASES, RELAY_PHASES])
    {
        complete(ns, wall, phases);
    }
    if heap_seen {
        complete(&mut lanes[4], wall, HEAP_PHASES);
    }
    if wall >= w.worst {
        w.worst = wall;
        w.worst_lanes.copy_from_slice(&lanes[..3]);
    }
    for (s, ns) in w.lanes.iter_mut().zip(lanes) {
        s.push(ns);
    }
    for (i, n) in ack.into_iter().enumerate() {
        w.ack[i] += n;
        w.ack_peak[i] = w.ack_peak[i].max(n);
    }
    for (i, n) in overlaps.into_iter().enumerate() {
        w.overlaps[i] += n;
        w.overlap_peak[i] = w.overlap_peak[i].max(n);
    }
    w.flight += flight;
    w.flight_peak = w.flight_peak.max(flight);
    w.flights += flights;
    w.wall += wall;
    w.frames += 1;
    let Some(queries) = queries else { return };
    let window = std::mem::replace(
        &mut *w,
        Window {
            last: Some(now),
            ..Window::default()
        },
    );
    let frames = window.frames as f64;
    let counters = take_counts();
    drop(w);
    crate::query_why::log(frames, queries);
    log_counts(frames, counters);
    let overlap = [
        "submit_prepare",
        "relay_gpu_wait",
        "card_merge",
        "shadow_merge",
        "publish",
        "queued_writes",
        "submit_idle",
    ]
    .iter()
    .enumerate()
    .map(|(i, name)| {
        format!(
            "{name}={:.3}/{:.3}",
            window.overlaps[i] as f64 / frames / 1e6,
            window.overlap_peak[i] as f64 / 1e6
        )
    })
    .collect::<Vec<_>>()
    .join(" ");
    tracing::info!("why2 drain overlap ms avg/max {overlap}");
    {
        // Does the submit worker idle for want of
        // lists while the present settles, or otherwise?
        let idle = window.lanes[1].sum[Idle as usize];
        let during = window.overlaps[6].min(idle);
        tracing::info!(
            "why2 submit worker idle ms per frame: {:.3} while the present's settle was in progress, {:.3} otherwise",
            during as f64 / frames / 1e6,
            (idle - during) as f64 / frames / 1e6
        );
    }
    crate::why_more::log(frames);
    let worst = |lane: usize, phases: &[Phase]| {
        phases
            .iter()
            .map(|p| {
                let i = *p as usize;
                format!(
                    "{}={:.3}",
                    LABELS[i],
                    window.worst_lanes[lane][i] as f64 / 1e6
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    tracing::info!(
        "why2 worst frame ms wall={:.3} presenter {} | submit {} | relay {}",
        window.worst as f64 / 1e6,
        worst(0, CALLER_PHASES),
        worst(1, SUBMIT_PHASES),
        worst(2, RELAY_PHASES)
    );
    tracing::info!(
        "why2 title ms avg/max frames={} frame={:.3}/{:.3} presenter {} | other callers (thread-ms) {} | completed ack waits all callers prepared={:.3}/{:.3} runnable_prefix={:.3}/{:.3}",
        window.frames,
        window.wall as f64 / frames / 1e6,
        window.worst as f64 / 1e6,
        window.lanes[0].fields(frames, CALLER_PHASES),
        window.lanes[3].fields(frames, &CALLER_PHASES[..CALLER_PHASES.len() - 1]),
        window.ack[0] as f64 / frames / 1e6,
        window.ack_peak[0] as f64 / 1e6,
        window.ack[1] as f64 / frames / 1e6,
        window.ack_peak[1] as f64 / 1e6
    );
    tracing::info!(
        "why2 worker ms avg/max submit {} | relay {} | overlapping execute-to-observed-fence ms/frame={:.3}/{:.3} flights/frame={:.2}",
        window.lanes[1].fields(frames, SUBMIT_PHASES),
        window.lanes[2].fields(frames, RELAY_PHASES),
        window.flight as f64 / frames / 1e6,
        window.flight_peak as f64 / 1e6,
        window.flights as f64 / frames
    );
    if window.lanes[4].sum.iter().any(|&v| v != 0) {
        tracing::info!(
            "why2 heap worker ms avg/max {}",
            window.lanes[4].fields(frames, HEAP_PHASES)
        );
    }
}

static ACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
pub(crate) fn ack(prefix: bool, start: Instant) {
    if enabled() {
        ACK[usize::from(prefix)].fetch_add(start.elapsed().as_nanos() as u64, Relaxed);
    }
}
static FLIGHTS: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// All sends covered by the card clock's next fence observation. Constant
/// storage even if the observer falls behind. wall*n - sum(start offsets)
/// gives the sum of the overlapping latencies in this batch.
#[derive(Default)]
pub(crate) struct FlightBatch {
    origin: Option<Instant>,
    offsets: i128,
    sends: u64,
}
impl FlightBatch {
    pub(crate) fn sent(&mut self, start: Instant) {
        let origin = *self.origin.get_or_insert(start);
        // Competing senders may take the clock lock in a different order.
        self.offsets += if start >= origin {
            (start - origin).as_nanos() as i128
        } else {
            -((origin - start).as_nanos() as i128)
        };
        self.sends += 1;
    }
    fn total(&self, observed: Instant) -> u64 {
        self.origin.map_or(0, |o| {
            ((observed - o).as_nanos() as i128 * i128::from(self.sends) - self.offsets) as u64
        })
    }
    pub(crate) fn observed(self) {
        if self.sends == 0 {
            return;
        }
        let total = self.total(Instant::now());
        let mut s = lock(&FLIGHTS);
        s.0 += total;
        s.1 += self.sends;
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Count {
    Checked,
    Changed,
    SelfMark,
    Hash,
    Hit,
    Alias,
    Slice,
    Epoch,
    NewSpan,
    Content,
    Uncached,
    Root,
    RootsSame,
}
static COUNTS: [AtomicU64; 13] = [const { AtomicU64::new(0) }; 13];
pub(crate) fn count(c: Count, n: u64) {
    if enabled() {
        COUNTS[c as usize].fetch_add(n, Relaxed);
    }
}
pub(crate) static CULL: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
fn take_counts() -> ([u64; 13], [u64; 9]) {
    let take = |v: &AtomicU64| v.swap(0, Relaxed);
    (COUNTS.each_ref().map(take), CULL.each_ref().map(take))
}
fn log_counts(frames: f64, (counts, culling): ([u64; 13], [u64; 9])) {
    let avg = |v: u64| v as f64 / frames;
    let [
        checked,
        changed,
        marks,
        hashes,
        hit,
        alias,
        slice,
        epoch,
        new,
        content,
        uncached,
        roots,
        same,
    ] = counts.map(avg);
    tracing::info!(
        "why2 descriptors per frame checked={checked:.1} record_changed={changed:.1} self_marked={marks:.1} spans_hashed={hashes:.1} hits exact={hit:.1} alias={alias:.1} slice={slice:.1} misses epoch={epoch:.1} first_span={new:.1} changed_contents_or_format={content:.1} key_not_retained={uncached:.1} root_vectors={roots:.1} unchanged_pso={same:.1}"
    );
    let [
        tree,
        imposter,
        grass,
        prop,
        nocull,
        simple,
        slod,
        meshlet,
        depth,
    ] = culling.map(avg);
    tracing::info!(
        "why2 culling dispatch calls per frame tree={tree:.2} imposter={imposter:.2} grass={grass:.2} prop={prop:.2} prop_nocull={nocull:.2} simple={simple:.2} slod={slod:.2} meshlet_args={meshlet:.2} depth_pyramid={depth:.2}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn nested_wait_is_exclusive_and_split_at_present() {
        let start = Instant::now();
        let at = |n| start + Duration::from_nanos(n);
        let mut t = Track::new(start);
        t.switch(Record, at(2));
        let parent = t.switch(Memory, at(5));
        let a = t.sample(at(7));
        assert_eq!(
            (a[Other as usize], a[Record as usize], a[Memory as usize]),
            (2, 3, 2)
        );
        t.switch(parent, at(10));
        t.switch(Other, at(14));
        let b = t.sample(at(15));
        assert_eq!(
            (b[Other as usize], b[Record as usize], b[Memory as usize]),
            (1, 4, 3)
        );
    }
    #[test]
    fn batched_fence_latencies_allow_concurrent_senders() {
        let t = Instant::now();
        let mut b = FlightBatch::default();
        b.sent(t + Duration::from_nanos(4));
        b.sent(t);
        assert_eq!(b.total(t + Duration::from_nanos(10)), 16);
    }
    #[test]
    fn concurrent_lanes_each_cover_the_frame_and_keep_independent_peaks() {
        let mut submit = [0; N];
        submit[Prepare as usize] = 6;
        let mut relay = [0; N];
        relay[Merge as usize] = 8;
        for (lane, phases) in [(&mut submit, SUBMIT_PHASES), (&mut relay, RELAY_PHASES)] {
            complete(lane, 10, phases);
            assert_eq!(lane.iter().sum::<u64>(), 10);
        }
        let mut w = Summary::default();
        w.push(submit);
        w.push(relay);
        assert_eq!((w.sum[Other as usize], w.peak[Other as usize]), (6, 4));
    }
}
