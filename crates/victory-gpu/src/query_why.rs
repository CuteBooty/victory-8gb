//! Query API observations. Only counts leave this module. A recorded resolve
//! and a later overlapping Map indicate read intent, not an observed CPU load.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex, MutexGuard};

const TYPES: [&str; 5] = [
    "occlusion",
    "binary",
    "timestamp",
    "pipeline_stats",
    "other",
];
fn kind(k: u32) -> usize {
    (k as usize).min(4)
}
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Union of disjoint, non-adjacent half-open ranges. A batch resolve costs
/// one insertion even if it covers thousands of results.
#[derive(Default)]
struct Ranges(BTreeMap<u64, u64>);
impl Ranges {
    fn add(&mut self, mut a: u64, mut b: u64) {
        if a >= b {
            return;
        }
        if let Some((&lo, &hi)) = self.0.range(..=a).next_back() {
            if hi >= b {
                return;
            }
            if hi >= a {
                a = lo;
            }
        }
        while let Some((&lo, &hi)) = self.0.range(a..=b).next() {
            let _ = self.0.remove(&lo);
            b = b.max(hi);
        }
        let _ = self.0.insert(a, b);
    }
    fn overlaps(&self, a: u64, b: u64) -> bool {
        a < b && self.0.range(..b).next_back().is_some_and(|(_, &hi)| hi > a)
    }
    fn len(&self) -> u64 {
        self.0.iter().map(|(a, b)| b - a).sum()
    }
}

