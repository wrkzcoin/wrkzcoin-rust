// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The `/getwalletsyncdata` response cache (`RpcServer.cpp:698-803`,
//! `--rpc-sync-cache-size`).
//!
//! Every wallet syncing past the same height asks for the same range, and
//! assembling one answer is thousands of reads plus the encoding. The C++ keeps
//! finished response bodies, keyed on everything that decides their bytes, and
//! so does this, with the C++'s three rules for what may be kept:
//!
//! - the key's start index is the one the node *resolved* from the caller's
//!   checkpoints, not the checkpoints themselves, so two wallets at the same
//!   height share an entry although their checkpoint tails differ;
//! - only a range ending at least `2 × CRYPTONOTE_MAX_ALT_BLOCK_DEPTH` (360)
//!   blocks behind the tip is stored: the node refuses any reorganisation
//!   deeper than 180 blocks, so it can never unwind such a range, and the
//!   doubling leaves room for the tip to move while the entry lives;
//! - when the tip goes *down* — the one visible sign of a reorganisation —
//!   everything is dropped.
//!
//! A cached body is byte for byte the body that was built, so the cache changes
//! how fast an answer comes back and nothing else. It is bounded by the total
//! size of the bodies it holds and evicts the least recently used first, so a
//! range every wallet is syncing past outlives one a single wallet asked for
//! once.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use wrkz_primitives::constants::CRYPTONOTE_MAX_ALT_BLOCK_DEPTH;

/// `rpcSyncCacheBytes`, 64 MiB (`DaemonConfiguration.h:94`).
pub const DEFAULT_SYNC_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// How far behind the tip a range must end to be kept (`RpcServer.cpp:742`).
pub const REORG_SAFETY_MARGIN: u64 = 2 * CRYPTONOTE_MAX_ALT_BLOCK_DEPTH;

/// `WalletSyncCacheKey` (`RpcServer.h:38`): everything that decides the bytes of
/// an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyncCacheKey {
    /// The resolved start index, after the lite and prune clamp.
    pub start_index: u64,
    pub block_count: u64,
    pub end_height: u64,
    pub skip_coinbase_transactions: bool,
    pub skip_input_key_offsets: bool,
    pub skip_empty_blocks: bool,
    pub base64: bool,
}

struct Entry {
    body: Arc<[u8]>,
    /// Position in [`Inner::order`].
    stamp: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<SyncCacheKey, Entry>,
    /// Least recently used first.
    order: BTreeMap<u64, SyncCacheKey>,
    next_stamp: u64,
    bytes: usize,
    /// `m_syncCacheTopBlockIndex`: the tip last seen, so a lower one reads as a
    /// reorganisation.
    top_index: u64,
}

/// See the module documentation.
pub struct SyncCache {
    max_bytes: usize,
    inner: Mutex<Inner>,
    /// Lookups that found a body, for `/metrics`.
    hits: AtomicU64,
    /// Lookups that did not.
    misses: AtomicU64,
}

