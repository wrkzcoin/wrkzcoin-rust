// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`DbConfig`]: the engine knobs, as plain data.
//!
//! This module carries no RocksDB types, so it compiles — and its defaults are
//! tested — on a host that cannot build the engine at all. `rocks::options`
//! turns one of these into a `rocksdb::Options`; that translation is the only
//! part that needs libclang, and it is deliberately mechanical so that the
//! decisions live here where they can be read and asserted. The arithmetic the
//! translation needs — how the read cache is split, the block size in bytes,
//! the dictionary sizes as the C API's `int` — is here too, as methods.
//!
//! Re-exported as `wrkz_storage::rocks::DbConfig`, which is where every caller
//! names it.
//!
//! # The three profiles
//!
//! [`DbConfig::default`] is what the **daemon serves from**: the C++ node's
//! options (`RocksDBWrapper.cpp:551-665`) with the C++ daemon's defaults
//! (`DaemonConfiguration.h:62-67`) — level compaction over seven levels, no
//! compression on L0/L1 and ZSTD below, 10-bit bloom filters, 4 KiB data
//! blocks, a 256 MB read cache of which an eighth is a row cache, no filters on
//! the bottommost level, a 64 MB write buffer, the write-ahead log on, and only
//! warnings in the engine's own `LOG` file. The daemon's `--db-*` options move
//! the fields they name and nothing else.
//!
//! Every one of those only decides how the engine reads and how it writes the
//! *next* SST file. None of them is recorded anywhere a reader depends on: a
//! file keeps the block size and filters it was written with, and a database
//! written under one set of options opens under any other.
//!
//! [`DbConfig::import`] is for `wrkz-replay` writing our state, where the
//! access pattern is the opposite of serving: one process, no readers, a
//! continuous stream of small batched writes, and reads that are almost all
//! misses ("do we already have this block hash" — we never do). See each field
//! for what it is set to and why.
//!
//! [`DbConfig::source`] is the C++ database a replay reads.
//!
//! The import and source profiles keep the table options they were measured
//! with — 16 KiB blocks, the whole read cache as block cache, filters on every
//! level, the engine's default log — rather than following the serving
//! profile to the C++ defaults. An import's reads are misses, which is exactly
//! what a bottommost filter answers without touching a data block.

/// How a database is going to be used. Only [`DbConfig::import`] and
/// [`DbConfig::source`] set anything but [`DbProfile::Serving`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DbProfile {
    /// A daemon answering RPC and P2P out of this database.
    #[default]
    Serving,
    /// A one-shot bulk load: `wrkz-replay --state`.
    Import,
    /// The C++ database a replay reads and never writes.
    Source,
}

