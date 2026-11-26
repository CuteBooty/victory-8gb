//! SPDX-License-Identifier: GPL-3.0-or-later
//!
//! Sets of 64 KiB blocks of the address space, a bit each, in atomic words:
//! a look at a range is a few words, and a thread may change one block's
//! bit while others read and change the bits of other blocks of the same
//! word. After avatarmanz's renderer port 6 (9d723e88, its `Bits` and
//! `Blocks`); written anew for this tree.
//!
//! The words are made a piece (4 GiB of address space) at a time when a bit
//! there is first set, and let go only with the set. Blocks past the user
//! address space (128 TiB) are never in a set.

use std::sync::atomic::{
    AtomicPtr, AtomicU64, AtomicUsize,
    Ordering::{AcqRel, Acquire, SeqCst},
};

/// Words of one piece: 65,536 blocks, 4 GiB.
const WORDS: usize = 1 << 10;
const IN_PIECE: usize = WORDS * 64;
/// Blocks of the user address space.
pub(super) const SPACE: usize = (1usize << 47) / super::BLOCK;
const PIECES: usize = SPACE / IN_PIECE;

type Piece = [AtomicU64; WORDS];

pub(super) struct Blocks {
    pieces: [AtomicPtr<Piece>; PIECES],
    /// How many are set (for `is_empty`).
    count: AtomicUsize,
}

/// The bits of word `word` that stand for blocks of `[first, last)`.
fn in_word(word: usize, first: usize, last: usize) -> u64 {
    let base = word * 64;
    let lo = first.saturating_sub(base).min(64);
    let hi = last.saturating_sub(base).min(64);
    if lo >= hi {
        return 0;
    }
    let upper = if hi == 64 { u64::MAX } else { (1u64 << hi) - 1 };
    upper & !((1u64 << lo) - 1)
}

impl Blocks {
    pub(super) const fn new() -> Self {
        Self { pieces: [const { AtomicPtr::new(std::ptr::null_mut()) }; PIECES], count: AtomicUsize::new(0) }
    }

    fn piece(&self, word: usize) -> Option<&Piece> {
        let p = self.pieces.get(word / WORDS)?.load(Acquire);
        // SAFETY: null, or a piece made by `made`, freed only with the set.
        unsafe { p.as_ref() }
    }

    fn made(&self, word: usize) -> Option<&Piece> {
        let place = self.pieces.get(word / WORDS)?;
        let p = place.load(Acquire);
        if !p.is_null() {
            // SAFETY: as in `piece`.
            return unsafe { p.as_ref() };
        }
        let new: *mut Piece = Box::into_raw(Box::new([const { AtomicU64::new(0) }; WORDS]));
        match place.compare_exchange(std::ptr::null_mut(), new, AcqRel, Acquire) {
            // SAFETY: made just now and placed.
            Ok(_) => unsafe { new.as_ref() },
            Err(there) => {
                // SAFETY: made just now; another thread's piece is there.
                drop(unsafe { Box::from_raw(new) });
                // SAFETY: as in `piece`.
                unsafe { there.as_ref() }
            }
        }
    }

    fn word(&self, word: usize) -> u64 {
        self.piece(word).map_or(0, |piece| piece[word % WORDS].load(SeqCst))
    }

    pub(super) fn contains(&self, block: usize) -> bool {
        block < SPACE && self.word(block / 64) & 1 << (block % 64) != 0
    }

    /// Sets it; whether it was not set.
    pub(super) fn insert(&self, block: usize) -> bool {
        if block >= SPACE {
            return false;
        }
        let bit = 1u64 << (block % 64);
        let Some(piece) = self.made(block / 64) else {
            return false;
        };
        let new = piece[block / 64 % WORDS].fetch_or(bit, SeqCst) & bit == 0;
        if new {
            self.count.fetch_add(1, SeqCst);
        }
        new
    }

    /// Clears it; whether it was set.
    pub(super) fn remove(&self, block: usize) -> bool {
        if block >= SPACE {
            return false;
        }
        let bit = 1u64 << (block % 64);
        let was = self.piece(block / 64).is_some_and(|piece| piece[block / 64 % WORDS].fetch_and(!bit, SeqCst) & bit != 0);
        if was {
            self.count.fetch_sub(1, SeqCst);
        }
        was
    }

    pub(super) fn extend(&self, blocks: impl IntoIterator<Item = usize>) {
        for block in blocks {
            self.insert(block);
        }
    }

    pub(super) fn insert_range(&self, first: usize, last: usize) {
        let last = last.min(SPACE);
        for word in first / 64..last.div_ceil(64) {
            let mask = in_word(word, first, last);
            if mask == 0 {
                continue;
            }
            if let Some(piece) = self.made(word) {
                let old = piece[word % WORDS].fetch_or(mask, SeqCst);
                self.count.fetch_add((mask & !old).count_ones() as usize, SeqCst);
            }
        }
    }

