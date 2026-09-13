// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Chain state and consensus validation (spec/06-transactions.md,
//! spec/07-blocks-consensus.md).
//!
//! The C++ node splits this work between `Core` (block acceptance order, the
//! reward rule, chain segments), `ValidateTransaction` (transaction rules),
//! `Currency` (reward, proof of work), `Checkpoints` and the two
//! `IBlockchainCache` implementations (the state a validator reads). This crate
//! keeps the same split:
//!
//! - [`keys`] and [`records`] — our own key namespace and record encodings,
//!   deliberately *not* the C++ RocksDB layout of spec/11: a port that writes
//!   its own database is free to choose, and big-endian keys give RocksDB the
//!   numeric order that the C++ KV-document keys cannot have.
//! - [`state`] — [`ChainState`], the consensus state a validator needs, over
//!   any [`wrkz_storage::KvStore`], plus block acceptance, alternative chains
//!   and reorganisation.
//! - [`validate`] — `validate_transaction` in the `ValidateTransaction::validate`
//!   order, over the [`ChainAccess`] view of the chain, plus
//!   [`revalidate_after_height_change`], the subset of those rules the C++
//!   re-runs on a pooled transaction when the height moves under it.
//! - [`reward`] — `Currency::getBlockReward`, `getPenalizedAmount`, the median.
//! - [`fusion`] — `Currency::isFusionTransaction`.
//! - [`checkpoints`] — the compiled-in checkpoint table and the zone semantics,
//!   with a switch that disables checkpoints from a chosen height so that the
//!   rules the zone skips can be exercised against real data.
//! - [`replay`] — the engine behind `wrkz-replay`: read a C++ node's database
//!   block by block, apply it here, and check every derived value against the
//!   C++ records, linearly or in windows.
//! - [`windows`] — which slices of the chain a windowed replay covers: the
//!   blocks on either side of every height where a rule changes.
//! - [`verify`] — rebuild a state someone handed you from its own block bodies
//!   and compare it record for record, so a snapshot can be trusted.
//! - [`dump`] — the C++ daemon's blockchain dump file, read and written, and
//!   the export and import behind `--export-blockchain` and
//!   `--import-blockchain`.
//! - `snapshot` (feature `lite-snapshot`) — the C++ lite node snapshot file,
//!   `.litesnap`: its container, and moving the region below a lite height into
//!   and out of this crate's state byte-compatibly with `Wrkzd`.
//!
//! Where the C++ has a quirk, the quirk is the rule, and the comment cites the
//! C++ line it came from.
//!
//! # Example
//!
//! ```
//! use wrkz_chain::{ChainState, Config, Checkpoints};
//! use wrkz_storage::MemStore;
//!
//! let mut chain = ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet())
//!     .expect("genesis applies");
//! assert_eq!(chain.tip_index(), Some(0));
//! assert_eq!(chain.tip_info().unwrap().already_generated_coins, 1_500_000_000_000);
//! ```

pub mod checkpoints;
pub mod dump;
pub mod fusion;
pub mod interrupt;
pub mod keys;
pub mod records;
pub mod replay;
pub mod reward;
#[cfg(feature = "lite-snapshot")]
pub mod snapshot;
pub mod state;
pub mod validate;
pub mod verify;
pub mod windows;

pub use checkpoints::Checkpoints;
pub use records::{BlockInfo, OutputRecord};
pub use state::{
    difficulty_for_next_block_from, difficulty_window_indexes, AddOutcome, AddReport, AddStatus, ChainState, ChainView,
    Config, PowHint, Timings, UnwoundBlock, MIN_PRUNE_DEPTH,
};
pub use validate::{revalidate_after_height_change, ChainAccess, RevalidateContext, TxRule, TxValidation};
pub use windows::Window;

use wrkz_primitives::Hash;