/// Engine options. Every field is a performance or footprint choice; none of
/// them can change what the chain accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbConfig {
    pub profile: DbProfile,
    /// `--db-write-buffer-size`, in megabytes.
    pub write_buffer_mb: u64,
    /// `--db-read-buffer-size`, in megabytes: the row cache and the block cache
    /// together. See [`DbConfig::row_cache_bytes`].
    pub read_cache_mb: u64,
    /// `--db-max-open-files`.
    pub max_open_files: i32,
    /// `--db-threads`.
    pub background_threads: i32,
    /// `--db-enable-compression`.
    pub compression: bool,
    /// `--db-compression-level`: the ZSTD level for the bottommost level
    /// (0 = RocksDB's default).
    pub compression_level: i32,
    /// `--db-compression-dict-bytes`: per-SST ZSTD dictionary size in bytes
    /// (0 = off).
    pub compression_dict_bytes: u64,
    /// `--db-block-size`, in KiB: the uncompressed size of an SST data block.
    /// Zero is taken as 1, as the C++ takes it (`Daemon.cpp:630`).
    pub block_size_kb: u64,
    /// `--db-row-cache-percent`. `Some(0)` is the C++'s built-in eighth of the
    /// read cache; `Some(n)` is `n` percent of it, at most 90; `None` is no row
    /// cache at all, which no daemon option can ask for and which the import
    /// and source profiles use.
    pub row_cache_percent: Option<u64>,
    /// `--db-bottom-filters`: keep bloom filters on the bottommost level.
    /// Without them (`optimize_filters_for_hits`) that level is written without
    /// filters and read without consulting them — right for a lookup that
    /// finds its key, and a block read for one that does not.
    pub bottommost_filters: bool,
    /// Size bloom filters to what the allocator actually hands out. RocksDB
    /// 10.10, which the C++ node builds against, turns this on by default; the
    /// 8.10 bundled here does not, so it is set explicitly.
    pub optimize_filters_for_memory: bool,
    /// `info_log_level = WARN` (`RocksDBWrapper.cpp:555`): only warnings and
    /// errors in the engine's `LOG` file.
    pub quiet_engine_log: bool,

    // -- bulk-load knobs ----------------------------------------------------
    /// Memtables held at once. More of them lets flushes to L0 run behind the
    /// writer instead of stalling it.
    pub max_write_buffer_number: i32,
    /// Memtables merged into one L0 file. Merging two 256 MB memtables into one
    /// L0 file halves the number of files L0 has to hold and the number of
    /// bloom filters a read has to probe.
    pub min_write_buffer_number_to_merge: i32,
    /// Flushes and compactions that may run at once (`max_background_jobs`).
    /// Distinct from [`DbConfig::background_threads`], which sizes the pool.
    pub background_jobs: i32,
    /// Threads one manual compaction may split itself over.
    pub max_subcompactions: u32,
    /// L0 files before compaction into L1 starts.
    pub level0_file_num_compaction_trigger: i32,
    /// L0 files at which writes are slowed down.
    pub level0_slowdown_writes_trigger: i32,
    /// L0 files at which writes stop until compaction catches up.
    pub level0_stop_writes_trigger: i32,
    /// Turn automatic compaction off for the whole run. **Off by default even
    /// for an import**; see [`DbConfig::import`] for the reason.
    pub disable_auto_compactions: bool,
    /// Skip the write-ahead log on every write. See [`DbConfig::import`].
    pub disable_wal: bool,
    /// Ask the OS to start writing SST bytes back every this many bytes
    /// (0 = leave it to RocksDB). Turns one enormous fsync at file close into a
    /// steady trickle, which is what keeps a bulk load from stuttering.
    pub bytes_per_sync: u64,
    /// Overlap the WAL append of one batch with the memtable insert of the
    /// previous one.
    pub pipelined_write: bool,
}

impl Default for DbConfig {
    /// The serving profile: the C++ node's options at the C++ daemon's
    /// defaults.
    fn default() -> Self {
        Self {
            profile: DbProfile::Serving,
            write_buffer_mb: 64,
            read_cache_mb: 256,
            max_open_files: 4096,
            background_threads: 8,
            compression: true,
            compression_level: 0,
            compression_dict_bytes: 0,
            block_size_kb: 4,
            row_cache_percent: Some(0),
            bottommost_filters: false,
            optimize_filters_for_memory: true,
            quiet_engine_log: true,
            max_write_buffer_number: 6,
            min_write_buffer_number_to_merge: 2,
            background_jobs: 0,
            max_subcompactions: 0,
            level0_file_num_compaction_trigger: 20,
            level0_slowdown_writes_trigger: 30,
            level0_stop_writes_trigger: 40,
            disable_auto_compactions: false,
            disable_wal: false,
            bytes_per_sync: 0,
            pipelined_write: false,
        }
    }
}

/// `1 << 20`, without a shift that could overflow a `u64` megabyte count.
const MIB: u64 = 1024 * 1024;

