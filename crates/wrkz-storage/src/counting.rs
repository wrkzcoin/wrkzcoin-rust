// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`CountingStore`]: a [`KvStore`] that counts what passes through it.
//!
//! An import that is not compute-bound is bound by the number of engine
//! operations it asks for and the bytes they carry, and those two numbers are
//! properties of the *code*, not of the machine: they are the same on a laptop
//! with a `MemStore` as on the operator's 40 GB RocksDB. Measuring them here is
//! therefore the only part of the import's cost that can be measured on a host
//! without RocksDB and still be trusted about the host with it.
//!
//! Wrap either store:
//!
//! ```
//! use wrkz_storage::{counting::CountingStore, KvStore, MemStore};
//! let mut s = CountingStore::new(MemStore::default());
//! s.put(b"k".to_vec(), b"v".to_vec()).unwrap();
//! assert_eq!(s.counts().batches, 1);
//! assert_eq!(s.counts().ops, 1);
//! ```

use crate::{KvStore, Result, WriteOp};
use std::cell::Cell;

/// What a [`CountingStore`] saw. All counters are cumulative since the last
/// [`CountingStore::reset`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Calls to [`KvStore::get`].
    pub gets: u64,
    /// Calls to [`KvStore::multi_get`].
    pub multi_gets: u64,
    /// Keys asked for across all `multi_get` calls.
    pub multi_get_keys: u64,
    /// Calls to [`KvStore::write_batch`] that actually reached the engine.
    pub batches: u64,
    /// Entries written or deleted across those batches.
    pub ops: u64,
    /// Key plus value bytes across those batches.
    pub written_bytes: u64,
    /// Bytes returned by reads that found something.
    pub read_bytes: u64,
    /// Calls to [`KvStore::flush`].
    pub flushes: u64,
    /// Calls to [`KvStore::sync`].
    pub syncs: u64,
}

impl Counts {
    /// Engine round trips: every read and every batch that reached the store.
    pub fn engine_calls(&self) -> u64 {
        self.gets + self.multi_gets + self.batches
    }
}

/// A store that forwards everything to `inner` and counts it.
///
/// The counters are behind [`Cell`]s so that the read side stays `&self`, which
/// is what [`KvStore::get`] gives it. That makes the wrapper `!Sync`; it is a
/// measurement tool for a single-threaded import, not something to serve from.
pub struct CountingStore<S: KvStore> {
    inner: S,
    gets: Cell<u64>,
    multi_gets: Cell<u64>,
    multi_get_keys: Cell<u64>,
    batches: Cell<u64>,
    ops: Cell<u64>,
    written_bytes: Cell<u64>,
    read_bytes: Cell<u64>,
    flushes: Cell<u64>,
    syncs: Cell<u64>,
}

impl<S: KvStore> CountingStore<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            gets: Cell::new(0),
            multi_gets: Cell::new(0),
            multi_get_keys: Cell::new(0),
            batches: Cell::new(0),
            ops: Cell::new(0),
            written_bytes: Cell::new(0),
            read_bytes: Cell::new(0),
            flushes: Cell::new(0),
            syncs: Cell::new(0),
        }
    }

    pub fn counts(&self) -> Counts {
        Counts {
            gets: self.gets.get(),
            multi_gets: self.multi_gets.get(),
            multi_get_keys: self.multi_get_keys.get(),
            batches: self.batches.get(),
            ops: self.ops.get(),
            written_bytes: self.written_bytes.get(),
            read_bytes: self.read_bytes.get(),
            flushes: self.flushes.get(),
            syncs: self.syncs.get(),
        }
    }

    pub fn reset(&self) {
        self.gets.set(0);
        self.multi_gets.set(0);
        self.multi_get_keys.set(0);
        self.batches.set(0);
        self.ops.set(0);
        self.written_bytes.set(0);
        self.read_bytes.set(0);
        self.flushes.set(0);
        self.syncs.set(0);
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: KvStore> KvStore for CountingStore<S> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.gets.set(self.gets.get() + 1);
        let v = self.inner.get(key)?;
        if let Some(v) = &v {
            self.read_bytes.set(self.read_bytes.get() + v.len() as u64);
        }
        Ok(v)
    }

    fn multi_get(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>> {
        self.multi_gets.set(self.multi_gets.get() + 1);
        self.multi_get_keys.set(self.multi_get_keys.get() + keys.len() as u64);
        let vs = self.inner.multi_get(keys)?;
        let got: u64 = vs.iter().flatten().map(|v| v.len() as u64).sum();
        self.read_bytes.set(self.read_bytes.get() + got);
        Ok(vs)
    }

    fn write_batch(&mut self, ops: Vec<WriteOp>) -> Result<()> {
        self.batches.set(self.batches.get() + 1);
        self.ops.set(self.ops.get() + ops.len() as u64);
        let bytes: u64 = ops.iter().map(|(k, v)| k.len() as u64 + v.as_ref().map_or(0, |v| v.len()) as u64).sum();
        self.written_bytes.set(self.written_bytes.get() + bytes);
        self.inner.write_batch(ops)
    }

    fn flush(&mut self) -> Result<()> {
        self.flushes.set(self.flushes.get() + 1);
        self.inner.flush()
    }

    fn flush_if_full(&mut self) -> Result<bool> {
        self.inner.flush_if_full()
    }

    fn sync(&mut self) -> Result<()> {
        self.syncs.set(self.syncs.get() + 1);
        self.inner.sync()
    }

    fn pending_bytes(&self) -> usize {
        self.inner.pending_bytes()
    }

    /// Forwarded, and not counted: nothing an import does scans.
    fn scan(&self, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> Result<crate::ScanPage> {
        self.inner.scan(prefix, after, limit)
    }
}
