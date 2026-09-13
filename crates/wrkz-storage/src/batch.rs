// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`BatchStore`]: many logical writes, one engine batch.
//!
//! # The problem
//!
//! `ChainState::push_block` writes one [`KvStore::write_batch`] per block, so
//! importing 4.2 million blocks asks the engine for 4.2 million batches. On
//! RocksDB each of those is a write-ahead-log append, a memtable insert under
//! the write lock, and a trip through the write-group machinery — a fixed cost
//! paid per *batch*, not per byte, and an empty block's batch carries about
//! fifteen entries and well under a kilobyte. Amortising that cost over a
//! thousand blocks costs nothing but memory.
//!
//! # The overlay, and why it is not optional
//!
//! Blocks are not independent. Block `N + 1` may spend an output block `N`
//! created, its ring may name that output's `(amount, global index)` pair, its
//! coinbase takes the next global index from the per-amount counter block `N`
//! bumped, and the double-spend check reads key images block `N` wrote. A
//! writer that held a thousand blocks back before showing them to the engine
//! would have to answer those reads itself or answer them wrongly.
//!
//! This is that writer. Every pending write goes into `pending`, an ordered map
//! from key to "this value" or "deleted", and **every read consults `pending`
//! first**: a key present there is answered from there, present-but-deleted is
//! answered as absent, and only a key it has never seen falls through to the
//! base store. Since [`KvStore`] is the *only* way `ChainState` reaches its
//! store — every one of its reads is a `get` or a `multi_get` on the store it
//! was opened with — wrapping the store is enough to make read-your-own-writes
//! total. There is no second path to miss.
//!
//! # Atomicity and the resume height
//!
//! A flush hands the whole overlay to the base store as **one**
//! [`KvStore::write_batch`], which RocksDB applies atomically. The state's
//! resume height (`META_TIP`) is written by the same `push_block` that writes
//! the block's records, so it is one more entry of that same overlay and lands
//! in the same atomic batch. A crash therefore leaves the state at some flush
//! boundary — never inside one — and the height it reports is exactly the
//! height whose records are there. Re-running resumes from it and re-applies
//! the blocks whose batch never landed.
//!
//! What a crash costs is bounded by the two limits: at most `max_points`
//! blocks, or `max_bytes` of accumulated writes, whichever comes first.
//!
//! Note where a flush can and cannot happen. [`KvStore::write_batch`] **never**
//! flushes, however large the overlay has grown; only [`KvStore::flush_if_full`]
//! and [`KvStore::flush`] do, and the caller decides where to put those. That is
//! what keeps a multi-step operation atomic: a reorganisation unwinds several
//! blocks and applies several more, each as its own `write_batch`, and none of
//! those intermediate states is a state to resume from. Since the replay only
//! calls `flush_if_full` after a whole block has been applied and cross-checked,
//! the engine never sees a reorganisation half done. The byte limit is therefore
//! a limit that is *noticed* at the next consistent point rather than one that
//! is enforced the instant it is crossed.
//!
//! The corollary is that a caller who never calls [`KvStore::flush_if_full`]
//! or [`KvStore::flush`] never commits. The daemon does both: `flush_if_full`
//! after every block it applies, so the limits hold, and `flush` after every
//! event its engine handles, so a downloaded batch of blocks reaches the engine
//! as one write batch and a relayed block at the tip is still committed on its
//! own.
//!
//! # What it is not
//!
//! It is not a transaction and it does not roll back. If the base store rejects
//! a flush the pending writes are gone — they cannot be handed back, since
//! `write_batch` consumes them — but the base store is untouched, because the
//! batch is atomic. The durable state is therefore still the previous flush
//! boundary while the caller's own in-memory state is ahead of it, so the
//! caller must stop; that is what a replay does with any storage error.

use crate::{KvStore, Result, WriteOp};
use std::collections::BTreeMap;

/// Blocks per batch when the caller expresses no preference.
///
/// A thousand empty blocks is roughly 15,000 entries and about a megabyte of
/// overlay; a thousand of the busiest blocks on the chain is a few hundred
/// megabytes, which is why [`DEFAULT_BATCH_BYTES`] exists as a second limit.
pub const DEFAULT_BATCH_POINTS: u32 = 1_000;

/// Accumulated key and value bytes that force a flush regardless of the block
/// count.
pub const DEFAULT_BATCH_BYTES: usize = 64 << 20;