/// RocksDB's integer properties about compaction, as `rocks::CompactionHandle`
/// reads them. Plain data, here rather than in `rocks`, so a daemon built
/// without the engine can still name what it would print. `None` is a
/// property the engine did not answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineCompactionStats {
    /// `rocksdb.compaction-pending`: RocksDB has at least one compaction it
    /// wants to run.
    pub compaction_pending: Option<bool>,
    /// `rocksdb.num-running-compactions`.
    pub running_compactions: Option<u64>,
    /// `rocksdb.estimate-pending-compaction-bytes`: what RocksDB would have to
    /// rewrite to bring every level under its target size.
    pub pending_compaction_bytes: Option<u64>,
    /// `rocksdb.live-sst-files-size`: the SST files the current version uses.
    pub live_sst_bytes: Option<u64>,
    /// `rocksdb.total-sst-files-size`: every SST file, including those an old
    /// version or a running compaction still holds.
    pub total_sst_bytes: Option<u64>,
    /// `rocksdb.background-errors`, since the database was opened.
    pub background_errors: Option<u64>,
}

impl DbConfig {
    /// The state database of a bulk import.
    ///
    /// What changes from [`DbConfig::default`], and why:
    ///
    /// - **`disable_wal`**. Every batch would otherwise be appended to the
    ///   write-ahead log before it reaches the memtable. The log buys crash
    ///   recovery, which an import does not need: the resume height is written
    ///   inside the same atomic batch as the records it describes, so whatever
    ///   the engine has persisted is a *prefix of whole batches* and the height
    ///   it reports is the height whose records are there. Re-running resumes
    ///   from it. The cost is that the loss window is no longer "the current
    ///   batch" but "everything since the last memtable flush", which is why
    ///   `wrkz-replay` flushes explicitly ([`crate::KvStore::sync`]) at the end
    ///   of a run, on an interrupt, on an error, and every `--sync-every`
    ///   batches in between. `--wal` puts the log back.
    /// - **`write_buffer_mb` 64 → 256, `max_write_buffer_number` 6 → 8**. A
    ///   larger memtable turns more of the import into in-memory work and
    ///   produces fewer, larger L0 files; more of them means a flush can run
    ///   behind the writer rather than stalling it.
    /// - **`background_jobs`**. Flushes and compactions of 256 MB memtables
    ///   need more than the default two jobs to keep up with a writer that
    ///   never pauses.
    /// - **L0 triggers 20/30/40 → 4/24/40**. This is the one that moves the
    ///   *wrong* way from a pure bulk-load recipe, deliberately. An import is
    ///   not write-only: every block asks "do we have this hash" and "how many
    ///   outputs of this amount are there", and a read has to probe the bloom
    ///   filter of every L0 file, because L0 files overlap. Letting twenty
    ///   256 MB files pile up in L0 would make those reads twenty probes each.
    ///   Compacting out of L0 early keeps them at four, and the slowdown
    ///   trigger is left far above the compaction trigger so that the writer is
    ///   never actually throttled.
    /// - **`disable_auto_compactions` stays `false`**, for the same reason. The
    ///   classic `PrepareForBulkLoad` recipe turns compaction off entirely and
    ///   compacts once at the end; that is right for a load that never reads,
    ///   and wrong here — with compaction off, L0 grows to thousands of files
    ///   and every per-block lookup degrades linearly in the number of them.
    ///   The field exists for an operator who has measured otherwise;
    ///   `RocksStore::compact` is the explicit compaction to run at the end if
    ///   they use it.
    /// - **`bytes_per_sync` 8 MB**. Without it the page cache fills with dirty
    ///   SST pages and the writeback lands in one lump at file close.
    /// - **`pipelined_write`**. Lets one batch's log append overlap the
    ///   previous batch's memtable insert. Nearly free with the log off, and
    ///   the reason to leave it on either way.
    /// - **`read_cache_mb` 256 → 512**. Those per-block lookups are the whole
    ///   read side; the index and filter blocks they touch should stay resident.
    /// - **The table options stay as the import has always run them**: 16 KiB
    ///   blocks, no row cache, filters on the bottommost level (every lookup
    ///   an import makes is meant to miss, and the filter is what answers a
    ///   miss), and RocksDB's default log level, whose compaction and stall
    ///   lines are what explain a slow import afterwards.
    ///
    /// Compression is *not* turned off: the database this writes is the one the
    /// daemon then serves, and a load that skipped ZSTD would have to be
    /// rewritten by compaction later anyway. L0 and L1 are already
    /// uncompressed, which is where a bulk load spends its time.
    pub fn import(background_threads: i32) -> Self {
        let threads = background_threads.max(2);
        Self {
            profile: DbProfile::Import,
            write_buffer_mb: 256,
            read_cache_mb: 512,
            max_open_files: 8192,
            background_threads: threads,
            max_write_buffer_number: 8,
            min_write_buffer_number_to_merge: 2,
            background_jobs: threads.max(6),
            max_subcompactions: threads.max(4) as u32,
            level0_file_num_compaction_trigger: 4,
            level0_slowdown_writes_trigger: 24,
            level0_stop_writes_trigger: 40,
            disable_auto_compactions: false,
            disable_wal: true,
            bytes_per_sync: 8 << 20,
            pipelined_write: true,
            ..Self::offline_tables()
        }
    }

