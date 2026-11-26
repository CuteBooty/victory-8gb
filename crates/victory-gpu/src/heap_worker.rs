//! The pass that uploads textures picked by index, on a thread of its own
//! (`VICTORY_HEAP_WORKER=1`; off by default; needs `VICTORY_LATE_WAIT`).
//!
//! Port of fxceless's victory-fork d4a528ba (fork pull request 1007, its
//! worker commit 233a65f4), hand-adapted to this tree.
//!
//! [`crate::draw::State::heap_textures`] runs on whichever thread of the
//! title records the frame's first shader that picks textures by index: it
//! asks what of each due texture is mapped, makes the host texture, makes a
//! staging buffer and records the copy, up to 250 ms of every frame while a
//! backlog waits, and the threads waiting for that list wait with it. The
//! detiling of what it recorded then runs on the thread that submits. With
//! the switch the pass runs here, and the recording thread only wakes it.
//!
//! What keeps the picture as it was:
//! - the uploads are recorded into host lists of ours ([`Batch`]), which the
//!   next submission of the title's runs ahead of its own lists
//!   ([`take_ready`], from `Host::execute_late`): texture states are settled
//!   by submission order, as between any two lists (all late submissions go
//!   to the one host queue);
//! - a texture's host slot is given its view at that submission, not before
//!   ([`Publish`]): until then shaders see what the slot showed, never a
//!   texture without its contents. A slot the title wrote again meanwhile
//!   is left to the write's own turn in the queue. A slot the pass takes
//!   out of the queue without an upload of its own (a texture a table draw
//!   uploads, one the host writes) is made at once, as on the recording
//!   thread;
//! - the detiling done here is checked at submission as any texture's is:
//!   memory the title wrote in between is read again;
//! - a title texture the pass works on is held ([`crate::resource::hold`]),
//!   and so is the title's heap;
//! - the uploads before the title's decoration bake, which must be there
//!   for that draw, still run on the recording thread; the worker makes way
//!   for them and hands over what it recorded so far first ([`urgent`]);
//! - at a device removal the worker ends and the pass goes back to the
//!   recording threads, as without the switch; [`stop`] ends and joins it.
// shortcut: what a submission shows for a texture is up to a frame later
// than when the recording thread did it all; with a backlog it is earlier,
// since the pass has the whole frame instead of a slice of it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use crate::descriptor::Heap;
use crate::host::HostList;
use crate::object::This;

/// Whether the pass runs on its own thread now.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_WORKER").is_some_and(|v| v == "1")) && crate::host::late()
}

/// For the five-second line ([`log_interval`]), since the last one:
/// nanoseconds the worker was at work, of them detiling; lists given to
/// submissions, textures published, slots the title had written again,
/// textures in those lists whose detiling was not done here; passes;
/// times a recording thread waited for the worker to make way and its
/// nanoseconds; nanoseconds submissions spent taking the worker's lists.
pub(crate) static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];

/// One upload list holds at most this much texture memory, or this many
/// milliseconds of work, so that its textures show soon.
const LIST_BYTES: u64 = 48 << 20;
const LIST_MS: u128 = 40;

/// A view to give a host slot once its texture's upload is in the queue:
/// the title's heap, the slot, the record's handle, and the record's key as
/// it was when the upload was recorded.
pub(crate) struct Publish {
    heap: usize,
    index: u32,
    handle: u64,
    key: u64,
}

/// Holds a reference to a title heap.
struct HeldHeap(This);
// SAFETY: a reference counted title object, of which only the count and the
// parts behind locks are touched.
unsafe impl Send for HeldHeap {}

impl HeldHeap {
    fn new(heap: &Heap) -> Self {
        let this: This = std::ptr::from_ref(heap).cast();
        crate::object::add_ref(this);
        HeldHeap(this)
    }

    fn heap(&self) -> &Heap {
        crate::descriptor::heap(self.0)
    }
}

impl Drop for HeldHeap {
    fn drop(&mut self) {
        crate::object::release_boxed::<Heap>(self.0);
    }
}

/// A closed upload list, and what is published when it is submitted.
struct Ready {
    list: HostList,
    publish: Vec<Publish>,
    _heap: HeldHeap,
}
// SAFETY: D3D12 objects are free-threaded; the list is closed and touched
// by nobody until it is taken out.
unsafe impl Send for Ready {}

static READY: Mutex<Vec<Ready>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a pass on the worker's thread records into: one upload list after
/// the other, made when first needed.
pub(crate) struct Batch {
    heap: This,
    list: Option<HostList>,
    publish: Vec<Publish>,
    bytes: u64,
    began: std::time::Instant,
}

impl Batch {
    fn new(heap: &Heap) -> Self {
        Batch {
            heap: std::ptr::from_ref(heap).cast(),
            list: None,
            publish: Vec::new(),
            bytes: 0,
            began: std::time::Instant::now(),
        }
    }