/// Bookkeeping charged per distinct key held in the overlay, so that the byte
/// limit reflects the map's real footprint — two `Vec` headers and a tree node —
/// rather than only the payload.
const ENTRY_OVERHEAD: usize = 64;

/// A [`KvStore`] that accumulates writes and applies them to `base` in one
/// batch. See the module documentation.
pub struct BatchStore<S: KvStore> {
    base: S,
    /// Key to `Some(value)` to write, `None` to delete. Ordered, so that a
    /// flush hands the engine its keys sorted: cheaper for a skip-list memtable
    /// and reproducible run to run.
    pending: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    bytes: usize,
    points: u32,
    max_points: u32,
    max_bytes: usize,
    stats: BatchStats,
}

/// What a [`BatchStore`] has done since it was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchStats {
    /// Batches handed to the base store.
    pub flushes: u64,
    /// Consistent points seen ([`KvStore::flush_if_full`] calls); blocks, for a
    /// replay.
    pub points: u64,
    /// Logical writes and deletes accepted.
    pub ops: u64,
    /// Writes that overwrote a key already in the overlay, and so never reached
    /// the engine at all.
    pub coalesced: u64,
    /// The largest the overlay ever grew, in bytes.
    pub peak_bytes: usize,
}

impl<S: KvStore> BatchStore<S> {
    /// A batch store with the default limits.
    pub fn new(base: S) -> Self {
        Self::with_limits(base, DEFAULT_BATCH_POINTS, DEFAULT_BATCH_BYTES)
    }

    /// A batch store flushing after `max_points` consistent points or
    /// `max_bytes` of accumulated writes, whichever comes first.
    ///
    /// `max_points` is clamped up to 1: zero would have to mean "never flush by
    /// count", and a caller who means that should say so with a large number
    /// rather than a zero that reads like "no batching".
    ///
    /// `with_limits(base, 1, _)` reproduces the unbatched behaviour exactly —
    /// one engine batch per consistent point — which is what the tests compare
    /// against.
    pub fn with_limits(base: S, max_points: u32, max_bytes: usize) -> Self {
        Self {
            base,
            pending: BTreeMap::new(),
            bytes: 0,
            points: 0,
            max_points: max_points.max(1),
            max_bytes,
            stats: BatchStats::default(),
        }
    }

    /// The store underneath. Reads through it **do not see** the overlay, so
    /// this is for inspecting what has actually been committed — never for
    /// answering a question about the current state.
    pub fn base(&self) -> &S {
        &self.base
    }

    pub fn stats(&self) -> BatchStats {
        self.stats
    }

    /// Blocks (consistent points) accumulated since the last flush.
    pub fn pending_points(&self) -> u32 {
        self.points
    }

    /// Distinct keys held in the overlay.
    pub fn pending_keys(&self) -> usize {
        self.pending.len()
    }

    /// Throw the accumulated writes away without applying them, exactly as a
    /// process that died would.
    ///
    /// The base store keeps whatever the last flush left there, which is a
    /// consistent height because the resume height flushes with it. This is how
    /// the tests simulate a crash; nothing in the import calls it.
    pub fn abandon_pending(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        self.bytes = 0;
        self.points = 0;
        n
    }

    fn drain(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            let ops: Vec<WriteOp> = std::mem::take(&mut self.pending).into_iter().collect();
            self.base.write_batch(ops)?;
            self.stats.flushes += 1;
        }
        self.bytes = 0;
        self.points = 0;
        Ok(())
    }
}