    /// The C++ database a replay reads.
    ///
    /// Opened read-only and never written, so the write buffer is a formality;
    /// the block cache is what matters, because the three records the replay
    /// reads per block (the raw block, the block info, the hash index) are point
    /// lookups into a 40 GB database and their index and filter blocks want to
    /// stay resident. The table options are the import's, for the same reason.
    pub fn source(read_cache_mb: u64) -> Self {
        Self { profile: DbProfile::Source, write_buffer_mb: 8, read_cache_mb, ..Self::offline_tables() }
    }

    /// The serving profile with the table options the offline tools have
    /// always opened with. Not a profile of its own.
    fn offline_tables() -> Self {
        Self {
            block_size_kb: 16,
            row_cache_percent: None,
            bottommost_filters: true,
            optimize_filters_for_memory: false,
            quiet_engine_log: false,
            ..Self::default()
        }
    }

    /// Whether writes to this database skip the write-ahead log.
    pub fn wal_disabled(&self) -> bool {
        self.disable_wal
    }

    /// The whole read cache, in bytes.
    pub fn read_cache_bytes(&self) -> u64 {
        self.read_cache_mb.saturating_mul(MIB)
    }

    /// The write buffer, in bytes.
    pub fn write_buffer_bytes(&self) -> u64 {
        self.write_buffer_mb.saturating_mul(MIB)
    }

    /// The row cache carved out of the read cache (`RocksDBWrapper.cpp:567-574`):
    /// `n` percent of it, at most 90, or an eighth when no percentage is given.
    ///
    /// Carved out, not added on top, so the memory an operator asked for with
    /// `--db-read-buffer-size` is the memory the two caches use together. The
    /// integer arithmetic is the C++'s to the byte: the read cache is divided
    /// by 100 before it is multiplied.
    pub fn row_cache_bytes(&self) -> u64 {
        let read = self.read_cache_bytes();
        match self.row_cache_percent {
            None => 0,
            Some(0) => read / 8,
            Some(percent) => (read / 100) * percent.min(90),
        }
    }

    /// What is left of the read cache for the block cache.
    pub fn block_cache_bytes(&self) -> u64 {
        self.read_cache_bytes() - self.row_cache_bytes()
    }

    /// The SST data block size in bytes: at least 1 KiB (`Daemon.cpp:630`).
    pub fn block_size_bytes(&self) -> u64 {
        self.block_size_kb.max(1).saturating_mul(1024)
    }

    /// `max_dict_bytes`, as the `int` RocksDB's C API takes. Saturates rather
    /// than wraps: the C++ casts to `uint32_t` and a dictionary above 4 GiB
    /// would silently become a small one.
    pub fn dict_bytes_c(&self) -> i32 {
        i32::try_from(self.compression_dict_bytes).unwrap_or(i32::MAX)
    }