    /// The open upload list.
    pub(crate) fn list(&mut self) -> Option<&HostList> {
        if self.list.is_none() {
            self.list = crate::list::fresh_list();
            self.began = std::time::Instant::now();
        }
        self.list.as_ref()
    }

    /// Slot `index` (its record at `handle`, then with `key`) shows its
    /// texture when the open list is submitted.
    pub(crate) fn publish(&mut self, index: u32, handle: u64, key: u64) {
        self.publish.push(Publish { heap: self.heap as usize, index, handle, key });
    }

    /// `bytes` more of texture memory in the open list, which is closed
    /// and handed over when it holds enough.
    pub(crate) fn uploaded(&mut self, bytes: u64) {
        self.bytes += bytes;
        if self.bytes >= LIST_BYTES || self.began.elapsed().as_millis() >= LIST_MS {
            self.flush();
        }
    }

    /// Detiles what the open list uploads, closes it and hands it to the
    /// next submission.
    fn flush(&mut self) {
        self.bytes = 0;
        let publish = std::mem::take(&mut self.publish);
        let Some(list) = self.list.take() else {
            return;
        };
        let started = std::time::Instant::now();
        let left = list.refresh_textures_ahead();
        STATS[1].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        STATS[5].fetch_add(left, Relaxed);
        list.close();
        let heap = HeldHeap::new(crate::descriptor::heap(self.heap));
        lock(&READY).push(Ready { list, publish, _heap: heap });
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.flush();
    }
}

/// The upload lists ready now, for the submission about to be made, which
/// runs them ahead of its own lists; their host slots are given their views.
/// Called by the thread that submits.
pub(crate) fn take_ready() -> Vec<HostList> {
    // (Asked without the lock by every submission of a run without it.)
    if JOB.get().is_none() {
        return Vec::new();
    }
    let started = std::time::Instant::now();
    let ready = std::mem::take(&mut *lock(&READY));
    STATS[2].fetch_add(ready.len() as u64, Relaxed);
    let lists = ready
        .into_iter()
        .map(|r| {
            let Ready { list, publish, _heap: heap } = r;
            for p in &publish {
                let Some(host) = crate::descriptor::heap(p.heap as This).host() else {
                    continue;
                };
                let shown = host.publish(p.index, p.handle, p.key);
                STATS[if shown { 3 } else { 4 }].fetch_add(1, Relaxed);
            }
            // The heap is let go only after its slots are published.
            drop(heap);
            list
        })
        .collect();
    STATS[9].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
    lists
}

/// The heap the next pass is over, and what wakes the worker.
struct Job {
    heap: Mutex<Option<HeldHeap>>,
    wake: Condvar,
}

static JOB: OnceLock<Job> = OnceLock::new();
/// The worker's thread, until [`stop`] joins it.
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// Set by [`stop`], or by the worker at a device removal: no more passes
/// here, the recording threads run them again.
static STOP: AtomicBool = AtomicBool::new(false);
/// The worker's thread is running.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Held by whoever runs the pass.
static PASS: Mutex<()> = Mutex::new(());
/// A recording thread waits for [`PASS`].
static URGENT: AtomicBool = AtomicBool::new(false);
/// The frame of the last [`log_interval`].
static LOGGED_AT: AtomicU64 = AtomicU64::new(0);

/// Starts the worker once; false if it is not running (no thread, or
/// stopped).
fn start() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started = *STARTED.get_or_init(|| {
        JOB.get_or_init(|| Job { heap: Mutex::new(None), wake: Condvar::new() });
        LOGGED_AT.store(crate::frame::current(), Relaxed);
        RUNNING.store(true, Relaxed);
        match std::thread::Builder::new().name("victory-heap".into()).spawn(worker) {
            Ok(handle) => {
                *lock(&THREAD) = Some(handle);
                tracing::info!("textures picked by index are uploaded by a thread of their own (VICTORY_HEAP_WORKER)");
                true
            }
            Err(e) => {
                RUNNING.store(false, Relaxed);
                tracing::warn!(error = %e, "no thread for textures picked by index");
                false
            }
        }
    });
    started && !STOP.load(Relaxed)
}

/// A pass over `heap` is due (once a frame). False if there is no worker:
/// the caller runs the pass itself.
pub(crate) fn kick(heap: &Heap) -> bool {
    if !on() {
        return false;
    }
    // Stopped (by the worker itself at a device removal, too), or the
    // device went: the worker's thread is joined, once, and the caller runs
    // the pass, as without the switch.
    if STOP.load(Relaxed) || crate::descriptor::host::device_removed().is_some() {
        stop();
        return false;
    }
    if !start() {
        return false;
    }
    let Some(job) = JOB.get() else {
        return false;
    };
    *lock(&job.heap) = Some(HeldHeap::new(heap));
    job.wake.notify_one();
    if STOP.load(Relaxed) {
        // Stopped meanwhile: nobody takes the pass; the caller runs it.
        let pending = lock(&job.heap).take();
        drop(pending);
        return false;
    }
    true
}