    pub(super) fn remove_range(&self, first: usize, last: usize) {
        let last = last.min(SPACE);
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            let Some(piece) = self.piece(word) else {
                // Nothing set in the piece: on to the next.
                word = (word / WORDS + 1) * WORDS;
                continue;
            };
            let mask = in_word(word, first, last);
            if mask != 0 {
                let old = piece[word % WORDS].fetch_and(!mask, SeqCst);
                self.count.fetch_sub((mask & old).count_ones() as usize, SeqCst);
            }
            word += 1;
        }
    }

    /// All of them cleared.
    pub(super) fn clear(&self) {
        self.remove_range(0, SPACE);
    }

    /// Those set of `[first, last)` as they are when looked at, in order.
    pub(super) fn range(&self, blocks: std::ops::Range<usize>) -> impl Iterator<Item = usize> + '_ {
        let (first, last) = (blocks.start, blocks.end.min(SPACE).max(blocks.start));
        let mut word = first / 64;
        let end = last.div_ceil(64);
        let mut bits = 0u64;
        let mut base = 0;
        std::iter::from_fn(move || loop {
            if bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                return Some(base + bit);
            }
            if word >= end {
                return None;
            }
            match self.piece(word) {
                Some(piece) => {
                    bits = piece[word % WORDS].load(SeqCst) & in_word(word, first, last);
                    base = word * 64;
                    word += 1;
                }
                None => word = (word / WORDS + 1) * WORDS,
            }
        })
    }

    /// Whether every block of `[first, last)` is set (yes for none).
    pub(super) fn contains_range(&self, first: usize, last: usize) -> bool {
        self.next_uncovered(first, last) >= last
    }

    /// Whether any block of `[first, last)` is set.
    pub(super) fn any(&self, first: usize, last: usize) -> bool {
        self.range(first..last).next().is_some()
    }

    /// The first block from `block` on that is not set, or `last`.
    pub(super) fn next_uncovered(&self, block: usize, last: usize) -> usize {
        if block >= last {
            return last;
        }
        if block >= SPACE {
            return block;
        }
        let end = last.min(SPACE);
        for word in block / 64..end.div_ceil(64) {
            let free = in_word(word, block, end) & !self.word(word);
            if free != 0 {
                return word * 64 + free.trailing_zeros() as usize;
            }
        }
        if end < last { end } else { last }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.count.load(SeqCst) == 0
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.count.load(SeqCst)
    }
}

impl Drop for Blocks {
    fn drop(&mut self) {
        for place in &mut self.pieces {
            let p = std::mem::replace(place.get_mut(), std::ptr::null_mut());
            if !p.is_null() {
                // SAFETY: made by `made` and no longer reachable.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Against a `BTreeSet` of the same, at random; and the runs' looks.
    #[test]
    fn blocks_are_a_set() {
        let set = Box::new(Blocks::new());
        let mut other = BTreeSet::new();
        let mut seed = 7u64;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        // Two places: a word edge, and a piece edge.
        let bases = [1000usize, IN_PIECE - 70];
        for _ in 0..20_000 {
            let base = bases[next(2)];
            let (a, n) = (base + next(200), next(140));
            match next(6) {
                0 => assert_eq!(set.insert(a), other.insert(a)),
                1 => assert_eq!(set.remove(a), other.remove(&a)),
                2 => {
                    set.insert_range(a, a + n);
                    other.extend(a..a + n);
                }
                3 => {
                    set.remove_range(a, a + n);
                    other.retain(|b| !(a..a + n).contains(b));
                }
                4 => assert_eq!(set.contains_range(a, a + n), (a..a + n).all(|b| other.contains(&b))),
                _ => {
                    let want = (a..a + n).find(|b| !other.contains(b)).unwrap_or(a + n);
                    assert_eq!(set.next_uncovered(a, a + n), want);
                }
            }
            assert_eq!(set.contains(a), other.contains(&a));
            assert_eq!(set.len(), other.len());
            assert_eq!(set.any(a, a + n), other.range(a..a + n).next().is_some());
        }
        for base in bases {
            let got: Vec<usize> = set.range(base..base + 400).collect();
            let want: Vec<usize> = other.range(base..base + 400).copied().collect();
            assert_eq!(got, want);
        }
        set.clear();
        assert!(set.is_empty());
        assert_eq!(set.range(0..SPACE).count(), 0);
    }

    /// Past the address space nothing is set, and the looks do not overflow.
    #[test]
    fn the_end_of_the_space() {
        let set = Box::new(Blocks::new());
        assert!(!set.insert(SPACE));
        assert!(!set.insert(usize::MAX));
        set.insert_range(SPACE - 2, usize::MAX);
        assert!(set.contains(SPACE - 1));
        assert!(set.contains_range(SPACE - 2, SPACE));
        assert!(!set.contains_range(SPACE - 2, SPACE + 1));
        assert_eq!(set.next_uncovered(SPACE - 2, usize::MAX), SPACE);
        assert_eq!(set.range(SPACE - 4..usize::MAX).collect::<Vec<_>>(), [SPACE - 2, SPACE - 1]);
        set.remove_range(0, usize::MAX);
        assert!(set.is_empty());
    }

    /// Threads setting and clearing their own blocks of the same words lose
    /// none of one another's bits.
    #[test]
    fn threads_change_their_own_bits_of_shared_words() {
        let set = Box::new(Blocks::new());
        std::thread::scope(|s| {
            for t in 0..8usize {
                let set = &set;
                s.spawn(move || {
                    for round in 0..2000usize {
                        let block = 64 + t + 8 * (round % 16);
                        assert!(set.insert(block));
                        assert!(set.contains(block));
                        assert!(set.remove(block));
                    }
                    set.insert(64 + t);
                });
            }
        });
        assert_eq!(set.range(0..1024).collect::<Vec<_>>(), (64..72).collect::<Vec<_>>());
        assert_eq!(set.len(), 8);
    }
}
