//! A submission's preparing on a few threads (`VICTORY_PREP_THREADS=<n>`:
//! the submitting thread and `n - 1` kept threads; unset, 0 or 1: off, all
//! of it on the submitting thread as before).
//!
//! What goes to the threads, and why the outcome is the serial one:
//!
//! - A texture's refresh (`Step::Texture`) starts on a thread where the
//!   serial order ran it and is waited for at the next flush of the batch,
//!   which comes before every step that writes title memory (`Step::Copy`,
//!   `Step::Run`) and before the lists go to the card. It reads title
//!   memory and writes its own staging buffer under its own lock; the steps
//!   it is moved past write no title memory (fills write the list's upload
//!   memory, refreshes and snapshots the buffers' host copies, host work
//!   its own memory) and none of them touches a staging buffer but
//!   `Step::Uploaded`, kept after it ([`later`]).
//! - A flush's buffer copies (`Batch::flush`), each on a thread from two up,
//!   as `VICTORY_WIDE_REFRESH=1` does from four: each writes its own copy
//!   and its own notes, and reads title memory.
//!
//! With `VICTORY_WATCH_SPLIT=1` the watch orders overlapping polls by
//! region. The frame line includes contention from all watch callers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, OnceLock};
use std::time::Instant;

pub(crate) type Job = Box<dyn FnOnce() + Send>;

type Panic = Box<dyn std::any::Any + Send>;

/// The most threads the switch asks for that are used.
const MOST: usize = 8;

/// Threads preparing a submission, the submitting one among them; 0: off.
pub(crate) fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| parse(std::env::var("VICTORY_PREP_THREADS").ok().as_deref()))
}

fn parse(value: Option<&str>) -> usize {
    match value.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) if n >= 2 => n.min(MOST),
        _ => 0,
    }
}

/// Whether preparing goes to threads at all.
pub(crate) fn on() -> bool {
    threads() != 0
}

/// Whether a list's note that it uploads a staging buffer
/// (`Step::Uploaded`) waits until the group's jobs are done: only when a
/// refresh of that texture may still be running, that is, it was refreshed
/// in this stretch between title writes (`refreshed_here`) and jobs are out
/// (`busy`). The serial order ran that refresh before the note; a note
/// before the refresh was started is made at once, as it was.
pub(crate) fn later(busy: bool, refreshed_here: bool) -> bool {
    busy && refreshed_here
}

/// Counts since the last line, per [`line`].
static STATS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
const PARALLEL: usize = 0;
const SINGLE: usize = 1;
const JOBS: usize = 2;
const WORK_US: usize = 3;
const WALL_US: usize = 4;
const LONGEST_US: usize = 5;
const ONE_COPY: usize = 6;
const NO_THREADS: usize = 7;
const KEPT_NOTES: usize = 8;
const UPFRONT_US: usize = 9;
const UPFRONT_PASSES: usize = 10;

pub(crate) fn note_upfront(us: u64) {
    STATS[UPFRONT_US].fetch_add(us, Relaxed);
    STATS[UPFRONT_PASSES].fetch_add(1, Relaxed);
}

struct Pool {
    queue: Mutex<VecDeque<Job>>,
    more: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Threads to keep besides the submitting one.
fn workers() -> usize {
    if cfg!(test) { 3 } else { threads().saturating_sub(1) }
}

/// The kept threads, made at the first use; `None` if none could be made.
fn pool() -> Option<&'static Pool> {
    static POOL: OnceLock<Pool> = OnceLock::new();
    static START: Once = Once::new();
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    let pool = POOL.get_or_init(|| Pool {
        queue: Mutex::new(VecDeque::new()),
        more: Condvar::new(),
    });
    START.call_once(|| {
        for i in 0..workers() {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-prep-{i}"))
                .spawn(|| work(POOL.get().expect("made above")));
            if spawned.is_ok() {
                STARTED.fetch_add(1, Relaxed);
            }
        }
    });
    (STARTED.load(Relaxed) > 0).then_some(pool)
}

