//! SPDX-License-Identifier: GPL-3.0-or-later
//!
//! Renderer port 6 (`VICTORY_PORT_GUARD=1`, off by default): the guards'
//! lock taken shared by write faults, by refreshes guarding their ranges and
//! by publications of the light refresh, each holding the claims of the
//! blocks whose state it changes. After avatarmanz's renderer port 6
//! (9d723e88, 2edf8446, 88b63a2c); written anew for this tree, whose guards
//! differ.
//!
//! Without the switch nothing here runs: [`GUARDS`] is taken alone by all
//! that changes it, as before.
//!
//! With it:
//! - The guards' sets (protected, clean, absent, aliased) are of atomic
//!   words ([`super::blocks`]). A block's state changes only while its claim
//!   ([`BUSY`], [`Claims`]) is held, by a thread holding [`GUARDS`] shared;
//!   or by a thread holding it alone (mappings, kernel writes, our own
//!   writes, frees, write windows, quarantine: all as before), which no
//!   claim holder can be beside. One exception, on the safe side: a refresh
//!   that finds a group of views not all claimed and read-only clears their
//!   clean bits (and their published bits) without their claims.
//! - A write fault takes the lock shared and waits for the claims of its
//!   block and, if the block is aliased, of the page's other views, in
//!   ascending block order; if the set of views changed meanwhile it lets
//!   go and waits again for the new set. Then it opens them (`open`).
//! - A refresh only tries for claims: a block it cannot claim stays as it
//!   is and is told written (looked at again at the next asking). A block
//!   becomes clean only when it and every view of its page are read-only
//!   and claimed by the refresh, after the views were guarded, so that no
//!   reader without claims sees a clean block with a writable view.
//! - A publication of the light refresh waits for the claims of the block
//!   and its views (ascending), then checks and sets the published bit
//!   (`light::publish_block`, whose bit words change by compare and swap).
//! - The heat of `VICTORY_GUARD_HOT` (ours, unrelated to their switch of
//!   that name) and the light refresh's write windows change only under the
//!   lock alone: with `VICTORY_GUARD_HOT=1` the port stays off.
//!
//! No two threads wait for each other: only faults and publications wait
//! for claims, each only for blocks above every claim it holds; holders of
//! claims wait for nothing but the views table's lock (a leaf), and never
//! take [`GUARDS`] again. Lock order: the poll regions (`VICTORY_WATCH_SPLIT`,
//! in stripe order), the journal, [`GUARDS`], claims (ascending), the views
//! table.
//!
//! [`GUARDS`]: super::GUARDS

use super::light::{self, Cause};
use super::{BLOCK, Guards, MANY_VIEWS, PAGE, blocks::Blocks};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether the port is on: `VICTORY_PORT_GUARD=1`, and not with
/// `VICTORY_GUARD_HOT=1` (whose heat changes only under the lock alone).
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if IN_TEST.get() {
        return !super::hot_guards();
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let asked = std::env::var("VICTORY_PORT_GUARD").is_ok_and(|v| v.trim() == "1");
        if asked && super::hot_guards() {
            tracing::warn!("VICTORY_PORT_GUARD=1 is left off with VICTORY_GUARD_HOT=1");
        } else if asked {
            tracing::info!("renderer port 6: the guards' lock shared, a block's claim held while its state changes (VICTORY_PORT_GUARD=1)");
        }
        asked && !super::hot_guards()
    })
}