/// Every consensus rule this crate can reject a block or a transaction with.
///
/// One variant per C++ error code of `BlockValidationError` /
/// `TransactionValidationError` that a block can actually hit, plus the two
/// "not an error" outcomes of `Core::addBlock` (`ALREADY_EXISTS`,
/// `REJECTED_AS_ORPHANED`). The payloads carry what a replay log needs to say
/// what went wrong without re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// `ALREADY_EXISTS` — the block is already in some segment (not an error).
    AlreadyExists,
    /// `REJECTED_AS_ORPHANED` — no segment holds the parent.
    RejectedAsOrphaned,
    /// `DESERIALIZATION_FAILED`
    DeserializationFailed(&'static str),
    /// `CUMULATIVE_BLOCK_SIZE_TOO_BIG`, from either the `maxBlockCumulativeSize`
    /// cap of `Core::addBlock` or the `2 * median` ceiling inside
    /// `Currency::getBlockReward`.
    CumulativeBlockSizeTooBig { size: u64, limit: u64 },
    /// `WRONG_VERSION`
    WrongVersion { expected: u8, got: u8 },
    /// `PARENT_BLOCK_WRONG_VERSION`
    ParentBlockWrongVersion,
    /// `PARENT_BLOCK_SIZE_TOO_BIG`
    ParentBlockSizeTooBig,
    /// `TIMESTAMP_TOO_FAR_IN_FUTURE`
    TimestampTooFarInFuture { timestamp: u64, limit: u64 },
    /// `TIMESTAMP_TOO_FAR_IN_PAST`
    TimestampTooFarInPast { timestamp: u64, median: u64 },
    /// `INPUT_WRONG_COUNT` (coinbase)
    CoinbaseInputWrongCount(usize),
    /// `INPUT_UNEXPECTED_TYPE` (coinbase)
    CoinbaseInputUnexpectedType,
    /// `BASE_INPUT_WRONG_BLOCK_INDEX`
    BaseInputWrongBlockIndex { expected: u64, got: u64 },
    /// `WRONG_TRANSACTION_UNLOCK_TIME` (coinbase)
    CoinbaseWrongUnlockTime { expected: u64, got: u64 },
    /// `BASE_INVALID_SIGNATURES_COUNT`
    CoinbaseHasSignatures,
    /// `OUTPUT_ZERO_AMOUNT` (coinbase)
    CoinbaseOutputZeroAmount,
    /// `OUTPUT_INVALID_KEY` (coinbase)
    CoinbaseOutputInvalidKey,
    /// `OUTPUTS_AMOUNT_OVERFLOW` (coinbase)
    CoinbaseOutputsAmountOverflow,
    /// `DIFFICULTY_OVERHEAD` — `getDifficultyForNextBlock` returned 0, or the
    /// windows had no defined result (spec/07 "Difficulty", the two undefined
    /// inputs).
    DifficultyOverhead,
    /// `TRANSACTION_DUPLICATES` (from index 600,000)
    TransactionDuplicates,
    /// `TRANSACTION_INCONSISTENCY` (from index 600,000)
    TransactionInconsistency,
    /// `BLOCK_REWARD_MISMATCH`
    BlockRewardMismatch { expected: u64, got: u64 },
    /// `CHECKPOINT_BLOCK_HASH_MISMATCH`
    CheckpointBlockHashMismatch { expected: Hash, got: Hash },
    /// `PROOF_OF_WORK_TOO_WEAK`
    ProofOfWorkTooWeak { difficulty: u64 },
    /// A transaction of the block failed `ValidateTransaction`.
    Transaction { hash: Hash, index: usize, rule: TxRule },
    /// A reorganisation was required but the blocks to unwind are no longer
    /// available to keep as an alternative chain. Not a C++ code: the C++ keeps
    /// every raw block, we make that optional ([`Config::store_raw_blocks`]).
    ///
    /// The alternative block that triggered the switch is kept — it is valid,
    /// it is simply heavier than a chain this state cannot rewind — so the main
    /// chain is unchanged and adding the same block again reports
    /// [`Rule::AlreadyExists`]. A node that follows forks must run with
    /// `store_raw_blocks` on; an offline linear replay never reaches this.
    ReorganisationUnavailable { at_index: u32 },
    /// A reorganisation reached below the height from which this node keeps
    /// block bodies — a lite node's `--lite-height`, or a pruned node's
    /// retention window. Not a C++ code either; the C++ throws
    /// `"Cannot split below the lite node height"` from
    /// `DatabaseBlockchainCache::split` and the matching
    /// `"Cannot rewind below the lite node height"` from `rewind`
    /// (`DatabaseBlockchainCache.cpp:820`, `:917`).
    ///
    /// The main chain is left exactly as it was. This is refused rather than
    /// approximated: the branch may genuinely be the heavier chain, and a node
    /// that cannot rewind to the fork point cannot follow it without inventing
    /// state it never stored.
    ReorganisationBelowBodyFloor { at_index: u32, floor: u32 },
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rule::AlreadyExists => write!(f, "ALREADY_EXISTS"),
            Rule::RejectedAsOrphaned => write!(f, "REJECTED_AS_ORPHANED"),
            Rule::DeserializationFailed(w) => write!(f, "DESERIALIZATION_FAILED ({w})"),
            Rule::CumulativeBlockSizeTooBig { size, limit } => {
                write!(f, "CUMULATIVE_BLOCK_SIZE_TOO_BIG (size {size} > {limit})")
            }
            Rule::WrongVersion { expected, got } => write!(f, "WRONG_VERSION (expected {expected}, got {got})"),
            Rule::ParentBlockWrongVersion => write!(f, "PARENT_BLOCK_WRONG_VERSION"),
            Rule::ParentBlockSizeTooBig => write!(f, "PARENT_BLOCK_SIZE_TOO_BIG"),
            Rule::TimestampTooFarInFuture { timestamp, limit } => {
                write!(f, "TIMESTAMP_TOO_FAR_IN_FUTURE ({timestamp} > {limit})")
            }
            Rule::TimestampTooFarInPast { timestamp, median } => {
                write!(f, "TIMESTAMP_TOO_FAR_IN_PAST ({timestamp} < median {median})")
            }
            Rule::CoinbaseInputWrongCount(n) => write!(f, "INPUT_WRONG_COUNT (coinbase has {n} inputs)"),
            Rule::CoinbaseInputUnexpectedType => write!(f, "INPUT_UNEXPECTED_TYPE (coinbase input is not a BaseInput)"),
            Rule::BaseInputWrongBlockIndex { expected, got } => {
                write!(f, "BASE_INPUT_WRONG_BLOCK_INDEX (expected {expected}, got {got})")
            }
            Rule::CoinbaseWrongUnlockTime { expected, got } => {
                write!(f, "WRONG_TRANSACTION_UNLOCK_TIME (expected {expected}, got {got})")
            }
            Rule::CoinbaseHasSignatures => write!(f, "BASE_INVALID_SIGNATURES_COUNT"),
            Rule::CoinbaseOutputZeroAmount => write!(f, "OUTPUT_ZERO_AMOUNT (coinbase)"),
            Rule::CoinbaseOutputInvalidKey => write!(f, "OUTPUT_INVALID_KEY (coinbase)"),
            Rule::CoinbaseOutputsAmountOverflow => write!(f, "OUTPUTS_AMOUNT_OVERFLOW (coinbase)"),
            Rule::DifficultyOverhead => write!(f, "DIFFICULTY_OVERHEAD"),
            Rule::TransactionDuplicates => write!(f, "TRANSACTION_DUPLICATES"),
            Rule::TransactionInconsistency => write!(f, "TRANSACTION_INCONSISTENCY"),
            Rule::BlockRewardMismatch { expected, got } => {
                write!(f, "BLOCK_REWARD_MISMATCH (expected {expected}, got {got})")
            }
            Rule::CheckpointBlockHashMismatch { expected, got } => write!(
                f,
                "CHECKPOINT_BLOCK_HASH_MISMATCH (expected {}, got {})",
                hex::encode(expected),
                hex::encode(got)
            ),
            Rule::ProofOfWorkTooWeak { difficulty } => write!(f, "PROOF_OF_WORK_TOO_WEAK (difficulty {difficulty})"),
            Rule::Transaction { hash, index, rule } => {
                write!(f, "transaction {index} ({}): {rule}", hex::encode(hash))
            }
            Rule::ReorganisationUnavailable { at_index } => {
                write!(f, "cannot reorganise: no raw block stored for index {at_index}")
            }
            Rule::ReorganisationBelowBodyFloor { at_index, floor } => write!(
                f,
                "cannot reorganise to index {at_index}, below this node's full block data height                  {floor}. The data needed to undo those blocks was never stored."
            ),
        }
    }
}

/// Anything that can stop a block from being applied.
#[derive(Debug)]
pub enum ChainError {
    /// A consensus rule rejected the block.
    Rule(Rule),
    /// The state could not be read or written.
    Storage(wrkz_storage::StorageError),
    /// A record in our own state did not decode, or a block blob did not parse
    /// where the caller had already promised it would.
    Corrupt(String),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::Rule(r) => write!(f, "{r}"),
            ChainError::Storage(e) => write!(f, "storage: {e}"),
            ChainError::Corrupt(m) => write!(f, "corrupt state: {m}"),
        }
    }
}

impl std::error::Error for ChainError {}

impl From<Rule> for ChainError {
    fn from(r: Rule) -> Self {
        ChainError::Rule(r)
    }
}

impl From<wrkz_storage::StorageError> for ChainError {
    fn from(e: wrkz_storage::StorageError) -> Self {
        ChainError::Storage(e)
    }
}

impl ChainError {
    /// The rule that rejected the block, if the failure was a rule and not a
    /// storage or decode fault.
    pub fn rule(&self) -> Option<&Rule> {
        match self {
            ChainError::Rule(r) => Some(r),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, ChainError>;
