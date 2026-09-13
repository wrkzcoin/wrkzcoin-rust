// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The replay engine: read every raw block out of a C++ node's database,
//! validate it and apply it to our own state, checking our derived values
//! against the C++ records as it goes (spec/12-roadmap.md, stage 3 step 2).
//!
//! The engine is generic over both stores, so the same code that replays a
//! 4.2-million-block RocksDB on the Linux host replays a handful of blocks out
//! of a `MemStore` in the tests here. `wrkz-replay` is the command-line wrapper
//! that opens the two RocksDB databases and calls one of the two entry points.
//!
//! # Two modes
//!
//! [`replay`] is the **linear** pass: genesis to the top, in order, with the
//! checkpoint zone exactly as the C++ has it, so proof of work and signatures
//! are skipped below 4,188,000 just as the C++ skips them. It is dominated by
//! record decoding and state writes, it is resumable, and it is the only mode
//! that proves the emission and the cumulative difficulty of the whole chain.
//! It is also hours of work against a 40 GB database.
//!
//! [`replay_windows`] is the **windowed** pass: a slice of the chain around
//! every height where a rule changes ([`crate::windows`]), or a random sample,
//! with checkpoints switched **off** so that the proof of work, the ring
//! signatures, the transaction proof of work and every state rule run on real
//! blocks. Each window's starting state is seeded out of the source database —
//! block infos for the difficulty, timestamp and reward windows, key outputs
//! for ring resolution, and the spent-key-image heights where the source has
//! them (spec/11 prefix `7`). Minutes, not hours.
//!
//! The two leave incompatible states behind — a windowed run seeds history it
//! never validated — so the state carries a mode tag ([`TAG_LINEAR`],
//! [`TAG_WINDOWS`]) and a run of one mode refuses a directory written by the
//! other.

use crate::windows::Window;
use crate::{ChainState, Timings};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::tx::{relative_offsets_to_absolute, Input, Transaction};
use wrkz_primitives::Hash;
use wrkz_storage::reader::ChainReader;
use wrkz_storage::records::{CachedBlockInfo, RawBlockRecord};
use wrkz_storage::KvStore;

/// The mode tag [`replay`] writes into the state.
pub const TAG_LINEAR: &str = "linear";
/// The mode tag [`replay_windows`] writes into the state.
pub const TAG_WINDOWS: &str = "windows";

/// How many block infos are seeded below a window's first block.
///
/// The deepest window any rule reads is the legacy difficulty window,
/// `DIFFICULTY_WINDOW_V1 + DIFFICULTY_LAG_V1` = 2,895 entries, which only
/// applies when the *next* block is major version 1 or 2 (parent index 0 or 1).
/// Everything above that reads at most 100 (the reward size median), 61 (LWMA-2)
/// or 60 (the timestamp median). Seeding 3,000 covers all of them with room to
/// spare, and costs 3,000 record reads once per window.
pub const SEED_DEPTH: u32 = 3_000;

/// What to replay and how loudly.
#[derive(Clone, Copy, Debug)]
pub struct ReplayOptions {
    /// First index to apply. Must be the state's applied height plus one; a
    /// lower value is ignored with a note, a higher one is an error, because a
    /// replay cannot skip blocks.
    pub from: Option<u32>,
    /// Last index to apply; clamped to the source's top block.
    pub to: Option<u32>,
    /// Log a progress line every this many blocks.
    pub progress: u32,
    /// Make the state durable ([`ChainState::sync`]) every this many blocks.
    /// `0` syncs only at the end of the run.
    ///
    /// This costs nothing worth measuring while the state's write-ahead log is
    /// on, where a sync is one fsync of a log that is already written. It is
    /// there for the import profile, which turns the log **off**: with no log,
    /// the only thing that makes a write survive a crash is a memtable flush,
    /// and without this a crash would throw away everything since the engine
    /// last decided to flush one on its own. See
    /// `wrkz_storage::dbconfig::DbConfig::import`.
    pub sync_every: u32,
    /// A flag the caller may raise to stop the run cleanly.
    ///
    /// It is read once per block, between one block and the next, so a run that
    /// honours it stops on a block boundary: the state is flushed, made durable
    /// and left at a height a later run resumes from. It never stops inside a
    /// batch, because a batch is only ever committed whole.
    ///
    /// `&'static` so that [`ReplayOptions`] stays `Copy`; `wrkz-replay` points
    /// it at a process-wide static its signal handler sets, and a test leaks
    /// one of its own so that two tests cannot interfere.
    pub stop: Option<&'static AtomicBool>,
    /// Read the source's records a window at a time instead of one at a time.
    ///
    /// On by default. It cannot change *what* is read, *when* a record is
    /// decoded, or which block a fault is reported against — the documentation
    /// on the read-ahead itself makes that argument — only how many engine calls
    /// it takes. Turning it off is a way to measure what it is worth, and a way
    /// out for an operator whose source database sits on something where a large
    /// `MultiGet` behaves badly.
    pub read_ahead: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self { from: None, to: None, progress: 10_000, sync_every: 0, stop: None, read_ahead: true }
    }
}

impl ReplayOptions {
    /// Whether the caller has asked for the run to stop.
    fn stopping(&self) -> bool {
        self.stop.is_some_and(|f| f.load(Ordering::Relaxed))
    }
}

/// Where the replay's time went.
///
/// The four spans partition the linear loop, so they add up to `elapsed` bar
/// the progress lines themselves. `chain` breaks `add_block` down further into
/// the decode, validate and commit the state itself measured.
///
/// This exists because "258 blocks/s" is not a diagnosis. An import at 3.9
/// milliseconds a block, on blocks whose whole validation cost is sixty
/// microseconds, is spending its time somewhere else, and there are only four
/// somewhere-elses: reading the source, decoding, validating, and writing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayCost {
    /// Blocks the linear pass applied.
    pub blocks: u64,
    /// Reading the raw block record out of the source database.
    pub source_read: Duration,
    /// The per-block cross-check against the source: reading the `6` record and
    /// the `5` record, and comparing them.
    pub source_check: Duration,
    /// The whole of [`ChainState::add_block`].
    pub add_block: Duration,
    /// Committing batches: the batch boundary that may flush, and the periodic
    /// and final durability syncs.
    pub commit: Duration,
    /// The whole loop.
    pub elapsed: Duration,
    /// What [`ChainState`] measured inside `add_block`.
    pub chain: Timings,
    /// Batched reads issued against the source by the read-ahead.
    pub source_reads: u64,
    /// Records those reads fetched.
    pub source_records: u64,
    /// Batch boundaries at which the store committed.
    pub flushes: u64,
    /// [`crate::Config::validate_threads`], so that the curve line can say what
    /// the batch settle had to work with.
    pub validate_threads: usize,
}

impl ReplayCost {
    /// `part` as a percentage of the whole loop.
    pub fn percent(&self, part: Duration) -> f64 {
        let total = self.elapsed.as_secs_f64();
        if total <= 0.0 {
            0.0
        } else {
            100.0 * part.as_secs_f64() / total
        }
    }