impl<S: KvStore> KvStore for BatchStore<S> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.pending.get(key) {
            Some(pending) => Ok(pending.clone()),
            None => self.base.get(key),
        }
    }

    /// The overlay answers what it can and the rest go to the base store in one
    /// batched read, with the results put back in the order asked.
    fn multi_get(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut out: Vec<Option<Vec<u8>>> = Vec::with_capacity(keys.len());
        let mut misses: Vec<Vec<u8>> = Vec::new();
        let mut miss_slots: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            match self.pending.get(key) {
                Some(pending) => out.push(pending.clone()),
                None => {
                    out.push(None);
                    miss_slots.push(i);
                    misses.push(key.clone());
                }
            }
        }
        if !misses.is_empty() {
            for (slot, value) in miss_slots.into_iter().zip(self.base.multi_get(&misses)?) {
                out[slot] = value;
            }
        }
        Ok(out)
    }

    fn write_batch(&mut self, ops: Vec<WriteOp>) -> Result<()> {
        for (key, value) in ops {
            self.stats.ops += 1;
            let key_len = key.len();
            let value_len = value.as_ref().map_or(0, |v| v.len());
            match self.pending.insert(key, value) {
                Some(previous) => {
                    self.stats.coalesced += 1;
                    let previous_len = previous.as_ref().map_or(0, |v| v.len());
                    self.bytes = (self.bytes + value_len).saturating_sub(previous_len);
                }
                None => self.bytes += key_len + value_len + ENTRY_OVERHEAD,
            }
        }
        self.stats.peak_bytes = self.stats.peak_bytes.max(self.bytes);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.drain()?;
        self.base.flush()
    }

    fn flush_if_full(&mut self) -> Result<bool> {
        self.points += 1;
        self.stats.points += 1;
        if self.points >= self.max_points || self.bytes >= self.max_bytes {
            self.flush()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn sync(&mut self) -> Result<()> {
        self.flush()?;
        self.base.sync()
    }

    /// Forwarded. The overlay is unaffected: what it holds reaches the engine
    /// at the next flush, under whichever mode is in force then.
    fn set_write_ahead_log(&mut self, on: bool) -> Result<()> {
        self.base.set_write_ahead_log(on)
    }

    fn pending_bytes(&self) -> usize {
        self.bytes
    }

    /// The base store's page with the overlay merged over exactly the key range
    /// that page covers: a pending value wins, a pending delete removes. A page
    /// the merge makes longer than `limit` is cut there and resumes from the
    /// cut, so every key is seen once, in order, whatever the overlay holds.
    fn scan(&self, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> Result<crate::ScanPage> {
        use std::ops::Bound;
        let limit = limit.max(1);
        let base = self.base.scan(prefix, after, limit)?;
        let start = crate::scan_start(prefix, after);
        let end = match &base.resume_after {
            Some(key) => Bound::Included(key.clone()),
            None => Bound::Unbounded,
        };
        let inverted = match (&start, &end) {
            (Bound::Included(s) | Bound::Excluded(s), Bound::Included(e)) => e < s,
            _ => false,
        };
        let mut merged: BTreeMap<Vec<u8>, Vec<u8>> = base.entries.into_iter().collect();
        if !inverted {
            for (key, pending) in self.pending.range((start, end)) {
                if !key.starts_with(prefix) {
                    break;
                }
                match pending {
                    Some(value) => merged.insert(key.clone(), value.clone()),
                    None => merged.remove(key),
                };
            }
        }
        let mut page = crate::ScanPage { entries: merged.into_iter().collect(), resume_after: base.resume_after };
        if page.entries.len() > limit {
            page.entries.truncate(limit);
            page.resume_after = page.entries.last().map(|(k, _)| k.clone());
        }
        Ok(page)
    }
}

/// A last-resort flush, so that a caller who forgot one loses nothing.
///
/// It is not the intended path: an error raised here can only go to stderr, and
/// a replay that means to stop calls [`KvStore::sync`] itself and handles the
/// failure. It exists because the alternative — silently discarding writes the
/// caller was told had been accepted — is the kind of fault that surfaces as a
/// wrong chain state hours later.
impl<S: KvStore> Drop for BatchStore<S> {
    fn drop(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let keys = self.pending.len();
        if let Err(e) = self.drain() {
            eprintln!("wrkz-storage: BatchStore dropped with {keys} pending writes that could not be flushed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;

    fn key(n: u8) -> Vec<u8> {
        vec![b'k', n]
    }

    #[test]
    fn a_scan_pages_through_one_prefix_in_key_order() {
        let mut m = MemStore::default();
        for n in [5u8, 1, 3, 9, 7] {
            m.put(vec![b'p', n], vec![n]).unwrap();
        }
        m.put(b"o9".to_vec(), b"before".to_vec()).unwrap();
        m.put(b"q0".to_vec(), b"after".to_vec()).unwrap();
        let entry = |n: u8| (vec![b'p', n], vec![n]);

        let page = m.scan(b"p", None, 2).unwrap();
        assert_eq!(page.entries, vec![entry(1), entry(3)]);
        assert_eq!(page.resume_after, Some(vec![b'p', 3]));
        let page = m.scan(b"p", page.resume_after.as_deref(), 2).unwrap();
        assert_eq!(page.entries, vec![entry(5), entry(7)]);
        let page = m.scan(b"p", page.resume_after.as_deref(), 2).unwrap();
        assert_eq!((page.entries, page.resume_after), (vec![entry(9)], None));

        assert_eq!(m.scan(b"p", None, 0).unwrap().entries, vec![entry(1)], "a limit of 0 is one");
        assert_eq!(m.scan(b"r", None, 10).unwrap(), crate::ScanPage::default());
        assert_eq!(m.scan(b"p", Some(b"a"), 1).unwrap().entries, vec![entry(1)], "an `after` below the prefix");
        assert_eq!(m.scan(b"p", Some(b"pz"), 1).unwrap(), crate::ScanPage::default(), "and one past it");
    }

    #[test]
    fn a_batched_scan_sees_its_pending_writes_and_deletes() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
        for n in 1..=6u8 {
            s.put(vec![b'p', n], vec![n]).unwrap();
        }
        s.put(b"q1".to_vec(), b"other".to_vec()).unwrap();
        s.flush().unwrap();
        s.delete(vec![b'p', 2]).unwrap();
        s.put(vec![b'p', 3], vec![33]).unwrap();
        s.put(vec![b'p', 0], vec![0]).unwrap();
        s.put(vec![b'p', 7], vec![7]).unwrap();
        s.put(vec![b'p', 8], vec![8]).unwrap();

        for limit in [1usize, 2, 3, 100] {
            let mut seen = Vec::new();
            let mut after = None;
            loop {
                let page = s.scan(b"p", after.as_deref(), limit).unwrap();
                assert!(page.entries.len() <= limit);
                seen.extend(page.entries);
                match page.resume_after {
                    Some(k) => after = Some(k),
                    None => break,
                }
            }
            let want: Vec<(Vec<u8>, Vec<u8>)> = [(0, 0), (1, 1), (3, 33), (4, 4), (5, 5), (6, 6), (7, 7), (8, 8)]
                .map(|(k, v)| (vec![b'p', k], vec![v]))
                .into();
            assert_eq!(seen, want, "limit {limit}");
        }
        assert_eq!(s.base().scan(b"p", None, 100).unwrap().entries.len(), 6, "and nothing was committed by scanning");
    }

    #[test]
    fn a_pending_write_is_read_back_before_it_reaches_the_base() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
        s.put(key(1), b"one".to_vec()).unwrap();
        assert_eq!(s.get(&key(1)).unwrap().as_deref(), Some(&b"one"[..]));
        // Nothing has reached the base store yet.
        assert_eq!(s.base().get(&key(1)).unwrap(), None);
        assert_eq!(s.stats().flushes, 0);
        s.flush().unwrap();
        assert_eq!(s.base().get(&key(1)).unwrap().as_deref(), Some(&b"one"[..]));
        assert_eq!(s.stats().flushes, 1);
    }

    #[test]
    fn a_pending_delete_hides_a_committed_value() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
        s.put(key(1), b"one".to_vec()).unwrap();
        s.flush().unwrap();
        s.delete(key(1)).unwrap();
        assert_eq!(s.get(&key(1)).unwrap(), None, "the tombstone must win over the committed value");
        assert_eq!(s.base().get(&key(1)).unwrap().as_deref(), Some(&b"one"[..]), "and not yet be committed");
        s.flush().unwrap();
        assert_eq!(s.base().get(&key(1)).unwrap(), None);
    }

    #[test]
    fn multi_get_mixes_the_overlay_and_the_base_in_order() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
        s.put(key(1), b"committed".to_vec()).unwrap();
        s.put(key(3), b"gone".to_vec()).unwrap();
        s.flush().unwrap();
        s.put(key(2), b"pending".to_vec()).unwrap();
        s.delete(key(3)).unwrap();
        let got = s.multi_get(&[key(1), key(2), key(3), key(4)]).unwrap();
        assert_eq!(
            got,
            vec![Some(b"committed".to_vec()), Some(b"pending".to_vec()), None, None],
            "order and provenance must both survive"
        );
    }

    #[test]
    fn the_block_limit_and_the_byte_limit_both_flush() {
        let mut s = BatchStore::with_limits(MemStore::default(), 3, 1 << 30);
        for n in 0..3u8 {
            s.put(key(n), vec![n]).unwrap();
            let flushed = s.flush_if_full().unwrap();
            assert_eq!(flushed, n == 2, "only the third point fills a batch of three");
        }
        assert_eq!(s.stats().flushes, 1);

        // A byte limit small enough that one value fills it.
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000_000, 8);
        s.put(key(0), vec![0; 64]).unwrap();
        assert!(s.flush_if_full().unwrap(), "the byte limit fires before the block limit");
        assert_eq!(s.stats().flushes, 1);
    }

    /// The byte limit must not fire in the middle of a `write_batch`.
    ///
    /// A reorganisation is several `write_batch` calls — unwind, unwind, apply,
    /// apply — and none of the states between them is one to resume from. So the
    /// limit is *noticed* at the next consistent point rather than enforced the
    /// instant it is crossed, and this pins that down.
    #[test]
    fn a_write_never_flushes_on_its_own_however_large_it_is() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 8);
        for n in 0..10u8 {
            s.put(key(n), vec![0; 1024]).unwrap();
            assert_eq!(s.stats().flushes, 0, "a write must not commit by itself");
            assert_eq!(s.base().get(&key(n)).unwrap(), None);
        }
        assert!(s.pending_bytes() > 8, "the limit is long past");
        assert!(s.flush_if_full().unwrap(), "and the next consistent point acts on it");
        assert_eq!(s.stats().flushes, 1);
    }

    #[test]
    fn a_batch_of_one_is_the_unbatched_path() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1, 1 << 30);
        for n in 0..4u8 {
            s.put(key(n), vec![n]).unwrap();
            assert!(s.flush_if_full().unwrap());
            assert_eq!(s.base().get(&key(n)).unwrap(), Some(vec![n]), "committed immediately");
        }
        assert_eq!(s.stats().flushes, 4);
    }

    #[test]
    fn rewriting_a_key_inside_one_batch_reaches_the_engine_once() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
        for n in 0..5u8 {
            s.put(key(0), vec![n]).unwrap();
        }
        assert_eq!(s.pending_keys(), 1);
        assert_eq!(s.stats().coalesced, 4);
        s.flush().unwrap();
        assert_eq!(s.base().get(&key(0)).unwrap(), Some(vec![4]), "the last write wins");
    }

    #[test]
    fn abandoning_a_batch_leaves_the_last_flush_behind() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
        s.put(key(1), b"kept".to_vec()).unwrap();
        s.flush().unwrap();
        s.put(key(2), b"lost".to_vec()).unwrap();
        assert_eq!(s.abandon_pending(), 1);
        assert_eq!(s.get(&key(2)).unwrap(), None);
        assert_eq!(s.get(&key(1)).unwrap().as_deref(), Some(&b"kept"[..]));
        assert_eq!(s.pending_bytes(), 0);
        assert_eq!(s.pending_points(), 0);
    }

    /// Drop must not swallow accepted writes. The store cannot be inspected
    /// after it is dropped, so the base is handed in behind a counting wrapper
    /// whose tally survives: one batch reached it, without an explicit flush.
    #[test]
    fn dropping_a_dirty_store_still_commits() {
        use crate::counting::CountingStore;
        use std::rc::Rc;

        struct Shared(Rc<std::cell::RefCell<MemStore>>);
        impl KvStore for Shared {
            fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
                self.0.borrow().get(key)
            }
            fn write_batch(&mut self, ops: Vec<WriteOp>) -> Result<()> {
                self.0.borrow_mut().write_batch(ops)
            }
        }

        let base = Rc::new(std::cell::RefCell::new(MemStore::default()));
        {
            let mut s = BatchStore::new(CountingStore::new(Shared(Rc::clone(&base))));
            s.put(key(9), b"v".to_vec()).unwrap();
            // No explicit flush: Drop must do it.
        }
        assert_eq!(base.borrow().get(&key(9)).unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn the_byte_estimate_tracks_writes_and_overwrites() {
        let mut s = BatchStore::with_limits(MemStore::default(), 1_000_000, 1 << 30);
        s.put(key(0), vec![0; 100]).unwrap();
        let one = s.pending_bytes();
        assert!(one >= 102, "{one}");
        s.put(key(0), vec![0; 10]).unwrap();
        assert_eq!(s.pending_bytes(), one - 90, "an overwrite adjusts rather than adds");
        s.flush().unwrap();
        assert_eq!(s.pending_bytes(), 0);
    }
}
