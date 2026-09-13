// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The RocksDB engine.
//!
//! The serving options are the C++ node's (`RocksDBWrapper.cpp:551-665`):
//! default column family only, level compaction, 7 levels, no compression on
//! L0/L1 and ZSTD from L2 down including the bottommost level, 10-bit bloom
//! filters, index/filter blocks in the block cache, a row cache carved out of
//! the read cache. A bulk import selects [`DbConfig::import`] instead, which
//! changes the write path and nothing the daemon uses.
//!
//! Every decision lives in [`crate::dbconfig`], which compiles without this
//! feature; [`options`] is only the translation into `rocksdb::Options`.
//!
//! [`CompactionHandle`] is the one way to reach the engine without going
//! through the store that owns it: a full compaction of a serving database runs
//! for tens of minutes, and it must not do that under the chain lock.

pub use crate::dbconfig::{DbConfig, DbProfile, EngineCompactionStats};

use crate::{KvStore, Result, StorageError, WriteOp};
use rocksdb::properties;
use rocksdb::{
    BlockBasedOptions, BottommostLevelCompaction, Cache, CompactOptions, DBCompressionType, LogLevel, Options,
    WriteBatch, WriteOptions, DB,
};
use std::path::Path;
use std::sync::Arc;

/// A byte count as the `size_t` the engine takes. Only a 32-bit target can
/// saturate, and there a cache larger than the address space means "all of it".
fn size(bytes: u64) -> usize {
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

pub fn options(cfg: &DbConfig) -> Options {
    let mut o = Options::default();
    o.increase_parallelism(cfg.background_threads);
    if cfg.quiet_engine_log {
        o.set_log_level(LogLevel::Warn);
    }
    o.set_max_open_files(cfg.max_open_files);
    o.set_skip_stats_update_on_db_open(true);
    o.set_compaction_readahead_size(2 * 1024 * 1024);
    let row_cache = cfg.row_cache_bytes();
    if row_cache > 0 {
        o.set_row_cache(&Cache::new_lru_cache(size(row_cache)));
    }
    let wb = cfg.write_buffer_bytes();
    o.set_write_buffer_size(size(wb));
    o.set_min_write_buffer_number_to_merge(cfg.min_write_buffer_number_to_merge);
    o.set_max_write_buffer_number(cfg.max_write_buffer_number);
    o.set_level_zero_file_num_compaction_trigger(cfg.level0_file_num_compaction_trigger);
    o.set_level_zero_slowdown_writes_trigger(cfg.level0_slowdown_writes_trigger);
    o.set_level_zero_stop_writes_trigger(cfg.level0_stop_writes_trigger);
    o.set_target_file_size_base((wb / 2).max(8 * 1024 * 1024));
    o.set_max_bytes_for_level_base(wb.saturating_mul(4).max(64 * 1024 * 1024));
    o.set_num_levels(7);
    o.set_target_file_size_multiplier(2);
    o.set_compaction_style(rocksdb::DBCompactionStyle::Level);
    let comp = if cfg.compression { DBCompressionType::Zstd } else { DBCompressionType::None };
    o.set_compression_per_level(&[DBCompressionType::None, DBCompressionType::None, comp, comp, comp, comp, comp]);
    o.set_bottommost_compression_type(comp);
    if cfg.compression && cfg.compression_dict_bytes > 0 {
        o.set_compression_options(-14, 32767, 0, cfg.dict_bytes_c());
        o.set_zstd_max_train_bytes(cfg.zstd_train_bytes_c());
    }
    if cfg.compression && (cfg.compression_dict_bytes > 0 || cfg.compression_level > 0) {
        o.set_bottommost_compression_options(
            -14,
            if cfg.compression_level > 0 { cfg.compression_level } else { 32767 },
            0,
            cfg.dict_bytes_c(),
            true,
        );
        if cfg.compression_dict_bytes > 0 {
            o.set_bottommost_zstd_max_train_bytes(cfg.zstd_train_bytes_c(), true);
        }
    }
    // `RocksDBWrapper.cpp:664`. Always set, so the offline profiles say in
    // `dbconfig` what they get rather than inheriting the engine default.
    o.set_optimize_filters_for_hits(!cfg.bottommost_filters);
    // The bulk-load knobs. Every one of them is left at the engine default
    // unless the profile asked for it.
    if cfg.background_jobs > 0 {
        o.set_max_background_jobs(cfg.background_jobs);
    }
    if cfg.max_subcompactions > 0 {
        o.set_max_subcompactions(cfg.max_subcompactions);
    }
    if cfg.disable_auto_compactions {
        o.set_disable_auto_compactions(true);
    }
    if cfg.bytes_per_sync > 0 {
        o.set_bytes_per_sync(cfg.bytes_per_sync);
        o.set_wal_bytes_per_sync(cfg.bytes_per_sync);
    }
    if cfg.pipelined_write {
        o.set_enable_pipelined_write(true);
    }
    let mut t = BlockBasedOptions::default();
    t.set_block_cache(&Cache::new_lru_cache(size(cfg.block_cache_bytes())));
    t.set_block_size(size(cfg.block_size_bytes()));
    t.set_bloom_filter(10.0, false);
    t.set_optimize_filters_for_memory(cfg.optimize_filters_for_memory);
    t.set_cache_index_and_filter_blocks(true);
    t.set_pin_l0_filter_and_index_blocks_in_cache(true);
    o.set_block_based_table_factory(&t);
    o
}

pub struct RocksStore {
    /// Shared with every [`CompactionHandle`] taken from this store, which is
    /// what lets a compaction run without the store's owner.
    pub db: Arc<DB>,
    /// Built once from the config. The only thing on it that is ever not the
    /// default is `disable_wal`.
    write_opts: WriteOptions,
    /// Whether the write-ahead log is on, which decides what [`KvStore::sync`]
    /// has to do to make the writes durable.
    wal: bool,
}

impl RocksStore {
    fn from_db(db: DB, cfg: &DbConfig) -> Self {
        let mut write_opts = WriteOptions::default();
        write_opts.disable_wal(cfg.disable_wal);
        Self { db: Arc::new(db), write_opts, wal: !cfg.disable_wal }
    }

    /// Open an existing database read-only (the migration and replay path).
    pub fn open_read_only(path: &Path, cfg: &DbConfig) -> Result<Self> {
        let o = options(cfg);
        DB::open_for_read_only(&o, path, false)
            .map(|db| Self::from_db(db, cfg))
            .map_err(|e| StorageError::Engine(e.to_string()))
    }

    /// Open (or create) for writing.
    pub fn open(path: &Path, cfg: &DbConfig) -> Result<Self> {
        let mut o = options(cfg);
        o.create_if_missing(true);
        DB::open(&o, path).map(|db| Self::from_db(db, cfg)).map_err(|e| StorageError::Engine(e.to_string()))
    }

    /// Compact the whole database.
    ///
    /// Only worth running after an import that was done with
    /// [`DbConfig::disable_auto_compactions`] on, where L0 has been left to
    /// pile up on purpose. It rewrites every level and takes as long as a
    /// substantial fraction of the import did, so it is never automatic.
    pub fn compact(&self) {
        self.db.compact_range(None::<&[u8]>, None::<&[u8]>);
    }

    /// A handle that compacts this database from another thread, for as long
    /// as the handle lives. The database stays open until the store *and*
    /// every handle are gone.
    pub fn compaction_handle(&self) -> CompactionHandle {
        CompactionHandle { db: Arc::clone(&self.db) }
    }
}

/// A full-range compaction of a serving database, and the engine counters
/// that describe one: `compact_db` and the daemon's compaction scheduler.
///
/// # What rocksdb 0.22 does not give us
///
/// `compact_range_opt` returns nothing, not a `Status`, and its
/// `CompactOptions` has no `canceled` flag (the C++ passes
/// `CompactRangeOptions::canceled`, `RocksDBWrapper.cpp:312`). So:
///
/// - a failure is detected by the engine's own `rocksdb.background-errors`
///   counter moving while the compaction ran;
/// - the only way to stop a running compaction is
///   [`CompactionHandle::stop_all_background_work`], which RocksDB cannot undo:
///   after it no flush or compaction runs again until the database is
///   reopened. It is for shutdown and nothing else.
#[derive(Clone)]
pub struct CompactionHandle {
    db: Arc<DB>,
}

impl CompactionHandle {
    /// Compact every level, blocking until RocksDB returns.
    ///
    /// `CompactRange` over the whole key space with `change_level` and
    /// `target_level = -1`, as the C++ (`RocksDBWrapper.cpp:305-327`): the
    /// output lands in the lowest level that can hold it. The bottommost
    /// level is left alone unless `rewrite_bottommost`, because after the first
    /// full compaction it holds nearly the whole database and rewriting it is
    /// only worth it to apply changed compression or block settings (`kForce`,
    /// not `kForceOptimized`, which would skip exactly the files carrying the
    /// old settings).
    ///
    /// Not exclusive: automatic compactions and writes carry on alongside it,
    /// so the node keeps syncing. That is RocksDB 8.10's default already and is
    /// set anyway, because an exclusive manual compaction would hold every
    /// automatic one back for as long as it runs and let L0 pile up under a
    /// syncing node until writes stall.
    ///
    /// `Err` carries how many background errors RocksDB counted meanwhile;
    /// the engine's `LOG` file in the database directory has the reasons.
    pub fn compact_full(&self, rewrite_bottommost: bool) -> std::result::Result<(), String> {
        let errors_before = self.property(properties::BACKGROUND_ERRORS).unwrap_or(0);
        let mut opts = CompactOptions::default();
        opts.set_exclusive_manual_compaction(false);
        opts.set_change_level(true);
        opts.set_target_level(-1);
        if rewrite_bottommost {
            opts.set_bottommost_level_compaction(BottommostLevelCompaction::Force);
        }
        self.db.compact_range_opt(None::<&[u8]>, None::<&[u8]>, &opts);
        let errors_after = self.property(properties::BACKGROUND_ERRORS).unwrap_or(0);
        if errors_after > errors_before {
            return Err(format!(
                "RocksDB counted {} background error(s) while the compaction ran; its LOG file in the \
                 database directory names them",
                errors_after - errors_before
            ));
        }
        Ok(())
    }

    /// Stop every flush and compaction, waiting for the running ones to notice.
    ///
    /// A manual compaction in [`CompactionHandle::compact_full`] returns soon
    /// after: RocksDB answers it `ShutdownInProgress`. **This cannot be
    /// undone** — the database accepts writes afterwards but never flushes or
    /// compacts them again until it is reopened — so it may only be called once
    /// the owner has made its last write durable. RocksDB flushes unpersisted
    /// write-ahead-log-less writes first when it is called for the first time,
    /// but a caller should not rely on that.
    pub fn stop_all_background_work(&self) {
        self.db.cancel_all_background_work(true);
    }

    /// The engine's own view of compaction, for `db_status`.
    pub fn stats(&self) -> EngineCompactionStats {
        EngineCompactionStats {
            compaction_pending: self.property(properties::COMPACTION_PENDING).map(|v| v != 0),
            running_compactions: self.property(properties::NUM_RUNNING_COMPACTIONS),
            pending_compaction_bytes: self.property(properties::ESTIMATE_PENDING_COMPACTION_BYTES),
            live_sst_bytes: self.property(properties::LIVE_SST_FILES_SIZE),
            total_sst_bytes: self.property(properties::TOTAL_SST_FILES_SIZE),
            background_errors: self.property(properties::BACKGROUND_ERRORS),
        }
    }

    fn property(&self, name: &rocksdb::properties::PropName) -> Option<u64> {
        self.db.property_int_value(name).ok().flatten()
    }
}

impl KvStore for RocksStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.db.get(key).map_err(|e| StorageError::Engine(e.to_string()))
    }

    /// One `MultiGet`: a single snapshot and one pass over the block cache and
    /// bloom filters for the whole batch, instead of one lookup per key.
    fn multi_get(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>> {
        self.db.multi_get(keys).into_iter().map(|r| r.map_err(|e| StorageError::Engine(e.to_string()))).collect()
    }

    /// One RocksDB `WriteBatch`, so the batch is atomic. A store opened with
    /// [`RocksStore::open_read_only`] rejects this, as the engine does.
    fn write_batch(&mut self, ops: Vec<WriteOp>) -> Result<()> {
        let mut batch = WriteBatch::default();
        for (key, value) in ops {
            match value {
                Some(v) => batch.put(key, v),
                None => batch.delete(key),
            }
        }
        self.db.write_opt(batch, &self.write_opts).map_err(|e| StorageError::Engine(e.to_string()))
    }

    /// Make the writes durable.
    ///
    /// With the write-ahead log on, an fsync of the log is enough and is cheap:
    /// the memtable can stay where it is, because the log can rebuild it.
    /// With the log **off** — the import profile — there is no log to fsync and
    /// the only way a write survives is to be in an SST file, so this has to
    /// flush the memtable. That is the expensive branch, and it is why
    /// `wrkz-replay` calls this at the end of a run and on an interrupt rather
    /// than at every batch.
    fn sync(&mut self) -> Result<()> {
        if self.wal {
            self.db.flush_wal(true).map_err(|e| StorageError::Engine(e.to_string()))
        } else {
            self.db.flush().map_err(|e| StorageError::Engine(e.to_string()))
        }
    }

    /// See [`KvStore::set_write_ahead_log`]. Switching on flushes the memtables
    /// first, so every write made with the log off is in an SST file before a
    /// logged one can follow it.
    fn set_write_ahead_log(&mut self, on: bool) -> Result<()> {
        if on == self.wal {
            return Ok(());
        }
        if on {
            self.db.flush().map_err(|e| StorageError::Engine(e.to_string()))?;
        }
        let mut write_opts = WriteOptions::default();
        write_opts.disable_wal(!on);
        self.write_opts = write_opts;
        self.wal = on;
        Ok(())
    }

    /// A forward iterator from the scan's start, stopped at the end of the
    /// prefix or at `limit`. Each page is its own iterator and so its own
    /// implicit snapshot; the lite snapshot export that pages through a live
    /// node relies only on the region it exports not changing.
    fn scan(&self, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> Result<crate::ScanPage> {
        let start: &[u8] = match after {
            Some(a) if a >= prefix => a,
            _ => prefix,
        };
        let iter = self.db.iterator(rocksdb::IteratorMode::From(start, rocksdb::Direction::Forward));
        let mut entries = Vec::new();
        for item in iter {
            let (key, value) = item.map_err(|e| StorageError::Engine(e.to_string()))?;
            if !key.starts_with(prefix) {
                break;
            }
            if after.is_some_and(|a| &*key == a) {
                continue;
            }
            entries.push((key.into_vec(), value.into_vec()));
            if entries.len() >= limit.max(1) {
                break;
            }
        }
        Ok(crate::collect_page(entries.into_iter(), prefix, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("wrkz-rocks-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Every profile's options open a database, and the serving one reads back
    /// what the import one wrote: none of the table options is on-disk state.
    #[test]
    fn a_database_written_under_one_profile_opens_under_another() {
        let dir = TempDir::new("profiles");
        {
            let mut store = RocksStore::open(&dir.0, &DbConfig::import(2)).expect("opens with the import profile");
            store.write_batch((0u8..200).map(|i| (vec![b'k', i], Some(vec![i; 100]))).collect()).unwrap();
            store.sync().unwrap();
        }
        let tuned = DbConfig {
            block_size_kb: 32,
            row_cache_percent: Some(50),
            bottommost_filters: true,
            compression_level: 9,
            compression_dict_bytes: 64 * 1024 * 1024,
            ..DbConfig::default()
        };
        for cfg in [DbConfig::default(), tuned, DbConfig::source(16)] {
            let store = RocksStore::open(&dir.0, &cfg).expect("reopens");
            assert_eq!(store.get(&[b'k', 7]).unwrap(), Some(vec![7; 100]), "{:?}", cfg.profile);
        }
    }

    /// The engine's forward scan, which the lite snapshot export pages through:
    /// one prefix, in key order, resumed after the last key of a page, and a
    /// batching store's pending writes merged over it.
    #[test]
    fn a_scan_pages_through_one_prefix_in_key_order() {
        let dir = TempDir::new("scan");
        let mut store = RocksStore::open(&dir.0, &DbConfig::default()).unwrap();
        let mut ops: Vec<WriteOp> = (0u8..10).map(|n| (vec![b'p', n], Some(vec![n]))).collect();
        ops.push((b"o9".to_vec(), Some(b"before".to_vec())));
        ops.push((b"q0".to_vec(), Some(b"after".to_vec())));
        store.write_batch(ops).unwrap();

        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = store.scan(b"p", after.as_deref(), 3).unwrap();
            assert!(page.entries.len() <= 3);
            seen.extend(page.entries);
            match page.resume_after {
                Some(key) => after = Some(key),
                None => break,
            }
        }
        assert_eq!(seen, (0u8..10).map(|n| (vec![b'p', n], vec![n])).collect::<Vec<_>>());
        assert_eq!(store.scan(b"p", Some(b"a"), 1).unwrap().entries, vec![(vec![b'p', 0], vec![0])]);
        assert_eq!(store.scan(b"p", Some(b"pz"), 1).unwrap(), crate::ScanPage::default());
        assert_eq!(store.scan(b"r", None, 5).unwrap(), crate::ScanPage::default());

        let mut batched = crate::batch::BatchStore::new(store);
        batched.delete(vec![b'p', 4]).unwrap();
        batched.put(vec![b'p', 42], vec![42]).unwrap();
        let all = batched.scan(b"p", None, 100).unwrap();
        assert_eq!(all.entries.len(), 10);
        assert!(!all.entries.iter().any(|(k, _)| k == &[b'p', 4]), "a pending delete hides the engine's record");
        assert!(all.entries.contains(&(vec![b'p', 42], vec![42])), "and a pending write shows");
    }

    /// The handle compacts through its own `Arc` while the store keeps
    /// writing, and a forced pass rewrites the bottommost level too.
    #[test]
    fn a_compaction_handle_compacts_while_the_store_is_in_use() {
        let dir = TempDir::new("compact");
        let mut store = RocksStore::open(&dir.0, &DbConfig::default()).unwrap();
        for round in 0u8..4 {
            store
                .write_batch((0u16..500).map(|i| (i.to_le_bytes().to_vec(), Some(vec![round; 64]))).collect())
                .unwrap();
            store.db.flush().unwrap();
        }
        let handle = store.compaction_handle();
        let worker = std::thread::spawn(move || handle.compact_full(false));
        store.put(b"during".to_vec(), b"yes".to_vec()).unwrap();
        assert_eq!(worker.join().unwrap(), Ok(()));
        assert_eq!(store.get(&7u16.to_le_bytes()).unwrap(), Some(vec![3; 64]), "the last write wins");
        assert_eq!(store.get(b"during").unwrap().as_deref(), Some(&b"yes"[..]));

        let handle = store.compaction_handle();
        assert_eq!(handle.compact_full(true), Ok(()));
        let stats = handle.stats();
        assert_eq!(stats.background_errors, Some(0));
        assert!(stats.live_sst_bytes.is_some_and(|b| b > 0), "{stats:?}");
        assert_eq!(stats.running_compactions, Some(0));
    }

    /// Once background work has been stopped a compaction returns at once
    /// instead of blocking, which is what lets the daemon's shutdown join its
    /// compaction thread. The store is still readable; it never compacts again.
    #[test]
    fn stopping_background_work_is_terminal_but_leaves_reads_working() {
        let dir = TempDir::new("stop");
        let mut store = RocksStore::open(&dir.0, &DbConfig::default()).unwrap();
        store.put(b"k".to_vec(), b"v".to_vec()).unwrap();
        store.db.flush().unwrap();
        let handle = store.compaction_handle();
        handle.stop_all_background_work();
        // RocksDB answers the manual compaction `ShutdownInProgress` rather than
        // blocking, and counts no background error for it.
        assert_eq!(handle.compact_full(false), Ok(()));
        assert_eq!(store.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    }
}