    /// Microseconds of `part` per block applied.
    pub fn micros(&self, part: Duration) -> f64 {
        if self.blocks == 0 {
            0.0
        } else {
            part.as_secs_f64() * 1e6 / self.blocks as f64
        }
    }

    /// The breakdown, as one line for an operator.
    ///
    /// Percentages of wall clock, then microseconds per block, so that a run
    /// whose total is dominated by one phase says which and by how much.
    pub fn line(&self) -> String {
        let c = &self.chain;
        format!(
            "cost/block {:.0}us = source read {:.0} ({:.0}%) + decode {:.0} ({:.0}%) + validate {:.0} ({:.0}%) \
             + commit {:.0} ({:.0}%) + cross-check {:.0} ({:.0}%) + batch commit {:.0} ({:.0}%) + other {:.0}",
            self.micros(self.elapsed),
            self.micros(self.source_read),
            self.percent(self.source_read),
            self.micros(c.decode),
            self.percent(c.decode),
            self.micros(c.validate),
            self.percent(c.validate),
            self.micros(c.commit),
            self.percent(c.commit),
            self.micros(self.source_check),
            self.percent(self.source_check),
            self.micros(self.commit),
            self.percent(self.commit),
            self.micros(
                self.elapsed
                    .saturating_sub(self.source_read)
                    .saturating_sub(self.add_block)
                    .saturating_sub(self.source_check)
                    .saturating_sub(self.commit)
            ) + self.micros(c.other()),
        )
    }

    /// The `validate` phase, split into the steps of `Core::addBlock`.
    ///
    /// "validate is 74% of the run" is not a diagnosis either; this says which
    /// check. The step numbers are the ones in `ChainState::add_to_main`.
    pub fn validate_line(&self) -> String {
        let p = self.chain.phases;
        let per = |n: u64| if self.blocks == 0 { 0.0 } else { n as f64 / self.blocks as f64 };
        format!(
            "validate {:.0}us = size {:.0} + block checks {:.0} [version {:.0} + timestamp {:.0} \
             + coinbase {:.0} over {:.1} outputs + other {:.0}] + difficulty {:.0} + tx list {:.0} \
             + transactions {:.0} [batch settle {:.0}] + reward {:.0} + {} {:.0} + other {:.0}",
            self.micros(self.chain.validate),
            self.micros(p.size),
            self.micros(p.block_checks),
            self.micros(p.block_version),
            self.micros(p.block_timestamp),
            self.micros(p.block_coinbase),
            per(p.coinbase_outputs),
            self.micros(p.block_other()),
            self.micros(p.difficulty),
            self.micros(p.tx_list),
            self.micros(p.transactions),
            self.micros(p.settle),
            self.micros(p.reward),
            if p.pow_blocks == 0 { "checkpoint" } else { "PROOF OF WORK" },
            self.micros(p.checkpoint_or_pow),
            self.micros(self.chain.validate_other()),
        )
    }

    /// The per-block curve work: how many ed25519 operations a block asked for,
    /// which is what the `validate` phase is mostly made of.
    ///
    /// A key-image domain check is one scalar multiplication and a `check_key`
    /// one point decompression, and **neither is skipped inside the checkpoint
    /// zone** — only the ring signatures and the transaction proof of work are.
    /// So a linear import whose `validate` grows with height is usually reading
    /// this line: the scalarmult count tracks transaction volume, not block
    /// count.
    pub fn curve_line(&self) -> String {
        let p = self.chain.phases;
        let per = |n: u64| if self.blocks == 0 { 0.0 } else { n as f64 / self.blocks as f64 };
        format!(
            "curve work/block: {:.2} key inputs, {:.2} tx outputs, {:.2} coinbase outputs; batched \
             {:.2} scalarmults (key image domain) + {:.2} check_keys + {:.2} ring signatures, \
             settled on up to {} thread(s)",
            per(p.tx_inputs),
            per(p.tx_outputs),
            per(p.coinbase_outputs),
            per(p.key_image_checks),
            per(p.output_key_checks),
            per(p.ring_checks),
            self.validate_threads,
        )
    }

    /// The block-info window line: whether the in-memory recent window is
    /// actually answering the difficulty, timestamp and reward windows, whether
    /// anything in `validate` reached the store at all, and whether any block
    /// ran a proof of work.
    ///
    /// A linear pass inside the checkpoint zone must report zero proof-of-work
    /// blocks, a 100% hit rate and zero reads from `validate`. Any of those
    /// being otherwise is most of the explanation of a slow import, and none of
    /// them can be inferred from a duration.
    pub fn window_line(&self) -> String {
        let p = self.chain.phases;
        let per = |n: u64| if self.blocks == 0 { 0.0 } else { n as f64 / self.blocks as f64 };
        let hits = if p.info_lookups == 0 { 100.0 } else { 100.0 * p.info_cache_hits as f64 / p.info_lookups as f64 };
        format!(
            "block-info windows: {:.1} lookups/block, {hits:.2}% from the recent window, {:.2} \
             store reads/block; validate issued {:.2} store reads/block in total; proof of work on \
             {} of {} blocks",
            per(p.info_lookups),
            per(p.info_store_reads),
            per(p.store_reads),
            p.pow_blocks,
            self.blocks,
        )
    }
    /// The engine-operation line: how many batched source reads, how many
    /// records they carried, and how many state commits there were.
    pub fn engine_line(&self) -> String {
        let per_block = |n: u64| if self.blocks == 0 { 0.0 } else { n as f64 / self.blocks as f64 };
        format!(
            "source: {} batched reads for {} records ({:.2} reads/block, {:.1} records/read); \
             state: {} commits ({:.0} blocks/commit)",
            self.source_reads,
            self.source_records,
            per_block(self.source_reads),
            if self.source_reads == 0 { 0.0 } else { self.source_records as f64 / self.source_reads as f64 },
            self.flushes,
            if self.flushes == 0 { 0.0 } else { self.blocks as f64 / self.flushes as f64 },
        )
    }
}

/// Where the replay ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayReport {
    /// Our applied top index.
    pub top: u32,
    /// How many blocks this run applied.
    pub applied: u32,
    /// The source database's top index.
    pub source_top: u32,
    /// Whether [`ReplayOptions::stop`] ended the run before `to`. The state is
    /// consistent and resumable either way; this only says whether there is
    /// more to do.
    pub stopped: bool,
    /// Where the time went.
    pub cost: ReplayCost,
}

/// A failure, already phrased for an operator: it names the block index, its
/// hash where one exists, and the rule or mismatch that stopped the replay.
pub type ReplayError = String;

/// Indexes the read-ahead fetches at once when it has nothing to go on yet.
const PREFETCH_START: u32 = 128;

/// Ceiling on the read-ahead window. Past a few hundred the per-call cost is
/// long since amortised and the only thing a larger window buys is memory.
const PREFETCH_MAX: u32 = 512;

