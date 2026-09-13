// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`ChainState`]: the consensus state a validator needs, plus block
//! acceptance in the `Core::addBlock` order, alternative chains and
//! reorganisation (spec/07).
//!
//! The state lives in any [`KvStore`], so the same code runs on `MemStore` in
//! tests and on RocksDB for the replay and, later, for a syncing node. What it
//! keeps, and why:
//!
//! | Record | Read by |
//! | --- | --- |
//! | block info by index | difficulty windows, timestamp median, reward median, emission, chain selection |
//! | index by block hash | "do we have this block", the parent lookup, reorganisation |
//! | spent key images | `checkIfSpent` |
//! | key outputs by `(amount, global index)` | ring member resolution and the unlock check |
//! | per-amount output counts | the next global index; decoy serving later |
//! | per-block key images and output references | unwinding a block without its body |
//! | transaction hashes per block | the block's transaction list, for RPC and P2P |
//! | block index per transaction hash | `Core::isTransactionInChain`, and the RPC's `/get_transactions_status` and `gettransaction` |
//! | the raw block (optional) | reorganisation, and serving blocks to peers |
//!
//! The last `Config::recent_window` block infos are kept in memory, exactly as
//! the C++ `DatabaseBlockchainCache::unitsCache` does: every block needs the
//! previous 61 cumulative differences, the previous 60 or 11 timestamps and the
//! previous 100 sizes, and reading 200 records per block from RocksDB would
//! dominate a 4.2-million-block replay.

use crate::checkpoints::Checkpoints;
use crate::records::{
    decode_hashes, decode_output_refs, decode_payment_id_refs, decode_raw_block, encode_hashes, encode_output_refs,
    encode_payment_id_refs, encode_raw_block, BlockInfo, OutputRecord,
};
use crate::reward::{get_block_reward, median_value};
use crate::validate::{validate_transaction_deferred, BlockRingBatch, ChainAccess, TxContext, TxRule, ValidatorState};
use crate::{keys, ChainError, Result, Rule};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use wrkz_primitives::block::{BlockTemplate, BLOCK_MAJOR_VERSION_1, BLOCK_MAJOR_VERSION_2};
use wrkz_primitives::constants::*;
use wrkz_primitives::ser::Writer;
use wrkz_primitives::tx::{parse_extra_wallet, Input, PaymentId, Transaction};
use wrkz_primitives::Hash;
use wrkz_storage::{KvStore, WriteOp};

/// How the state is kept. Every field trades disk for capability; none of them
/// changes a consensus rule.
#[derive(Clone, Debug)]
pub struct Config {
    /// Keep the block blob and its transaction blobs for every applied block.
    /// A node needs this to serve peers and to put an unwound block back as an
    /// alternative chain; an offline replay reading a C++ database already has
    /// the bodies and can turn it off.
    pub store_raw_blocks: bool,
    /// **Lite mode.** Store full block data only from this height upward; below
    /// it the state keeps every index, output, key image and transaction-index
    /// record and no block or transaction bytes. `0` — the default — is a full
    /// node.
    ///
    /// This is a refinement of [`Config::store_raw_blocks`], not a second
    /// switch: with `store_raw_blocks: false` nothing is stored at any height.
    ///
    /// The choice is **permanent for a database** (`--lite`,
    /// `DaemonConfiguration.cpp:105`: "Permanent for this database"). Nothing
    /// can re-create a body that was never written, so
    /// [`ChainState::open`] records it under [`keys::META_LITE_HEIGHT`] the
    /// first time and refuses a later open that contradicts it.
    ///
    /// Consensus is untouched. A lite node validates every block it applies
    /// exactly as a full one does — the bodies it does not keep are the ones it
    /// has already finished validating.
    pub lite_start_height: u32,
    /// **Pruned mode.** Keep block bodies for at least this many blocks behind
    /// the tip and delete them below that. `None` — the default — keeps every
    /// body.
    ///
    /// Must be at least [`MIN_PRUNE_DEPTH`]: a reorganisation may reach
    /// `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH` (180) blocks back and needs the body of
    /// every block it unwinds, so a shallower depth would let pruning delete
    /// what a legal reorganisation still requires. [`ChainState::open`] refuses
    /// a smaller value rather than clamping it.
    ///
    /// Like lite mode this only decides which *bodies* exist; every consensus
    /// record — block infos, key images, outputs, per-amount counts, the
    /// transaction index, the per-block unwind records — is written and kept
    /// exactly as a full node writes it.
    pub prune_depth: Option<u32>,
    /// How many blocks of per-block *output reference* records to keep behind
    /// the tip. Only an unwind reads them, and `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH`
    /// (180) bounds how deep a reorganisation can go, so anything above that is
    /// dead weight — about 12 bytes per output over the whole chain.
    ///
    /// Beyond it an unwind — a reorganisation, or [`ChainState::rewind_to`] —
    /// rebuilds a block's pairs from its body instead, and is refused only
    /// where the body is not held either.
    pub unwind_history: u32,
    /// Block infos held in memory, ending at the tip. Must be at least
    /// `CRYPTONOTE_REWARD_BLOCKS_WINDOW` (100) and `DIFFICULTY_BLOCKS_COUNT_V3`
    /// (61) for the windows to be served without touching the store.
    pub recent_window: usize,
    /// How many threads a transaction's ring signatures may be verified on.
    ///
    /// Ring signature verification is the dominant cost of every block outside
    /// the checkpoint zone, and it is a pure function of its arguments, so it
    /// is spread over this many threads
    /// ([`wrkz_pow::parallel::first_invalid_ring`]). `1` forces the sequential
    /// loop. A transaction with fewer than
    /// [`wrkz_pow::parallel::PARALLEL_THRESHOLD`] key inputs takes the
    /// sequential path whatever this says, because below that the hand-off
    /// costs more than the work.
    ///
    /// **This is a performance knob and cannot change a verdict.** The same
    /// blocks are accepted and rejected, and a rejection names the same rule
    /// and the same input index, at every value; see
    /// [`wrkz_pow::parallel::first_invalid_ring`] for the argument.
    ///
    /// The default is the detected parallelism, capped
    /// ([`wrkz_pow::parallel::default_threads`]), which is what an offline
    /// replay with the machine to itself wants. **A daemon that serves RPC
    /// while it syncs usually wants fewer** — every thread here is a core not
    /// answering a request — so set it to a fraction of the cores there.
    pub validate_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            store_raw_blocks: true,
            lite_start_height: 0,
            prune_depth: None,
            unwind_history: 512,
            recent_window: 256,
            validate_threads: wrkz_pow::parallel::default_threads(),
        }
    }
}

/// The shallowest [`Config::prune_depth`] this crate will open a state with.
///
/// A reorganisation may reach `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH` (180) blocks
/// behind the tip (`prune_alternative_chains`, the C++
/// `pruneStaleAlternativeChains`) and [`ChainState`]'s chain switch reads the
/// raw block of every height it unwinds, so the retention window has to cover
/// those 180 blocks and the one the prune pass is about to drop.
///
/// That is the depth of a branch that forks *within* 180 blocks of the tip. A
/// branch that keeps being extended is judged by its tip and can fork deeper
/// (`prune_alternative_chains`); a node whose bodies do not reach its fork
/// point refuses it before unwinding anything, as the C++ does.
///
/// This is the **consensus** floor and it is all this crate enforces. The C++
/// keeps a much larger *network-health* minimum one layer up —
/// `DaemonConfiguration::MIN_PRUNE_DEPTH`, `EXPECTED_NUMBER_OF_BLOCKS_PER_DAY *
/// 7` = 10,080 blocks — and **clamps** a smaller `--prune-depth` up to it with a
/// message rather than refusing to start (`DaemonConfiguration.cpp:31-44`).
/// `wrkz-node` clamps identically; see `MIN_PRUNE_DEPTH` there. Splitting the
/// two is deliberate: the daemon can match the C++'s operator-facing behaviour
/// exactly while the invariant that actually protects a reorganisation is
/// checked where reorganisation lives.
pub const MIN_PRUNE_DEPTH: u32 = CRYPTONOTE_MAX_ALT_BLOCK_DEPTH as u32 + 1;

impl Config {
    /// Whether a block at `index` has its body kept, with the chain at `tip`.
    ///
    /// The inverse of the C++ `isLiteIndexOnlyHeight`
    /// (`DatabaseBlockchainCache.h:476`) with the prune window on top of it:
    ///
    /// - nothing at all is kept without [`Config::store_raw_blocks`];
    /// - below [`Config::lite_start_height`] nothing is kept **except genesis**,
    ///   which the C++ exempts explicitly (`blockIndex != 0`) so that a lite
    ///   node can still answer for the one block it constructs itself;
    /// - a body falls out of a pruned node's window once
    ///   `index + prune_depth <= tip`, which keeps exactly `prune_depth` blocks
    ///   — the C++ `pruneBeforeHeight = topHeight - pruneDepth` over a
    ///   `topHeight` that is a count (`DatabaseBlockchainCache.cpp:3054-3060`).
    ///   Genesis is *not* exempt from pruning; the C++ prunes from 0 up.
    pub fn keeps_body(&self, index: u32, tip: u32) -> bool {
        if !self.store_raw_blocks {
            return false;
        }
        if self.lite_start_height != 0 && index != 0 && index < self.lite_start_height {
            return false;
        }
        if let Some(depth) = self.prune_depth {
            if u64::from(index) + u64::from(depth) <= u64::from(tip) {
                return false;
            }
        }
        true
    }

    /// The lowest index whose body this configuration keeps, with the chain at
    /// `tip`. `None` when no height keeps one at all.
    ///
    /// Genesis is deliberately not folded in: it is an exception to the lite
    /// line, not part of the servable range, and reporting `0` as the floor of a
    /// lite node would say the node can serve everything.
    pub fn body_floor(&self, tip: u32) -> Option<u32> {
        if !self.store_raw_blocks {
            return None;
        }
        let prune_floor = self.prune_depth.map(|d| (u64::from(tip) + 1).saturating_sub(u64::from(d))).unwrap_or(0);
        Some(self.lite_start_height.max(prune_floor as u32))
    }
}

/// Where [`ChainState::add_block`] spends its time, accumulated over every
/// block it has been given since the last [`ChainState::reset_timings`].
///
/// The three phases partition the work `add_block` does, in the order it does
/// it, and the remainder ([`Timings::other`]) is everything between them: the
/// "do we already have this block" lookup, the parent lookup, and — during a
/// reorganisation — the unwinding.
///
/// # Why it is always on
///
/// Two [`Instant::now`] calls per phase is about a hundred nanoseconds on the
/// hosts this runs on, against a block that costs tens of microseconds to
/// validate at the very least. An import that turns out to be spending 95% of
/// its time in one phase is worth a tenth of a percent to find out about, and a
/// number that is only there when someone remembered a flag is a number nobody
/// has when they need it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    /// Blocks passed to [`ChainState::add_block`], accepted or not.
    pub blocks: u64,
    /// Parsing the block blob, hashing it, and parsing the transaction blobs.
    pub decode: Duration,
    /// Every consensus check: the size caps, `validateBlock`, the difficulty,
    /// the transaction list, every transaction, the reward, and the checkpoint
    /// or proof of work.
    pub validate: Duration,
    /// Assembling and writing the block's records: `push_block`, which is the
    /// one write batch per block and the median-size recomputation after it.
    pub commit: Duration,
    /// The whole of `add_block`, so that the three phases can be read as
    /// fractions of something.
    pub total: Duration,
    /// [`Timings::validate`], broken into the steps of `Core::addBlock`.
    pub phases: ValidateTimings,
}

/// Where the `validate` phase of [`ChainState::add_block`] spent its time, step
/// by step of `Core::addBlock`, plus the counters that turn "the cache should
/// be hit" into a number.
///
/// The step numbers are the ones in the comments of `add_to_main`. The named
/// durations partition [`Timings::validate`] for a block that was accepted; a
/// block rejected part way through leaves the steps it never reached at zero,
/// exactly as [`Timings::validate`] itself is not charged at all in that case.
///
/// # Why it is always on
///
/// The same argument as [`Timings`], one level down. Ten more [`Instant::now`]
/// calls is half a microsecond against a phase that is the whole cost of an
/// import, and "validate is 74% of the run" is not a diagnosis either — the
/// question is always *which check*, and an operator who has to rebuild with a
/// flag to find out does not have the number when the import is running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ValidateTimings {
    /// Step 4: re-serializing the coinbase for its size, and the hard
    /// cumulative-size cap.
    pub size: Duration,
    /// Step 5, `Core::validateBlock`, in full. The three fields below split it
    /// and their remainder is [`ValidateTimings::block_other`].
    pub block_checks: Duration,
    /// Step 5, the block version rule and the v2+ parent-block rules (which
    /// re-serialize the parent block).
    pub block_version: Duration,
    /// Step 5, the future-time limit and the timestamp median.
    pub block_timestamp: Duration,
    /// Step 5, the coinbase input, unlock-time and signature rules and the
    /// per-output `check_key` loop — an ed25519 point decompression each.
    pub block_coinbase: Duration,
    /// Step 6: the difficulty window and LWMA.
    pub difficulty: Duration,
    /// Step 7: the transaction list against the block's hashes.
    pub tx_list: Duration,
    /// Step 8: every transaction, including the block's batch of pure checks.
    pub transactions: Duration,
    /// Of [`ValidateTimings::transactions`], the parallel pass that runs the
    /// block's batched key-image domain checks, output key checks and ring
    /// signatures. The rest of step 8 is on one thread, so this is the part
    /// that `--threads` moves.
    pub settle: Duration,
    /// Key inputs across the block's transactions.
    pub tx_inputs: u64,
    /// Outputs across the block's transactions (the coinbase's are counted by
    /// [`ValidateTimings::coinbase_outputs`] instead).
    pub tx_outputs: u64,
    /// Key-image domain checks batched: one ed25519 scalar multiplication
    /// each, and **not** skipped inside the checkpoint zone.
    pub key_image_checks: u64,
    /// Output `check_key`s batched: one point decompression each, also not
    /// skipped inside the zone.
    pub output_key_checks: u64,
    /// Ring signatures batched. Zero inside the checkpoint zone.
    pub ring_checks: u64,
    /// Step 9: the parent info, the reward size median and the reward rule.
    pub reward: Duration,
    /// Step 10: the checkpoint lookup, or the proof of work when the index is
    /// outside the checkpoint zone.
    pub checkpoint_or_pow: Duration,
    /// Coinbase outputs `check_key` ran on, over the blocks timed.
    pub coinbase_outputs: u64,
    /// Blocks whose step 10 took the proof-of-work branch. **This is the number
    /// that says whether the checkpoint zone is on**: during a linear import
    /// below the last checkpoint it must be zero.
    pub pow_blocks: u64,
    /// [`ChainState::block_info`] calls made while the state was open: the
    /// difficulty window, the timestamp median, the reward median, the parent
    /// lookup and the median-size recomputation.
    pub info_lookups: u64,
    /// Of those, the ones the in-memory recent window answered.
    pub info_cache_hits: u64,
    /// Of those, the ones that went to the store. A linear import at the tip
    /// should have none.
    pub info_store_reads: u64,
    /// **Every** read the store was asked for while steps 4 to 10 ran — not
    /// only the block-info windows: a `get` counts one, a `multi_get` counts
    /// its keys.
    ///
    /// This is what settles whether a slow `validate` can be the storage engine
    /// or a write overlay in front of it. For a block with no transactions it
    /// is zero, because nothing between step 4 and step 10 reaches the store at
    /// all; a non-zero figure means a read this crate does not know it does.
    pub store_reads: u64,
}

impl ValidateTimings {
    /// [`ValidateTimings::block_checks`] less its three named parts.
    pub fn block_other(&self) -> Duration {
        self.block_checks
            .saturating_sub(self.block_version)
            .saturating_sub(self.block_timestamp)
            .saturating_sub(self.block_coinbase)
    }

    /// Accumulate one block's steps. Only the fields `add_to_main` measures
    /// through `&mut self`: the three splits of step 5 and the counters come
    /// from [`Probe`] and are folded in by [`ChainState::timings`].
    fn add(&mut self, block: &Self) {
        self.size += block.size;
        self.block_checks += block.block_checks;
        self.difficulty += block.difficulty;
        self.tx_list += block.tx_list;
        self.transactions += block.transactions;
        self.settle += block.settle;
        self.tx_inputs += block.tx_inputs;
        self.tx_outputs += block.tx_outputs;
        self.key_image_checks += block.key_image_checks;
        self.output_key_checks += block.output_key_checks;
        self.ring_checks += block.ring_checks;
        self.reward += block.reward;
        self.checkpoint_or_pow += block.checkpoint_or_pow;
        self.pow_blocks += block.pow_blocks;
        self.store_reads += block.store_reads;
    }

    /// The named steps, summed: what [`Timings::validate`] should be less the
    /// bookkeeping between them.
    pub fn accounted(&self) -> Duration {
        self.size
            + self.block_checks
            + self.difficulty
            + self.tx_list
            + self.transactions
            + self.reward
            + self.checkpoint_or_pow
    }
}

impl Timings {
    /// `total` less the three named phases: the block-known and parent lookups,
    /// and reorganisation.
    pub fn other(&self) -> Duration {
        self.total.saturating_sub(self.decode).saturating_sub(self.validate).saturating_sub(self.commit)
    }

    /// `part` as a percentage of `total`, 0 when nothing has been timed.
    pub fn percent(&self, part: Duration) -> f64 {
        let total = self.total.as_secs_f64();
        if total <= 0.0 {
            0.0
        } else {
            100.0 * part.as_secs_f64() / total
        }
    }

    /// Microseconds per block for one phase, 0 when no block has been timed.
    pub fn micros_per_block(&self, part: Duration) -> f64 {
        if self.blocks == 0 {
            0.0
        } else {
            part.as_secs_f64() * 1e6 / self.blocks as f64
        }
    }