    /// `zstd_max_train_bytes`: a hundred times the dictionary, which is what
    /// RocksDB recommends over sampling less (`RocksDBWrapper.cpp:626`),
    /// saturated to the C API's `int`. `n as i32 * 100` overflowed from a
    /// dictionary of about 21 MB.
    pub fn zstd_train_bytes_c(&self) -> i32 {
        i32::try_from(self.compression_dict_bytes.saturating_mul(100)).unwrap_or(i32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serving profile is the contract with the running daemon: it must be
    /// the C++ node's options at the C++ daemon's defaults and nothing else.
    /// Spelt out so that a change to the import profile that leaked into the
    /// default would fail here.
    #[test]
    fn the_serving_profile_is_the_cpp_nodes() {
        let d = DbConfig::default();
        assert_eq!(d.profile, DbProfile::Serving);
        assert_eq!(d.write_buffer_mb, 64);
        assert_eq!(d.read_cache_mb, 256);
        assert_eq!(d.max_open_files, 4096);
        assert_eq!(d.background_threads, 8);
        assert!(d.compression);
        assert_eq!(d.compression_level, 0);
        assert_eq!(d.compression_dict_bytes, 0);
        // `DaemonConfiguration.h:62-67`.
        assert_eq!(d.block_size_kb, 4);
        assert_eq!(d.row_cache_percent, Some(0));
        assert!(!d.bottommost_filters);
        assert!(d.optimize_filters_for_memory);
        assert!(d.quiet_engine_log);
        assert_eq!(d.max_write_buffer_number, 6);
        assert_eq!(d.min_write_buffer_number_to_merge, 2);
        assert_eq!(d.level0_file_num_compaction_trigger, 20);
        assert_eq!(d.level0_slowdown_writes_trigger, 30);
        assert_eq!(d.level0_stop_writes_trigger, 40);
        assert!(!d.disable_auto_compactions, "a serving node must compact");
        assert!(!d.disable_wal, "a serving node must not lose acknowledged writes");
        assert_eq!(d.bytes_per_sync, 0);
        assert!(!d.pipelined_write);
    }

    /// 256 MB at the built-in eighth is a 32 MiB row cache and a 224 MiB block
    /// cache, as the C++ node splits it.
    #[test]
    fn the_read_cache_is_split_as_the_cpp_splits_it() {
        let d = DbConfig::default();
        assert_eq!(d.row_cache_bytes(), 32 * MIB);
        assert_eq!(d.block_cache_bytes(), 224 * MIB);

        // A percentage divides first and multiplies second, and stops at 90.
        let quarter = DbConfig { row_cache_percent: Some(25), ..DbConfig::default() };
        assert_eq!(quarter.row_cache_bytes(), (256 * MIB / 100) * 25);
        assert_eq!(quarter.row_cache_bytes() + quarter.block_cache_bytes(), 256 * MIB);
        let greedy = DbConfig { row_cache_percent: Some(500), ..DbConfig::default() };
        assert_eq!(greedy.row_cache_bytes(), (256 * MIB / 100) * 90);

        // No row cache is the whole read cache as block cache.
        let none = DbConfig { row_cache_percent: None, ..DbConfig::default() };
        assert_eq!(none.row_cache_bytes(), 0);
        assert_eq!(none.block_cache_bytes(), 256 * MIB);

        // A read cache too small to divide leaves nothing to carve out.
        let tiny = DbConfig { read_cache_mb: 0, ..DbConfig::default() };
        assert_eq!((tiny.row_cache_bytes(), tiny.block_cache_bytes()), (0, 0));
    }

    #[test]
    fn sizes_clamp_and_saturate_instead_of_wrapping() {
        let zero = DbConfig { block_size_kb: 0, ..DbConfig::default() };
        assert_eq!(zero.block_size_bytes(), 1024, "zero is one kilobyte, as Daemon.cpp:630 has it");
        assert_eq!(DbConfig::default().block_size_bytes(), 4096);

        let dict = DbConfig { compression_dict_bytes: 16 * 1024, ..DbConfig::default() };
        assert_eq!(dict.dict_bytes_c(), 16 * 1024);
        assert_eq!(dict.zstd_train_bytes_c(), 1_638_400);
        // `n as i32 * 100` overflowed here.
        let big = DbConfig { compression_dict_bytes: 64 * MIB, ..DbConfig::default() };
        assert_eq!(big.dict_bytes_c(), 64 * MIB as i32);
        assert_eq!(big.zstd_train_bytes_c(), i32::MAX);
        let huge = DbConfig { compression_dict_bytes: u64::MAX, read_cache_mb: u64::MAX, ..DbConfig::default() };
        assert_eq!((huge.dict_bytes_c(), huge.zstd_train_bytes_c()), (i32::MAX, i32::MAX));
        assert_eq!(huge.read_cache_bytes(), u64::MAX);
        assert_eq!(huge.row_cache_bytes() + huge.block_cache_bytes(), u64::MAX);
    }

    #[test]
    fn the_import_profile_trades_durability_for_throughput_but_keeps_compaction() {
        let i = DbConfig::import(16);
        assert_eq!(i.profile, DbProfile::Import);
        assert!(i.disable_wal, "the resume height makes the log redundant for an import");
        assert!(i.write_buffer_mb > DbConfig::default().write_buffer_mb);
        assert!(i.max_write_buffer_number > DbConfig::default().max_write_buffer_number);
        assert!(i.background_jobs >= 6);
        assert!(i.bytes_per_sync > 0);
        assert!(i.pipelined_write);
        // The deliberate departure from a textbook bulk load: an import reads
        // its own state on every block, so L0 must stay shallow.
        assert!(!i.disable_auto_compactions);
        assert!(i.level0_file_num_compaction_trigger < DbConfig::default().level0_file_num_compaction_trigger);
        assert!(i.level0_slowdown_writes_trigger > i.level0_file_num_compaction_trigger);
        assert!(i.level0_stop_writes_trigger > i.level0_slowdown_writes_trigger);
        // Compression is kept: the daemon serves this database afterwards.
        assert!(i.compression);
    }

    /// The offline tools keep the table options they were measured with when
    /// the serving profile moved to the C++ defaults.
    #[test]
    fn the_import_and_source_profiles_keep_their_table_options() {
        for cfg in [DbConfig::import(8), DbConfig::source(512)] {
            assert_eq!(cfg.block_size_kb, 16, "{:?}", cfg.profile);
            assert_eq!(cfg.row_cache_percent, None, "{:?}", cfg.profile);
            assert_eq!(cfg.row_cache_bytes(), 0);
            assert_eq!(cfg.block_cache_bytes(), cfg.read_cache_bytes());
            assert!(cfg.bottommost_filters, "an import's lookups are misses: {:?}", cfg.profile);
            assert!(!cfg.optimize_filters_for_memory);
            assert!(!cfg.quiet_engine_log);
        }
    }

    #[test]
    fn the_import_profile_scales_its_jobs_with_the_threads_it_is_given() {
        assert_eq!(DbConfig::import(1).background_threads, 2, "clamped up");
        assert!(DbConfig::import(32).background_jobs >= 32);
        assert!(DbConfig::import(32).max_subcompactions >= 32);
    }

    #[test]
    fn the_source_profile_only_moves_the_caches() {
        let s = DbConfig::source(512);
        assert_eq!(s.profile, DbProfile::Source);
        assert_eq!(s.read_cache_mb, 512);
        assert_eq!(s.write_buffer_mb, 8);
        assert!(!s.disable_wal, "it is never written, so this must stay the safe value");
        assert_eq!(s.level0_file_num_compaction_trigger, DbConfig::default().level0_file_num_compaction_trigger);
    }
}