/// Raw block bytes the read-ahead is allowed to hold. Blocks are not uniform —
/// an empty one is a few hundred bytes, a full one over a hundred kilobytes —
/// so the window is sized from the bytes the last refill actually carried.
const PREFETCH_BYTES: usize = 32 << 20;

/// The three records the linear pass reads per block, fetched a window at a
/// time instead of one at a time.
///
/// # Why this is worth anything
///
/// Every index costs three point lookups into the source: the `4` raw block,
/// the `6` block info the cross-check compares against, and the `5` hash index
/// that check then confirms. The keys are KV documents holding **little-endian**
/// integers (spec/11), so walking the chain in numeric order walks the key
/// space in an order that is not even locally sequential: index `N` and index
/// `N + 1` differ in their lowest key byte and sort in different regions of the
/// table. There is no iterator to use, and no locality to exploit — but there is
/// a batch. `MultiGet` takes one snapshot, makes one pass over the block cache
/// and the filter blocks for the whole group, and lets the engine coalesce the
/// reads that do land in the same SST file, instead of paying all of that per
/// key.
///
/// # What it does not change
///
/// Documents are held **undecoded**. A record that does not parse must be
/// reported against its own index, and decoding a window eagerly would report
/// block `N + 40`'s corruption while the run was at block `N`. So a refill
/// fetches bytes and nothing else; every decode still happens at the block it
/// belongs to, in the same order, with the same message.
///
/// The `5` lookup is keyed by a hash rather than an index, so it is prefetched
/// by the hash the `6` record *claims*. That is exact rather than approximate:
/// the cross-check compares our hash against the `6` record's before it ever
/// looks at the `5` record, so by the time the answer is wanted the two are
/// known to be equal. [`SourceAhead::block_index_by_hash`] confirms that
/// equality anyway and falls back to a direct read if it does not hold, so the
/// window can only ever save a read, never answer a different question.
struct SourceAhead {
    /// Whether to fetch ahead at all. A windowed replay jumps between distant
    /// slices of the chain, so its read-ahead would be thrown away at every
    /// boundary; it uses a disabled one and reads directly.
    enabled: bool,
    /// The first index the window holds.
    from: u32,
    /// One past the last index the window holds.
    until: u32,
    /// `4` documents for `from..until`.
    raw: Vec<Option<Vec<u8>>>,
    /// `6` documents for `from..until`.
    infos: Vec<Option<Vec<u8>>>,
    /// The `6` documents that parsed, already decoded — a refill has to decode
    /// them anyway, to know which hashes to look the `5` records up by, and
    /// decoding a record twice is not free. An entry is `None` where the
    /// document was absent **or did not parse**, and in the second case the
    /// caller decodes the document itself when it reaches that block, which is
    /// what keeps a corrupt record reported against its own index.
    decoded: Vec<Option<CachedBlockInfo>>,
    /// For `from..until`, the block hash the `6` record names and the `5`
    /// record's answer for it. `None` where the `6` record was absent or did
    /// not parse, which leaves the fallback to do the work and report it.
    hash_index: Vec<Option<(Hash, Option<u32>)>>,
    /// Indexes to fetch at the next refill.
    window: u32,
    /// Batched reads issued, and records they carried.
    reads: u64,
    records: u64,
}

impl SourceAhead {
    fn new() -> Self {
        Self {
            enabled: true,
            from: 0,
            until: 0,
            raw: Vec::new(),
            infos: Vec::new(),
            decoded: Vec::new(),
            hash_index: Vec::new(),
            window: PREFETCH_START,
            reads: 0,
            records: 0,
        }
    }

    /// A read-ahead that never reads ahead: every lookup goes straight to the
    /// source, exactly as the code did before this existed.
    fn direct() -> Self {
        Self { enabled: false, ..Self::new() }
    }

    fn slot(&self, index: u32) -> Option<usize> {
        (self.enabled && index >= self.from && index < self.until).then(|| (index - self.from) as usize)
    }

    /// Fill the window with `[index, min(index + window, end + 1))`.
    fn refill<Src: KvStore>(&mut self, source: &ChainReader<Src>, index: u32, end: u32) -> Result<(), ReplayError> {
        let span = self.window.min(end.saturating_sub(index).saturating_add(1)).max(1);
        let indexes: Vec<u32> = (index..index.saturating_add(span)).collect();
        self.raw = source
            .raw_block_docs(&indexes)
            .map_err(|e| format!("block {index}: reading raw block records ahead: {e}"))?;
        self.infos = source
            .block_info_docs(&indexes)
            .map_err(|e| format!("block {index}: reading block info records ahead: {e}"))?;
        self.reads += 2;
        self.records += 2 * indexes.len() as u64;

        // The hashes the infos name, so the `5` records can be fetched in one
        // batch too. An info that is absent or unparsable contributes no hash;
        // its index simply has no cached answer and falls back.
        let mut hashes: Vec<Hash> = Vec::with_capacity(self.infos.len());
        let mut at: Vec<usize> = Vec::with_capacity(self.infos.len());
        self.decoded = Vec::with_capacity(self.infos.len());
        for (i, doc) in self.infos.iter().enumerate() {
            let info = doc.as_ref().and_then(|doc| CachedBlockInfo::decode(doc).ok());
            if let Some(info) = &info {
                at.push(i);
                hashes.push(info.block_hash);
            }
            self.decoded.push(info);
        }
        self.hash_index = vec![None; self.infos.len()];
        if !hashes.is_empty() {
            let found = source
                .block_indexes_by_hash(&hashes)
                .map_err(|e| format!("block {index}: reading the C++ hash index ahead: {e}"))?;
            self.reads += 1;
            self.records += hashes.len() as u64;
            for ((slot, hash), answer) in at.into_iter().zip(hashes).zip(found) {
                self.hash_index[slot] = Some((hash, answer));
            }
        }

        self.from = index;
        self.until = index + indexes.len() as u32;

        // Size the next window from what this one weighed. A window of empty
        // blocks stays at the ceiling; one of full blocks shrinks to whatever
        // fits the budget, which is the point — the memory a read-ahead is
        // allowed to hold has to be a byte budget, not a block count.
        let bytes: usize = self.raw.iter().flatten().map(|d| d.len()).sum();
        let average = bytes / indexes.len().max(1);
        self.window = match PREFETCH_BYTES.checked_div(average) {
            // Nothing came back at all: keep the window wide, since a run of
            // absent records is about to end the replay anyway.
            None => PREFETCH_MAX,
            Some(fits) => (fits as u32).clamp(1, PREFETCH_MAX),
        };
        Ok(())
    }

    /// The `4` document for `index`, taken out of the window.
    ///
    /// Each index is asked for once, in order, so taking rather than cloning is
    /// exact and saves copying a block body that can be a megabyte.
    fn raw_doc<Src: KvStore>(
        &mut self,
        source: &ChainReader<Src>,
        index: u32,
        end: u32,
    ) -> Result<Option<Vec<u8>>, ReplayError> {
        if !self.enabled {
            self.reads += 1;
            self.records += 1;
            return source
                .raw_block_docs(&[index])
                .map(|mut v| v.pop().flatten())
                .map_err(|e| format!("block {index}: reading the raw block record: {e}"));
        }
        if self.slot(index).is_none() {
            self.refill(source, index, end)?;
        }
        let slot = self.slot(index).expect("just refilled at this index");
        Ok(self.raw[slot].take())
    }