#[cfg(test)]
thread_local! {
    /// [`on`] for what this thread does.
    pub(super) static IN_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The blocks whose state a thread holding the guards shared is changing:
/// a bit each, set by who changes it ([`Claims`]).
static BUSY: Blocks = Blocks::new();

thread_local! {
    /// The claims this thread holds: a fault of its own (none is known; no
    /// claim is held where title memory is written) does not wait for them.
    static HELD: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn held_here(block: usize) -> bool {
    HELD.with_borrow(|held| held.contains(&block))
}

/// Claims of blocks ([`BUSY`]), let go when dropped.
pub(super) struct Claims(Vec<usize>);

impl Claims {
    pub(super) fn none() -> Claims {
        Claims(Vec::new())
    }

    /// The claims of `blocks` (ascending, each once), waited for one after
    /// the other; those this thread holds already are not waited for.
    /// `fault`: counted as a fault's.
    pub(super) fn wait_for(blocks: &[usize], fault: bool) -> Claims {
        debug_assert!(blocks.windows(2).all(|w| w[0] < w[1]));
        let mut claims = Claims::none();
        let mut waited = None;
        for &block in blocks {
            if held_here(block) {
                count(SELF_HELD, 1);
                continue;
            }
            let mut tries = 0u32;
            while !BUSY.insert(block) {
                waited.get_or_insert_with(std::time::Instant::now);
                tries += 1;
                if tries < 64 {
                    std::hint::spin_loop();
                } else {
                    // Another thread's protection change of the block.
                    std::thread::yield_now();
                }
            }
            claims.took(block);
        }
        if let Some(since) = waited {
            count(if fault { FAULTS_WAITED } else { PUBLISH_WAITED }, 1);
            count(if fault { FAULT_WAIT_NS } else { PUBLISH_WAIT_NS }, since.elapsed().as_nanos() as u64);
        }
        claims
    }

    fn took(&mut self, block: usize) {
        self.0.push(block);
        HELD.with_borrow_mut(|held| held.push(block));
    }

    /// The claim of `block` if it is free (or held already); nothing waited
    /// for.
    pub(super) fn try_one(&mut self, block: usize) -> bool {
        if self.holds(block) {
            return true;
        }
        count(TRIED, 1);
        if BUSY.insert(block) {
            self.took(block);
            true
        } else {
            count(FAILED, 1);
            false
        }
    }

    /// Whether `block`'s claim is held by this (or by this thread).
    pub(super) fn holds(&self, block: usize) -> bool {
        self.0.contains(&block) || held_here(block)
    }
}

impl Drop for Claims {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        HELD.with_borrow_mut(|held| held.retain(|b| !self.0.contains(b)));
        for &block in &self.0 {
            BUSY.remove(block);
        }
    }
}

/// Counts since the last line ([`line`]).
static COUNTS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const SHARED: usize = 0;
const SHARED_WAIT_NS: usize = 1;
const EXCLUSIVE: usize = 2;
const TRIED: usize = 3;
const FAILED: usize = 4;
const FAULTS: usize = 5;
const FAULTS_WAITED: usize = 6;
const FAULT_WAIT_NS: usize = 7;
const RESTARTS: usize = 8;
const GUARDS_SHARED: usize = 9;
const PUBLISHED: usize = 10;
const PUBLISH_WAITED: usize = 11;
const PUBLISH_WAIT_NS: usize = 12;
const SELF_HELD: usize = 13;

fn count(what: usize, n: u64) {
    COUNTS[what].fetch_add(n, Relaxed);
}

/// The guards taken alone, with the port on: counted ([`super::guards`]).
pub(super) fn exclusive_taken() {
    count(EXCLUSIVE, 1);
}

/// The guards, shared: what changes a block's state holds its claim.
#[track_caller]
pub(super) fn guards_shared() -> super::Hold<std::sync::RwLockReadGuard<'static, Guards>> {
    let began = std::time::Instant::now();
    let g = super::guards_read();
    count(SHARED, 1);
    count(SHARED_WAIT_NS, began.elapsed().as_nanos() as u64);
    g
}

/// The port's line of the frame lines, per frame.
pub(crate) fn line(frames: f64) {
    let n: Vec<f64> = COUNTS.iter().map(|c| c.swap(0, Relaxed) as f64 / frames).collect();
    if !on() {
        return;
    }
    tracing::info!(
        "renderer port 6 (VICTORY_PORT_GUARD=1) per frame: the guards' lock taken shared {:.0} times ({:.2} ms waiting), alone {:.0} times; \
         claims tried {:.0}, failed {:.1} (blocks left written); faults opened shared {:.1}, {:.1} of them waited for claims ({:.2} ms), \
         {:.1} waited again for a changed set of views; refreshes' guards done shared {:.1}; publications shared {:.1}, \
         {:.1} waited for claims ({:.2} ms); claims a thread already held {:.1}",
        n[SHARED],
        n[SHARED_WAIT_NS] / 1e6,
        n[EXCLUSIVE],
        n[TRIED],
        n[FAILED],
        n[FAULTS],
        n[FAULTS_WAITED],
        n[FAULT_WAIT_NS] / 1e6,
        n[RESTARTS],
        n[GUARDS_SHARED],
        n[PUBLISHED],
        n[PUBLISH_WAITED],
        n[PUBLISH_WAIT_NS] / 1e6,
        n[SELF_HELD],
    );
}

