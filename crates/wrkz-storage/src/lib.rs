// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Chain storage with the C++ node's RocksDB layout (spec/11-storage.md).
//!
//! - [`codec`] — every key and value is a KV-binary document; these are the
//!   builders and parsers for the prefix/key/value shapes of `DBUtils.h`.
//! - [`records`] — `CachedBlockInfo`, `CachedTransactionInfo`,
//!   `ExtendedTransactionInfo`, `KeyOutputInfo` and the raw block record.
//! - [`fixed`] — fixed-layout encoders and strict decoders for the three
//!   records a lite node snapshot carries (`6`, `7`, `j`), keys and values.
//! - [`reader`] — a typed read API over any [`KvStore`], answering what
//!   `getblockheaderbyheight` and ring-member resolution need.
//! - [`batch`] — [`batch::BatchStore`], the write-batching overlay a bulk
//!   import writes through: N blocks in one engine batch, with every read
//!   consulting the pending writes first.
//! - [`counting`] — a [`KvStore`] that counts what passes through it, so that
//!   the engine-operation cost of an import can be measured without an engine.
//! - [`dbconfig`] — the engine options as plain data, including the bulk-load
//!   profile. Compiles without the `rocksdb` feature; re-exported as
//!   `rocks::DbConfig`.
//! - `rocks` (feature `rocksdb`) — the engine, opened with the C++ options
//!   (level compaction, ZSTD from level 2 down, bloom filters).

pub mod batch;
pub mod codec;
pub mod counting;
pub mod dbconfig;
pub mod fixed;
pub mod reader;
pub mod records;
#[cfg(feature = "rocksdb")]
pub mod rocks;

use std::collections::BTreeMap;