    /// [`Timings::validate`] less the steps [`ValidateTimings`] names: the size
    /// arithmetic and the `BlockInfo` assembly between them.
    pub fn validate_other(&self) -> Duration {
        self.validate.saturating_sub(self.phases.accounted())
    }
}

/// The sub-phase counters that are gathered from `&self` methods.
///
/// [`ChainState::validate_block`] and [`ChainState::block_info`] are shared
/// borrows — the template builder and the RPC call them — so their share of
/// [`ValidateTimings`] cannot be written through `&mut self` the way
/// `add_to_main`'s steps are. Relaxed atomics rather than [`std::cell::Cell`]
/// so that `ChainState` stays `Sync`, which the node needs; a relaxed fetch-add
/// is a locked increment on an uncontended line and does not order anything,
/// which is all that is wanted of a counter.
#[derive(Debug, Default)]
struct Probe {
    version_ns: AtomicU64,
    timestamp_ns: AtomicU64,
    coinbase_ns: AtomicU64,
    coinbase_outputs: AtomicU64,
    info_lookups: AtomicU64,
    info_cache_hits: AtomicU64,
    info_store_reads: AtomicU64,
    /// Every key the store has been asked for, ever, so that a caller can take
    /// the difference across a stretch of code.
    store_reads: AtomicU64,
}

impl Probe {
    fn add(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    fn nanos(d: Duration) -> u64 {
        u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
    }

    fn reset(&self) {
        for c in [
            &self.version_ns,
            &self.timestamp_ns,
            &self.coinbase_ns,
            &self.coinbase_outputs,
            &self.info_lookups,
            &self.info_cache_hits,
            &self.info_store_reads,
            &self.store_reads,
        ] {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// Keys the store has been asked for since this probe was reset.
    fn reads(&self) -> u64 {
        self.store_reads.load(Ordering::Relaxed)
    }

    /// Fold what has been gathered into a caller's [`ValidateTimings`].
    fn merge_into(&self, into: &mut ValidateTimings) {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        into.block_version += Duration::from_nanos(get(&self.version_ns));
        into.block_timestamp += Duration::from_nanos(get(&self.timestamp_ns));
        into.block_coinbase += Duration::from_nanos(get(&self.coinbase_ns));
        into.coinbase_outputs += get(&self.coinbase_outputs);
        into.info_lookups += get(&self.info_lookups);
        into.info_cache_hits += get(&self.info_cache_hits);
        into.info_store_reads += get(&self.info_store_reads);
    }
}

/// Where a block ended up (`Core::addBlock`'s `AddBlockErrorCode` successes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddStatus {
    /// `ADDED_TO_MAIN`
    Main,
    /// `ADDED_TO_ALTERNATIVE`
    Alternative,
    /// `ADDED_TO_ALTERNATIVE_AND_SWITCHED`
    AlternativeAndSwitched,
}

/// What [`ChainState::add_block`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddOutcome {
    pub index: u32,
    pub hash: Hash,
    pub cumulative_difficulty: u64,
    pub already_generated_coins: u64,
    pub difficulty: u64,
    pub status: AddStatus,
}

/// A block on an alternative chain, kept in memory the way the C++ keeps its
/// alternative segments (spec/11: "Alternative chains are not stored; they are
/// in-memory segments and are lost on restart").
#[derive(Clone, Debug)]
struct AltBlock {
    index: u32,
    prev_hash: Hash,
    info: BlockInfo,
    block_blob: Vec<u8>,
    tx_blobs: Vec<Vec<u8>>,
}

/// The chain as seen from some parent: the main chain up to `fork_index`, then
/// `branch` (ascending, starting at `fork_index + 1`). An empty branch is the
/// main chain itself.
///
/// This is what the C++ passes around as the `IBlockchainCache *` a block is
/// being validated against: the main database segment, or an in-memory
/// alternative segment whose parent chain is the main one below its fork index.
pub struct ChainView<'a> {
    fork_index: u32,
    branch: &'a [BlockInfo],
}

impl<'a> ChainView<'a> {
    /// The main chain, whose top block is at `tip`.
    pub fn main_chain(tip: u32) -> Self {
        Self { fork_index: tip, branch: &[] }
    }

    /// An alternative segment: the main chain up to `fork_index`, then
    /// `branch`, oldest first.
    pub fn with_branch(fork_index: u32, branch: &'a [BlockInfo]) -> Self {
        Self { fork_index, branch }
    }

    /// The index of the top block of this view.
    pub fn top_index(&self) -> u32 {
        self.fork_index + self.branch.len() as u32
    }
}

/// A block blob and its transaction blobs, as `/getrawblocks` and P2P carry
/// them.
pub type RawBlockBlobs = (Vec<u8>, Vec<Vec<u8>>);

/// One block that left the main chain during a reorganisation.
///
/// The C++ never needs this: `Core::addBlock` still holds the old leaf when it
/// calls `copyTransactionsToPool(*chainsLeaves[?])` (`Core.cpp:1712`), so it
/// walks the unwound blocks directly. A caller of [`ChainState::add_block`] has
/// no such handle, and without this had to snapshot the chain tail *before*
/// every block that might reorganise just to find out what left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnwoundBlock {
    /// The index the block held on the main chain.
    pub index: u32,
    /// Its block hash. It is an alternative block now, so
    /// [`ChainState::block_index_by_hash`] no longer finds it but
    /// [`ChainState::alternative_cumulative_difficulty`] does.
    pub hash: Hash,
    /// The hashes of its transactions, **coinbase excluded**, in block order:
    /// one per entry of [`UnwoundBlock::transactions`].
    pub transaction_hashes: Vec<Hash>,
    /// The transaction blobs, coinbase excluded — exactly the `tx_blobs` the
    /// block was added with, which is what
    /// `Core::copyTransactionsToPool` offers back to the pool.
    pub transactions: Vec<Vec<u8>>,
}

/// A block's proof-of-work hash, computed ahead of time — on another thread,
/// while the blocks before it are still being applied — so that step 10 of
/// `addBlock` is a comparison instead of a CryptoNight run.
///
/// It can only be built from the block itself ([`PowHint::compute`]), and
/// [`ChainState`] uses it only for the block whose id it carries. So a hint
/// can vouch for nothing but its own block: a stale or foreign one is ignored
/// and the hash is computed again, which is also exactly what happens without
/// one. Which blocks are accepted cannot change; only where the hash ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowHint {
    block_hash: Hash,
    pow_hash: Hash,
}

impl PowHint {
    /// The hint for `block`, or `None` when its id or its proof-of-work input
    /// cannot be built — the chain then reports that itself, as it would have.
    pub fn compute(block: &BlockTemplate) -> Option<Self> {
        Some(Self { block_hash: block.hash().ok()?, pow_hash: block.pow_hash().ok()? })
    }

    /// The id of the block this hint belongs to.
    pub fn block_hash(&self) -> &Hash {
        &self.block_hash
    }

    /// [`PowHint::compute`] for every `Some` entry, on up to `threads` threads,
    /// in the order given. `None` entries get no hint; so does every entry when
    /// fewer than two are wanted or `threads` is below 2, because then there is
    /// nothing to spread and the chain may as well hash inline.
    pub fn compute_many(blocks: &[Option<&BlockTemplate>], threads: usize) -> Vec<Option<PowHint>> {
        let mut hints: Vec<Option<PowHint>> = vec![None; blocks.len()];
        let threads = threads.min(blocks.iter().filter(|b| b.is_some()).count());
        if threads < 2 {
            return hints;
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(entry) = blocks.get(i) else { break };
                            if let Some(block) = entry {
                                done.push((i, PowHint::compute(block)));
                            }
                        }
                        // Each worker hashed with a CryptoNight scratchpad of
                        // its own; give it back now rather than at thread exit.
                        wrkz_pow::release_thread_scratchpad();
                        done
                    })
                })
                .collect();
            for worker in workers {
                for (i, hint) in worker.join().unwrap_or_else(|p| std::panic::resume_unwind(p)) {
                    hints[i] = hint;
                }
            }
        });
        hints
    }
}

/// What [`ChainState::add_block_detailed`] reports: the outcome, plus the
/// blocks the main chain lost if this one caused a switch.
#[derive(Clone, Debug)]
pub struct AddReport {
    pub outcome: AddOutcome,
    /// The blocks that left the main chain, in the order they were unwound:
    /// the old tip first, down to the block just above the fork point. Empty
    /// unless `outcome.status == AddStatus::AlternativeAndSwitched`.
    pub unwound: Vec<UnwoundBlock>,
}

/// One block leaving the main chain during a reorganisation: its index, the
/// info record it had, and the bytes needed to put it back.
type UnwoundRaw = (u32, BlockInfo, Vec<u8>, Vec<Vec<u8>>);

/// One page of [`ChainState::scan_key_images`]: `(image, spending block)`
/// pairs, and the image to continue after.
pub type KeyImagePage = (Vec<(Hash, u32)>, Option<Hash>);

/// One page of [`ChainState::scan_output_amounts`]: `(amount, count)` pairs,
/// and the amount to continue after.
pub type OutputAmountPage = (Vec<(u64, u32)>, Option<u64>);

/// What one block's [`BlockRingBatch`] was asked to verify, summed over the
/// settles it took. A batch that fills up is settled and cleared mid-block, so
/// the counts have to be taken before each settle rather than read off the
/// batch at the end.
#[derive(Clone, Copy, Debug, Default)]
struct SettleCounts {
    key_images: u64,
    output_keys: u64,
    rings: u64,
}

impl SettleCounts {
    fn take(&mut self, batch: &BlockRingBatch<'_>) {
        self.key_images += batch.key_image_checks() as u64;
        self.output_keys += batch.output_key_checks() as u64;
        self.rings += batch.ring_checks() as u64;
    }
}

/// The consensus state, over any key-value store.
pub struct ChainState<S: KvStore> {
    store: S,
    cfg: Config,
    checkpoints: Checkpoints,
    /// The applied top block index; `None` only before genesis.
    tip: Option<u32>,
    /// Block infos ending at `tip`, oldest first, at most `cfg.recent_window`.
    recent: VecDeque<BlockInfo>,
    /// `Core::blockMedianSize`, recomputed after every main-chain block.
    block_median_size: u64,
    alt_blocks: HashMap<Hash, AltBlock>,
    /// Filled by the last chain switch, drained by
    /// [`ChainState::add_block_detailed`]. A switch happens several frames
    /// below the call the caller made, and the alternative it produces is a
    /// `Vec`, so it cannot ride back inside the `Copy` [`AddOutcome`].
    unwound: Vec<UnwoundBlock>,
    /// Overrides the wall clock, for deterministic tests and for replaying
    /// historical blocks with a fixed "now".
    clock: Option<u64>,
    /// Where `add_block` has spent its time. See [`Timings`].
    timings: Timings,
    /// The parts of [`ValidateTimings`] gathered from shared borrows.
    probe: Probe,
    /// Set by [`ChainState::add_block_detailed_with_pow`] for the length of
    /// one call, and read only for the block whose id it carries.
    pow_hint: Option<PowHint>,
    /// Hold the transaction bodies of blocks below 600,000 to their
    /// `tx_hashes` too. See [`ChainState::set_strict_transaction_list`].
    strict_transaction_list: bool,
    /// The height below which this state holds no transaction records; 0 for
    /// every state but one imported from a lite snapshot. Read from the tag at
    /// open and whenever the tag changes. See [`ChainState::transactions_floor`].
    transactions_floor: u32,
}

impl<S: KvStore> ChainState<S> {
    /// Open an existing state. Fails on a state written by another schema
    /// version rather than misreading its records.
    ///
    /// Also settles the **body policy** for this run:
    ///
    /// - a [`Config::prune_depth`] below [`MIN_PRUNE_DEPTH`] is refused, so
    ///   pruning can never delete a body a legal reorganisation needs;
    /// - a [`Config::lite_start_height`] that contradicts what this database
    ///   was created with is refused, because lite is permanent per database
    ///   and no later run can write the bodies an earlier one skipped.
    pub fn open(store: S, cfg: Config, checkpoints: Checkpoints) -> Result<Self> {
        assert!(cfg.recent_window >= CRYPTONOTE_REWARD_BLOCKS_WINDOW, "recent_window must cover the reward window");
        if let Some(depth) = cfg.prune_depth {
            if depth < MIN_PRUNE_DEPTH {
                return Err(ChainError::Corrupt(format!(
                    "prune depth {depth} is below the {MIN_PRUNE_DEPTH} blocks a reorganisation \
                     can need: a reorganisation may reach {CRYPTONOTE_MAX_ALT_BLOCK_DEPTH} blocks \
                     behind the tip and needs the body of every block it unwinds. Raise \
                     --prune-depth to at least {MIN_PRUNE_DEPTH}."
                )));
            }
        }
        if cfg.unwind_history < MIN_PRUNE_DEPTH {
            return Err(ChainError::Corrupt(format!(
                "unwind_history {} is below the {MIN_PRUNE_DEPTH} blocks a reorganisation can \
                 need: the per-block output records it keeps are what an unwind reads.",
                cfg.unwind_history
            )));
        }
        let mut chain = Self {
            store,
            cfg,
            checkpoints,
            tip: None,
            recent: VecDeque::new(),
            block_median_size: 0,
            alt_blocks: HashMap::new(),
            unwound: Vec::new(),
            clock: None,
            timings: Timings::default(),
            probe: Probe::default(),
            pow_hint: None,
            strict_transaction_list: true,
            transactions_floor: 0,
        };
        match chain.store.get(&keys::meta(keys::META_VERSION))? {
            Some(raw) if raw.len() == 4 => {
                let v = u32::from_le_bytes(raw[..4].try_into().expect("4 bytes"));
                // An older readable schema is not rewritten here: `open` must
                // work on a state another process is writing, and the next
                // applied block records the current version anyway.
                if !(keys::OLDEST_READABLE_SCHEMA_VERSION..=keys::STATE_SCHEMA_VERSION).contains(&v) {
                    return Err(ChainError::Corrupt(format!(
                        "state schema version {v}, this build reads {} to {}",
                        keys::OLDEST_READABLE_SCHEMA_VERSION,
                        keys::STATE_SCHEMA_VERSION
                    )));
                }
            }
            Some(raw) => return Err(ChainError::Corrupt(format!("state version record is {} bytes", raw.len()))),
            None => {}
        }
        chain.tip = match chain.store.get(&keys::meta(keys::META_TIP))? {
            Some(raw) if raw.len() == 4 => Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes"))),
            Some(raw) => return Err(ChainError::Corrupt(format!("tip record is {} bytes", raw.len()))),
            None => None,
        };
        chain.settle_lite_height()?;
        chain.transactions_floor = chain.read_transactions_floor()?;
        if let Some(depth) = chain.cfg.prune_depth {
            chain.store.put(keys::meta(keys::META_PRUNE_DEPTH), depth.to_le_bytes().to_vec())?;
        }
        chain.reload_recent()?;
        chain.update_block_median_size()?;
        Ok(chain)
    }

    /// Record this database's lite height, or refuse an open that contradicts
    /// the one already recorded.
    ///
    /// The C++ help text calls `--lite` "Permanent for this database"
    /// (`DaemonConfiguration.cpp:105`) and it is not a policy choice: the
    /// blocks below the line were never written with bodies, and reopening the
    /// same directory as a full node would produce a node that believes it can
    /// serve them. The cases:
    ///
    /// | recorded | asked for | outcome |
    /// | --- | --- | --- |
    /// | none | full | a full database, opened as one |
    /// | none | lite `H`, empty state | adopted: `H` is written |
    /// | none | lite `H`, non-empty state | refused; see [`ChainState::declare_lite_height`] |
    /// | `H` | the same `H` | opened |
    /// | `H` | anything else | refused |
    fn settle_lite_height(&mut self) -> Result<()> {
        let recorded = self.recorded_lite_height()?;
        let asked = self.cfg.lite_start_height;
        match (recorded, asked) {
            (None, 0) => Ok(()),
            (None, h) if self.tip.is_none() => {
                self.store.put(keys::meta(keys::META_LITE_HEIGHT), h.to_le_bytes().to_vec())?;
                Ok(())
            }
            (None, h) => Err(ChainError::Corrupt(format!(
                "this database was built as a full node and holds block bodies from genesis; it \
                 cannot be reopened as a lite node with full block data from height {h}, because \
                 lite mode is permanent for a database. Start a lite node in an empty --data-dir."
            ))),
            (Some(r), a) if r == a => Ok(()),
            (Some(r), 0) => Err(ChainError::Corrupt(format!(
                "this database was created as a lite node with full block data from height {r}: \
                 it has no block bodies below {r} and cannot serve them, so it cannot be opened \
                 as a full node. Pass --lite --lite-height {r}, or point --data-dir at a full \
                 database."
            ))),
            (Some(r), a) => Err(ChainError::Corrupt(format!(
                "this database was created as a lite node with full block data from height {r}, \
                 and lite mode is permanent for a database: it has no bodies below {r} and no run \
                 can write them now. Pass --lite-height {r}, not {a}."
            ))),
        }
    }