/// The blocks a fault at `block` opens ([`super::open`] of one byte): the
/// block, and if it is aliased every view of its page; ascending.
fn fault_views(g: &Guards, block: usize) -> Vec<usize> {
    let mut views = vec![block];
    if g.aliased.contains(block) {
        views.extend(crate::inplace::pages_of(block * BLOCK).into_iter().map(|at| at / BLOCK));
        if light::on() {
            views.extend(crate::inplace::guard_views_of(block * BLOCK).into_iter().map(|at| at / BLOCK));
        }
    }
    views.sort_unstable();
    views.dedup();
    views
}

/// A write fault at `at` with the port on ([`super::handle_write_fault`]):
/// the guards shared, the claims of the block and its views waited for.
/// Whether the write may go on.
pub(super) fn handle_write_fault(at: usize, retry_writable: bool) -> bool {
    let block = at / BLOCK;
    if block >= super::blocks::SPACE {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let g = guards_shared();
    #[cfg(feature = "diagnostics")]
    let wait = started.elapsed();
    let cause = if retry_writable { Cause::Fault } else { Cause::Plain };
    let mut views = vec![block];
    let (guarded, _opened) = loop {
        let claims = Claims::wait_for(&views, true);
        let now = fault_views(&g, block);
        if now.iter().any(|&b| !claims.holds(b)) {
            // Aliased meanwhile, or another view mapped: their claims too.
            count(RESTARTS, 1);
            views = now;
            continue;
        }
        let opened = super::open(&g, at, at.saturating_add(1), cause);
        drop(claims);
        break opened;
    };
    if guarded {
        count(FAULTS, 1);
    }
    // (A second writer may fault before the first opens the block.)
    let resume = guarded || (retry_writable && super::writable(at));
    drop(g);
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::GuardHandlerNs, started.elapsed().as_nanos() as u64);
        add(C::GuardHandlerWaitNs, wait.as_nanos() as u64);
        add(C::GuardHandlerCalls, 1);
        if guarded {
            add(C::GuardFaults, 1);
            add(C::FaultOpenedPages, (_opened * (BLOCK / super::KERNEL_PAGE)) as u64);
        }
    }
    resume
}

/// [`super::guard_blocks`] with the port on, the guards held shared: the
/// pages of `[first, last)`'s blocks written since they were last guarded,
/// guarding them again. A block whose claim another holds is told written
/// and left as it is.
pub(super) fn guard_blocks(g: &Guards, first: usize, last: usize) -> Vec<usize> {
    if light::opaque_io() {
        return (first..last).collect();
    }
    count(GUARDS_SHARED, 1);
    let mut written = Vec::new();
    if g.clean.contains_range(first, last) {
        // Nothing written, mapped or new since the range was last taken.
        return written;
    }
    let mut claims = Claims::none();
    let mut run: Option<usize> = None;
    let mut uncommitted_until = first;
    let mut block = g.clean.next_uncovered(first, last);
    while block < last {
        if !claims.try_one(block) {
            // A fault opens it, or another refresh guards it: as it is,
            // written, and looked at again at the next asking.
            if let Some(from) = run.take() {
                protect(g, from, block, &mut claims);
            }
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            block = g.clean.next_uncovered(block + 1, last);
            continue;
        }
        let mut open = true;
        if g.absent.contains(block) {
            // As `super::guard_blocks`: still wholly uncommitted, left.
            if block >= uncommitted_until {
                uncommitted_until = super::uncommitted_end(block);
            }
            open = block >= uncommitted_until;
            if open {
                g.absent.remove(block);
            }
        }
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            let aliased = g.aliased.contains(block);
            let check_aliases = aliased && (super::alias_lazy() || super::reuse_arm());
            if g.protected.contains(block) && (!aliased || check_aliases) {
                if let Some(from) = run.take() {
                    protect(g, from, block, &mut claims);
                }
                // Written through the runtime, still read-only; or through
                // another view of its pages (`alias_lazy`): clean once its
                // other views are read-only again.
                if !check_aliases || guard_aliases(g, block, &mut claims) {
                    g.clean.insert(block);
                }
            } else {
                run.get_or_insert(block);
            }
        } else if let Some(from) = run.take() {
            protect(g, from, block, &mut claims);
        }
        let next = block + 1;
        let next_dirty = g.clean.next_uncovered(next, last);
        if next_dirty != next
            && let Some(from) = run.take()
        {
            protect(g, from, next, &mut claims);
        }
        block = next_dirty;
    }
    if let Some(from) = run.take() {
        protect(g, from, last, &mut claims);
    }
    written
}