    /// The `6` record for `index`: absent, or decoded.
    ///
    /// A record that the refill already decoded is handed back as it stands —
    /// decoding is a pure function of the bytes, so doing it twice could only
    /// waste time. One that the refill could not decode is decoded again here,
    /// so that the failure is raised at this block with this block's message.
    fn block_info<Src: KvStore>(
        &mut self,
        source: &ChainReader<Src>,
        index: u32,
        end: u32,
    ) -> Result<Option<CachedBlockInfo>, ReplayError> {
        let doc = if !self.enabled {
            self.reads += 1;
            self.records += 1;
            source
                .block_info_docs(&[index])
                .map(|mut v| v.pop().flatten())
                .map_err(|e| format!("block {index}: reading the C++ block info: {e}"))?
        } else {
            if self.slot(index).is_none() {
                self.refill(source, index, end)?;
            }
            let slot = self.slot(index).expect("just refilled at this index");
            if let Some(info) = self.decoded[slot].take() {
                return Ok(Some(info));
            }
            self.infos[slot].take()
        };
        match doc {
            Some(doc) => CachedBlockInfo::decode(&doc)
                .map(Some)
                .map_err(|e| format!("block {index}: reading the C++ block info: {e}")),
            None => Ok(None),
        }
    }

    /// The `5` record's answer for `hash`.
    ///
    /// Answered from the window only when the window's cached hash for this
    /// index is the hash asked about — which the cross-check has already proved
    /// it is — and by a direct read otherwise, so the answer is the same either
    /// way.
    fn block_index_by_hash<Src: KvStore>(
        &self,
        source: &ChainReader<Src>,
        index: u32,
        hash: &Hash,
    ) -> Result<Option<u32>, ReplayError> {
        if let Some(slot) = self.slot(index) {
            if let Some((cached, answer)) = &self.hash_index[slot] {
                if cached == hash {
                    return Ok(*answer);
                }
            }
        }
        source.block_index_by_hash(hash).map_err(|e| format!("block {index}: reading the C++ hash index: {e}"))
    }
}

/// What one window cost and covered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowReport {
    pub window: Window,
    pub blocks: u32,
    /// Transactions other than the coinbase.
    pub transactions: u64,
    /// Ring signatures verified: one per key input, since checkpoints are off
    /// inside a window and `validateTransactionInputsExpensive` therefore runs
    /// for every one of them.
    pub rings: u64,
    /// The largest number of key inputs any single transaction of this window
    /// carried, 0 when the window held no key inputs at all.
    ///
    /// `rings` divided by `transactions` only gives the mean, and a window of
    /// mostly two-input transactions with a handful of 100-input fusions has
    /// the same mean as a window of uniform three-input spends while costing
    /// something quite different per transaction. This is what tells the two
    /// apart, and it is the number that settles whether a slow stretch is
    /// large fusion transactions (twelve key inputs at the least, by
    /// `FUSION_TX_MIN_INPUT_COUNT`) rather than ring count alone.
    pub max_tx_inputs: u32,
    pub elapsed_secs: f64,
}

/// Where a windowed run ended up.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowsReport {
    pub windows: Vec<WindowReport>,
    pub source_top: u32,
    /// Whether the source database answered spent-key-image lookups. When it
    /// does not (a lite or pruned database with no prefix `7` records), a
    /// double spend is only caught if both halves are inside one window.
    pub key_image_history: bool,
}

impl WindowsReport {
    pub fn blocks(&self) -> u64 {
        self.windows.iter().map(|w| w.blocks as u64).sum()
    }
    pub fn transactions(&self) -> u64 {
        self.windows.iter().map(|w| w.transactions).sum()
    }
    pub fn rings(&self) -> u64 {
        self.windows.iter().map(|w| w.rings).sum()
    }
    /// The largest number of key inputs any transaction of any window carried.
    pub fn max_tx_inputs(&self) -> u32 {
        self.windows.iter().map(|w| w.max_tx_inputs).max().unwrap_or(0)
    }
}

/// Replay `source` into `chain`.
///
/// Every applied block is checked against the C++ records for its index: the
/// block hash against the `6` record and against the `5` (hash → index) record,
/// and our cumulative difficulty, already-generated coins, cumulative block
/// size and transaction count against the `6` record's fields. The genesis
/// record is checked the same way before the first block, which is what proves
/// our `apply_genesis` against `DatabaseBlockchainCache::addGenesisBlock`.
///
/// The first mismatch stops the run and is returned; the state keeps everything
/// applied before it, so a fixed build can be pointed at the same state
/// directory and will resume one block below the failure.
pub fn replay<Src: KvStore, Dst: KvStore>(
    source: &ChainReader<Src>,
    chain: &mut ChainState<Dst>,
    opts: &ReplayOptions,
    log: &mut dyn FnMut(&str),
) -> Result<ReplayReport, ReplayError> {
    let source_top = check_source(source, log)?;
    ensure_mode(chain, TAG_LINEAR)?;

    let applied = chain.tip_index().ok_or("our state has no genesis block")?;
    let mut start = applied + 1;
    if let Some(from) = opts.from {
        if from > start {
            return Err(format!(
                "--from {from} but our state only reaches {applied}; a replay cannot skip blocks, \
                 rerun without --from to continue from {start}"
            ));
        }
        if from < start {
            log(&format!("--from {from} is at or below the applied height {applied}; continuing from {start}"));
        } else {
            start = from;
        }
    }
    let end = opts.to.unwrap_or(source_top).min(source_top);
    if start > end {
        log(&format!("nothing to do: our state is at {applied}, the range ends at {end}"));
        // `ensure_mode` may have written the mode tag, and a batched store is
        // holding it. Commit before returning rather than leaving it to the
        // store's destructor, which has nowhere to report a failure.
        chain.sync().map_err(|e| format!("committing the state: {e}"))?;
        return Ok(ReplayReport { top: applied, applied: 0, source_top, stopped: false, cost: ReplayCost::default() });
    }
    log(&format!("replaying {start} to {end} (source top {source_top}), state at {applied}"));

    // Genesis is constructed, never downloaded (spec/07), so the loop below
    // would never compare it. Do it once, here.
    if start == 1 {
        let ours = *chain.tip_info().ok_or("our state has no genesis block")?;
        let theirs = source
            .block_info(0)
            .map_err(|e| format!("block 0: reading the C++ block info: {e}"))?
            .ok_or("block 0: the C++ database has no block info record")?;
        compare(0, &ours, &theirs)?;
    }

    chain.reset_timings();
    let began = Instant::now();
    let mut cost = ReplayCost::default();
    let mut ahead = if opts.read_ahead { SourceAhead::new() } else { SourceAhead::direct() };

    // The loop is its own function so that whatever it returns — the end of the
    // range, an interrupt, or a failure — the state is committed and made
    // durable exactly once, below. A batched store holds up to `--batch-blocks`
    // blocks in memory, and a return path that forgot to commit them would
    // silently throw away work that the caller has been told was applied.
    let outcome = linear_pass(source, chain, opts, log, start, end, began, &mut cost, &mut ahead);

    let committing = Instant::now();
    let committed = chain.sync().map_err(|e| format!("committing the state: {e}"));
    cost.commit += committing.elapsed();
    let stopped = match outcome {
        Ok(stopped) => {
            committed?;
            stopped
        }
        // A failure wins over a commit failure: it is the one that says what
        // went wrong. The commit is still attempted, so that everything applied
        // before the failure survives, which is what makes a fixed build able
        // to resume just below it.
        Err(e) => {
            if let Err(commit) = committed {
                return Err(format!("{e} (and {commit})"));
            }
            return Err(e);
        }
    };

    cost.elapsed = began.elapsed();
    cost.chain = chain.timings();
    cost.validate_threads = chain.config().validate_threads;
    cost.source_reads = ahead.reads;
    cost.source_records = ahead.records;
    let top = chain.tip_index().expect("at least genesis");
    cost.blocks = (top as u64 + 1).saturating_sub(start as u64);
    let info = *chain.tip_info().expect("at least genesis");
    if stopped {
        log(&format!("interrupted after block {top}: the state is committed and a later run resumes from {}", top + 1));
    }
    log(&format!(
        "top {top} hash {} cumulative difficulty {} coins {} in {:.1}s",
        hex::encode(info.block_hash),
        info.cumulative_difficulty,
        info.already_generated_coins,
        cost.elapsed.as_secs_f64()
    ));
    log(&cost.line());
    log(&cost.validate_line());
    log(&cost.curve_line());
    log(&cost.window_line());
    log(&cost.engine_line());
    Ok(ReplayReport { top, applied: cost.blocks as u32, source_top, stopped, cost })
}