/// Takes the pass from the worker for the caller, which cannot wait for a
/// later submission: what the worker has recorded so far is handed over
/// first, so the caller's list runs behind it.
pub(crate) fn urgent() -> MutexGuard<'static, ()> {
    if let Ok(pass) = PASS.try_lock() {
        return pass;
    }
    let started = std::time::Instant::now();
    URGENT.store(true, Relaxed);
    let pass = lock(&PASS);
    URGENT.store(false, Relaxed);
    STATS[7].fetch_add(1, Relaxed);
    STATS[8].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
    pass
}

/// Whether the worker is to stop where it is ([`urgent`], [`stop`]).
pub(crate) fn make_way() -> bool {
    URGENT.load(Relaxed) || STOP.load(Relaxed)
}

/// Ends the worker and waits for its thread (not from the worker itself).
/// Passes run on the recording threads from then on, as without the
/// switch; lists it handed over still go with the next submission.
pub(crate) fn stop() {
    STOP.store(true, Relaxed);
    if let Some(job) = JOB.get() {
        // Under the lock: the worker looks at `STOP` with it held before
        // it waits, so this wake is not lost.
        let pending = lock(&job.heap).take();
        job.wake.notify_all();
        drop(pending);
    }
    let handle = lock(&THREAD).take();
    if let Some(handle) = handle {
        if handle.thread().id() == std::thread::current().id() {
            return;
        }
        if handle.join().is_err() {
            tracing::warn!("the thread for textures picked by index ended in a panic");
        }
    }
}

/// Marks the worker as ended when its thread leaves, whichever way.
struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Relaxed);
    }
}

fn worker() {
    let _running = Running;
    let Some(job) = JOB.get() else {
        return;
    };
    // A lane of its own in `VICTORY_FRAME_WHY2`'s timeline (lab-v8a-s).
    crate::frame_why::role(crate::frame_why::Role::Heap);
    loop {
        let waiting = crate::frame_why::span(crate::frame_why::Phase::Idle);
        let heap = {
            let mut next = lock(&job.heap);
            loop {
                if STOP.load(Relaxed) {
                    return;
                }
                match next.take() {
                    Some(heap) => break heap,
                    None => next = job.wake.wait(next).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        drop(waiting);
        if crate::descriptor::host::device_removed().is_some() {
            STOP.store(true, Relaxed);
            tracing::warn!("the host device went: textures picked by index are uploaded by the recording threads again");
            return;
        }
        let started = std::time::Instant::now();
        let _working = crate::frame_why::span(crate::frame_why::Phase::Prepare);
        {
            let _pass = lock(&PASS);
            let heap = heap.heap();
            if let Some(host) = heap.host() {
                let mut batch = Batch::new(heap);
                let _ = crate::draw::State::heap_textures(crate::draw::TexturesInto::Ahead(&mut batch), heap, host);
            }
        }
        drop(heap);
        STATS[0].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        STATS[6].fetch_add(1, Relaxed);
        // Whoever wants the pass now has it first.
        while URGENT.load(Relaxed) && !STOP.load(Relaxed) {
            std::thread::yield_now();
        }
    }
}

/// The worker's line, every five seconds with the texture queue's: per
/// frame since the last one.
pub(crate) fn log_interval() {
    if JOB.get().is_none() {
        return;
    }
    let now = crate::frame::current();
    let frames = now.saturating_sub(LOGGED_AT.swap(now, Relaxed)).max(1) as f64;
    let [busy, detiling, lists, shown, again, left, passes, waits, waited, taking] =
        std::array::from_fn::<f64, 10, _>(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    let state = if RUNNING.load(Relaxed) { "running" } else { "stopped: the recording threads run the pass" };
    tracing::info!(
        "textures picked by index, their own thread (VICTORY_HEAP_WORKER, {state}), per frame over {frames:.0} frames: \
         {:.1} ms at work in {passes:.2} passes, of them {:.1} ms detiling; {lists:.1} lists handed to submissions, \
         {shown:.0} slots shown, {again:.1} written again before, {left:.1} textures left to the submission to detile; \
         recording threads waited {:.2} ms ({waits:.2} times) for it to make way, submissions {:.2} ms taking its lists",
        busy / 1e6,
        detiling / 1e6,
        waited / 1e6,
        taking / 1e6
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker waits for a pass without one, and [`stop`] ends and joins
    /// it; no pass is handed to it after that.
    #[test]
    fn the_worker_stops_and_joins() {
        assert!(start());
        assert!(RUNNING.load(Relaxed));
        assert!(take_ready().is_empty());
        stop();
        assert!(!RUNNING.load(Relaxed));
        assert!(lock(&THREAD).is_none());
        assert!(!start());
        assert!(make_way());
        // The pass is free for a recording thread.
        drop(urgent());
    }
}