fn work(pool: &'static Pool) {
    loop {
        let job = {
            let mut queue = lock(&pool.queue);
            loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = pool.more.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        job();
    }
}

#[derive(Default)]
struct Left {
    jobs: usize,
    panic: Option<Panic>,
}

/// What a group's jobs share with it.
#[derive(Default)]
struct Shared {
    left: Mutex<Left>,
    done: Condvar,
    work_us: AtomicU64,
    longest_us: AtomicU64,
}

/// Jobs out on the kept threads since the last [`Group::join`], and work
/// to do after them, in order.
#[derive(Default)]
pub(crate) struct Group {
    shared: Option<Arc<Shared>>,
    jobs: u64,
    after: Vec<Job>,
}

impl Group {
    /// Whether jobs are out.
    pub(crate) fn busy(&self) -> bool {
        self.shared.is_some()
    }

    /// Starts `job` on a kept thread (on this one, now, if there are none).
    pub(crate) fn spawn(&mut self, job: Job) {
        let Some(pool) = pool() else {
            STATS[NO_THREADS].fetch_add(1, Relaxed);
            job();
            return;
        };
        let shared: Arc<Shared> = self.shared.get_or_insert_with(Default::default).clone();
        lock(&shared.left).jobs += 1;
        self.jobs += 1;
        let task: Job = Box::new(move || {
            let started = Instant::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            let us = started.elapsed().as_micros() as u64;
            shared.work_us.fetch_add(us, Relaxed);
            shared.longest_us.fetch_max(us, Relaxed);
            let mut left = lock(&shared.left);
            if let Err(panic) = result
                && left.panic.is_none()
            {
                left.panic = Some(panic);
            }
            left.jobs -= 1;
            if left.jobs == 0 {
                shared.done.notify_all();
            }
        });
        lock(&pool.queue).push_back(task);
        pool.more.notify_one();
    }

    /// `work` after the jobs out now are done ([`later`]).
    pub(crate) fn after(&mut self, work: Job) {
        STATS[KEPT_NOTES].fetch_add(1, Relaxed);
        self.after.push(work);
    }

    /// Waits for the jobs out (taking queued ones on this thread
    /// meanwhile), then does the `after` work in order. A job's panic goes
    /// on here.
    pub(crate) fn join(&mut self) {
        if let Some(panic) = self.finish() {
            std::panic::resume_unwind(panic);
        }
    }

    fn finish(&mut self) -> Option<Panic> {
        let mut panic = None;
        if let Some(shared) = self.shared.take() {
            let started = Instant::now();
            if let Some(pool) = pool() {
                while lock(&shared.left).jobs > 0 {
                    let next = lock(&pool.queue).pop_front();
                    match next {
                        Some(job) => job(),
                        None => break,
                    }
                }
            }
            let mut left = lock(&shared.left);
            while left.jobs > 0 {
                left = shared.done.wait(left).unwrap_or_else(|e| e.into_inner());
            }
            panic = left.panic.take();
            drop(left);
            let wall = started.elapsed().as_micros() as u64;
            note(self.jobs, shared.work_us.load(Relaxed), wall, shared.longest_us.load(Relaxed));
            self.jobs = 0;
        }
        for work in self.after.drain(..) {
            work();
        }
        panic
    }
}

impl Drop for Group {
    /// Nothing a job writes is used after the batch without it being done.
    fn drop(&mut self) {
        if self.finish().is_some() {
            tracing::error!("a step of a submission's preparing panicked on a preparing thread (VICTORY_PREP_THREADS)");
        }
    }
}

fn note(jobs: u64, work_us: u64, wall_us: u64, longest_us: u64) {
    STATS[if jobs >= 2 { PARALLEL } else { SINGLE }].fetch_add(1, Relaxed);
    STATS[JOBS].fetch_add(jobs, Relaxed);
    STATS[WORK_US].fetch_add(work_us, Relaxed);
    STATS[WALL_US].fetch_add(wall_us, Relaxed);
    STATS[LONGEST_US].fetch_max(longest_us, Relaxed);
}

/// A flush's buffer copies (`Batch::flush`), all done on return, on the
/// threads from two up. Returns the microseconds it took.
pub(crate) fn refreshes(jobs: Vec<Job>) -> u64 {
    let started = Instant::now();
    if jobs.len() < 2 {
        STATS[ONE_COPY].fetch_add(1, Relaxed);
        for job in jobs {
            job();
        }
    } else {
        let mut group = Group::default();
        for job in jobs {
            group.spawn(job);
        }
        group.join();
    }
    started.elapsed().as_micros() as u64
}

/// The line on the frame line's beat; `frames` since the last.
pub(crate) fn line(frames: f64) {
    let [parallel, single, jobs, work, wall, longest, one_copy, no_threads, kept, upfront_us, passes] =
        STATS.each_ref().map(|v| v.swap(0, Relaxed));
    let [journal_us, journal_n, guards_us, guards_n, regions_us, regions_n] = crate::watch::lock_waits();
    let [upfront, live, table, clean] = crate::watch::poll_counts();
    let batches = parallel + single;
    let per = |v: u64| v as f64 / frames;
    tracing::info!(
        "preparing on threads per frame (VICTORY_PREP_THREADS={}, VICTORY_WATCH_SPLIT={}): {:.1} batches in parallel, {:.1} serial \
         ({:.1} of one step, {:.1} copy flushes of one copy, {:.1} steps without threads), {:.1} steps a batch, \
         steps {:.1} ms summed across threads, {:.1} ms on the submitting thread joining batches, \
         longest step this reporting interval {:.1} ms, {:.1} upload notes kept until after; \
         all watch callers waiting on the watch journal's lock {:.1} ms ({:.0}x), on the guards' lock {:.1} ms ({:.0}x), \
         on poll regions {:.1} ms ({:.0}x); {:.1} GetWriteWatch calls ({:.1} up front, {:.1} live), \
         {:.1} ranges answered from the ahead table, {:.1} from clean guard runs, \
         {:.1} up-front watch passes taking {:.2} ms on the submitting thread{}",
        threads(),
        u8::from(crate::watch::split()),
        per(parallel),
        per(single + one_copy) + per(no_threads),
        per(single),
        per(one_copy),
        per(no_threads),
        if batches > 0 { jobs as f64 / batches as f64 } else { 0.0 },
        per(work) / 1e3,
        per(wall) / 1e3,
        longest as f64 / 1e3,
        per(kept),
        per(journal_us) / 1e3,
        per(journal_n),
        per(guards_us) / 1e3,
        per(guards_n),
        per(regions_us) / 1e3,
        per(regions_n),
        per(upfront + live),
        per(upfront),
        per(live),
        per(table),
        per(clean),
        per(passes),
        per(upfront_us) / 1e3,
        crate::watch::light::summary(frames),
    );
}

#[cfg(test)]
mod tests {
    use super::{Group, Job, later, parse, refreshes};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_switch_reads_two_threads_up() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("1")), 0);
        assert_eq!(parse(Some("x")), 0);
        assert_eq!(parse(Some("3")), 3);
        assert_eq!(parse(Some(" 4 ")), 4);
        assert_eq!(parse(Some("64")), super::MOST);
    }

    #[test]
    fn a_note_waits_only_for_its_own_running_refresh() {
        assert!(later(true, true));
        assert!(!later(true, false));
        assert!(!later(false, true));
        assert!(!later(false, false));
    }

    /// Some work, so that jobs overlap.
    fn spin(n: u64) -> u64 {
        (0..n).fold(1u64, |a, b| a.wrapping_mul(6364136223846793005).wrapping_add(b))
    }

    #[test]
    fn join_waits_for_every_job_then_runs_the_after_work_in_order() {
        let done = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut group = Group::default();
        assert!(!group.busy());
        for i in 0..64u64 {
            let done = done.clone();
            group.spawn(Box::new(move || {
                let _ = std::hint::black_box(spin(1000 + 37 * i));
                done.fetch_add(1, SeqCst);
            }));
        }
        assert!(group.busy());
        for k in 0..5 {
            let (done, order) = (done.clone(), order.clone());
            group.after(Box::new(move || {
                assert_eq!(done.load(SeqCst), 64);
                order.lock().unwrap().push(k);
            }));
        }
        group.join();
        assert!(!group.busy());
        assert_eq!(done.load(SeqCst), 64);
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4]);
        // Again with the same group.
        let d = done.clone();
        group.spawn(Box::new(move || {
            d.fetch_add(1, SeqCst);
        }));
        group.join();
        assert_eq!(done.load(SeqCst), 65);
    }

    #[test]
    fn a_dropped_group_finishes_its_jobs() {
        let done = Arc::new(AtomicUsize::new(0));
        {
            let mut group = Group::default();
            for _ in 0..16 {
                let done = done.clone();
                group.spawn(Box::new(move || {
                    let _ = std::hint::black_box(spin(5000));
                    done.fetch_add(1, SeqCst);
                }));
            }
        }
        assert_eq!(done.load(SeqCst), 16);
    }

    #[test]
    fn a_job_panic_reaches_the_joining_thread() {
        let mut group = Group::default();
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        fn boom() {
            panic!("a test job panics");
        }
        group.spawn(Box::new(boom));
        group.spawn(Box::new(move || r.store(true, SeqCst)));
        let joined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| group.join()));
        assert!(joined.is_err());
        assert!(ran.load(SeqCst));
        assert!(!group.busy());
    }

    #[test]
    fn refreshes_runs_every_job_by_any_count() {
        for n in [0usize, 1, 2, 7] {
            let done = Arc::new(AtomicUsize::new(0));
            let jobs: Vec<Job> = (0..n)
                .map(|_| {
                    let done = done.clone();
                    Box::new(move || {
                        done.fetch_add(1, SeqCst);
                    }) as Job
                })
                .collect();
            refreshes(jobs);
            assert_eq!(done.load(SeqCst), n);
        }
    }

    /// One step of a modelled submission: a texture's refresh (its detile
    /// sets `dirty`), a list's upload note (clears it), a title write.
    #[derive(Clone, Copy, Debug)]
    enum Op {
        Texture(usize),
        Uploaded(usize),
        TitleWrite,
    }

    const TEXTURES: usize = 4;

    /// The serial order, as `run_deferred` runs it without the switch.
    fn serial(ops: &[Op]) -> [bool; TEXTURES] {
        let mut dirty = [false; TEXTURES];
        let mut due = [true; TEXTURES];
        for &op in ops {
            match op {
                Op::Texture(t) if due[t] => {
                    due[t] = false;
                    dirty[t] = true;
                }
                Op::Texture(_) => {}
                Op::Uploaded(t) => dirty[t] = false,
                Op::TitleWrite => due = [true; TEXTURES],
            }
        }
        dirty
    }

    /// With the switch: refreshes on the threads, notes as `run_deferred`
    /// places them, a join at every title write and at the end.
    fn threaded(ops: &[Op], seed: u64) -> [bool; TEXTURES] {
        let dirty: Arc<[AtomicBool; TEXTURES]> = Arc::new(std::array::from_fn(|_| AtomicBool::new(false)));
        let mut due = [true; TEXTURES];
        let mut group = Group::default();
        for (i, &op) in ops.iter().enumerate() {
            match op {
                Op::Texture(t) if due[t] => {
                    due[t] = false;
                    let dirty = dirty.clone();
                    let n = 200 + (seed.wrapping_mul(31).wrapping_add(i as u64) % 7) * 3000;
                    group.spawn(Box::new(move || {
                        let _ = std::hint::black_box(spin(n));
                        dirty[t].store(true, SeqCst);
                    }));
                }
                Op::Texture(_) => {}
                Op::Uploaded(t) => {
                    if later(group.busy(), !due[t]) {
                        let dirty = dirty.clone();
                        group.after(Box::new(move || dirty[t].store(false, SeqCst)));
                    } else {
                        dirty[t].store(false, SeqCst);
                    }
                }
                Op::TitleWrite => {
                    group.join();
                    due = [true; TEXTURES];
                }
            }
        }
        group.join();
        std::array::from_fn(|t| dirty[t].load(SeqCst))
    }

    #[test]
    fn threaded_texture_refreshes_end_as_the_serial_order_does() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state >> 33
        };
        for seed in 0..300u64 {
            let len = 1 + (next() % 24) as usize;
            let ops: Vec<Op> = (0..len)
                .map(|_| match next() % 9 {
                    0 => Op::TitleWrite,
                    1..=4 => Op::Texture((next() % TEXTURES as u64) as usize),
                    _ => Op::Uploaded((next() % TEXTURES as u64) as usize),
                })
                .collect();
            assert_eq!(threaded(&ops, seed), serial(&ops), "{ops:?}");
        }
    }
}