/// The linear pass's loop. Returns whether [`ReplayOptions::stop`] ended it.
fn linear_pass<Src: KvStore, Dst: KvStore>(
    source: &ChainReader<Src>,
    chain: &mut ChainState<Dst>,
    opts: &ReplayOptions,
    log: &mut dyn FnMut(&str),
    start: u32,
    end: u32,
    began: Instant,
    cost: &mut ReplayCost,
    ahead: &mut SourceAhead,
) -> Result<bool, ReplayError> {
    let mut window = Instant::now();
    let mut window_start = start;
    let mut since_sync = 0u32;

    for index in start..=end {
        // Between two blocks, so the batch below is whole either way.
        if opts.stopping() {
            return Ok(true);
        }

        let reading = Instant::now();
        let doc = ahead.raw_doc(source, index, end)?.ok_or_else(|| {
            format!(
                "block {index}: no raw block record. A pruned or lite database has no block \
                 bodies down there and cannot be replayed from genesis"
            )
        })?;
        let raw =
            RawBlockRecord::decode(&doc).map_err(|e| format!("block {index}: reading the raw block record: {e}"))?;
        cost.source_read += reading.elapsed();

        let adding = Instant::now();
        let outcome = chain.add_block(&raw.block, &raw.transactions).map_err(|e| {
            // Name the block even when the block itself is what failed: if we
            // got this far the blob parsed, so it has a hash.
            let hash = BlockTemplate::from_bytes(&raw.block)
                .ok()
                .and_then(|b| b.hash().ok())
                .map(hex::encode)
                .unwrap_or_else(|| "<unparsable>".into());
            format!("block {index} ({hash}): {e}")
        })?;
        cost.add_block += adding.elapsed();

        let checking = Instant::now();
        check_against_source(source, chain, index, &outcome, ahead, end)?;
        cost.source_check += checking.elapsed();

        // The block is applied and it agrees with the source: a point a later
        // run can resume from, and therefore the only place a batch may be
        // committed.
        let committing = Instant::now();
        if chain.block_boundary().map_err(|e| format!("block {index}: committing the state batch: {e}"))? {
            cost.flushes += 1;
        }
        since_sync += 1;
        if opts.sync_every > 0 && since_sync >= opts.sync_every {
            chain.sync().map_err(|e| format!("block {index}: making the state durable: {e}"))?;
            since_sync = 0;
        }
        cost.commit += committing.elapsed();

        if index % opts.progress == 0 || index == end {
            let done = index - window_start + 1;
            let rate = done as f64 / window.elapsed().as_secs_f64().max(1e-9);
            let overall = (index - start + 1) as f64 / began.elapsed().as_secs_f64().max(1e-9);
            // From the recent rate, not the overall one: the chain's early
            // blocks are nearly empty and would make any average optimistic.
            let remaining = end.saturating_sub(index);
            let eta = if remaining > 0 && rate > 0.0 {
                format!("  ETA {}", format_eta(remaining as f64 / rate))
            } else {
                String::new()
            };
            log(&format!(
                "{index}/{end}  {rate:.0} blocks/s (overall {overall:.0}){eta}  cum diff {}  coins {}",
                outcome.cumulative_difficulty, outcome.already_generated_coins
            ));
            let mut snapshot = *cost;
            snapshot.elapsed = began.elapsed();
            snapshot.chain = chain.timings();
            snapshot.validate_threads = chain.config().validate_threads;
            snapshot.blocks = (index - start + 1) as u64;
            log(&format!("  {}", snapshot.line()));
            log(&format!("  {}", snapshot.validate_line()));
            log(&format!("  {}", snapshot.curve_line()));
            log(&format!("  {}", snapshot.window_line()));
            let pending = chain.pending_bytes();
            if pending > 0 {
                log(&format!("  state batch holding {} KiB not yet committed", pending / 1024));
            }
            window = Instant::now();
            window_start = index + 1;
        }
    }
    Ok(false)
}