impl SyncCache {
    /// A cache holding at most `max_bytes` of bodies. `0` disables it: nothing
    /// is ever stored and every lookup misses.
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes, inner: Mutex::new(Inner::default()), hits: AtomicU64::new(0), misses: AtomicU64::new(0) }
    }

    /// Lookups answered from the cache since it was made.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Lookups that found nothing since it was made.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn enabled(&self) -> bool {
        self.max_bytes > 0
    }

    // A poisoned lock guards a map that was consistent before the panic — every
    // mutation below completes or does not start — so carry on with it.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `lookupSyncCache`: the body stored under `key`, which becomes the most
    /// recently used.
    pub fn lookup(&self, key: &SyncCacheKey) -> Option<Arc<[u8]>> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let Some(entry) = inner.entries.get_mut(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.hits.fetch_add(1, Ordering::Relaxed);
        inner.order.remove(&entry.stamp);
        inner.next_stamp += 1;
        entry.stamp = inner.next_stamp;
        inner.order.insert(entry.stamp, *key);
        Some(Arc::clone(&entry.body))
    }

    /// `storeSyncCache`: keep `body` under `key` if the C++ would.
    ///
    /// `first_height` and `last_height` are the heights of the first and last
    /// block the body carries — a body with no blocks is never offered — and
    /// `top_index` is the tip read *before* the body was built. Returns whether
    /// it was stored.
    pub fn store(&self, key: SyncCacheKey, body: &[u8], first_height: u64, last_height: u64, top_index: u64) -> bool {
        if self.max_bytes == 0 || body.is_empty() || body.len() > self.max_bytes {
            return false;
        }
        // The key's start came from a resolve that ran before the body was
        // built, so a reorganisation in between could have moved the body's own
        // start earlier. Filed under the later height it would hand a caller
        // blocks it did not ask for. Starting *above* the key is legitimate:
        // everything in between held nothing worth sending.
        if first_height < key.start_index {
            return false;
        }
        if last_height.saturating_add(REORG_SAFETY_MARGIN) > top_index {
            return false;
        }
        let mut guard = self.lock();
        let inner = &mut *guard;
        // Another worker may have built the same range meanwhile; keep theirs.
        if inner.entries.contains_key(&key) {
            return false;
        }
        inner.next_stamp += 1;
        let stamp = inner.next_stamp;
        inner.entries.insert(key, Entry { body: Arc::from(body), stamp });
        inner.order.insert(stamp, key);
        inner.bytes += body.len();
        while inner.bytes > self.max_bytes {
            let Some((_, oldest)) = inner.order.pop_first() else { break };
            if let Some(evicted) = inner.entries.remove(&oldest) {
                inner.bytes -= evicted.body.len();
            }
        }
        true
    }

    /// `discardSyncCacheOnReorg`: the tip only moves down when the chain
    /// reorganised, and there is no cheap way to tell which cached bodies still
    /// describe the main chain, so a lower tip drops them all.
    pub fn observe_tip(&self, top_index: u64) {
        if self.max_bytes == 0 {
            return;
        }
        let mut guard = self.lock();
        let inner = &mut *guard;
        if top_index < inner.top_index {
            inner.entries.clear();
            inner.order.clear();
            inner.bytes = 0;
        }
        inner.top_index = top_index;
    }

    /// Entries held.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of bodies held.
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(start_index: u64) -> SyncCacheKey {
        SyncCacheKey {
            start_index,
            block_count: 100,
            end_height: 0,
            skip_coinbase_transactions: false,
            skip_input_key_offsets: false,
            skip_empty_blocks: false,
            base64: false,
        }
    }

    #[test]
    fn a_stored_body_comes_back_byte_for_byte() {
        let cache = SyncCache::new(1 << 20);
        assert!(cache.lookup(&key(1000)).is_none());
        assert!(cache.store(key(1000), b"{\"items\":[]}", 1000, 1099, 5000));
        assert_eq!(&*cache.lookup(&key(1000)).unwrap(), b"{\"items\":[]}");
        assert_eq!((cache.hits(), cache.misses()), (1, 1));
        // Every field is part of the key.
        assert!(cache.lookup(&SyncCacheKey { base64: true, ..key(1000) }).is_none());
        assert!(cache.lookup(&SyncCacheKey { block_count: 99, ..key(1000) }).is_none());
        assert!(cache.lookup(&key(1001)).is_none());
        // The first body built for a range is the one kept.
        assert!(!cache.store(key(1000), b"other", 1000, 1099, 5000));
        assert_eq!(&*cache.lookup(&key(1000)).unwrap(), b"{\"items\":[]}");
    }

    #[test]
    fn only_ranges_a_reorganisation_cannot_reach_are_kept() {
        let cache = SyncCache::new(1 << 20);
        // Last block 1099: the tip must be at least 1099 + 360.
        assert!(!cache.store(key(1000), b"x", 1000, 1099, 1458));
        assert!(cache.store(key(1000), b"x", 1000, 1099, 1459));
        // A body that begins below its key is refused; above it is fine.
        assert!(!cache.store(key(2000), b"x", 1999, 2050, 9000));
        assert!(cache.store(key(3000), b"x", 3007, 3050, 9000));
    }

    #[test]
    fn the_least_recently_used_body_goes_first() {
        let cache = SyncCache::new(10);
        assert!(cache.store(key(1), b"aaaa", 1, 1, 1000));
        assert!(cache.store(key(2), b"bbbb", 2, 2, 1000));
        // Touch 1, so 2 is now the oldest.
        assert!(cache.lookup(&key(1)).is_some());
        assert!(cache.store(key(3), b"cccc", 3, 3, 1000));
        assert!(cache.lookup(&key(2)).is_none(), "the least recently used was evicted");
        assert!(cache.lookup(&key(1)).is_some());
        assert!(cache.lookup(&key(3)).is_some());
        assert_eq!(cache.bytes(), 8);
        // A body larger than the whole budget is never stored.
        assert!(!cache.store(key(4), &[0u8; 11], 4, 4, 1000));
    }

    #[test]
    fn a_lower_tip_drops_everything() {
        let cache = SyncCache::new(1 << 20);
        cache.observe_tip(5000);
        assert!(cache.store(key(1000), b"x", 1000, 1099, 5000));
        cache.observe_tip(5001);
        assert_eq!(cache.len(), 1, "the tip moving up changes nothing");
        cache.observe_tip(4990);
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn a_zero_budget_disables_it() {
        let cache = SyncCache::new(0);
        assert!(!cache.enabled());
        assert!(!cache.store(key(1), b"x", 1, 1, 1000));
        assert!(cache.lookup(&key(1)).is_none());
    }
}