#[derive(Debug)]
pub enum StorageError {
    Engine(String),
    Decode(String),
    Missing(&'static str),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for StorageError {}

impl From<wrkz_primitives::Error> for StorageError {
    fn from(e: wrkz_primitives::Error) -> Self {
        StorageError::Decode(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// One entry of a [`KvStore::write_batch`]: `None` deletes the key.
pub type WriteOp = (Vec<u8>, Option<Vec<u8>>);

/// One page of a [`KvStore::scan`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanPage {
    /// Entries under the prefix, ascending by key.
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// The key to pass as `after` for the next page, or `None` once the prefix
    /// is exhausted.
    pub resume_after: Option<Vec<u8>>,
}

/// Where a scan of `prefix` after `after` starts: just past `after` when it is
/// inside or above the prefix's range, at the prefix otherwise.
pub(crate) fn scan_start(prefix: &[u8], after: Option<&[u8]>) -> std::ops::Bound<Vec<u8>> {
    match after {
        Some(a) if a >= prefix => std::ops::Bound::Excluded(a.to_vec()),
        _ => std::ops::Bound::Included(prefix.to_vec()),
    }
}

/// A page from an ascending run of entries that starts at the scan's start:
/// taken while the keys carry `prefix`, at most `limit` (at least one).
pub(crate) fn collect_page(entries: impl Iterator<Item = (Vec<u8>, Vec<u8>)>, prefix: &[u8], limit: usize) -> ScanPage {
    let limit = limit.max(1);
    let mut page = ScanPage::default();
    for (key, value) in entries {
        if !key.starts_with(prefix) {
            break;
        }
        page.entries.push((key, value));
        if page.entries.len() == limit {
            page.resume_after = page.entries.last().map(|(k, _)| k.clone());
            break;
        }
    }
    page
}

/// The key-value interface the reader and the (stage 3) writer need.
/// Implemented by the RocksDB engine and by [`MemStore`] for tests.
///
/// There is no snapshot or iterator here yet: the reader resolves everything by
/// exact key, and the alternative-chain and rewind paths that need ordered
/// scans belong with the stage 3 write path (spec/11, "Bulk load contract").
/// Note that keys are KV documents holding little-endian integers, so RocksDB's
/// byte order is *not* the numeric order of a block index; anything that grows
/// into a range scan has to carry its own index.
pub trait KvStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Read several keys at once, in the order given.
    ///
    /// The default walks [`KvStore::get`]; an engine with a batched read
    /// overrides it. RocksDB's `multi_get` shares one snapshot and one set of
    /// filter-block lookups across the keys, which is worth having on the one
    /// read that is genuinely a batch: resolving a transaction's ring members,
    /// two to eight outputs of the same amount, on every input of every block
    /// outside the checkpoint zone.
    fn multi_get(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>> {
        keys.iter().map(|k| self.get(k)).collect()
    }

    /// Write one key. The C++ node writes through `BlockchainWriteBatch`, so
    /// prefer [`KvStore::write_batch`] for anything block-sized.
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.write_batch(vec![(key, Some(value))])
    }

    /// Delete one key. Deleting an absent key is not an error, as in RocksDB.
    fn delete(&mut self, key: Vec<u8>) -> Result<()> {
        self.write_batch(vec![(key, None)])
    }

    /// Apply a batch of writes and deletes. Atomic where the engine supports it
    /// (RocksDB does), which is what the per-block record set needs: a block's
    /// `4`, `6`, `1`, `5`, `j`, `b` and `7` records must land together or not at
    /// all, or a crash leaves a half-written height behind.
    fn write_batch(&mut self, ops: Vec<WriteOp>) -> Result<()>;

    /// Push anything this store is holding back through to the engine.
    ///
    /// A plain engine holds nothing back, so the default does nothing.
    /// [`batch::BatchStore`] overrides it to hand its overlay over as one
    /// batch. This makes the writes *visible* to a reopened store; it does not
    /// promise they survive a machine crash — that is [`KvStore::sync`].
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// "The state is consistent here; flush if you have accumulated enough."
    ///
    /// The caller promises that every read and write belonging to one logical
    /// unit — for an import, one block — has already happened, so that a store
    /// which flushes here leaves behind a state that can be resumed from.
    /// Returns whether it actually flushed.
    ///
    /// The default does nothing and reports `false`: an unbatched store is
    /// already consistent after every write.
    fn flush_if_full(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Make everything written so far durable against a crash of the machine,
    /// not merely of the process.
    ///
    /// On RocksDB this is an fsync of the write-ahead log, or — when the log is
    /// disabled for a bulk load — a memtable flush, which is the only thing
    /// that makes a write survive without one. It is expensive; a caller runs
    /// it at the end of a run, on an interrupt, and occasionally in between.
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    /// Turn the engine's write-ahead log on or off for the writes that follow.
    ///
    /// Off is the bulk-load mode: a crash loses what has not reached an SST
    /// file, and nothing else, because every write batch is atomic and the
    /// engine flushes memtables in order — the state left behind is always a
    /// whole number of batches. Turning it back **on** first flushes the
    /// memtables: logged writes replayed on top of a state that lost the
    /// unlogged ones before them would not be a state at all.
    ///
    /// The default does nothing: a store with no log has nothing to switch.
    fn set_write_ahead_log(&mut self, on: bool) -> Result<()> {
        let _ = on;
        Ok(())
    }

    /// Bytes this store is holding back, for a caller sizing its own batches.
    /// Zero for a store that holds nothing back.
    fn pending_bytes(&self) -> usize {
        0
    }

    /// The entries whose keys start with `prefix`, ascending, strictly after
    /// `after` when it is given: at most `limit` of them (a `limit` of 0 is
    /// read as 1), and [`ScanPage::resume_after`] to continue from.
    ///
    /// The one ordered read the trait has, added for the lite snapshot export,
    /// which walks the spent key images and the per-amount counters of a live
    /// node a page at a time so it never holds the chain's lock for a whole
    /// table. A page may hold fewer than `limit` entries and still not be the
    /// last — a batching store's pending deletes can remove what the engine
    /// returned — so a caller continues until `resume_after` is `None`.
    ///
    /// The default refuses: a store that cannot iterate says so rather than
    /// answering "empty".
    fn scan(&self, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> Result<ScanPage> {
        let _ = (prefix, after, limit);
        Err(StorageError::Engine("this store cannot scan its keys in order".into()))
    }
}

/// In-memory store for tests and for building fixtures.
#[derive(Default)]
pub struct MemStore {
    pub map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl KvStore for MemStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn write_batch(&mut self, ops: Vec<WriteOp>) -> Result<()> {
        for (key, value) in ops {
            match value {
                Some(v) => self.map.insert(key, v),
                None => self.map.remove(&key),
            };
        }
        Ok(())
    }

    fn scan(&self, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> Result<ScanPage> {
        let start = scan_start(prefix, after);
        let run = self.map.range((start, std::ops::Bound::Unbounded)).map(|(k, v)| (k.clone(), v.clone()));
        Ok(collect_page(run, prefix, limit))
    }
}