/// A coarse duration for the progress line: the two largest units, no more.
fn format_eta(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    let (days, hours, minutes, seconds) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Our block info against the C++ `CachedBlockInfo` for the same index. Every
/// field of the record is a value this crate derived from the block and its
/// parent, so any disagreement is a consensus divergence and worth stopping on.
fn compare(
    index: u32,
    ours: &crate::BlockInfo,
    theirs: &wrkz_storage::records::CachedBlockInfo,
) -> Result<(), ReplayError> {
    let at = |what: &str, a: String, b: String| {
        format!("block {index} ({}): our {what} is {a} but the C++ record says {b}", hex::encode(ours.block_hash))
    };
    if ours.block_hash != theirs.block_hash {
        return Err(format!(
            "block {index}: our block hash is {} but the C++ record says {}",
            hex::encode(ours.block_hash),
            hex::encode(theirs.block_hash)
        ));
    }
    if ours.cumulative_difficulty != theirs.cumulative_difficulty {
        return Err(at(
            "cumulative difficulty",
            ours.cumulative_difficulty.to_string(),
            theirs.cumulative_difficulty.to_string(),
        ));
    }
    if ours.already_generated_coins != theirs.already_generated_coins {
        return Err(at(
            "already-generated coins",
            ours.already_generated_coins.to_string(),
            theirs.already_generated_coins.to_string(),
        ));
    }
    if ours.block_size != theirs.block_size {
        return Err(at("cumulative block size", ours.block_size.to_string(), theirs.block_size.to_string()));
    }
    if ours.already_generated_transactions != theirs.already_generated_transactions {
        return Err(at(
            "transaction count",
            ours.already_generated_transactions.to_string(),
            theirs.already_generated_transactions.to_string(),
        ));
    }
    if ours.timestamp != theirs.timestamp {
        return Err(at("timestamp", ours.timestamp.to_string(), theirs.timestamp.to_string()));
    }
    Ok(())
}

/// The source database's schema version and top index, refusing anything that
/// is not a WrkzCoin database this build understands.
fn check_source<Src: KvStore>(source: &ChainReader<Src>, log: &mut dyn FnMut(&str)) -> Result<u32, ReplayError> {
    match source.schema_version().map_err(|e| format!("reading db_scheme_version: {e}"))? {
        Some(v) if v > wrkz_primitives::constants::DB_SCHEME_VERSION => {
            return Err(format!("source database schema version {v} is newer than this build knows"));
        }
        Some(v) => log(&format!("source database schema version {v}")),
        None => return Err("no db_scheme_version record: this is not a WrkzCoin database".into()),
    }
    source.last_block_index().map_err(|e| format!("reading last_block_index: {e}"))
}

/// Claim the state directory for one mode, or refuse it.
///
/// A windowed run seeds block infos it never validated, so a linear run must
/// not continue on top of one; and a linear chain is not a set of windows. An
/// untagged state is adopted only when it holds nothing but genesis, so a
/// directory written before mode tags existed cannot be silently reinterpreted.
fn ensure_mode<Dst: KvStore>(chain: &mut ChainState<Dst>, mode: &str) -> Result<(), ReplayError> {
    match chain.tag().map_err(|e| e.to_string())? {
        Some(existing) if existing == mode => Ok(()),
        Some(existing) => Err(format!(
            "this state directory was written by a `{existing}` replay and a `{mode}` replay cannot \
             continue on it: point --state at a different directory, or delete this one"
        )),
        None if chain.tip_index().unwrap_or(0) > 0 => Err(format!(
            "this state directory holds a chain but no mode tag, so a `{mode}` replay cannot tell \
             what produced it: point --state at a different directory, or delete this one"
        )),
        None => chain.set_tag(mode).map_err(|e| e.to_string()),
    }
}

/// Replay only `windows`, seeding each one's starting state from `source`
/// (spec/12-roadmap.md, stage 3 step 2: the run that takes minutes, not hours).
///
/// Checkpoints are switched off for the whole run, so inside every window the
/// proof of work, the ring signatures, the transaction proof of work and every
/// state rule are checked on real blocks — which is exactly what a linear run
/// with checkpoints on does *not* do below 4,188,000.
///
/// Each block is checked against the C++ records the same way [`replay`] checks
/// them. The genesis comparison is not repeated: a windowed run seeds index 0
/// from the source rather than constructing it, so there would be nothing to
/// compare.
pub fn replay_windows<Src: KvStore, Dst: KvStore>(
    source: &ChainReader<Src>,
    chain: &mut ChainState<Dst>,
    windows: &[Window],
    opts: &ReplayOptions,
    log: &mut dyn FnMut(&str),
) -> Result<WindowsReport, ReplayError> {
    let source_top = check_source(source, log)?;
    ensure_mode(chain, TAG_WINDOWS)?;

    // Off for the whole run: a window exists to make the rules the checkpoint
    // zone skips actually run.
    chain.checkpoints_mut().disable_from(Some(0));

    let windows: Vec<Window> = windows.iter().copied().filter(|w| w.start >= 1 && w.start <= source_top).collect();
    let total_blocks: u64 = windows.iter().map(|w| w.end.min(source_top) as u64 + 1 - w.start as u64).sum();
    log(&format!("{} window(s), {total_blocks} blocks, checkpoints off (source top {source_top})", windows.len()));

    let mut report = WindowsReport { windows: Vec::with_capacity(windows.len()), source_top, key_image_history: true };
    let began = Instant::now();
    // One cache for the run, emptied at every window boundary. See `SeedCache`.
    let mut cache = SeedCache::default();
    // A windowed run jumps between slices of the chain that are millions of
    // blocks apart, so a read-ahead would be discarded at every boundary and at
    // every block inside one that the seeding already had to read. It reads
    // directly, exactly as it did before the linear pass gained one.
    let mut direct = SourceAhead::direct();
    let (mut run_lookups, mut run_hits) = (0u64, 0u64);

    for w in &windows {
        let end = w.end.min(source_top);
        cache.start_window();
        seed_window(source, chain, w.start)?;
        let started = Instant::now();
        let mut transactions = 0u64;
        let mut rings = 0u64;
        let mut max_tx_inputs = 0u32;

        for index in w.start..=end {
            let raw = source
                .raw_block(index)
                .map_err(|e| format!("block {index}: reading the raw block record: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "block {index}: no raw block record. A pruned or lite database has no block \
                         bodies there and that window cannot be replayed"
                    )
                })?;

            let block = BlockTemplate::from_bytes(&raw.block)
                .map_err(|e| format!("block {index}: the raw block record does not parse: {e}"))?;
            let txs = raw
                .transactions
                .iter()
                .map(|b| Transaction::from_bytes(b))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| format!("block {index}: a transaction blob does not parse: {e}"))?;

            let seeded = seed_block_state(source, chain, index, &block, &txs, &mut cache, log)?;
            if !seeded.key_image_history {
                report.key_image_history = false;
            }
            transactions += txs.len() as u64;
            rings += seeded.rings;
            max_tx_inputs = max_tx_inputs.max(seeded.max_tx_inputs);

            let outcome = chain.add_block(&raw.block, &raw.transactions).map_err(|e| {
                let hash = block.hash().ok().map(hex::encode).unwrap_or_else(|| "<unhashable>".into());
                format!("block {index} ({hash}): {e}")
            })?;
            check_against_source(source, chain, index, &outcome, &mut direct, end)?;
            // The same batch boundary the linear pass uses. A windowed run is
            // not resumable, but a batched store still has to be told where one
            // block ends and the next begins or it would hold the whole window.
            chain.block_boundary().map_err(|e| format!("block {index}: committing the state batch: {e}"))?;

            if opts.progress > 0 && index.is_multiple_of(opts.progress) {
                let done = index + 1 - w.start;
                let rate = done as f64 / started.elapsed().as_secs_f64().max(1e-9);
                log(&format!("  {index}/{end}  {rate:.0} blocks/s"));
            }
        }

        let elapsed_secs = started.elapsed().as_secs_f64();
        let blocks = end + 1 - w.start;
        log(&format!(
            "window {w}: {blocks} blocks, {transactions} txs, {rings} rings, max {max_tx_inputs} inputs/tx, \
             {elapsed_secs:.2}s, {:.0} blocks/s (ring-member cache {:.1}% of {} lookups)",
            blocks as f64 / elapsed_secs.max(1e-9),
            cache.hit_rate(),
            cache.lookups
        ));
        run_lookups += cache.lookups;
        run_hits += cache.hits;
        report.windows.push(WindowReport { window: *w, blocks, transactions, rings, max_tx_inputs, elapsed_secs });
        // Nothing of this window is read again, so commit it rather than carry
        // it into the seeding of the next one.
        chain.flush().map_err(|e| format!("window {w}: committing the state: {e}"))?;
    }

    let elapsed = began.elapsed().as_secs_f64();
    log(&format!(
        "{} windows, {} blocks, {} txs, {} rings, max {} inputs/tx in {elapsed:.1}s ({:.0} blocks/s)",
        report.windows.len(),
        report.blocks(),
        report.transactions(),
        report.rings(),
        report.max_tx_inputs(),
        report.blocks() as f64 / elapsed.max(1e-9)
    ));
    if run_lookups > 0 {
        let saved = 100.0 * run_hits as f64 / run_lookups as f64;
        log(&format!(
            "ring-member seeding: {run_lookups} lookups, {run_hits} answered from the cache ({saved:.1}%), \
             {} read from the source",
            run_lookups - run_hits
        ));
    }
    if !report.key_image_history {
        log("note: the source database answered no spent-key-image lookups, so a double spend was only detected when both halves were inside one window");
    }
    chain.sync().map_err(|e| format!("committing the state: {e}"))?;
    Ok(report)
}