#[derive(Default)]
pub(crate) struct Recording {
    draws: AtomicU64,
    elements: AtomicU64,
    unsized_draws: AtomicU64,
    open: Mutex<HashMap<(usize, u32, u32), [u64; 3]>>,
}
impl Recording {
    pub(crate) fn draw(&self, elements: Option<u64>) {
        self.draws.fetch_add(1, Relaxed);
        if let Some(n) = elements {
            self.elements.fetch_add(n, Relaxed);
        } else {
            self.unsized_draws.fetch_add(1, Relaxed);
        }
    }
    pub(crate) fn mark(&self, heap: usize, ty: u32, index: u32, begin: bool) {
        let mut q = lock(&QUERIES);
        let k = kind(ty);
        q.indices[k]
            .entry(heap)
            .or_default()
            .add(index.into(), u64::from(index) + 1);
        let row = &mut q.window[k];
        let key = (heap, ty, index);
        let recorded = [
            self.draws.load(Relaxed),
            self.elements.load(Relaxed),
            self.unsized_draws.load(Relaxed),
        ];
        if begin {
            row.begun += 1;
            let _ = lock(&self.open).insert(key, recorded);
        } else {
            row.ended += 1;
            if let Some(first) = lock(&self.open).remove(&key) {
                let inside = recorded[0].wrapping_sub(first[0]);
                row.elements = row
                    .elements
                    .saturating_add(recorded[1].wrapping_sub(first[1]));
                row.unsized_draws += recorded[2].wrapping_sub(first[2]);
                row.draws += inside;
                row.pairs[inside.min(2) as usize] += 1;
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Counts {
    begun: u64,
    ended: u64,
    batches: [u64; 4],
    results: u64,
    indices: u64,
    destinations: u64,
    bytes: u64,
    maps: u64,
    copies: u64,
    draws: u64,
    elements: u64,
    unsized_draws: u64,
    pairs: [u64; 3],
}

#[derive(Default)]
struct Queries {
    // Additive counts accumulate directly across the window. Unique keys
    // are per frame and contribute their cardinality once at each boundary.
    window: [Counts; 5],
    indices: [HashMap<usize, Ranges>; 5],
    destinations: [HashSet<usize>; 5],
    // Live buffer objects only. Stored ranges indicate historical resolves
    // (a later ordinary write can overwrite them).
    buffers: HashMap<usize, [Ranges; 5]>,
}
static QUERIES: LazyLock<Mutex<Queries>> = LazyLock::new(Default::default);

pub(crate) fn resolve(heap: usize, ty: u32, first: u32, count: u32) {
    let mut q = lock(&QUERIES);
    let k = kind(ty);
    q.window[k].results += u64::from(count);
    let bin = match count {
        0 => 0,
        1 => 1,
        2..=8 => 2,
        _ => 3,
    };
    q.window[k].batches[bin] += 1;
    q.indices[k]
        .entry(heap)
        .or_default()
        .add(first.into(), u64::from(first) + u64::from(count));
}
pub(crate) fn destination(ty: u32, buffer: usize, from: u64, to: u64) {
    if from >= to {
        return;
    }
    let mut q = lock(&QUERIES);
    let k = kind(ty);
    q.window[k].bytes += to - from;
    q.destinations[k].insert(buffer);
    q.buffers.entry(buffer).or_default()[k].add(from, to);
}
pub(crate) fn access(buffer: usize, from: u64, to: u64, copy: bool) {
    if !crate::frame_why::enabled() {
        return;
    }
    let mut q = lock(&QUERIES);
    let Some(ranges) = q.buffers.get(&buffer) else {
        return;
    };
    let matches = ranges.each_ref().map(|r| r.overlaps(from, to));
    for (k, hit) in matches.into_iter().enumerate() {
        if copy {
            q.window[k].copies += u64::from(hit);
        } else {
            q.window[k].maps += u64::from(hit);
        }
    }
}
pub(crate) fn forget(buffer: usize) {
    if crate::frame_why::enabled() {
        let _ = lock(&QUERIES).buffers.remove(&buffer);
    }
}

pub(crate) fn frame(keep: bool, report: bool) -> Option<[Counts; 5]> {
    let mut q = lock(&QUERIES);
    for k in 0..5 {
        let unique: u64 = q.indices[k].drain().map(|(_, r)| r.len()).sum();
        let dests = q.destinations[k].len() as u64;
        q.destinations[k].clear();
        if keep {
            q.window[k].indices += unique;
            q.window[k].destinations += dests;
        } else {
            q.window[k] = Counts::default();
        }
    }
    report.then(|| std::mem::take(&mut q.window))
}
pub(crate) fn log(frames: f64, totals: [Counts; 5]) {
    for (ty, row) in TYPES.into_iter().zip(totals) {
        let avg = |n: u64| n as f64 / frames;
        let [
            begin,
            end,
            results,
            indices,
            dsts,
            bytes,
            maps,
            copies,
            draws,
        ] = [
            row.begun,
            row.ended,
            row.results,
            row.indices,
            row.destinations,
            row.bytes,
            row.maps,
            row.copies,
            row.draws,
        ]
        .map(avg);
        let elements = avg(row.elements);
        let unsized_draws = avg(row.unsized_draws);
        let calls: u64 = row.batches.iter().sum();
        let resolves = avg(calls);
        let [b0, b1, b8, big] = row.batches.map(avg);
        let [zero, one, many] = row.pairs.map(avg);
        let batch = if resolves == 0.0 {
            0.0
        } else {
            results / resolves
        };
        tracing::info!(
            "why2 queries {ty} per frame begin={begin:.2} end={end:.2} resolves={resolves:.2} results={results:.2} results/resolve={batch:.2} batches_0/1/2to8/9plus={b0:.2}/{b1:.2}/{b8:.2}/{big:.2} distinct_indices={indices:.2} destination_buffers={dsts:.2} bytes={bytes:.1} overlap_map_intent={maps:.2} overlap_copy_source={copies:.2} enclosed_draw_calls={draws:.2} direct_vertex_or_index_elements={elements:.1} unsized_draw_calls={unsized_draws:.2} pairs_0/1/2plus={zero:.2}/{one:.2}/{many:.2} window_begin/end/resolve={}/{}/{calls} predication=unobserved native_cpu_reads=unobserved",
            row.begun,
            row.ended
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unions_marks_and_resolves_and_matches_only_nonempty_overlaps() {
        let mut r = Ranges::default();
        for (a, b) in [(8, 10), (4, 5), (7, 8), (3, 9), (12, 14), (6, 6)] {
            r.add(a, b);
        }
        assert_eq!(r.len(), 9);
        assert!(!r.overlaps(10, 12));
        assert!(!r.overlaps(12, 12));
        assert!(r.overlaps(10, 13));
    }
}