    /// The lite height recorded in this database, if it was created as a lite
    /// node (see [`keys::META_LITE_HEIGHT`]).
    pub fn recorded_lite_height(&self) -> Result<Option<u32>> {
        match self.store_get(&keys::meta(keys::META_LITE_HEIGHT))? {
            Some(raw) if raw.len() == 4 => Ok(Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes")))),
            Some(raw) => Err(ChainError::Corrupt(format!("lite height record is {} bytes", raw.len()))),
            None => Ok(None),
        }
    }

    /// The prune depth this database was last opened with, for reporting.
    pub fn recorded_prune_depth(&self) -> Result<Option<u32>> {
        match self.store_get(&keys::meta(keys::META_PRUNE_DEPTH))? {
            Some(raw) if raw.len() == 4 => Ok(Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes")))),
            Some(raw) => Err(ChainError::Corrupt(format!("prune depth record is {} bytes", raw.len()))),
            None => Ok(None),
        }
    }

    /// Stamp a lite height onto a **non-empty** state that has no marker yet.
    ///
    /// The one legitimate case: a state imported without block bodies below
    /// some height — `wrkz-replay` without `--store-raw` — which is materially
    /// a lite database that was never labelled one. The caller must have
    /// established that the bodies really do begin at `height` (the daemon
    /// probes for it, `wrkz_node::daemon::lowest_stored_block`); this only
    /// writes the label down. A state that already carries a *different* label
    /// is refused, so this cannot be used around the permanence rule.
    pub fn declare_lite_height(&mut self, height: u32) -> Result<()> {
        match self.recorded_lite_height()? {
            Some(r) if r == height => Ok(()),
            Some(r) => Err(ChainError::Corrupt(format!(
                "this database is already recorded as a lite node with full block data from \
                 height {r}; it cannot be relabelled to {height}."
            ))),
            None => {
                self.store.put(keys::meta(keys::META_LITE_HEIGHT), height.to_le_bytes().to_vec())?;
                self.cfg.lite_start_height = height;
                Ok(())
            }
        }
    }

    /// The lowest index this state is willing to answer a body question for.
    ///
    /// Configuration only: it says what the policy *is*, not what the store
    /// happens to hold. [`ChainState::raw_block`] refuses below it rather than
    /// looking, so a body that has survived a prune pass cannot make a pruned
    /// node answer inconsistently from one minute to the next.
    pub fn body_floor(&self) -> Option<u32> {
        self.cfg.body_floor(self.tip.unwrap_or(0))
    }

    /// Delete the block bodies that have fallen out of a pruned node's window
    /// but are still on disk, at most `budget` of them, and report how many
    /// went.
    ///
    /// `push_block` drops one body per block applied, which keeps
    /// a node that has always been pruned exactly at its depth. This is the
    /// catch-up for the case that does not: a database that was full — or was
    /// pruned at a larger depth — when `--prune` was given. The C++ does the
    /// whole thing in one blind pass from height 0 on a timer, in 10,000-key
    /// batches (`DatabaseBlockchainCache.cpp:3048-3086`), and records nothing,
    /// so every pass rescans the whole chain: "Pruning records no height it
    /// pruned to" (`Core.cpp:2987`). This keeps a resume point instead
    /// ([`keys::META_PRUNE_FLOOR`]), so the second pass starts where the first
    /// stopped and the steady state costs nothing.
    ///
    /// `Ok(0)` when nothing is pruned, including on a node with no
    /// `prune_depth`.
    pub fn prune_bodies(&mut self, budget: u32) -> Result<u32> {
        let (Some(tip), Some(_)) = (self.tip, self.cfg.prune_depth) else { return Ok(0) };
        let Some(floor) = self.cfg.body_floor(tip) else { return Ok(0) };
        let mut from = self.pruned_below()?;
        if from == 0 {
            // The first pass on a database that was not built pruned. Walking
            // from 0 would read millions of keys that never held a body — an
            // import below the tip, or a lite region — so the start is found
            // the way the daemon finds its body floor: a binary search over
            // "is there a body here", about `log2(tip)` lookups.
            from = self.lowest_stored_body()?.unwrap_or(floor);
        }
        if from >= floor {
            // Nothing to do, but record it so the next pass costs one read.
            if self.pruned_below()? < floor {
                self.store.put(keys::meta(keys::META_PRUNE_FLOOR), floor.to_le_bytes().to_vec())?;
            }
            return Ok(0);
        }
        // Bounded twice: by deletions, so one write batch stays the size the
        // C++ uses, and by keys looked at, so a pass over a region that turns
        // out to hold nothing cannot hold the write lock for millions of reads.
        const MAX_SCAN: u32 = 200_000;
        let end = floor.min(from.saturating_add(MAX_SCAN));
        let mut ops: Vec<WriteOp> = Vec::new();
        let mut removed = 0u32;
        let mut reached = end;
        for index in from..end {
            if removed >= budget {
                reached = index;
                break;
            }
            if self.store_get(&keys::raw_block(index))?.is_some() {
                ops.push((keys::raw_block(index), None));
                removed += 1;
            }
        }
        ops.push((keys::meta(keys::META_PRUNE_FLOOR), Some(reached.to_le_bytes().to_vec())));
        self.store.write_batch(ops)?;
        Ok(removed)
    }

    /// The lowest index whose body is **on disk**, ignoring the policy, or
    /// `None` when none is.
    ///
    /// Bodies are present from some height upward and absent below it — an
    /// import writes none below its start, a lite node none below its line, a
    /// prune pass deletes a prefix — so a binary search settles it in about
    /// `log2(tip)` lookups instead of a scan. A database that violated that
    /// shape would have this return one of its runs rather than the true
    /// minimum, which can only make a prune pass do *less*, never more.
    pub fn lowest_stored_body(&self) -> Result<Option<u32>> {
        let Some(tip) = self.tip else { return Ok(None) };
        let has = |index: u32| -> Result<bool> { Ok(self.store_get(&keys::raw_block(index))?.is_some()) };
        if !has(tip)? {
            return Ok(None);
        }
        if has(0)? {
            return Ok(Some(0));
        }
        let (mut lo, mut hi) = (0u32, tip);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if has(mid)? {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Ok(Some(hi))
    }

    /// The height below which every body is known to have been deleted, so a
    /// catch-up pass never rescans a region it has already cleared.
    pub fn pruned_below(&self) -> Result<u32> {
        match self.store_get(&keys::meta(keys::META_PRUNE_FLOOR))? {
            Some(raw) if raw.len() == 4 => Ok(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes"))),
            Some(raw) => Err(ChainError::Corrupt(format!("prune floor record is {} bytes", raw.len()))),
            None => Ok(0),
        }
    }

    /// [`ChainState::open`], applying the genesis block when the state is empty.
    ///
    /// A node MUST construct genesis itself; it is never downloaded (spec/07
    /// "Genesis").
    pub fn open_or_genesis(store: S, cfg: Config, checkpoints: Checkpoints) -> Result<Self> {
        let mut chain = Self::open(store, cfg, checkpoints)?;
        if chain.tip.is_none() {
            chain.apply_genesis()?;
        }
        Ok(chain)
    }

    /// Fix the value `now()` returns (the future-time-limit rule and the
    /// pre-600,000 unlock branch read it). `None` restores the system clock.
    pub fn set_clock(&mut self, now: Option<u64>) {
        self.clock = now;
    }

    pub fn checkpoints(&self) -> &Checkpoints {
        &self.checkpoints
    }

    pub fn checkpoints_mut(&mut self) -> &mut Checkpoints {
        &mut self.checkpoints
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Change [`Config::validate_threads`] on a live state.
    ///
    /// A performance knob only: it cannot change which blocks are accepted or
    /// which rule a rejection names, so it is safe to move at any time — a node
    /// can drop it while it serves RPC and raise it while it syncs. `0` is
    /// read as `1`, the sequential path.
    pub fn set_validate_threads(&mut self, threads: usize) {
        self.cfg.validate_threads = threads.max(1);
    }

    /// Whether a block below `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT` (600,000) must
    /// carry exactly the transactions its `tx_hashes` name, in order. On by
    /// default.
    ///
    /// The C++ checks only the *count* down there (`Core.cpp:1536`). The block
    /// id commits to the hashes, not to the blobs, and the whole range is
    /// inside the checkpoint zone, where no ring signature ties a spend to
    /// anything — so a peer serving an old block could hand over other
    /// transactions under the same id and every checkpoint would still match.
    /// A block relayed honestly carries its own transactions, so this accepts
    /// the chain the C++ accepts while closing that door; a full linear
    /// `wrkz-replay` is what confirms it for every historical block.
    /// `wrkz-replay --legacy-transaction-list` turns it off, should an import
    /// ever meet an old block that fails it.
    pub fn set_strict_transaction_list(&mut self, strict: bool) {
        self.strict_transaction_list = strict;
    }

    /// The underlying store, for readers that need records this API does not
    /// expose yet (the RPC layer of 3.5).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Where [`ChainState::add_block`] has spent its time.
    pub fn timings(&self) -> Timings {
        let mut t = self.timings;
        self.probe.merge_into(&mut t.phases);
        t
    }

    /// Zero the timings, so that a caller can report per-window rates.
    pub fn reset_timings(&mut self) {
        self.timings = Timings::default();
        self.probe.reset();
    }

    // -- the write path's pacing --------------------------------------------
    //
    // A store that batches writes across blocks ([`wrkz_storage::batch::BatchStore`])
    // needs to be told where a block ends, because that is the only place its
    // overlay may be handed to the engine and still leave a resumable state
    // behind. These three forward that to the store; on an unbatched store they
    // are no-ops, which is why every existing caller can ignore them.

    /// Tell the store that the state is consistent here — one block has been
    /// fully applied — so it may commit an accumulated batch.
    ///
    /// Returns whether it did. Call it after [`ChainState::add_block`] and
    /// after whatever cross-checking the caller does with that block, never in
    /// the middle of one.
    pub fn block_boundary(&mut self) -> Result<bool> {
        Ok(self.store.flush_if_full()?)
    }

    /// Commit everything the store is holding back, whether or not a batch is
    /// full. The state on disk is a resumable height afterwards.
    pub fn flush(&mut self) -> Result<()> {
        Ok(self.store.flush()?)
    }

    /// [`ChainState::flush`], and then make it durable against a crash of the
    /// machine. Expensive; a run does it at the end, on an interrupt and on an
    /// error, not per block.
    pub fn sync(&mut self) -> Result<()> {
        Ok(self.store.sync()?)
    }

    /// [`KvStore::set_write_ahead_log`] on the store. A durability knob only:
    /// which blocks are accepted is untouched.
    pub fn set_write_ahead_log(&mut self, on: bool) -> Result<()> {
        Ok(self.store.set_write_ahead_log(on)?)
    }

    /// Bytes the store is holding back, for a progress line.
    pub fn pending_bytes(&self) -> usize {
        self.store.pending_bytes()
    }

    /// Give the store back, consuming the state. Reopening it with
    /// [`ChainState::open`] recovers the same chain: everything except the
    /// in-memory alternative segments is on disk, which is exactly what the C++
    /// node loses on restart too (spec/11).
    pub fn into_store(self) -> S {
        self.store
    }

    /// The label a tool left on this state, if any (see [`keys::META_TAG`]).
    pub fn tag(&self) -> Result<Option<String>> {
        match self.store_get(&keys::meta(keys::META_TAG))? {
            Some(raw) => {
                String::from_utf8(raw).map(Some).map_err(|_| ChainError::Corrupt("state tag is not UTF-8".into()))
            }
            None => Ok(None),
        }
    }

    /// Label this state. Callers use it to refuse to mix two kinds of run in
    /// one directory.
    pub fn set_tag(&mut self, tag: &str) -> Result<()> {
        self.store.put(keys::meta(keys::META_TAG), tag.as_bytes().to_vec())?;
        self.transactions_floor = self.read_transactions_floor()?;
        Ok(())
    }

    /// The height below which this state holds **no transaction records**, or
    /// 0 when it holds them all the way down.
    ///
    /// Nonzero only for a state imported from a lite snapshot
    /// ([`keys::TAG_LITE_SNAPSHOT`]), where it is the lite height: below it
    /// [`ChainState::transaction_block_index`] and
    /// [`ChainState::transaction_hashes_by_payment_id`] find nothing,
    /// [`ChainState::block_transaction_hashes`] is empty, and an output's
    /// `transaction_hash` is zero — not because the chain says so, but because
    /// the snapshot never carried them. A reader that would otherwise report
    /// "not found" or "none" must report that it cannot know instead. A lite
    /// node that synced its own region keeps every one of those records, so
    /// its floor is 0 even though its body floor is not.
    pub fn transactions_floor(&self) -> u32 {
        self.transactions_floor
    }

    fn read_transactions_floor(&self) -> Result<u32> {
        match self.tag()?.as_deref() {
            Some(keys::TAG_LITE_SNAPSHOT) => self.recorded_lite_height()?.ok_or_else(|| {
                ChainError::Corrupt("this state is tagged as a lite snapshot import but records no lite height".into())
            }),
            _ => Ok(0),
        }
    }

    /// Import block infos and move the tip to `tip`, **without validating
    /// anything**.
    ///
    /// This is how a tool that verifies a *slice* of the chain gets the state
    /// that slice reads: the difficulty window, the timestamp median window and
    /// the reward size median all look at the previous blocks' infos, and a
    /// laptop cannot replay 4.2 million blocks to reach block 3,000,000. The
    /// infos come from a database that has already been validated by the node
    /// that wrote it; nothing here is a consensus decision, and a linear replay
    /// from genesis never calls it.
    ///
    /// `infos` is `(index, info)` pairs, ascending and contiguous, ending at
    /// `tip`; each one also gets its hash → index record so the next block's
    /// parent lookup finds it.
    ///
    /// No transaction index is written: the infos carry no transaction hashes,
    /// so [`ChainState::transaction_block_index`] answers `None` for every
    /// imported height. A windowed replay does not ask, and a node never
    /// imports.
    pub fn import_history(&mut self, infos: &[(u32, BlockInfo)], tip: u32) -> Result<()> {
        let mut ops: Vec<WriteOp> = Vec::with_capacity(infos.len() * 2 + 2);
        for (index, info) in infos {
            ops.push((keys::block_info(*index), Some(info.encode())));
            ops.push((keys::hash_to_index(&info.block_hash), Some(index.to_le_bytes().to_vec())));
        }
        ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
        ops.push((keys::meta(keys::META_TIP), Some(tip.to_le_bytes().to_vec())));
        self.store.write_batch(ops)?;
        self.tip = Some(tip);
        self.alt_blocks.clear();
        self.reload_recent()?;
        self.update_block_median_size()
    }

    /// Import the state a transaction validator reads but this state did not
    /// derive: key outputs by `(amount, global index)`, the per-amount output
    /// counters, and the block index each key image was spent at.
    ///
    /// The companion of [`ChainState::import_history`], and used the same way:
    /// a windowed replay resolves each block's ring members and spent key
    /// images out of the source database just before it validates that block.
    pub fn import_state(
        &mut self,
        outputs: &[(u64, u32, OutputRecord)],
        counts: &[(u64, u32)],
        spent_key_images: &[(Hash, u32)],
    ) -> Result<()> {
        let mut ops: Vec<WriteOp> = Vec::with_capacity(outputs.len() + counts.len() + spent_key_images.len());
        for (amount, global_index, record) in outputs {
            ops.push((keys::output(*amount, *global_index), Some(record.encode())));
        }
        for (amount, count) in counts {
            ops.push((keys::output_count(*amount), Some(count.to_le_bytes().to_vec())));
        }
        for (image, index) in spent_key_images {
            ops.push((keys::key_image(image), Some(index.to_le_bytes().to_vec())));
        }
        Ok(self.store.write_batch(ops)?)
    }

    /// Write block infos and their hash → index records, and nothing else: no
    /// tip move, no transaction records. A lite snapshot import writes the
    /// region below its height through this, a batch at a time, in whatever
    /// order the snapshot holds them, and states the tip once at the end
    /// ([`ChainState::finish_snapshot_import`]).
    pub fn import_block_infos(&mut self, infos: &[(u32, BlockInfo)]) -> Result<()> {
        let mut ops: Vec<WriteOp> = Vec::with_capacity(infos.len() * 2);
        for (index, info) in infos {
            ops.push((keys::block_info(*index), Some(info.encode())));
            ops.push((keys::hash_to_index(&info.block_hash), Some(index.to_le_bytes().to_vec())));
        }
        Ok(self.store.write_batch(ops)?)
    }

    /// The last write of a lite snapshot import: the tip at `lite_height - 1`,
    /// the schema version and the [`keys::TAG_LITE_SNAPSHOT`] tag, in one
    /// batch, and the in-memory windows reloaded from the imported infos.
    ///
    /// The caller must already have written every record below `lite_height`
    /// and must have opened this state as a lite node at that height.
    pub fn finish_snapshot_import(&mut self, lite_height: u32) -> Result<()> {
        if lite_height == 0 || self.recorded_lite_height()? != Some(lite_height) {
            return Err(ChainError::Corrupt(format!(
                "a lite snapshot at height {lite_height} can only finish into a state opened as a lite node at \
                 that height"
            )));
        }
        let tip = lite_height - 1;
        self.store.write_batch(vec![
            (keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())),
            (keys::meta(keys::META_TIP), Some(tip.to_le_bytes().to_vec())),
            (keys::meta(keys::META_TAG), Some(keys::TAG_LITE_SNAPSHOT.as_bytes().to_vec())),
        ])?;
        self.tip = Some(tip);
        self.alt_blocks.clear();
        self.transactions_floor = lite_height;
        self.reload_recent()?;
        self.update_block_median_size()
    }

    /// The block infos at `indexes`, in one batched read, answering in the
    /// order asked. The export walk reads the `[0, H)` region through this.
    pub fn block_infos(&self, indexes: &[u32]) -> Result<Vec<Option<BlockInfo>>> {
        let wanted: Vec<Vec<u8>> = indexes.iter().map(|i| keys::block_info(*i)).collect();
        self.store_multi_get(&wanted)?.into_iter().map(|raw| raw.map(|r| BlockInfo::decode(&r)).transpose()).collect()
    }

    /// How many outputs of `amount` blocks below `height` created: the
    /// per-amount count as the chain stood with a top block of `height - 1`.
    ///
    /// Global indexes are handed out in chain order, so those outputs are
    /// exactly indexes `0 .. n` and this is a binary search on the output
    /// records' block index, about `log2(count)` reads. It is what a lite
    /// snapshot's key output table is filtered to, and what its importer
    /// rebuilds the counters from.
    pub fn output_count_for_amount_below(&self, amount: u64, height: u32) -> Result<u32> {
        self.first_output_from_block(amount, height)
    }

    /// One page of the spent key images, ascending by image and strictly after
    /// `after`: `(image, spending block)` pairs, and the image to continue from
    /// (`None` at the end). The lite snapshot export walks the C++ `7` table
    /// through this, which sorts the same way.
    pub fn scan_key_images(&self, after: Option<&Hash>, limit: usize) -> Result<KeyImagePage> {
        let after_key = after.map(keys::key_image);
        let page = self.store.scan(&[keys::NS, keys::TAG_KEY_IMAGE], after_key.as_deref(), limit)?;
        let image_of = |key: &[u8]| -> Result<Hash> {
            key.get(2..)
                .and_then(|tail| Hash::try_from(tail).ok())
                .ok_or_else(|| ChainError::Corrupt(format!("a key image record's key is {} bytes", key.len())))
        };
        let mut out = Vec::with_capacity(page.entries.len());
        for (key, value) in &page.entries {
            let at = <[u8; 4]>::try_from(value.as_slice())
                .map_err(|_| ChainError::Corrupt(format!("key image record is {} bytes", value.len())))?;
            out.push((image_of(key)?, u32::from_le_bytes(at)));
        }
        let next = page.resume_after.as_deref().map(image_of).transpose()?;
        Ok((out, next))
    }

    /// One page of the per-amount output counters, ascending by amount and
    /// strictly after `after`: `(amount, count)` pairs, and the amount to
    /// continue from (`None` at the end).
    pub fn scan_output_amounts(&self, after: Option<u64>, limit: usize) -> Result<OutputAmountPage> {
        let after_key = after.map(keys::output_count);
        let page = self.store.scan(&[keys::NS, keys::TAG_OUTPUT_COUNT], after_key.as_deref(), limit)?;
        let amount_of = |key: &[u8]| -> Result<u64> {
            key.get(2..)
                .and_then(|tail| <[u8; 8]>::try_from(tail).ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| ChainError::Corrupt(format!("an output count record's key is {} bytes", key.len())))
        };
        let mut out = Vec::with_capacity(page.entries.len());
        for (key, value) in &page.entries {
            let count = <[u8; 4]>::try_from(value.as_slice())
                .map_err(|_| ChainError::Corrupt(format!("output count record is {} bytes", value.len())))?;
            out.push((amount_of(key)?, u32::from_le_bytes(count)));
        }
        let next = page.resume_after.as_deref().map(amount_of).transpose()?;
        Ok((out, next))
    }

    /// The applied top block index.
    pub fn tip_index(&self) -> Option<u32> {
        self.tip
    }

    /// The top block's info.
    pub fn tip_info(&self) -> Option<&BlockInfo> {
        self.recent.back()
    }

    /// `Core::blockMedianSize`.
    pub fn block_median_size(&self) -> u64 {
        self.block_median_size
    }

    /// How many alternative blocks are held.
    pub fn alternative_block_count(&self) -> usize {
        self.alt_blocks.len()
    }

    /// The cumulative difficulty of an alternative block, if it is held.
    pub fn alternative_cumulative_difficulty(&self, hash: &Hash) -> Option<u64> {
        self.alt_blocks.get(hash).map(|b| b.info.cumulative_difficulty)
    }

    /// A block held on an alternative chain: its index, its info record and
    /// its bytes. What the C++ reaches through `findSegmentContainingBlock`
    /// when a lookup by hash is not confined to the main chain
    /// (`getblockheaderbyhash`).
    pub fn alternative_block(&self, hash: &Hash) -> Option<(u32, BlockInfo, RawBlockBlobs)> {
        self.alt_blocks.get(hash).map(|b| (b.index, b.info, (b.block_blob.clone(), b.tx_blobs.clone())))
    }

    // -- reads ---------------------------------------------------------------

    /// [`KvStore::get`] on the state's store, counted.
    ///
    /// Every read this type does goes through here or through
    /// [`ChainState::store_multi_get`], which is what lets `add_to_main` report
    /// [`ValidateTimings::store_reads`] as a fact rather than a claim.
    fn store_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Probe::add(&self.probe.store_reads, 1);
        Ok(self.store.get(key)?)
    }

    /// [`KvStore::multi_get`], counted by keys asked for.
    fn store_multi_get(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>> {
        Probe::add(&self.probe.store_reads, keys.len() as u64);
        Ok(self.store.multi_get(keys)?)
    }

    /// The block info at `index`, from the in-memory window when it is there.
    ///
    /// Every call is counted into [`ValidateTimings::info_lookups`] and split
    /// between [`ValidateTimings::info_cache_hits`] and
    /// [`ValidateTimings::info_store_reads`], so that "the windows are served
    /// from memory" is a number an operator can read off a running import
    /// rather than a claim about the code.
    pub fn block_info(&self, index: u32) -> Result<Option<BlockInfo>> {
        Probe::add(&self.probe.info_lookups, 1);
        if let Some(tip) = self.tip {
            if index <= tip {
                let back = (tip - index) as usize;
                if back < self.recent.len() {
                    Probe::add(&self.probe.info_cache_hits, 1);
                    return Ok(Some(self.recent[self.recent.len() - 1 - back]));
                }
            }
        }
        Probe::add(&self.probe.info_store_reads, 1);
        match self.store_get(&keys::block_info(index))? {
            Some(raw) => Ok(Some(BlockInfo::decode(&raw)?)),
            None => Ok(None),
        }
    }

    /// The main-chain index of a block hash (the C++ `5` record).
    pub fn block_index_by_hash(&self, hash: &Hash) -> Result<Option<u32>> {
        match self.store_get(&keys::hash_to_index(hash))? {
            Some(raw) if raw.len() == 4 => Ok(Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes")))),
            Some(raw) => Err(ChainError::Corrupt(format!("hash index record is {} bytes", raw.len()))),
            None => Ok(None),
        }
    }

    /// The main-chain block index a transaction was mined in
    /// (`Core::isTransactionInChain`, `Core.cpp:1895`), or `None` when no block
    /// on the main chain holds it.
    ///
    /// One point lookup of a 34-byte key into a 4-byte value
    /// ([`keys::TAG_TRANSACTION_INDEX`]), which is what the pool's "already in
    /// the blockchain" answer and the RPC `/get_transactions_status` and
    /// `gettransaction` need. Coinbase transactions are indexed too, exactly as
    /// the C++ `pushTransaction` writes a record for the block's coinbase.
    ///
    /// Alternative segments are not indexed: a block that leaves the main chain
    /// takes its transactions out of the index with it, so a caller that wants
    /// "somewhere in this node" must also ask the pool.
    pub fn transaction_block_index(&self, hash: &Hash) -> Result<Option<u32>> {
        match self.store_get(&keys::transaction_index(hash))? {
            Some(raw) if raw.len() == 4 => Ok(Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes")))),
            Some(raw) => Err(ChainError::Corrupt(format!("transaction index record is {} bytes", raw.len()))),
            None => Ok(None),
        }
    }

    /// Whether the main chain holds this transaction. The `bool` half of
    /// [`ChainState::transaction_block_index`].
    pub fn has_transaction(&self, hash: &Hash) -> Result<bool> {
        Ok(self.transaction_block_index(hash)?.is_some())
    }

    /// [`ChainState::transaction_block_index`] for a whole list, in one
    /// batched read, answers in the order asked.
    ///
    /// This is the shape `/get_transactions_status` needs (`Core.cpp:800`): it
    /// is handed a set of hashes and has to sort them into "in a block", "in
    /// the pool" and "unknown". One `multi_get` shares a snapshot and one set
    /// of filter-block lookups across the batch, the same reason
    /// [`crate::ChainAccess::key_outputs`] exists.
    pub fn transaction_block_indexes(&self, hashes: &[Hash]) -> Result<Vec<Option<u32>>> {
        let keys: Vec<Vec<u8>> = hashes.iter().map(keys::transaction_index).collect();
        let mut out = Vec::with_capacity(hashes.len());
        for raw in self.store_multi_get(&keys)? {
            out.push(match raw {
                Some(raw) if raw.len() == 4 => Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes"))),
                Some(raw) => {
                    return Err(ChainError::Corrupt(format!("transaction index record is {} bytes", raw.len())))
                }
                None => None,
            });
        }
        Ok(out)
    }

    /// The transaction hashes of a block, coinbase first.
    pub fn block_transaction_hashes(&self, index: u32) -> Result<Vec<Hash>> {
        match self.store_get(&keys::block_tx_hashes(index))? {
            Some(raw) => decode_hashes(&raw),
            None => Ok(Vec::new()),
        }
    }

    /// The stored raw block, when the body policy keeps one for this height.
    ///
    /// `None` for a height the policy does not keep — a lite node's index-only
    /// region, or a pruned node's dropped tail — **without looking at the
    /// store**. A body can outlive its floor for a moment (a prune pass that
    /// has not caught up, a state that was full before `--prune` was added),
    /// and answering from the policy rather than from what happens to still be
    /// on disk is what stops two calls a minute apart giving different answers
    /// for the same height.
    pub fn raw_block(&self, index: u32) -> Result<Option<RawBlockBlobs>> {
        if !self.cfg.keeps_body(index, self.tip.unwrap_or(index)) {
            return Ok(None);
        }
        match self.store_get(&keys::raw_block(index))? {
            Some(raw) => Ok(Some(decode_raw_block(&raw)?)),
            None => Ok(None),
        }
    }

    /// The `(amount, global index)` pairs block `index` created, in creation
    /// order — the coinbase's outputs first, then each transaction's, each in
    /// its own order — which is what `/get_global_indexes_for_range`,
    /// `/get_o_indexes` and an unwind read.
    ///
    /// Normally that is the per-block record `push_block` wrote. A state built
    /// with a short [`Config::unwind_history`] — every `wrkz-replay` import
    /// before it kept them all — has dropped the record for blocks far below
    /// its tip, and then the pairs are rebuilt from the block's body and the
    /// output records, which are never dropped. Global indexes are handed out
    /// in chain order, so a block's outputs of one amount are the consecutive
    /// indexes from the first whose output record names this block or a later
    /// one, found by binary search; each rebuilt pair is then checked against
    /// its output record. `None` when neither the record nor the body is here.
    pub fn block_output_refs(&self, index: u32) -> Result<Option<Vec<(u64, u32)>>> {
        if let Some(raw) = self.store_get(&keys::block_outputs(index))? {
            return Ok(Some(decode_output_refs(&raw)?));
        }
        if self.tip.is_none_or(|tip| index > tip) {
            return Ok(None);
        }
        let Some(raw) = self.store_get(&keys::raw_block(index))? else { return Ok(None) };
        let (block_blob, tx_blobs) = decode_raw_block(&raw)?;
        let corrupt = |what: &str| ChainError::Corrupt(format!("stored block {index}: {what}"));
        let block = BlockTemplate::from_bytes(&block_blob).map_err(|_| corrupt("the block does not parse"))?;
        let mut outputs: Vec<(u64, Hash, u16)> = Vec::new();
        let coinbase_hash = block.base_transaction.hash().map_err(|_| corrupt("the coinbase does not hash"))?;
        for (i, o) in block.base_transaction.prefix.outputs.iter().enumerate() {
            outputs.push((o.amount, coinbase_hash, i as u16));
        }
        for (blob, tx_hash) in tx_blobs.iter().zip(&block.transaction_hashes) {
            let tx = Transaction::from_bytes(blob).map_err(|_| corrupt("a transaction does not parse"))?;
            for (i, o) in tx.prefix.outputs.iter().enumerate() {
                outputs.push((o.amount, *tx_hash, i as u16));
            }
        }
        let mut next: HashMap<u64, u32> = HashMap::new();
        let mut refs = Vec::with_capacity(outputs.len());
        for (amount, tx_hash, output_index) in outputs {
            let global_index = match next.get(&amount) {
                Some(g) => *g,
                None => self.first_output_from_block(amount, index)?,
            };
            let record = <Self as ChainAccess>::key_output(self, amount, u64::from(global_index))?;
            let belongs = record.is_some_and(|r| {
                r.block_index == index && r.transaction_hash == tx_hash && r.output_index == output_index
            });
            if !belongs {
                return Err(corrupt(&format!(
                    "output {output_index} of transaction {} (amount {amount}) is not at global index \
                     {global_index}, where the chain's order puts it",
                    hex::encode(tx_hash)
                )));
            }
            refs.push((amount, global_index));
            next.insert(amount, global_index + 1);
        }
        Ok(Some(refs))
    }

    /// The lowest global index of `amount` whose output record belongs to block
    /// `index` or a later one: a binary search over records whose block index
    /// never decreases.
    fn first_output_from_block(&self, amount: u64, index: u32) -> Result<u32> {
        let (mut low, mut high) = (0u32, self.output_count_for_amount(amount)?);
        while low < high {
            let mid = low + (high - low) / 2;
            match <Self as ChainAccess>::key_output(self, amount, u64::from(mid))? {
                Some(record) if record.block_index >= index => high = mid,
                Some(_) => low = mid + 1,
                None => {
                    return Err(ChainError::Corrupt(format!(
                        "no output record for amount {amount} at global index {mid}, below its count"
                    )))
                }
            }
        }
        Ok(low)
    }

    /// `--lite-height`: the height at and above which this state stores full
    /// block data. `0` for a full node.
    pub fn lite_start_height(&self) -> u32 {
        self.cfg.lite_start_height
    }

    /// `--prune-depth`, or `None` when this node keeps every body.
    pub fn prune_depth(&self) -> Option<u32> {
        self.cfg.prune_depth
    }

    /// The main-chain transactions that carried this **plaintext long** payment
    /// id, in the order they were mined (`Core::getTransactionHashesByPaymentId`).
    ///
    /// Encrypted short ids are not indexed and never will be; see
    /// [`keys::TAG_PAYMENT_ID`].
    pub fn transaction_hashes_by_payment_id(&self, payment_id: &Hash) -> Result<Vec<Hash>> {
        let mut hashes = self.legacy_payment_id_list(payment_id)?;
        let count = self.payment_id_entry_count(payment_id)?;
        if count > 0 {
            let wanted: Vec<Vec<u8>> = (0..count).map(|n| keys::payment_id_entry(payment_id, n)).collect();
            hashes.reserve(wanted.len());
            for (n, raw) in self.store_multi_get(&wanted)?.into_iter().enumerate() {
                hashes.push(decode_payment_id_entry(raw, n as u32, count)?);
            }
        }
        Ok(hashes)
    }

    /// The schema-3 list of [`keys::TAG_PAYMENT_ID`]: the frozen prefix.
    fn legacy_payment_id_list(&self, payment_id: &Hash) -> Result<Vec<Hash>> {
        match self.store_get(&keys::payment_id(payment_id))? {
            Some(raw) => decode_hashes(&raw),
            None => Ok(Vec::new()),
        }
    }

    /// How many [`keys::TAG_PAYMENT_ID_ENTRY`] entries follow the legacy list.
    fn payment_id_entry_count(&self, payment_id: &Hash) -> Result<u32> {
        match self.store_get(&keys::payment_id_count(payment_id))? {
            Some(raw) if raw.len() == 4 => Ok(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes"))),
            Some(raw) => Err(ChainError::Corrupt(format!("payment-id count record is {} bytes", raw.len()))),
            None => Ok(0),
        }
    }

    /// The block index that spent a key image, if any.
    pub fn key_image_spent_at(&self, image: &Hash) -> Result<Option<u32>> {
        match self.store_get(&keys::key_image(image))? {
            Some(raw) if raw.len() == 4 => Ok(Some(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes")))),
            Some(raw) => Err(ChainError::Corrupt(format!("key image record is {} bytes", raw.len()))),
            None => Ok(None),
        }
    }

    /// Number of outputs of `amount` so far: the next global index.
    pub fn output_count_for_amount(&self, amount: u64) -> Result<u32> {
        match self.store_get(&keys::output_count(amount))? {
            Some(raw) if raw.len() == 4 => Ok(u32::from_le_bytes(raw[..4].try_into().expect("4 bytes"))),
            Some(raw) => Err(ChainError::Corrupt(format!("output count record is {} bytes", raw.len()))),
            None => Ok(0),
        }
    }

    /// Whether any segment holds this block (`Core::hasBlockUnsafe`).
    pub fn has_block(&self, hash: &Hash) -> Result<bool> {
        Ok(self.alt_blocks.contains_key(hash) || self.block_index_by_hash(hash)?.is_some())
    }

    // -- windows -------------------------------------------------------------

    fn info_in_view(&self, view: &ChainView<'_>, index: u32) -> Result<Option<BlockInfo>> {
        if index > view.fork_index {
            let at = (index - view.fork_index - 1) as usize;
            return Ok(view.branch.get(at).copied());
        }
        self.block_info(index)
    }

    /// `IBlockchainCache::getLastUnits(count, index, useGenesis)`
    /// (`DatabaseBlockchainCache.cpp:2082`): the infos of indexes
    /// `max(index + 1 − count, useGenesis ? 0 : 1) ..= index`, oldest first.
    fn last_infos(&self, view: &ChainView<'_>, count: usize, index: u32, use_genesis: bool) -> Result<Vec<BlockInfo>> {
        let count = count as u64;
        let index64 = index as u64;
        let mut from = (index64 + 1).saturating_sub(count);
        if !use_genesis && from == 0 {
            from = 1;
        }
        if from > index64 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((index64 - from + 1) as usize);
        for i in from..=index64 {
            let info = self
                .info_in_view(view, i as u32)?
                .ok_or_else(|| ChainError::Corrupt(format!("block info for index {i} is missing")))?;
            out.push(info);
        }
        Ok(out)
    }

    /// `IBlockchainCache::getDifficultyForNextBlock(parentIndex)`
    /// (`DatabaseBlockchainCache.cpp:1933`). `None` is the C++
    /// `DIFFICULTY_OVERHEAD` case: either the algorithm returned 0 or the
    /// window had no defined result (spec/07 "Difficulty").
    ///
    /// Public because the block template builder needs exactly this number and
    /// must not re-derive the rule: the window length, the version the length
    /// is chosen by, the exclusion of genesis and the zero filter are all one
    /// implementation, [`difficulty_window_indexes`] and
    /// [`difficulty_for_next_block_from`].
    pub fn difficulty_for_next_block(&self, view: &ChainView<'_>, parent_index: u32) -> Result<Option<u64>> {
        let mut window = Vec::new();
        for i in difficulty_window_indexes(parent_index) {
            window.push(
                self.info_in_view(view, i)?
                    .ok_or_else(|| ChainError::Corrupt(format!("block info for index {i} is missing")))?,
            );
        }
        Ok(difficulty_for_next_block_from(parent_index, &window))
    }

    /// [`ChainState::difficulty_for_next_block`] against the main chain, for the
    /// block that would follow `parent_index`.
    pub fn main_chain_difficulty_for_next_block(&self, parent_index: u32) -> Result<Option<u64>> {
        self.difficulty_for_next_block(&ChainView::main_chain(parent_index), parent_index)
    }

    /// The median of the last `timestampCheckWindow` timestamps ending at
    /// `parent_index`, or `None` when fewer than a full window exists — the
    /// C++ only applies the rule when `timestamps.size() >= window`.
    fn timestamp_median(&self, view: &ChainView<'_>, parent_index: u32) -> Result<Option<u64>> {
        let window = timestamp_check_window(parent_index as u64 + 1);
        // `addGenesisBlock` = UseGenesis(true) (`Core.cpp:2736`).
        let infos = self.last_infos(view, window, parent_index, true)?;
        if infos.len() < window {
            return Ok(None);
        }
        let mut ts: Vec<u64> = infos.iter().map(|i| i.timestamp).collect();
        Ok(Some(median_value(&mut ts)))
    }

    /// The median of the last `CRYPTONOTE_REWARD_BLOCKS_WINDOW` block sizes
    /// ending at `parent_index`, genesis included (`Core.cpp:1611`).
    fn reward_size_median(&self, view: &ChainView<'_>, parent_index: u32) -> Result<u64> {
        let infos = self.last_infos(view, CRYPTONOTE_REWARD_BLOCKS_WINDOW, parent_index, true)?;
        let mut sizes: Vec<u64> = infos.iter().map(|i| i.block_size as u64).collect();
        Ok(median_value(&mut sizes))
    }

    /// `Core::updateBlockMedianSize` (`Core.cpp:5097`), run after every
    /// main-chain block: `max(median of the last 100 sizes ending at the tip,
    /// granted full reward zone for tip + 1)`.
    fn update_block_median_size(&mut self) -> Result<()> {
        let Some(tip) = self.tip else {
            self.block_median_size = full_reward_zone(block_major_version_for_index(0)) as u64;
            return Ok(());
        };
        let view = ChainView::main_chain(tip);
        let median = self.reward_size_median(&view, tip)?;
        let zone = full_reward_zone(block_major_version_for_index(tip as u64 + 1)) as u64;
        self.block_median_size = median.max(zone);
        Ok(())
    }

    fn reload_recent(&mut self) -> Result<()> {
        self.recent.clear();
        let Some(tip) = self.tip else { return Ok(()) };
        let window = self.cfg.recent_window as u64;
        let from = (tip as u64 + 1).saturating_sub(window) as u32;
        for i in from..=tip {
            let raw = self
                .store_get(&keys::block_info(i))?
                .ok_or_else(|| ChainError::Corrupt(format!("block info for index {i} is missing")))?;
            self.recent.push_back(BlockInfo::decode(&raw)?);
        }
        Ok(())
    }

    // -- block validation ----------------------------------------------------

    /// `Core::validateBlock` (`Core.cpp:2699`). Returns the coinbase output
    /// total, which `addBlock` compares against the computed reward.
    ///
    /// `previous_index` is the index of the parent in whatever segment holds
    /// it. Note that the C++ takes the *claimed* index — the one the coinbase's
    /// `BaseInput` carries (`CachedBlock::getBlockIndex`, `CachedBlock.cpp:185`,
    /// which returns 0 for a malformed coinbase) — for the version rule, the
    /// signature rule, the reward height and the checkpoint lookup, while
    /// `previousBlockIndex + 1` drives the size cap, the time limits and the
    /// coinbase's own index and unlock time. They can only differ on a block
    /// that is about to be rejected, but they differ in *which* error it gets,
    /// so both are reproduced here.
    pub fn validate_block(&self, block: &BlockTemplate, view: &ChainView<'_>, previous_index: u32) -> Result<u64> {
        let claimed_index = block.coinbase_height().unwrap_or(0);
        let index = previous_index as u64 + 1;

        let step = Instant::now();
        let expected_version = block_major_version_for_index(claimed_index);
        if expected_version != block.major_version {
            return Err(Rule::WrongVersion { expected: expected_version, got: block.major_version }.into());
        }

        if block.major_version >= BLOCK_MAJOR_VERSION_2 {
            let parent =
                block.parent_block.as_ref().ok_or(Rule::DeserializationFailed("v2+ block without a parent block"))?;
            // `Core.cpp:2714`: only for a v2 block, and 0 passes — the daemon's
            // own templates carry 0 because of the typo at `Core.cpp:2365`.
            // v3+ parent versions are unchecked (block 600,001 carries 12).
            if block.major_version == BLOCK_MAJOR_VERSION_2 && parent.major_version > BLOCK_MAJOR_VERSION_1 {
                return Err(Rule::ParentBlockWrongVersion.into());
            }
            let mut w = Writer::new();
            parent
                .write(&mut w, block.timestamp, block.nonce, false, false)
                .map_err(|_| Rule::DeserializationFailed("parent block does not re-serialize"))?;
            if w.len() > 2048 {
                return Err(Rule::ParentBlockSizeTooBig.into());
            }
        }
        Probe::add(&self.probe.version_ns, Probe::nanos(step.elapsed()));

        let step = Instant::now();
        let limit = self.now().saturating_add(block_future_time_limit(index));
        if block.timestamp > limit {
            return Err(Rule::TimestampTooFarInFuture { timestamp: block.timestamp, limit }.into());
        }

        if let Some(median) = self.timestamp_median(view, previous_index)? {
            if block.timestamp < median {
                return Err(Rule::TimestampTooFarInPast { timestamp: block.timestamp, median }.into());
            }
        }
        Probe::add(&self.probe.timestamp_ns, Probe::nanos(step.elapsed()));

        let step = Instant::now();
        let coinbase = &block.base_transaction;
        if coinbase.prefix.inputs.len() != 1 {
            return Err(Rule::CoinbaseInputWrongCount(coinbase.prefix.inputs.len()).into());
        }
        let Input::Base { block_index } = coinbase.prefix.inputs[0] else {
            return Err(Rule::CoinbaseInputUnexpectedType.into());
        };
        if block_index != index {
            return Err(Rule::BaseInputWrongBlockIndex { expected: index, got: block_index }.into());
        }
        let expected_unlock = index + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW;
        if coinbase.prefix.unlock_time != expected_unlock {
            return Err(
                Rule::CoinbaseWrongUnlockTime { expected: expected_unlock, got: coinbase.prefix.unlock_time }.into()
            );
        }
        if claimed_index >= TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT && !coinbase.signatures.is_empty() {
            return Err(Rule::CoinbaseHasSignatures.into());
        }

        let mut miner_reward: u64 = 0;
        for output in &coinbase.prefix.outputs {
            if output.amount == 0 {
                return Err(Rule::CoinbaseOutputZeroAmount.into());
            }
            if !wrkz_pow::curve::check_key(&output.key) {
                return Err(Rule::CoinbaseOutputInvalidKey.into());
            }
            miner_reward = miner_reward.checked_add(output.amount).ok_or(Rule::CoinbaseOutputsAmountOverflow)?;
        }
        Probe::add(&self.probe.coinbase_outputs, coinbase.prefix.outputs.len() as u64);
        Probe::add(&self.probe.coinbase_ns, Probe::nanos(step.elapsed()));
        Ok(miner_reward)
    }

    // -- block acceptance ----------------------------------------------------

    /// `Core::addBlock` (`Core.cpp:1465`), in its order.
    ///
    /// `block_blob` and `tx_blobs` are the bytes as they arrived: the sizes the
    /// block size rules measure come from them, so they must not be a
    /// re-serialization.
    pub fn add_block(&mut self, block_blob: &[u8], tx_blobs: &[Vec<u8>]) -> Result<AddOutcome> {
        Ok(self.add_block_detailed(block_blob, tx_blobs)?.outcome)
    }

    /// [`ChainState::add_block`], also reporting the blocks a chain switch took
    /// off the main chain ([`AddReport::unwound`]).
    ///
    /// This is what a node with a transaction pool wants:
    /// `Core::copyTransactionsToPool` (`Core.cpp:567`) needs the transactions
    /// of exactly those blocks, and nothing else can tell it which they were.
    pub fn add_block_detailed(&mut self, block_blob: &[u8], tx_blobs: &[Vec<u8>]) -> Result<AddReport> {
        self.unwound.clear();
        // The one place `total` is measured, so that a reorganisation — which
        // re-enters `add_to_main` several times — is counted once, and the
        // phases it accumulates inside show up against that one total.
        let began = Instant::now();
        let outcome = self.add_block_inner(block_blob, tx_blobs);
        self.timings.blocks += 1;
        self.timings.total += began.elapsed();
        let outcome = outcome?;
        // A failed switch rolls itself back, so a reported unwind only ever
        // accompanies a successful one.
        let unwound = std::mem::take(&mut self.unwound);
        debug_assert!(unwound.is_empty() || outcome.status == AddStatus::AlternativeAndSwitched);
        Ok(AddReport { outcome, unwound })
    }

    /// [`ChainState::add_block_detailed`] with the block's proof-of-work hash
    /// already computed ([`PowHint`]). It accepts and rejects exactly what
    /// `add_block_detailed` does, with the same rule named; a hint for some
    /// other block is ignored.
    pub fn add_block_detailed_with_pow(
        &mut self,
        block_blob: &[u8],
        tx_blobs: &[Vec<u8>],
        hint: Option<PowHint>,
    ) -> Result<AddReport> {
        self.pow_hint = hint;
        let report = self.add_block_detailed(block_blob, tx_blobs);
        self.pow_hint = None;
        report
    }

    /// Step 10's proof-of-work test, from the [`PowHint`] the caller handed in
    /// when it is this block's, and from a fresh hash otherwise.
    fn proof_of_work_passes(&self, block: &BlockTemplate, hash: &Hash, difficulty: u64) -> Result<bool> {
        let passes = match self.pow_hint.filter(|h| h.block_hash == *hash) {
            Some(hint) => block.check_proof_of_work_with(&hint.pow_hash, difficulty),
            None => block.check_proof_of_work(difficulty),
        };
        passes.map_err(|_| ChainError::Rule(Rule::DeserializationFailed("proof of work input")))
    }

    fn add_block_inner(&mut self, block_blob: &[u8], tx_blobs: &[Vec<u8>]) -> Result<AddOutcome> {
        let decoding = Instant::now();
        let block = BlockTemplate::from_bytes(block_blob)
            .map_err(|_| ChainError::Rule(Rule::DeserializationFailed("block blob")))?;
        let hash = block.hash().map_err(|_| ChainError::Rule(Rule::DeserializationFailed("block hash")))?;
        self.timings.decode += decoding.elapsed();

        // 1. already known, in any segment.
        if self.has_block(&hash)? {
            return Err(Rule::AlreadyExists.into());
        }

        // 2. the parent must be in some segment.
        let parent = self.locate_parent(&block.previous_block_hash)?;
        match parent {
            Parent::MainTip(previous_index) => {
                let outcome = self.add_to_main(&block, block_blob, tx_blobs, hash, previous_index)?;
                // `Core.cpp:1688`: after a main-chain block nothing is excluded.
                self.prune_alternative_chains(None);
                Ok(outcome)
            }
            Parent::MainDepth(previous_index) => {
                self.add_to_alternative(&block, block_blob, tx_blobs, hash, previous_index, None)
            }
            Parent::Alternative(prev_hash) => {
                let previous_index = self.alt_blocks[&prev_hash].index;
                self.add_to_alternative(&block, block_blob, tx_blobs, hash, previous_index, Some(prev_hash))
            }
        }
    }

    fn locate_parent(&self, prev_hash: &Hash) -> Result<Parent> {
        if self.alt_blocks.contains_key(prev_hash) {
            return Ok(Parent::Alternative(*prev_hash));
        }
        match self.block_index_by_hash(prev_hash)? {
            Some(index) if Some(index) == self.tip => Ok(Parent::MainTip(index)),
            Some(index) => Ok(Parent::MainDepth(index)),
            None => Err(Rule::RejectedAsOrphaned.into()),
        }
    }

    /// `addBlock` step 10: a block inside the checkpoint zone must match its
    /// checkpoint, where it has one; any other block must carry the work.
    fn checkpoint_or_proof_of_work(
        &self,
        block: &BlockTemplate,
        claimed_index: u64,
        hash: &Hash,
        difficulty: u64,
        phases: &mut ValidateTimings,
    ) -> Result<()> {
        let step = Instant::now();
        if self.checkpoints.is_in_checkpoint_zone(claimed_index) {
            if !self.checkpoints.check_block(claimed_index as u32, hash) {
                let expected = *self.checkpoints.get(claimed_index as u32).expect("checked above");
                return Err(Rule::CheckpointBlockHashMismatch { expected, got: *hash }.into());
            }
        } else {
            phases.pow_blocks = 1;
            if !self.proof_of_work_passes(block, hash, difficulty)? {
                return Err(Rule::ProofOfWorkTooWeak { difficulty }.into());
            }
        }
        phases.checkpoint_or_pow = step.elapsed();
        Ok(())
    }

    /// The main-chain path of `addBlock`: full validation, then the write.
    fn add_to_main(
        &mut self,
        block: &BlockTemplate,
        block_blob: &[u8],
        tx_blobs: &[Vec<u8>],
        hash: Hash,
        previous_index: u32,
    ) -> Result<AddOutcome> {
        let index = previous_index + 1;
        let claimed_index = block.coinbase_height().unwrap_or(0);
        let view = ChainView::main_chain(previous_index);

        // 3. deserialize every transaction blob.
        let decoding = Instant::now();
        let transactions = parse_transactions(block, tx_blobs)?;
        self.timings.decode += decoding.elapsed();
        let validating = Instant::now();
        let mut phases = ValidateTimings::default();
        let reads_before = self.probe.reads();
        let step = Instant::now();
        let cumulative_size: u64 = tx_blobs.iter().map(|t| t.len() as u64).sum();
        let coinbase_size = block
            .base_transaction
            .to_bytes()
            .map_err(|_| ChainError::Rule(Rule::DeserializationFailed("coinbase does not re-serialize")))?
            .len() as u64;
        let cumulative_block_size = coinbase_size + cumulative_size;

        // 4. the hard block size cap.
        let max_cumulative = max_block_cumulative_size(index as u64);
        if cumulative_block_size > max_cumulative {
            return Err(Rule::CumulativeBlockSizeTooBig { size: cumulative_block_size, limit: max_cumulative }.into());
        }
        phases.size = step.elapsed();

        // 5. validateBlock.
        let step = Instant::now();
        let miner_reward = self.validate_block(block, &view, previous_index)?;
        phases.block_checks = step.elapsed();

        // 6. the difficulty for this index.
        let step = Instant::now();
        let difficulty = self.difficulty_for_next_block(&view, previous_index)?.ok_or(Rule::DifficultyOverhead)?;
        phases.difficulty = step.elapsed();

        // 7. transaction list consistency, from BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT.
        let step = Instant::now();
        check_transaction_list(block, &transactions, claimed_index, self.strict_transaction_list)?;
        phases.tx_list = step.elapsed();

        // 10, ahead of 8 and 9 when the block carries transactions: checkpoint
        // or proof of work.
        //
        // The C++ checks this last (`Core.cpp:1644`), after every transaction.
        // Steps 4 to 10 must all pass and none of them writes anything, so the
        // order cannot change which blocks are accepted — only which rule a
        // block failing several of them names. A block with transactions is
        // checked here, so one without the work, or off its checkpoint, costs
        // one hash instead of a block's worth of ring signatures. A block with
        // none has no such work to save and keeps the C++ order, and with it
        // the C++'s name for every rule it can fail.
        let work_first = !transactions.is_empty();
        if work_first {
            self.checkpoint_or_proof_of_work(block, claimed_index, &hash, difficulty, &mut phases)?;
        }

        // 8. every transaction, at the previous index, accumulating fees.
        //
        // Three pure checks of the *whole block* — the key-image domain check
        // of every key input, `check_key` of every transaction output, and the
        // ring signature of every key input — are gathered into one
        // `BlockRingBatch` and run in a single parallel pass at the end, rather
        // than one at a time on this thread. The first two run inside the
        // checkpoint zone as well (they are `validateTransactionInputs` and
        // `validateTransactionOutputs`, not `validateTransactionInputsExpensive`),
        // and the domain check is one ed25519 scalar multiplication per input,
        // which is what a linear import spends most of its time on once the
        // blocks start carrying spends.
        //
        // Everything else — the key-image set in `validator_state`, the
        // transaction order, the running fee — happens exactly where it did
        // before, on this thread. The determinism argument is on
        // `validate::BlockRingBatch`, and `validate_transaction_deferred`
        // states the four-step contract this loop implements.
        let step = Instant::now();
        let mut validator_state = ValidatorState::new();
        let ctx = TxContext {
            block_height: previous_index as u64,
            block_median_size: self.block_median_size,
            block_timestamp: block.timestamp,
            is_pool_transaction: false,
            checkpoints: &self.checkpoints,
        };
        let threads = self.cfg.validate_threads;
        let mut cumulative_fee: u64 = 0;
        let mut batch = BlockRingBatch::new();
        // A batched check's failure, named against the transaction it belongs
        // to. `first_invalid` reports the lowest one in visit order, which is
        // where the sequential C++ validator would have stopped.
        let batched_failure = |tx_index: usize, rule: TxRule| {
            ChainError::Rule(Rule::Transaction { hash: block.transaction_hashes[tx_index], index: tx_index, rule })
        };
        // The first transaction to fail anything but a ring signature. It is
        // held back: the C++ would have reported a lower `(transaction, input)`
        // signature failure first, and this loop has not verified those yet.
        let mut held_back: Option<ChainError> = None;
        // What the batch was asked to check, kept across an early settle so the
        // report still counts what a block actually cost.
        let mut settled = SettleCounts::default();
        for (i, (tx, blob)) in transactions.iter().zip(tx_blobs).enumerate() {
            let tx_hash = block.transaction_hashes[i];
            match validate_transaction_deferred(tx, blob, &mut validator_state, self, &ctx, i, &mut batch) {
                Ok(result) => cumulative_fee = cumulative_fee.saturating_add(result.fee),
                Err(e) => {
                    held_back = Some(match e {
                        crate::validate::TxError::Rule(rule) => {
                            ChainError::Rule(Rule::Transaction { hash: tx_hash, index: i, rule })
                        }
                        crate::validate::TxError::Chain(e) => e,
                    });
                    // The C++ stops the block here, so nothing above this
                    // transaction is validated and nothing above it is
                    // gathered.
                    break;
                }
            }
            // Bound the batch's memory by a constant. Settling a prefix early
            // cannot change what is reported — every check not yet gathered is
            // above every check in the prefix — and no real block reaches this.
            if batch.members() >= crate::validate::RING_BATCH_MEMBER_LIMIT {
                settled.take(&batch);
                let settling = Instant::now();
                let verdict = batch.first_invalid(threads);
                phases.settle += settling.elapsed();
                if let Some((tx_index, rule)) = verdict {
                    return Err(batched_failure(tx_index, rule));
                }
                batch.clear();
            }
        }
        // A batched failure anywhere below the held-back failure wins, and
        // every check in the batch is at or below it — at the same position the
        // batched check is the one the C++ reaches first.
        settled.take(&batch);
        let settling = Instant::now();
        let verdict = batch.first_invalid(threads);
        phases.settle += settling.elapsed();
        if let Some((tx_index, rule)) = verdict {
            return Err(batched_failure(tx_index, rule));
        }
        if let Some(e) = held_back {
            return Err(e);
        }
        phases.transactions = step.elapsed();
        phases.tx_inputs = transactions.iter().map(|t| t.prefix.inputs.len() as u64).sum();
        phases.tx_outputs = transactions.iter().map(|t| t.prefix.outputs.len() as u64).sum();
        phases.key_image_checks = settled.key_images;
        phases.output_key_checks = settled.output_keys;
        phases.ring_checks = settled.rings;

        // 9. the reward.
        let step = Instant::now();
        let parent_info = self
            .block_info(previous_index)?
            .ok_or_else(|| ChainError::Corrupt(format!("block info for index {previous_index} is missing")))?;
        let size_median = self.reward_size_median(&view, previous_index)?;
        let reward = get_block_reward(
            block.major_version,
            size_median,
            cumulative_block_size,
            parent_info.already_generated_coins,
            cumulative_fee,
            claimed_index,
        )
        .ok_or(Rule::CumulativeBlockSizeTooBig {
            size: cumulative_block_size,
            limit: 2 * size_median.max(full_reward_zone(block.major_version) as u64),
        })?;
        if miner_reward != reward.reward {
            return Err(Rule::BlockRewardMismatch { expected: reward.reward, got: miner_reward }.into());
        }
        phases.reward = step.elapsed();

        // 10. checkpoint or proof of work, where the C++ has it.
        if !work_first {
            self.checkpoint_or_proof_of_work(block, claimed_index, &hash, difficulty, &mut phases)?;
        }

        // 11. push.
        let info = BlockInfo {
            block_hash: hash,
            timestamp: block.timestamp,
            block_size: u32::try_from(cumulative_block_size)
                .map_err(|_| ChainError::Corrupt("cumulative block size exceeds uint32".into()))?,
            cumulative_difficulty: parent_info.cumulative_difficulty.wrapping_add(difficulty),
            // `pushBlock` adds the signed emission change to a uint64
            // (`DatabaseBlockchainCache.cpp:1526`); it wraps there, so it wraps
            // here.
            already_generated_coins: parent_info.already_generated_coins.wrapping_add(reward.emission_change as u64),
            already_generated_transactions: parent_info.already_generated_transactions + transactions.len() as u64 + 1,
        };
        // Steps 4 to 10 are behind us. A block that failed one of them returned
        // from inside that stretch and its cost lands in `Timings::other`
        // instead — an import that rejects a block stops there, so the
        // distinction only matters to a caller feeding the state bad blocks on
        // purpose.
        self.timings.validate += validating.elapsed();
        phases.store_reads = self.probe.reads().saturating_sub(reads_before);
        self.timings.phases.add(&phases);
        let committing = Instant::now();
        self.push_block(index, block, block_blob, tx_blobs, &transactions, info, &validator_state)?;
        self.timings.commit += committing.elapsed();

        Ok(AddOutcome {
            index,
            hash,
            cumulative_difficulty: info.cumulative_difficulty,
            already_generated_coins: info.already_generated_coins,
            difficulty,
            status: AddStatus::Main,
        })
    }

    /// The alternative-chain path.
    ///
    /// Everything `addBlock` does is repeated against the branch's own windows
    /// except step 8, `ValidateTransaction`: that needs the key images and the
    /// output table *of the branch*, which only exist once the branch is the
    /// main chain. The C++ builds an in-memory segment to answer those reads;
    /// we defer them to the switch, where every branch block goes through the
    /// full main-chain path and a failure rolls the switch back. The fees the
    /// reward rule needs do not depend on chain state, so the reward, the
    /// difficulty and the proof of work are all checked here, before an
    /// alternative block is kept at all.
    fn add_to_alternative(
        &mut self,
        block: &BlockTemplate,
        block_blob: &[u8],
        tx_blobs: &[Vec<u8>],
        hash: Hash,
        previous_index: u32,
        parent_alt: Option<Hash>,
    ) -> Result<AddOutcome> {
        let (fork_index, branch_infos) = self.branch_infos(parent_alt, previous_index)?;
        let view = ChainView::with_branch(fork_index, &branch_infos);
        debug_assert_eq!(view.top_index(), previous_index);
        let index = previous_index + 1;
        let claimed_index = block.coinbase_height().unwrap_or(0);

        let transactions = parse_transactions(block, tx_blobs)?;
        let cumulative_size: u64 = tx_blobs.iter().map(|t| t.len() as u64).sum();
        let coinbase_size = block
            .base_transaction
            .to_bytes()
            .map_err(|_| ChainError::Rule(Rule::DeserializationFailed("coinbase does not re-serialize")))?
            .len() as u64;
        let cumulative_block_size = coinbase_size + cumulative_size;
        let max_cumulative = max_block_cumulative_size(index as u64);
        if cumulative_block_size > max_cumulative {
            return Err(Rule::CumulativeBlockSizeTooBig { size: cumulative_block_size, limit: max_cumulative }.into());
        }

        let miner_reward = self.validate_block(block, &view, previous_index)?;
        let difficulty = self.difficulty_for_next_block(&view, previous_index)?.ok_or(Rule::DifficultyOverhead)?;
        check_transaction_list(block, &transactions, claimed_index, self.strict_transaction_list)?;

        let mut cumulative_fee: u64 = 0;
        for (i, tx) in transactions.iter().enumerate() {
            let sum_in = tx.prefix.sum_inputs().ok_or(Rule::Transaction {
                hash: block.transaction_hashes[i],
                index: i,
                rule: TxRule::InputsAmountOverflow,
            })?;
            let sum_out = tx.prefix.sum_outputs().ok_or(Rule::Transaction {
                hash: block.transaction_hashes[i],
                index: i,
                rule: TxRule::OutputsAmountOverflow,
            })?;
            let fee = sum_in.checked_sub(sum_out).ok_or(Rule::Transaction {
                hash: block.transaction_hashes[i],
                index: i,
                rule: TxRule::WrongAmount,
            })?;
            cumulative_fee = cumulative_fee.saturating_add(fee);
        }

        let parent_info = self
            .info_in_view(&view, previous_index)?
            .ok_or_else(|| ChainError::Corrupt(format!("block info for index {previous_index} is missing")))?;
        let size_median = self.reward_size_median(&view, previous_index)?;
        let reward = get_block_reward(
            block.major_version,
            size_median,
            cumulative_block_size,
            parent_info.already_generated_coins,
            cumulative_fee,
            claimed_index,
        )
        .ok_or(Rule::CumulativeBlockSizeTooBig {
            size: cumulative_block_size,
            limit: 2 * size_median.max(full_reward_zone(block.major_version) as u64),
        })?;
        if miner_reward != reward.reward {
            return Err(Rule::BlockRewardMismatch { expected: reward.reward, got: miner_reward }.into());
        }

        if self.checkpoints.is_in_checkpoint_zone(claimed_index) {
            if !self.checkpoints.check_block(claimed_index as u32, &hash) {
                let expected = *self.checkpoints.get(claimed_index as u32).expect("checked above");
                return Err(Rule::CheckpointBlockHashMismatch { expected, got: hash }.into());
            }
        } else if !self.proof_of_work_passes(block, &hash, difficulty)? {
            return Err(Rule::ProofOfWorkTooWeak { difficulty }.into());
        }

        let info = BlockInfo {
            block_hash: hash,
            timestamp: block.timestamp,
            block_size: u32::try_from(cumulative_block_size)
                .map_err(|_| ChainError::Corrupt("cumulative block size exceeds uint32".into()))?,
            cumulative_difficulty: parent_info.cumulative_difficulty.wrapping_add(difficulty),
            already_generated_coins: parent_info.already_generated_coins.wrapping_add(reward.emission_change as u64),
            already_generated_transactions: parent_info.already_generated_transactions + transactions.len() as u64 + 1,
        };
        self.alt_blocks.insert(
            hash,
            AltBlock {
                index,
                prev_hash: block.previous_block_hash,
                info,
                block_blob: block_blob.to_vec(),
                tx_blobs: tx_blobs.to_vec(),
            },
        );

        // Chain selection: strictly greater, so a tie keeps the current main
        // chain (`Core.cpp:1707`).
        let main_cumulative = self.tip_info().map(|i| i.cumulative_difficulty).unwrap_or(0);
        let mut status = AddStatus::Alternative;
        if info.cumulative_difficulty > main_cumulative {
            self.switch_to(&hash)?;
            status = AddStatus::AlternativeAndSwitched;
        }
        // `Core.cpp:1811`: the branch this block went into is never pruned by
        // its own arrival. After a switch it is the main chain and this names
        // no alternative block at all, which is what the C++ exclusion of
        // `chainsLeaves[0]` amounts to.
        self.prune_alternative_chains(Some(hash));

        Ok(AddOutcome {
            index,
            hash,
            cumulative_difficulty: info.cumulative_difficulty,
            already_generated_coins: info.already_generated_coins,
            difficulty,
            status,
        })
    }

    /// The alternative chain ending at `hash` (exclusive of nothing: the block
    /// itself is the last entry), as `(fork index on the main chain, hashes
    /// ascending)`.
    fn branch_hashes(&self, hash: &Hash) -> Result<(u32, Vec<Hash>)> {
        let mut chain = Vec::new();
        let mut cursor = *hash;
        loop {
            let Some(alt) = self.alt_blocks.get(&cursor) else {
                // `cursor` must be a main-chain block: the fork point. Pruning
                // takes whole leaf segments, so a branch never loses its lower
                // end — but if one ever did, that is the block's problem and
                // not a local fault: a peer must never be able to turn it into
                // an error that stops the node. It is an orphan.
                let Some(index) = self.block_index_by_hash(&cursor)? else {
                    return Err(Rule::RejectedAsOrphaned.into());
                };
                chain.reverse();
                return Ok((index, chain));
            };
            chain.push(cursor);
            cursor = alt.prev_hash;
            // A branch being extended is never pruned (see
            // `prune_alternative_chains`), so it may be longer than the
            // alternative block budget; it can never be longer than the map.
            if chain.len() > self.alt_blocks.len() {
                return Err(ChainError::Corrupt("alternative chain loops back on itself".into()));
            }
        }
    }

    /// The block infos of the branch ending at `parent_alt`, with the fork
    /// index. When the parent is a main-chain block the branch is empty and the
    /// fork point is the parent itself: the new block starts a segment there
    /// (`Core::split`, spec/07 "Chain segments").
    fn branch_infos(&self, parent_alt: Option<Hash>, previous_index: u32) -> Result<(u32, Vec<BlockInfo>)> {
        let Some(parent) = parent_alt else { return Ok((previous_index, Vec::new())) };
        let (fork_index, hashes) = self.branch_hashes(&parent)?;
        let infos = hashes.iter().map(|h| self.alt_blocks[h].info).collect();
        Ok((fork_index, infos))
    }

    /// Switch the main chain to the branch ending at `hash`
    /// (`ADDED_TO_ALTERNATIVE_AND_SWITCHED`): unwind the main chain to the fork
    /// point, apply the branch, and on failure put the old chain back.
    fn switch_to(&mut self, hash: &Hash) -> Result<()> {
        let (fork_index, branch) = self.branch_hashes(hash)?;
        let tip = self.tip.ok_or_else(|| ChainError::Corrupt("switching chains on an empty state".into()))?;

        // A lite or pruned node refuses a switch that reaches below the bodies
        // it keeps, *before* it unwinds anything. The C++ throws from `split`
        // and from `rewind` for the same reason: "The data needed to undo those
        // blocks was never stored" (`DatabaseBlockchainCache.cpp:820`, `:917`).
        //
        // This cannot fire on a correctly configured node — `MIN_PRUNE_DEPTH`
        // keeps the window wider than `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH`, and a
        // lite node's `--lite-height` is checked against the network top — so
        // it is the last line rather than the first. Refusing is the only
        // honest answer: the alternative chain may well be the heavier one, and
        // this node simply cannot follow it.
        if let Some(floor) = self.cfg.body_floor(tip) {
            if fork_index + 1 < floor {
                return Err(Rule::ReorganisationBelowBodyFloor { at_index: fork_index + 1, floor }.into());
            }
        }

        // The blocks about to leave the main chain, so they can go back if the
        // branch turns out to be invalid, and become alternative blocks if it
        // does not. Their infos are read before the unwind deletes them: an
        // alternative segment needs the cumulative difficulty, the size and the
        // emission of every block on it, exactly like the main chain.
        let mut unwound: Vec<UnwoundRaw> = Vec::new();
        for index in ((fork_index + 1)..=tip).rev() {
            let info = self
                .block_info(index)?
                .ok_or_else(|| ChainError::Corrupt(format!("block info for index {index} is missing")))?;
            let (blob, txs) = self.raw_block(index)?.ok_or(Rule::ReorganisationUnavailable { at_index: index })?;
            unwound.push((index, info, blob, txs));
        }
        for (index, _, _, _) in &unwound {
            self.unwind_block(*index)?;
        }

        // Apply the branch through the full main-chain path.
        let mut applied = 0usize;
        let mut failure = None;
        let mut failed_at = None;
        for h in &branch {
            let alt = self.alt_blocks[h].clone();
            match self.add_to_main(
                &BlockTemplate::from_bytes(&alt.block_blob)
                    .map_err(|_| ChainError::Rule(Rule::DeserializationFailed("alternative block blob")))?,
                &alt.block_blob,
                &alt.tx_blobs,
                *h,
                alt.index - 1,
            ) {
                Ok(_) => applied += 1,
                Err(e) => {
                    failure = Some(e);
                    failed_at = Some(*h);
                    break;
                }
            }
        }

        if let Some(e) = failure {
            // Roll the switch back: drop what was applied, restore the old tail.
            for index in ((fork_index + 1)..=(fork_index + applied as u32)).rev() {
                self.unwind_block(index)?;
            }
            for (index, _, blob, txs) in unwound.iter().rev() {
                let block = BlockTemplate::from_bytes(blob)
                    .map_err(|_| ChainError::Corrupt(format!("stored block {index} no longer parses")))?;
                let h = block.hash().map_err(|_| ChainError::Corrupt("stored block has no hash".into()))?;
                self.add_to_main(&block, blob, txs, h, index - 1)?;
            }
            // The block that failed, and everything built on it, can never
            // become valid: what it is checked against is its own branch. The
            // C++ never keeps such a block — it validates an alternative
            // block's transactions against its segment when it arrives
            // (`Core.cpp:1582`) — and this port defers that check to the
            // switch, so the branch is forgotten here. Kept, every block a
            // peer mined on top of it would replay this whole unwind and
            // re-apply. A storage error says nothing about the branch.
            if let (ChainError::Rule(_), Some(bad)) = (&e, failed_at) {
                self.drop_alternative_subtree(bad);
            }
            return Err(e);
        }

        // The branch is the main chain now: its blocks stop being alternatives,
        // and the old tail becomes one.
        for h in &branch {
            self.alt_blocks.remove(h);
        }
        for (index, info, blob, txs) in unwound {
            let block = BlockTemplate::from_bytes(&blob)
                .map_err(|_| ChainError::Corrupt(format!("stored block {index} no longer parses")))?;
            self.unwound.push(UnwoundBlock {
                index,
                hash: info.block_hash,
                transaction_hashes: block.transaction_hashes.clone(),
                transactions: txs.clone(),
            });
            self.alt_blocks.insert(
                info.block_hash,
                AltBlock { index, prev_hash: block.previous_block_hash, info, block_blob: blob, tx_blobs: txs },
            );
        }
        Ok(())
    }

    /// `Core::pruneStaleAlternativeChains` (`Core.cpp:4446`), local policy.
    ///
    /// The C++ keeps alternative chains as segments and only ever deletes a
    /// whole **leaf segment**, in three passes:
    ///
    /// 1. every leaf whose *tip* is more than `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH`
    ///    behind the main tip;
    /// 2. the weakest leaf (lowest cumulative difficulty) while there are more
    ///    than `CRYPTONOTE_MAX_ALT_CHAIN_COUNT` leaves;
    /// 3. the weakest leaf while more than `CRYPTONOTE_MAX_ALT_BLOCK_COUNT`
    ///    alternative blocks remain.
    ///
    /// `exclude` — the leaf the block just added went into — is never pruned
    /// (`Core.cpp:1811`); after a main-chain block nothing is excluded
    /// (`Core.cpp:1688`). Two consequences decide which chain a node can
    /// follow, so they are reproduced exactly:
    ///
    /// - a branch is judged by its tip, and the one being extended is never
    ///   pruned: while its blocks keep arriving, a C++ node follows a
    ///   reorganisation of any depth and length above the last checkpoint;
    /// - nothing is removed from the *bottom* of a branch while blocks above it
    ///   stay, so every alternative block always reaches the main chain.
    ///
    /// Pruning single blocks by their own height, as this used to, broke both:
    /// it capped reorganisations at 180 blocks deep and 100 long where the C++
    /// has no cap, and it left branches without their base, which made the next
    /// block on one a local error that stopped the node.
    ///
    /// A segment here is the run from a leaf down to the nearest block that
    /// another alternative block also builds on, or to the main chain. That is
    /// the C++ segment except after a sibling leaf was pruned, where the C++
    /// still holds two segments this sees as one run; the difference is only
    /// in which of two equally stale runs goes first, never in whether a
    /// branch that is still being extended survives.
    fn prune_alternative_chains(&mut self, exclude: Option<Hash>) {
        let Some(tip) = self.tip else { return };

        // Pass 1. The segments are gathered before any is removed, so a leaf
        // freed by a removal is not judged in the same pass (the C++ appends
        // it behind its descending loop).
        let stale: Vec<Vec<Hash>> = self
            .alternative_leaves()
            .into_iter()
            .filter(|leaf| Some(*leaf) != exclude)
            .filter(|leaf| {
                let top = self.alt_blocks[leaf].index;
                tip > top && u64::from(tip - top) > CRYPTONOTE_MAX_ALT_BLOCK_DEPTH
            })
            .map(|leaf| self.leaf_segment(leaf))
            .collect();
        for segment in stale {
            for h in segment {
                self.alt_blocks.remove(&h);
            }
        }

        // Pass 2.
        while self.alternative_leaves().len() > wrkz_primitives::constants::CRYPTONOTE_MAX_ALT_CHAIN_COUNT {
            let Some(weakest) = self.weakest_leaf(exclude) else { break };
            for h in self.leaf_segment(weakest) {
                self.alt_blocks.remove(&h);
            }
        }

        // Pass 3.
        while self.alt_blocks.len() > CRYPTONOTE_MAX_ALT_BLOCK_COUNT {
            let Some(weakest) = self.weakest_leaf(exclude) else { break };
            for h in self.leaf_segment(weakest) {
                self.alt_blocks.remove(&h);
            }
        }
    }

    /// The alternative blocks no other alternative block builds on
    /// (`chainsLeaves` without the main chain).
    fn alternative_leaves(&self) -> Vec<Hash> {
        let parents: HashSet<Hash> = self.alt_blocks.values().map(|b| b.prev_hash).collect();
        self.alt_blocks.keys().filter(|h| !parents.contains(*h)).copied().collect()
    }

    /// `findWeakest`: the leaf other than `exclude` with the lowest cumulative
    /// difficulty. The C++ breaks a tie by its leaf order; this breaks it by
    /// height, then hash, so the choice never depends on map order.
    fn weakest_leaf(&self, exclude: Option<Hash>) -> Option<Hash> {
        self.alternative_leaves().into_iter().filter(|h| Some(*h) != exclude).min_by_key(|h| {
            let b = &self.alt_blocks[h];
            (b.info.cumulative_difficulty, b.index, *h)
        })
    }

    /// The leaf segment ending at `leaf`: the leaf and the blocks under it,
    /// down to — not including — the first block another alternative block
    /// also builds on, or the main chain.
    fn leaf_segment(&self, leaf: Hash) -> Vec<Hash> {
        let mut children: HashMap<Hash, usize> = HashMap::new();
        for b in self.alt_blocks.values() {
            *children.entry(b.prev_hash).or_default() += 1;
        }
        let mut segment = vec![leaf];
        let mut cursor = self.alt_blocks[&leaf].prev_hash;
        while let Some(block) = self.alt_blocks.get(&cursor) {
            if children.get(&cursor).copied().unwrap_or(0) > 1 {
                break;
            }
            segment.push(cursor);
            cursor = block.prev_hash;
        }
        segment
    }

    /// Forget an alternative block and every alternative block built on it.
    fn drop_alternative_subtree(&mut self, root: Hash) {
        let mut doomed: HashSet<Hash> = HashSet::from([root]);
        loop {
            let before = doomed.len();
            for (h, b) in &self.alt_blocks {
                if doomed.contains(&b.prev_hash) {
                    doomed.insert(*h);
                }
            }
            if doomed.len() == before {
                break;
            }
        }
        self.alt_blocks.retain(|h, _| !doomed.contains(h));
    }

    // -- writes --------------------------------------------------------------

    /// `Currency::generateGenesisBlock` applied through
    /// `DatabaseBlockchainCache::addGenesisBlock` (line 3660).
    ///
    /// The C++ builds the record with an aggregate initializer whose field
    /// order is the *declaration* order of `CachedBlockInfo`
    /// (`IBlockchainCache.h:85`: hash, timestamp, cumulativeDifficulty,
    /// alreadyGeneratedCoins, alreadyGeneratedTransactions, blockSize), so
    /// genesis gets cumulative difficulty 1, emission = the coinbase total,
    /// one transaction, and a `blockSize` of the **coinbase size** (157), not
    /// the size of the block blob.
    fn apply_genesis(&mut self) -> Result<()> {
        let block = wrkz_primitives::block::genesis_block();
        let blob = block.to_bytes().map_err(|_| ChainError::Corrupt("genesis does not serialize".into()))?;
        let hash = block.hash().map_err(|_| ChainError::Corrupt("genesis has no hash".into()))?;
        let coinbase_size = block
            .base_transaction
            .to_bytes()
            .map_err(|_| ChainError::Corrupt("genesis coinbase does not serialize".into()))?
            .len() as u32;
        let miner_reward = block
            .coinbase_output_total()
            .ok_or_else(|| ChainError::Corrupt("genesis coinbase total overflows".into()))?;
        let info = BlockInfo {
            block_hash: hash,
            timestamp: block.timestamp,
            block_size: coinbase_size,
            cumulative_difficulty: 1,
            already_generated_coins: miner_reward,
            already_generated_transactions: 1,
        };
        self.push_block(0, &block, &blob, &[], &[], info, &ValidatorState::new())
    }

    /// One block, one write batch: a crash must not leave half a height behind.
    fn push_block(
        &mut self,
        index: u32,
        block: &BlockTemplate,
        block_blob: &[u8],
        tx_blobs: &[Vec<u8>],
        transactions: &[Transaction],
        info: BlockInfo,
        validator_state: &ValidatorState,
    ) -> Result<()> {
        let mut ops: Vec<WriteOp> = Vec::new();

        let coinbase_hash = block
            .base_transaction
            .hash()
            .map_err(|_| ChainError::Rule(Rule::DeserializationFailed("coinbase hash")))?;
        let mut tx_hashes = Vec::with_capacity(1 + transactions.len());
        tx_hashes.push(coinbase_hash);
        tx_hashes.extend_from_slice(&block.transaction_hashes);

        // Outputs, in `pushTransaction` order: the coinbase first, then the
        // block's transactions in block order, each output in its own order.
        // Global indexes are per amount and dense.
        let mut counts: HashMap<u64, u32> = HashMap::new();
        let mut output_refs: Vec<(u64, u32)> = Vec::new();
        let push_outputs = |ops: &mut Vec<WriteOp>,
                            counts: &mut HashMap<u64, u32>,
                            output_refs: &mut Vec<(u64, u32)>,
                            tx: &Transaction,
                            tx_hash: Hash,
                            state: &Self|
         -> Result<()> {
            for (output_index, output) in tx.prefix.outputs.iter().enumerate() {
                let next = match counts.get(&output.amount) {
                    Some(n) => *n,
                    None => state.output_count_for_amount(output.amount)?,
                };
                let record = OutputRecord {
                    public_key: output.key,
                    unlock_time: tx.prefix.unlock_time,
                    transaction_hash: tx_hash,
                    output_index: output_index as u16,
                    block_index: index,
                };
                ops.push((keys::output(output.amount, next), Some(record.encode())));
                counts.insert(output.amount, next + 1);
                output_refs.push((output.amount, next));
            }
            Ok(())
        };
        push_outputs(&mut ops, &mut counts, &mut output_refs, &block.base_transaction, coinbase_hash, self)?;
        for (tx, tx_hash) in transactions.iter().zip(&block.transaction_hashes) {
            push_outputs(&mut ops, &mut counts, &mut output_refs, tx, *tx_hash, self)?;
        }
        for (amount, count) in counts {
            ops.push((keys::output_count(amount), Some(count.to_le_bytes().to_vec())));
        }

        // Spent key images: the block's validator state is exactly the set the
        // C++ writes under its `0` and `7` records.
        let mut images: Vec<Hash> = validator_state.spent_key_images.iter().copied().collect();
        images.sort_unstable();
        for image in &images {
            ops.push((keys::key_image(image), Some(index.to_le_bytes().to_vec())));
        }
        if !images.is_empty() {
            ops.push((keys::block_key_images(index), Some(encode_hashes(&images))));
        }

        ops.push((keys::block_info(index), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(index.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(index), Some(encode_hashes(&tx_hashes))));
        // `pushTransaction` writes a record for every transaction of the block,
        // the coinbase included, which is what `isTransactionInChain` reads.
        for tx_hash in &tx_hashes {
            ops.push((keys::transaction_index(tx_hash), Some(index.to_le_bytes().to_vec())));
        }
        ops.push((keys::block_outputs(index), Some(encode_output_refs(&output_refs))));

        // The payment-id index (`paymentIdIndex`, `Core.cpp`): the plaintext
        // long ids only, and a per-block record so an unwind can take exactly
        // these back out without the block body.
        let mut payment_refs: Vec<(Hash, Hash)> = Vec::new();
        for (tx, tx_hash) in transactions.iter().zip(&block.transaction_hashes) {
            if let Some(PaymentId::Long(id)) = parse_extra_wallet(&tx.prefix.extra).payment_id {
                payment_refs.push((id, *tx_hash));
            }
        }
        if !payment_refs.is_empty() {
            // One entry and one counter per id: constant work however often the
            // id was used before. The legacy list is never written again.
            let mut entry_counts: HashMap<Hash, u32> = HashMap::new();
            for (id, tx_hash) in &payment_refs {
                let n = match entry_counts.get(id) {
                    Some(n) => *n,
                    None => self.payment_id_entry_count(id)?,
                };
                ops.push((keys::payment_id_entry(id, n), Some(tx_hash.to_vec())));
                entry_counts.insert(*id, n + 1);
            }
            for (id, count) in &entry_counts {
                ops.push((keys::payment_id_count(id), Some(count.to_le_bytes().to_vec())));
            }
            ops.push((keys::block_payment_ids(index), Some(encode_payment_id_refs(&payment_refs))));
        }

        if self.cfg.keeps_body(index, index) {
            ops.push((keys::raw_block(index), Some(encode_raw_block(block_blob, tx_blobs))));
        }
        // Pruned mode: the body that has just fallen out of the retention
        // window. `prune_depth` is at least `MIN_PRUNE_DEPTH`, so the block
        // being dropped here is already further back than any reorganisation
        // this node will accept (`open` refuses a shallower depth).
        if let Some(depth) = self.cfg.prune_depth {
            if let Some(victim) = index.checked_sub(depth) {
                ops.push((keys::raw_block(victim), None));
            }
        }
        // Prune the unwind history beyond the deepest reorganisation policy allows.
        if index >= self.cfg.unwind_history {
            ops.push((keys::block_outputs(index - self.cfg.unwind_history), None));
        }
        ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
        ops.push((keys::meta(keys::META_TIP), Some(index.to_le_bytes().to_vec())));

        self.store.write_batch(ops)?;

        self.tip = Some(index);
        self.recent.push_back(info);
        while self.recent.len() > self.cfg.recent_window {
            self.recent.pop_front();
        }
        self.update_block_median_size()?;
        Ok(())
    }

    /// Remove the top block, restoring the state to its parent. The block's
    /// key images and outputs come from the per-block records, so no block body
    /// is needed — unless the output record was dropped behind
    /// [`Config::unwind_history`], when the outputs are rebuilt from the body
    /// ([`ChainState::block_output_refs`]).
    fn unwind_block(&mut self, index: u32) -> Result<()> {
        if self.tip != Some(index) {
            return Err(ChainError::Corrupt(format!("unwind of {index} but the tip is {:?}", self.tip)));
        }
        let mut scratch = UnwindScratch::default();
        let mut ops: Vec<WriteOp> = Vec::new();
        self.unwind_ops(index, &mut scratch, &mut ops)?;
        scratch.finish(&mut ops);
        if index == 0 {
            ops.push((keys::meta(keys::META_TIP), None));
        } else {
            ops.push((keys::meta(keys::META_TIP), Some((index - 1).to_le_bytes().to_vec())));
        }
        self.store.write_batch(ops)?;

        self.tip = index.checked_sub(1);
        self.reload_recent()?;
        self.update_block_median_size()?;
        Ok(())
    }

    /// Remove every main-chain block above `top_index`, so that the block at
    /// `top_index` is the tip again, and report how many blocks went.
    ///
    /// This is `--rewind-to-height` (`Core::rewind`, `Core.cpp:3991`, and
    /// `DatabaseBlockchainCache::rewind`, `DatabaseBlockchainCache.cpp:951`).
    /// The C++ option takes a block *count* `N` and leaves the block at `N - 1`
    /// on top; a caller holding that count passes `N - 1` here. Taking the index
    /// means there is no argument that could remove genesis.
    ///
    /// A `top_index` at or above the tip changes nothing and reports 0.
    ///
    /// # What it leaves behind
    ///
    /// Exactly the records the state held when the block at `top_index` was
    /// last the tip: every block's outputs, per-amount counters, spent key
    /// images, transaction-index and payment-id entries, block records and
    /// body go, as one unwind at a time would take them; the counters and the
    /// payment-id index are computed back from what the removed blocks added.
    /// One thing is put **back**: the per-block output records that
    /// `push_block` dropped behind [`Config::unwind_history`] while the removed
    /// blocks were applied, for the blocks the rewind keeps, rebuilt from their
    /// bodies where the bodies are held.
    ///
    /// What cannot come back is a body a pruned node deleted while the removed
    /// blocks were applied; the chain is still complete without it.
    ///
    /// Alternative chains are forgotten, as they are across a restart: they
    /// are in-memory and the C++ has none at the point it rewinds either.
    ///
    /// # Atomic
    ///
    /// Every removal is computed first and written as **one**
    /// [`KvStore::write_batch`], with the new tip in it. A refusal or an error
    /// before that write leaves the store untouched; the write itself is atomic
    /// on RocksDB. On a [`wrkz_storage::batch::BatchStore`] it lands in the
    /// overlay like a block does, and reaches the engine at the caller's next
    /// [`ChainState::flush`] — a crash before then leaves the old tip, whole.
    ///
    /// # Refusals
    ///
    /// - [`Rule::ReorganisationBelowBodyFloor`] when a removed block is below
    ///   the lite height, as the C++ refuses (`isLiteIndexOnlyHeight`,
    ///   `DatabaseBlockchainCache.cpp:960`). This state could undo those blocks
    ///   from its records alone; the refusal keeps the C++'s rule that a lite
    ///   node never goes back below its line.
    /// - [`Rule::ReorganisationUnavailable`] when a removed block has neither
    ///   its per-block output record nor its body, so what it created cannot be
    ///   found to take back out.
    ///
    /// There is no depth limit here; the daemon applies the C++'s
    /// `MAX_BLOCK_ALLOWED_TO_REWIND`. The batch holds a few dozen operations
    /// per removed block, so its size is the caller's to bound.
    pub fn rewind_to(&mut self, top_index: u32) -> Result<u32> {
        let Some(tip) = self.tip else { return Ok(0) };
        if top_index >= tip {
            return Ok(0);
        }
        let lite = self.cfg.lite_start_height;
        if lite != 0 && top_index + 1 < lite {
            return Err(Rule::ReorganisationBelowBodyFloor { at_index: top_index + 1, floor: lite }.into());
        }
        let mut scratch = UnwindScratch::default();
        let mut ops: Vec<WriteOp> = Vec::new();
        // From the top down: the payment-id entries come off the end of each
        // id's list, and the top block's are the last.
        for index in ((top_index + 1)..=tip).rev() {
            self.unwind_ops(index, &mut scratch, &mut ops)?;
        }
        scratch.finish(&mut ops);
        self.restore_output_refs(top_index, tip, &mut ops)?;
        ops.push((keys::meta(keys::META_TIP), Some(top_index.to_le_bytes().to_vec())));
        self.store.write_batch(ops)?;

        self.tip = Some(top_index);
        self.alt_blocks.clear();
        self.unwound.clear();
        self.reload_recent()?;
        self.update_block_median_size()?;
        Ok(tip - top_index)
    }

    /// The per-block output records `push_block` dropped while the blocks in
    /// `(top_index, old_tip]` were applied, for the blocks a rewind to
    /// `top_index` keeps.
    ///
    /// Applying block `i` drops the record of `i - unwind_history`, so those
    /// blocks dropped the records in `(top_index - history, old_tip - history]`,
    /// and a state whose tip is `top_index` keeps `(top_index - history,
    /// top_index]`. Where both hold and the record is absent it is rebuilt from
    /// the body. Computed against the store before the rewind is written, which
    /// gives the same pairs: a kept block's global indexes are all below the
    /// removed blocks' ones.
    fn restore_output_refs(&self, top_index: u32, old_tip: u32, ops: &mut Vec<WriteOp>) -> Result<()> {
        let history = u64::from(self.cfg.unwind_history);
        let (top, old) = (u64::from(top_index), u64::from(old_tip));
        let Some(last_dropped) = old.checked_sub(history) else { return Ok(()) };
        let low = (top + 1).saturating_sub(history);
        let high = top.min(last_dropped);
        for index in low..=high {
            let index = index as u32;
            if self.store_get(&keys::block_outputs(index))?.is_some() {
                continue;
            }
            // A block without a body — a seeded or body-less region — never had
            // one here that this could have been rebuilt from, so it stays as
            // the apply left it.
            if let Some(refs) = self.block_output_refs(index)? {
                ops.push((keys::block_outputs(index), Some(encode_output_refs(&refs))));
            }
        }
        Ok(())
    }

    /// The writes that take block `index` off the chain, appended to `ops`.
    ///
    /// Several blocks may be unwound in one batch ([`ChainState::rewind_to`]),
    /// and then the per-amount counters and the payment-id index have to be
    /// read as the blocks above this one leave them, not as the store still
    /// holds them: `scratch` carries those values from block to block, and
    /// [`UnwindScratch::finish`] writes them once at the end. Everything else a
    /// block wrote belongs to that block alone.
    fn unwind_ops(&self, index: u32, scratch: &mut UnwindScratch, ops: &mut Vec<WriteOp>) -> Result<()> {
        let info = self
            .block_info(index)?
            .ok_or_else(|| ChainError::Corrupt(format!("block info for index {index} is missing")))?;

        if let Some(raw) = self.store_get(&keys::block_key_images(index))? {
            for image in decode_hashes(&raw)? {
                ops.push((keys::key_image(&image), None));
            }
            ops.push((keys::block_key_images(index), None));
        }

        // The payment-id entries this block contributed, taken back out in
        // reverse order so the index is exactly what it was before the block.
        // Entries come off the end first; once an id has none left, the block
        // was applied under schema 3 and its hash is the legacy list's last.
        if let Some(raw) = self.store_get(&keys::block_payment_ids(index))? {
            let refs = decode_payment_id_refs(&raw)?;
            // Not what this block appended: a state written by another tool, or
            // a record that no longer matches. Removing the wrong hash would be
            // worse than leaving the index alone.
            let mismatch = || {
                ChainError::Corrupt(format!(
                    "payment-id index for block {index} does not end with the entry that block added"
                ))
            };
            for (id, tx_hash) in refs.iter().rev() {
                let count = match scratch.entry_counts.get(id) {
                    Some(n) => *n,
                    None => self.payment_id_entry_count(id)?,
                };
                if count > 0 {
                    let last = count - 1;
                    let raw = self.store_get(&keys::payment_id_entry(id, last))?;
                    if decode_payment_id_entry(raw, last, count)? != *tx_hash {
                        return Err(mismatch());
                    }
                    ops.push((keys::payment_id_entry(id, last), None));
                    scratch.entry_counts.insert(*id, last);
                } else {
                    let mut list = match scratch.legacy.remove(id) {
                        Some(list) => list,
                        None => self.legacy_payment_id_list(id)?,
                    };
                    if list.pop().as_ref() != Some(tx_hash) {
                        return Err(mismatch());
                    }
                    scratch.legacy.insert(*id, list);
                }
            }
            ops.push((keys::block_payment_ids(index), None));
        }

        // The record `push_block` wrote, or the pairs rebuilt from the body
        // when the record was dropped behind `unwind_history`.
        let refs = self.block_output_refs(index)?.ok_or(Rule::ReorganisationUnavailable { at_index: index })?;
        for (amount, global_index) in &refs {
            ops.push((keys::output(*amount, *global_index), None));
            let e = scratch.lowest_output.entry(*amount).or_insert(*global_index);
            *e = (*e).min(*global_index);
        }
        ops.push((keys::block_outputs(index), None));
        ops.push((keys::block_info(index), None));
        ops.push((keys::hash_to_index(&info.block_hash), None));
        for tx_hash in self.block_transaction_hashes(index)? {
            ops.push((keys::transaction_index(&tx_hash), None));
        }
        ops.push((keys::block_tx_hashes(index), None));
        ops.push((keys::raw_block(index), None));
        Ok(())
    }
}

/// The records several unwound blocks share, as the blocks already unwound in
/// the same batch leave them. See [`ChainState::unwind_ops`].
#[derive(Default)]
struct UnwindScratch {
    /// Per amount, the lowest global index an unwound block created.
    lowest_output: HashMap<u64, u32>,
    /// Per payment id, the entry count once the unwound blocks' entries are off.
    entry_counts: HashMap<Hash, u32>,
    /// Per payment id, the schema-3 list once the unwound blocks' hashes are off.
    legacy: HashMap<Hash, Vec<Hash>>,
}

impl UnwindScratch {
    /// The shared records' final values, one write each.
    fn finish(self, ops: &mut Vec<WriteOp>) {
        for (amount, first) in self.lowest_output {
            // Global indexes are dense and assigned in order, so removing a
            // block's outputs takes the counter back to the lowest one it added.
            if first == 0 {
                ops.push((keys::output_count(amount), None));
            } else {
                ops.push((keys::output_count(amount), Some(first.to_le_bytes().to_vec())));
            }
        }
        for (id, count) in &self.entry_counts {
            ops.push((keys::payment_id_count(id), (*count > 0).then(|| count.to_le_bytes().to_vec())));
        }
        for (id, list) in &self.legacy {
            ops.push((keys::payment_id(id), if list.is_empty() { None } else { Some(encode_hashes(list)) }));
        }
    }
}

/// The block indexes `getDifficultyForNextBlock(parent_index)` reads, oldest
/// first: the last `difficultyBlocksCount` infos ending at `parent_index`,
/// genesis excluded (`UseGenesis(false)`).
///
/// The range is empty when there are none — at `parent_index` 0, where the
/// only candidate is genesis itself.
pub fn difficulty_window_indexes(parent_index: u32) -> std::ops::RangeInclusive<u32> {
    let next_version = block_major_version_for_index(parent_index as u64 + 1);
    let count = difficulty_blocks_count(next_version, parent_index as u64) as u64;
    let mut from = (parent_index as u64 + 1).saturating_sub(count);
    if from == 0 {
        from = 1;
    }
    // `from` is at most `parent_index + 1`, so the cast cannot truncate; when
    // it is `parent_index + 1` the range is empty, which is what we want.
    (from.min(parent_index as u64 + 1) as u32)..=parent_index
}

/// The difficulty rule itself, over the infos of
/// [`difficulty_window_indexes`] in the same order.
///
/// `None` is the C++ `DIFFICULTY_OVERHEAD` case: the algorithm returned 0, or
/// the window had no defined result (spec/07 "Difficulty").
pub fn difficulty_for_next_block_from(parent_index: u32, window: &[BlockInfo]) -> Option<u64> {
    let next_version = block_major_version_for_index(parent_index as u64 + 1);
    let timestamps: Vec<u64> = window.iter().map(|i| i.timestamp).collect();
    let cumulative: Vec<u64> = window.iter().map(|i| i.cumulative_difficulty).collect();
    wrkz_primitives::difficulty::next_difficulty(next_version, parent_index as u64, &timestamps, &cumulative)
        .filter(|d| wrkz_primitives::difficulty::is_valid_difficulty(*d))
}

enum Parent {
    MainTip(u32),
    MainDepth(u32),
    Alternative(Hash),
}

/// `Core::extractTransactions`: every transaction blob must deserialize, and
/// the count must match the block's `tx_hashes`.
fn parse_transactions(block: &BlockTemplate, tx_blobs: &[Vec<u8>]) -> Result<Vec<Transaction>> {
    if tx_blobs.len() != block.transaction_hashes.len() {
        return Err(Rule::DeserializationFailed("transaction count does not match tx_hashes").into());
    }
    let mut out = Vec::with_capacity(tx_blobs.len());
    for blob in tx_blobs {
        out.push(
            Transaction::from_bytes(blob).map_err(|_| ChainError::Rule(Rule::DeserializationFailed("transaction")))?,
        );
    }
    Ok(out)
}

/// `Core::addBlock` step 7 (`Core.cpp:1534`), from
/// `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT` (600,000): no duplicate hashes on either
/// side, every supplied blob named by the template, and the two lists equal in
/// order. Below that height the C++ checks only the count, in the P2P layer;
/// with `strict` ([`ChainState::set_strict_transaction_list`], the default)
/// every blob must still be the transaction the template names at its place.
fn check_transaction_list(
    block: &BlockTemplate,
    transactions: &[Transaction],
    claimed_index: u64,
    strict: bool,
) -> Result<()> {
    if claimed_index < BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT {
        if !strict {
            return Ok(());
        }
        // `parse_transactions` has already matched the counts. Duplicates are
        // left alone: the template is what the block id commits to, and the
        // C++ took it as it was below this height.
        for (tx, named) in transactions.iter().zip(&block.transaction_hashes) {
            let hash = tx.hash().map_err(|_| ChainError::Rule(Rule::DeserializationFailed("transaction hash")))?;
            if hash != *named {
                return Err(Rule::TransactionInconsistency.into());
            }
        }
        return Ok(());
    }
    let template: &[Hash] = &block.transaction_hashes;
    if template.iter().collect::<HashSet<_>>().len() != template.len() {
        return Err(Rule::TransactionDuplicates.into());
    }
    let mut supplied = Vec::with_capacity(transactions.len());
    for tx in transactions {
        supplied.push(tx.hash().map_err(|_| ChainError::Rule(Rule::DeserializationFailed("transaction hash")))?);
    }
    if supplied.iter().collect::<HashSet<_>>().len() != supplied.len() {
        return Err(Rule::TransactionDuplicates.into());
    }
    for hash in &supplied {
        if !template.contains(hash) {
            return Err(Rule::TransactionInconsistency.into());
        }
    }
    if template != supplied.as_slice() {
        return Err(Rule::TransactionInconsistency.into());
    }
    Ok(())
}

impl<S: KvStore> ChainAccess for ChainState<S> {
    fn key_image_spent(&self, key_image: &Hash, block_index: u64) -> Result<bool> {
        // `DatabaseBlockchainCache::checkIfSpent`: spent *at or below* the
        // validation height. A key image first spent above it does not count,
        // which is what makes an alternative chain able to respend.
        Ok(matches!(self.key_image_spent_at(key_image)?, Some(at) if at as u64 <= block_index))
    }

    fn key_output(&self, amount: u64, global_index: u64) -> Result<Option<OutputRecord>> {
        let Ok(global_index) = u32::try_from(global_index) else { return Ok(None) };
        match self.store_get(&keys::output(amount, global_index))? {
            Some(raw) => Ok(Some(OutputRecord::decode(&raw)?)),
            None => Ok(None),
        }
    }

    fn key_outputs(&self, amount: u64, global_indexes: &[u64]) -> Result<Vec<Option<OutputRecord>>> {
        let mut wanted = Vec::with_capacity(global_indexes.len());
        let mut oversized = Vec::with_capacity(global_indexes.len());
        for gi in global_indexes {
            match u32::try_from(*gi) {
                Ok(gi) => {
                    oversized.push(false);
                    wanted.push(keys::output(amount, gi));
                }
                Err(_) => oversized.push(true),
            }
        }
        let found = self.store_multi_get(&wanted)?;
        let mut it = found.into_iter();
        let mut out = Vec::with_capacity(global_indexes.len());
        for over in oversized {
            if over {
                out.push(None);
                continue;
            }
            out.push(match it.next().flatten() {
                Some(raw) => Some(OutputRecord::decode(&raw)?),
                None => None,
            });
        }
        Ok(out)
    }

    fn top_block_timestamp(&self) -> u64 {
        self.tip_info().map(|i| i.timestamp).unwrap_or(0)
    }

    fn now(&self) -> u64 {
        self.clock.unwrap_or_else(|| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
        })
    }
}

/// One [`keys::TAG_PAYMENT_ID_ENTRY`] value, entry `n` of `count`: exactly a
/// hash, and present, since the counter says it was written.
fn decode_payment_id_entry(raw: Option<Vec<u8>>, n: u32, count: u32) -> Result<Hash> {
    match raw {
        Some(raw) => <Hash>::try_from(&raw[..])
            .map_err(|_| ChainError::Corrupt(format!("payment-id entry {n} is {} bytes", raw.len()))),
        None => Err(ChainError::Corrupt(format!("payment-id entry {n} of {count} is missing"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_storage::MemStore;

    fn chain() -> ChainState<MemStore> {
        ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap()
    }

    /// Below 600,000 the C++ checks only the count; strict (the default) holds
    /// every blob to the hash the template names at its place.
    #[test]
    fn below_the_shuffle_height_strict_holds_the_blobs_to_the_named_hashes() {
        let tx = |unlock_time| Transaction {
            prefix: wrkz_primitives::tx::TransactionPrefix { version: 1, unlock_time, ..Default::default() },
            signatures: Vec::new(),
        };
        let (one, two, other) = (tx(1), tx(2), tx(3));
        let block = BlockTemplate {
            transaction_hashes: vec![one.hash().unwrap(), two.hash().unwrap()],
            ..Default::default()
        };
        let below = BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT - 1;
        let refused = |r: Result<()>| r.unwrap_err().rule().cloned();

        assert!(check_transaction_list(&block, &[one.clone(), two.clone()], below, true).is_ok());
        let swapped = [one.clone(), other];
        assert_eq!(refused(check_transaction_list(&block, &swapped, below, true)), Some(Rule::TransactionInconsistency));
        let reordered = [two, one];
        assert_eq!(
            refused(check_transaction_list(&block, &reordered, below, true)),
            Some(Rule::TransactionInconsistency)
        );
        // `--legacy-transaction-list`: the C++ behaviour, count only.
        assert!(check_transaction_list(&block, &swapped, below, false).is_ok());
    }

    #[test]
    fn genesis_record_matches_the_cpp_add_genesis_block() {
        let c = chain();
        assert_eq!(c.tip_index(), Some(0));
        let info = *c.tip_info().unwrap();
        assert_eq!(hex::encode(info.block_hash), "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce");
        assert_eq!(info.timestamp, 0);
        assert_eq!(info.cumulative_difficulty, 1);
        assert_eq!(info.already_generated_coins, 1_500_000_000_000);
        assert_eq!(info.already_generated_transactions, 1);
        // `blockSize` is the coinbase size, from the aggregate initializer.
        assert_eq!(info.block_size, 157);
        // Three outputs of 500,000,000,000, global indexes 0..2.
        assert_eq!(c.output_count_for_amount(500_000_000_000).unwrap(), 3);
        assert!(c.key_output(500_000_000_000, 2).unwrap().is_some());
        assert!(c.key_output(500_000_000_000, 3).unwrap().is_none());
        assert_eq!(c.block_transaction_hashes(0).unwrap().len(), 1);
        assert_eq!(c.block_index_by_hash(&info.block_hash).unwrap(), Some(0));
        // The coinbase is in the transaction index, as `pushTransaction` puts
        // every transaction of the block there.
        let coinbase = wrkz_primitives::block::genesis_block().base_transaction.hash().unwrap();
        assert_eq!(c.transaction_block_index(&coinbase).unwrap(), Some(0));
        assert!(!c.has_transaction(&[0; 32]).unwrap());
        // The median size starts at the granted full reward zone for block 1
        // (v1: 10,000), never below it.
        assert_eq!(c.block_median_size(), 10_000);
    }

    #[test]
    fn reopening_recovers_the_tip_and_the_windows() {
        let c = chain();
        let store = c.into_store();
        let c = ChainState::open(store, Config::default(), Checkpoints::mainnet()).unwrap();
        assert_eq!(c.tip_index(), Some(0));
        assert_eq!(c.tip_info().unwrap().already_generated_coins, 1_500_000_000_000);
        assert_eq!(c.block_median_size(), 10_000);
    }

    #[test]
    fn a_state_from_another_schema_version_is_refused() {
        for v in [2u32, keys::STATE_SCHEMA_VERSION + 1, 99] {
            let mut store = MemStore::default();
            store.put(keys::meta(keys::META_VERSION), v.to_le_bytes().to_vec()).unwrap();
            assert!(ChainState::open(store, Config::default(), Checkpoints::mainnet()).is_err(), "version {v}");
        }
        // Schema 3 is complete under 4 (only new payment-id entries moved), so
        // it opens as it is.
        for v in keys::OLDEST_READABLE_SCHEMA_VERSION..=keys::STATE_SCHEMA_VERSION {
            let mut store = MemStore::default();
            store.put(keys::meta(keys::META_VERSION), v.to_le_bytes().to_vec()).unwrap();
            assert!(ChainState::open(store, Config::default(), Checkpoints::mainnet()).is_ok(), "version {v}");
        }
    }

    #[test]
    fn genesis_is_not_added_twice_and_an_orphan_is_named() {
        let mut c = chain();
        let blob = wrkz_primitives::block::genesis_block().to_bytes().unwrap();
        assert_eq!(c.add_block(&blob, &[]).unwrap_err().rule(), Some(&Rule::AlreadyExists));
        // A block whose parent nobody has.
        let mut orphan = wrkz_primitives::block::genesis_block();
        orphan.previous_block_hash = [9; 32];
        let blob = orphan.to_bytes().unwrap();
        assert_eq!(c.add_block(&blob, &[]).unwrap_err().rule(), Some(&Rule::RejectedAsOrphaned));
    }
}