/// Give `chain` the block infos a window's first block will read, straight out
/// of the source database, and move the tip to just below the window.
///
/// This is the one place a windowed run takes something on trust: the infos
/// come from a database a C++ node validated. Everything the window itself
/// derives is still computed here and still compared against the source.
fn seed_window<Src: KvStore, Dst: KvStore>(
    source: &ChainReader<Src>,
    chain: &mut ChainState<Dst>,
    start: u32,
) -> Result<(), ReplayError> {
    let first = start.saturating_sub(SEED_DEPTH);
    let mut infos = Vec::with_capacity((start - first) as usize);
    for index in first..start {
        let info = source
            .block_info(index)
            .map_err(|e| format!("seeding {start}: reading the C++ block info for {index}: {e}"))?
            .ok_or_else(|| format!("seeding {start}: the C++ database has no block info for {index}"))?;
        infos.push((
            index,
            crate::BlockInfo {
                block_hash: info.block_hash,
                timestamp: info.timestamp,
                block_size: info.block_size,
                cumulative_difficulty: info.cumulative_difficulty,
                already_generated_coins: info.already_generated_coins,
                already_generated_transactions: info.already_generated_transactions,
            },
        ));
    }
    chain.import_history(&infos, start - 1).map_err(|e| format!("seeding {start}: writing the block infos: {e}"))
}

/// What [`seed_block_state`] found.
struct Seeded {
    rings: u64,
    /// The largest key-input count of any transaction in the block.
    max_tx_inputs: u32,
    key_image_history: bool,
}

/// How many `(amount, global index)` pairs one generation of
/// [`SeedCache`] holds before it is retired.
///
/// 524,288 pairs is 6 MB of keys plus the hash table around them, and the cache
/// keeps two generations, so it is bounded at roughly 25 MB however long the
/// run is. That matters for the linear shape — a single window spanning the
/// whole 4.2 million blocks — where an unbounded set would grow to every
/// distinct output the chain ever spent, tens of millions of pairs, and would
/// cost more in resident memory than the lookups it saves.
const SEED_CACHE_GENERATION: usize = 1 << 19;

/// The source lookups [`seed_block_state`] would otherwise repeat.
///
/// # What it is allowed to skip, and why that is sound
///
/// Seeding writes a `(amount, global index)` key output through
/// `import_state`, and that record is immutable: it is whatever the C++
/// database holds for that pair, which does not change while a replay runs.
/// Writing it a second time therefore writes the same bytes, so **skipping the
/// second write cannot change any later read** — the only thing lost is the
/// source read and the state write themselves, which is exactly the point.
/// Nothing removes a seeded output either: an unwind only deletes the outputs a
/// block of ours created, and those live above every real global index (see
/// the per-amount counters below).
///
/// Why the hit rate is worth having: decoy selection is recency-weighted, so
/// the outputs a window's transactions name cluster in a narrow band of global
/// indexes just below the window, and the same popular output is chosen as a
/// decoy over and over. Without a cache, seeding a block re-reads and re-writes
/// every ring member of every input, whether or not the block before it seeded
/// the same one.
///
/// # Eviction
///
/// Two generations, `hot` and `cold`. A lookup checks both; a miss inserts into
/// `hot`; when `hot` fills, it becomes `cold` and a fresh `hot` starts, so the
/// most recent [`SEED_CACHE_GENERATION`] pairs are always retained and at most
/// that many older ones are dropped at a stroke.
///
/// This is deliberately not an LRU: an LRU needs a per-entry link or clock
/// value updated on every *hit*, and hits are the common case here, so it would
/// put a write on the hot path to buy a slightly better retention curve for a
/// cache that no real run ever fills. Retiring a generation is one pointer swap
/// and one deallocation, amortised over half a million lookups. A dropped pair
/// costs one re-read, never a wrong answer.
///
/// The cache is also cleared between windows ([`SeedCache::start_window`]):
/// windows sit far apart on the chain and their ring members do not overlap, so
/// carrying one window's pairs into the next only wastes memory.
#[derive(Default)]
struct SeedCache {
    hot: HashSet<(u64, u32)>,
    cold: HashSet<(u64, u32)>,
    /// Amounts whose per-amount output counter has been seeded.
    ///
    /// Hoisted out of [`seed_block_state`], where it used to be rebuilt per
    /// block, up to the window. The count `outputs_count_for_amount` returns is
    /// a property of the whole source database rather than of a block, and the
    /// only thing it is used for is to push the outputs *we* create above every
    /// global index a ring can name. Seeding it once per window leaves our
    /// counter growing monotonically from there as the window's blocks add
    /// outputs, instead of being pulled back to the source's count at every
    /// block; both stay at or above the source count, which is all the property
    /// requires. Denominations are powers of ten times a digit, so this set
    /// holds a few hundred entries at most and needs no bound.
    amounts: HashSet<u64>,
    /// Ring-member lookups asked of this cache in the current window, and how
    /// many of them it answered.
    lookups: u64,
    hits: u64,
    /// Whether the "this source has no spent-key-image records" warning has
    /// already been printed. A run-level fact, so unlike everything else here
    /// it survives [`SeedCache::start_window`].
    warned_no_key_images: bool,
}

impl SeedCache {
    /// Start a window: drop everything the cache remembers about outputs and
    /// amounts, counters included.
    fn start_window(&mut self) {
        self.hot.clear();
        self.cold.clear();
        self.amounts.clear();
        self.lookups = 0;
        self.hits = 0;
    }