/// [`super::protect`] of claimed blocks `[from, to)`: read-only, and every
/// other view of their pages too where its claim can be had. Each block is
/// noted clean only once its views are all claimed and read-only; else they
/// are all noted written ([`super::check_group`]), as are the views of a
/// block that could not be made read-only.
fn protect(g: &Guards, from: usize, to: usize, claims: &mut Claims) {
    let armed = protect_here(g, from, to);
    for block in from..to {
        let covered = !super::view_guard() || guard_aliases(g, block, claims);
        if covered && armed.binary_search(&block).is_ok() {
            g.clean.insert(block);
        }
    }
}

/// [`super::protect_here`], not noting the blocks clean: the blocks of
/// `[from, to)` made read-only (and noted so); those not committed are
/// noted absent.
fn protect_here(g: &Guards, from: usize, to: usize) -> Vec<usize> {
    if super::arm(g, from, to) {
        g.protected.insert_range(from, to);
        return (from..to).collect();
    }
    let mut armed = Vec::new();
    for block in from..to {
        if super::arm(g, block, block + 1) {
            g.protected.insert(block);
            armed.push(block);
        } else if block < super::uncommitted_end(block) {
            g.absent.insert(block);
        }
    }
    armed
}

/// The other views of `block`'s page made read-only where their claims can
/// be had ([`super::guard_aliases`]); whether all of them, `block` among
/// them, are claimed and read-only. If not, every view is noted written.
fn guard_aliases(g: &Guards, block: usize, claims: &mut Claims) -> bool {
    let shown = crate::inplace::guard_views_of(block * BLOCK);
    // One view (nothing else to guard), or the page shown at thousands.
    if shown.len() < 2 || shown.len() > MANY_VIEWS {
        return true;
    }
    #[cfg(feature = "diagnostics")]
    super::ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for &at in &shown {
        let alias = at / BLOCK;
        // (Without its claim: a fault that has not seen it aliased opens
        // only its own block, and this one is then not claimed.)
        g.aliased.insert(alias);
        if alias != block && claims.try_one(alias) && !g.protected.contains(alias) && super::arm(g, alias, alias + 1) {
            g.protected.insert(alias);
        }
    }
    let covered = shown.iter().all(|&at| claims.holds(at / BLOCK) && g.protected.contains(at / BLOCK));
    if !covered {
        for &at in &shown {
            light::clear(at / BLOCK, Cause::Alias);
            g.clean.remove(at / BLOCK);
        }
    }
    covered
}

/// The guards as held by who looks at a range and guards it: alone (the
/// port off), or shared.
pub(super) enum Held {
    Alone(super::Hold),
    Shared(super::Hold<std::sync::RwLockReadGuard<'static, Guards>>),
}

impl Held {
    /// Shared with the port on, else alone.
    pub(super) fn take() -> Held {
        if on() { Held::Shared(guards_shared()) } else { Held::Alone(super::guards()) }
    }
}

impl std::ops::Deref for Held {
    type Target = Guards;
    fn deref(&self) -> &Guards {
        match self {
            Held::Alone(g) => g,
            Held::Shared(g) => g,
        }
    }
}

/// The blocks a publication of `p` claims: `p` and its page's other views.
pub(super) fn publish_views(p: usize) -> Vec<usize> {
    let mut views: Vec<usize> = crate::inplace::guard_views_of(p * BLOCK).into_iter().map(|at| at / BLOCK).collect();
    views.push(p);
    views.sort_unstable();
    views.dedup();
    views
}

/// A publication made with the guards shared.
pub(super) fn published() {
    count(PUBLISHED, 1);
}

#[cfg(test)]
pub(super) fn claimed(block: usize) -> bool {
    BUSY.contains(block)
}

/// Faults opened under the shared lock, not taken by a line.
#[cfg(test)]
pub(super) fn count_of_faults() -> u64 {
    COUNTS[FAULTS].load(Relaxed)
}