    /// Whether this `(amount, global index)` has already been seeded; records
    /// it as seeded if not.
    fn seen_output(&mut self, amount: u64, global_index: u32) -> bool {
        self.lookups += 1;
        let key = (amount, global_index);
        if self.hot.contains(&key) || self.cold.contains(&key) {
            self.hits += 1;
            return true;
        }
        if self.hot.len() >= SEED_CACHE_GENERATION {
            self.cold = std::mem::take(&mut self.hot);
        }
        self.hot.insert(key);
        false
    }

    /// Whether this amount's counter has already been seeded; records it as
    /// seeded if not.
    fn seen_amount(&mut self, amount: u64) -> bool {
        !self.amounts.insert(amount)
    }

    /// `hits / lookups` as a percentage, 0 when nothing was looked up.
    fn hit_rate(&self) -> f64 {
        if self.lookups == 0 {
            0.0
        } else {
            100.0 * self.hits as f64 / self.lookups as f64
        }
    }
}

/// Give `chain` the outputs, per-amount counters and spent-key-image heights
/// that validating this one block will read.
///
/// Ring members are resolved through the source's `j` table by their absolute
/// global indexes, so a ring may name an output created a million blocks below
/// the window. The per-amount counters are seeded from the source's `b` table
/// the first time an amount is seen, which puts the outputs this window creates
/// above every real global index and so out of the way of the ring members
/// seeded here. Key image heights come from the source's `7` table; a source
/// that has none is reported once and the run continues, catching only double
/// spends inside a window.
fn seed_block_state<Src: KvStore, Dst: KvStore>(
    source: &ChainReader<Src>,
    chain: &mut ChainState<Dst>,
    index: u32,
    block: &BlockTemplate,
    txs: &[Transaction],
    cache: &mut SeedCache,
    log: &mut dyn FnMut(&str),
) -> Result<Seeded, ReplayError> {
    let mut outputs = Vec::new();
    let mut counts = Vec::new();
    let mut spent: Vec<(Hash, u32)> = Vec::new();
    let mut rings = 0u64;
    let mut max_tx_inputs = 0u32;
    let mut key_image_history = true;

    for output in block.base_transaction.prefix.outputs.iter().chain(txs.iter().flat_map(|t| &t.prefix.outputs)) {
        if !cache.seen_amount(output.amount) {
            let count = source
                .outputs_count_for_amount(output.amount)
                .map_err(|e| format!("block {index}: reading the C++ output count for {}: {e}", output.amount))?;
            counts.push((output.amount, count));
        }
    }

    // The `(amount, global index)` pairs this block's rings name and are not
    // already seeded, and the key images it spends, gathered first so that each
    // set is read in **one** batch rather than one lookup per ring member and
    // one per input. Both are genuine batches — the whole block's worth is known
    // before any of it is needed — so this is the shape `multi_get` exists for:
    // one snapshot and one pass over the filter blocks for hundreds of keys.
    //
    // The gathering walks the transactions and their inputs in the same order
    // the reads used to happen in, and the results are put back in that order,
    // so `outputs`, `spent` and the cache's contents are what they were.
    let mut wanted: Vec<(u64, u32)> = Vec::new();
    let mut images: Vec<Hash> = Vec::new();
    for tx in txs {
        let mut tx_inputs = 0u32;
        for input in &tx.prefix.inputs {
            let Input::Key { amount, key_offsets, key_image } = input else { continue };
            rings += 1;
            tx_inputs += 1;
            let absolute = relative_offsets_to_absolute(key_offsets)
                .ok_or_else(|| format!("block {index}: a key input's offsets overflow"))?;
            for gi in absolute {
                let Ok(gi) = u32::try_from(gi) else { continue };
                // Already written through `import_state`, by this block or an
                // earlier one of this window. The record is immutable, so the
                // write would be a copy of itself.
                if cache.seen_output(*amount, gi) {
                    continue;
                }
                wanted.push((*amount, gi));
            }
            images.push(*key_image);
        }
        max_tx_inputs = max_tx_inputs.max(tx_inputs);
    }

    if !wanted.is_empty() {
        let found = source
            .key_outputs(&wanted)
            .map_err(|e| format!("block {index}: reading the C++ ring member outputs: {e}"))?;
        for ((amount, gi), record) in wanted.into_iter().zip(found) {
            // A global index the source does not have is left absent, so the
            // validator reports INPUT_INVALID_GLOBAL_INDEX for it — which is the
            // divergence worth seeing, not one to paper over.
            let Some(record) = record else { continue };
            outputs.push((
                amount,
                gi,
                crate::OutputRecord {
                    public_key: record.public_key,
                    unlock_time: record.unlock_time,
                    transaction_hash: record.transaction_hash,
                    output_index: record.output_index,
                    block_index: record.block_index,
                },
            ));
        }
    }

    if !images.is_empty() {
        let found = source
            .spent_key_image_blocks(&images)
            .map_err(|e| format!("block {index}: reading the C++ spent key images: {e}"))?;
        for (image, at) in images.into_iter().zip(found) {
            match at {
                // Every key image of a block that is on chain is recorded as
                // spent at that block, so a `None` here means the source has no
                // key-image records at all.
                Some(at) => spent.push((image, at)),
                None => {
                    key_image_history = false;
                    if !cache.warned_no_key_images {
                        cache.warned_no_key_images = true;
                        log("warning: the source database has no spent-key-image records; double spends can only be detected within a single window");
                    }
                }
            }
        }
    }

    chain
        .import_state(&outputs, &counts, &spent)
        .map_err(|e| format!("block {index}: seeding the validator state: {e}"))?;
    Ok(Seeded { rings, max_tx_inputs, key_image_history })
}

/// The per-block cross-check both modes run: our record against the C++ `6`
/// record, and the C++ `5` record against the hash we computed.
///
/// It runs for **every** block of both modes; `ahead` only changes where the two
/// records come from, never whether they are read or what they have to say. A
/// disabled [`SourceAhead`] reads them one at a time exactly as this did before
/// the read-ahead existed.
fn check_against_source<Src: KvStore, Dst: KvStore>(
    source: &ChainReader<Src>,
    chain: &ChainState<Dst>,
    index: u32,
    outcome: &crate::AddOutcome,
    ahead: &mut SourceAhead,
    end: u32,
) -> Result<(), ReplayError> {
    let expected = ahead
        .block_info(source, index, end)?
        .ok_or_else(|| format!("block {index}: the C++ database has no block info record"))?;
    let ours = *chain.tip_info().ok_or("our state lost its tip")?;
    // Before anything reads the `5` record: this is what makes our hash and the
    // record's the same hash, and so what makes the read-ahead's cached `5`
    // answer the answer to the question actually being asked.
    compare(index, &ours, &expected)?;
    match ahead.block_index_by_hash(source, index, &outcome.hash)? {
        Some(at) if at == index => Ok(()),
        other => Err(format!("block {index} ({}): the C++ hash index maps it to {other:?}", hex::encode(outcome.hash))),
    }
}
