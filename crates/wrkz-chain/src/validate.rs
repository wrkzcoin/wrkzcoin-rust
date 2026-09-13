// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `ValidateTransaction` (`src/cryptonotecore/ValidateTransaction.cpp`;
//! spec/06 "Validation order").
//!
//! The checks run in the C++ order and stop at the first failure. Everything
//! that needs chain state goes through [`ChainAccess`], so the validator does
//! not care whether the state is the main chain, an alternative segment or a
//! test fixture.

use crate::checkpoints::Checkpoints;
use crate::fusion::is_fusion_transaction;
use crate::records::OutputRecord;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use wrkz_primitives::constants::*;
use wrkz_primitives::mixins::{validate_ring_sizes, MixinError};
use wrkz_primitives::tx::{relative_offsets_to_absolute, Input, Transaction};
use wrkz_primitives::Hash;

/// Every `TransactionValidationError` a block transaction can hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxRule {
    /// `SIZE_TOO_LARGE`
    SizeTooLarge { size: usize, limit: u64 },
    /// `EMPTY_INPUTS`
    EmptyInputs,
    /// `INPUT_UNKNOWN_TYPE` — a `BaseInput` outside a coinbase.
    InputUnknownType,
    /// `INPUT_IDENTICAL_KEYIMAGES` — two inputs of this transaction share one.
    InputIdenticalKeyImages,
    /// `INPUT_EMPTY_OUTPUT_USAGE`
    InputEmptyOutputUsage,
    /// `INPUT_INVALID_DOMAIN_KEYIMAGES` — `l·I != identity`.
    InputInvalidDomainKeyImages,
    /// `INPUT_IDENTICAL_OUTPUT_INDEXES` — a zero relative offset after the first.
    InputIdenticalOutputIndexes,
    /// `INPUT_KEYIMAGE_ALREADY_SPENT`, either against the block/pool validator
    /// state or against the chain at or below the validation height.
    InputKeyImageAlreadySpent { key_image: Hash },
    /// `INPUTS_AMOUNT_OVERFLOW`
    InputsAmountOverflow,
    /// `OUTPUT_ZERO_AMOUNT`
    OutputZeroAmount,
    /// `OUTPUT_AMOUNT_TOO_LARGE` (from index 800,000)
    OutputAmountTooLarge { amount: u64 },
    /// `OUTPUT_INVALID_KEY`
    OutputInvalidKey,
    /// `OUTPUTS_AMOUNT_OVERFLOW`
    OutputsAmountOverflow,
    /// `WRONG_AMOUNT` — outputs exceed inputs.
    WrongAmount,
    /// `WRONG_FEE`
    WrongFee { fee: u64, minimum: u64 },
    /// `EXTRA_TOO_LARGE`
    ExtraTooLarge { size: usize },
    /// `UNLOCK_TIME_TOO_SMALL`
    UnlockTimeTooSmall { unlock_time: u64, minimum: u64 },
    /// `EXCESSIVE_OUTPUTS`
    ExcessiveOutputs { count: usize },
    /// `INVALID_MIXIN`
    InvalidMixin(MixinError),
    /// `POW_INVALID`
    PowInvalid { difficulty: u64 },
    /// `INPUT_INVALID_GLOBAL_INDEX`
    InputInvalidGlobalIndex { amount: u64, global_index: u64 },
    /// `INPUT_SPEND_LOCKED_OUT`
    InputSpendLockedOut { amount: u64, global_index: u64, unlock_time: u64 },
    /// `INPUT_INVALID_SIGNATURES_COUNT` (pool always, blocks from 543,000)
    InputInvalidSignaturesCount { expected: usize, got: usize },
    /// `INPUT_INVALID_SIGNATURES`
    InputInvalidSignatures { input: usize },
    /// The C++ `validateTransactionFee` **throws** when the input sum is zero
    /// (`ValidateTransaction.cpp:394`), because it reads that as "the caller
    /// forgot to run the input checks". A transaction whose key inputs really do
    /// sum to zero therefore aborts validation rather than returning a code;
    /// either way the block is not accepted, and this names it.
    ZeroInputSum,
}

impl std::fmt::Display for TxRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxRule::SizeTooLarge { size, limit } => write!(f, "SIZE_TOO_LARGE ({size} > {limit})"),
            TxRule::EmptyInputs => write!(f, "EMPTY_INPUTS"),
            TxRule::InputUnknownType => write!(f, "INPUT_UNKNOWN_TYPE"),
            TxRule::InputIdenticalKeyImages => write!(f, "INPUT_IDENTICAL_KEYIMAGES"),
            TxRule::InputEmptyOutputUsage => write!(f, "INPUT_EMPTY_OUTPUT_USAGE"),
            TxRule::InputInvalidDomainKeyImages => write!(f, "INPUT_INVALID_DOMAIN_KEYIMAGES"),
            TxRule::InputIdenticalOutputIndexes => write!(f, "INPUT_IDENTICAL_OUTPUT_INDEXES"),
            TxRule::InputKeyImageAlreadySpent { key_image } => {
                write!(f, "INPUT_KEYIMAGE_ALREADY_SPENT ({})", hex::encode(key_image))
            }
            TxRule::InputsAmountOverflow => write!(f, "INPUTS_AMOUNT_OVERFLOW"),
            TxRule::OutputZeroAmount => write!(f, "OUTPUT_ZERO_AMOUNT"),
            TxRule::OutputAmountTooLarge { amount } => write!(f, "OUTPUT_AMOUNT_TOO_LARGE ({amount})"),
            TxRule::OutputInvalidKey => write!(f, "OUTPUT_INVALID_KEY"),
            TxRule::OutputsAmountOverflow => write!(f, "OUTPUTS_AMOUNT_OVERFLOW"),
            TxRule::WrongAmount => write!(f, "WRONG_AMOUNT"),
            TxRule::WrongFee { fee, minimum } => write!(f, "WRONG_FEE (fee {fee}, minimum {minimum})"),
            TxRule::ExtraTooLarge { size } => write!(f, "EXTRA_TOO_LARGE ({size})"),
            TxRule::UnlockTimeTooSmall { unlock_time, minimum } => {
                write!(f, "UNLOCK_TIME_TOO_SMALL ({unlock_time} < {minimum})")
            }
            TxRule::ExcessiveOutputs { count } => write!(f, "EXCESSIVE_OUTPUTS ({count})"),
            TxRule::InvalidMixin(e) => write!(f, "INVALID_MIXIN ({e:?})"),
            TxRule::PowInvalid { difficulty } => write!(f, "POW_INVALID (difficulty {difficulty})"),
            TxRule::InputInvalidGlobalIndex { amount, global_index } => {
                write!(f, "INPUT_INVALID_GLOBAL_INDEX (amount {amount}, index {global_index})")
            }
            TxRule::InputSpendLockedOut { amount, global_index, unlock_time } => {
                write!(f, "INPUT_SPEND_LOCKED_OUT (amount {amount}, index {global_index}, unlock {unlock_time})")
            }
            TxRule::InputInvalidSignaturesCount { expected, got } => {
                write!(f, "INPUT_INVALID_SIGNATURES_COUNT (expected {expected}, got {got})")
            }
            TxRule::InputInvalidSignatures { input } => write!(f, "INPUT_INVALID_SIGNATURES (input {input})"),
            TxRule::ZeroInputSum => write!(f, "input sum is zero (the C++ throws here)"),
        }
    }
}

/// Either a consensus rule rejected the transaction, or the chain state could
/// not be read at all.
///
/// The two must not be confused: the C++ `checkIfSpent` logs a database error
/// and returns `false`, which *accepts* the input. A replay that hits a read
/// error has to stop rather than record a wrong verdict, so a state fault
/// travels up as a fault.
#[derive(Debug)]
pub enum TxError {
    Rule(TxRule),
    Chain(crate::ChainError),
}

impl TxError {
    /// The rule, when the failure was a consensus decision.
    pub fn rule(&self) -> Option<&TxRule> {
        match self {
            TxError::Rule(r) => Some(r),
            TxError::Chain(_) => None,
        }
    }
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxError::Rule(r) => write!(f, "{r}"),
            TxError::Chain(e) => write!(f, "{e}"),
        }
    }
}

impl From<TxRule> for TxError {
    fn from(r: TxRule) -> Self {
        TxError::Rule(r)
    }
}

impl From<crate::ChainError> for TxError {
    fn from(e: crate::ChainError) -> Self {
        TxError::Chain(e)
    }
}

type TxResult<T> = std::result::Result<T, TxError>;

/// The result of a check that cannot read the chain, and therefore cannot fail
/// for any reason but a consensus rule.
type RuleResult<T> = std::result::Result<T, TxRule>;

/// The chain state a transaction validator reads.
///
/// The C++ equivalent is the `IBlockchainCache` the block is being added to,
/// plus the main chain's top timestamp that
/// `DatabaseBlockchainCache::isTransactionSpendTimeUnlocked` reaches for.
pub trait ChainAccess {
    /// `IBlockchainCache::checkIfSpent(keyImage, blockIndex)`: the key image is
    /// recorded as spent in a block at or below `block_index`.
    fn key_image_spent(&self, key_image: &Hash, block_index: u64) -> crate::Result<bool>;

    /// One key output by `(amount, global index)`, or `None` when no such
    /// output exists.
    fn key_output(&self, amount: u64, global_index: u64) -> crate::Result<Option<OutputRecord>>;

    /// A whole ring in one call. The default walks [`ChainAccess::key_output`];
    /// a store with a batched read overrides it, which is worth doing because
    /// this is on the hot path of every block outside the checkpoint zone.
    fn key_outputs(&self, amount: u64, global_indexes: &[u64]) -> crate::Result<Vec<Option<OutputRecord>>> {
        global_indexes.iter().map(|i| self.key_output(amount, *i)).collect()
    }

    /// The timestamp of the node's **current main chain tip**, which is what
    /// the unix-time branch of `isTransactionSpendTimeUnlocked` compares
    /// against — not the timestamp of the block being validated
    /// (`DatabaseBlockchainCache.cpp:1707`, `getLastTimestamps(1)` with no
    /// index).
    fn top_block_timestamp(&self) -> u64;

    /// The node's wall clock, used by the pre-600,000 branch of
    /// `isTransactionSpendTimeUnlocked`.
    fn now(&self) -> u64;
}

/// `isTransactionSpendTimeUnlocked(unlockTime, blockIndex)`
/// (`DatabaseBlockchainCache.cpp:1696`; spec/06 rule 10).
pub fn is_spend_time_unlocked<C: ChainAccess + ?Sized>(chain: &C, unlock_time: u64, block_index: u64) -> bool {
    if unlock_time < CRYPTONOTE_MAX_BLOCK_NUMBER {
        // Interpreted as a block index: `blockIndex + lockedTxAllowedDeltaBlocks
        // >= unlockTime`, and the delta is 1. A coinbase (unlock = index + 40)
        // is therefore spendable from block `index + 40` onward.
        return block_index + CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS >= unlock_time;
    }
    if block_index >= TRANSACTION_INPUT_BLOCKTIME_VALIDATION_HEIGHT {
        return chain.top_block_timestamp() + CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS >= unlock_time;
    }
    chain.now() + CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS >= unlock_time
}

/// The `TransactionValidatorState` of the block (or pool) being built: the key
/// images already claimed by the transactions validated so far. Two
/// transactions in one block that spend the same output are caught here.
#[derive(Clone, Debug, Default)]
pub struct ValidatorState {
    pub spent_key_images: HashSet<Hash>,
}

impl ValidatorState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The height-dependent context `ValidateTransaction` is constructed with.
#[derive(Clone, Copy, Debug)]
pub struct TxContext<'a> {
    /// `blockHeight`: the **previous** block's index inside a block
    /// (`Core.cpp:1586`), the top index at pool admission. Every "from height
    /// H" rule therefore fires for transactions in block `H + 1`.
    pub block_height: u64,
    /// `Core::blockMedianSize`: `max(median of the last 100 main-chain block
    /// sizes, granted full reward zone for the next block's version)`
    /// (`Core.cpp:5097`). A single node-wide value; alternative chains are
    /// validated against the main chain's median, which is the existing
    /// behaviour.
    pub block_median_size: u64,
    /// The timestamp of the block being added, or of the top block at pool
    /// admission.
    pub block_timestamp: u64,
    pub is_pool_transaction: bool,
    pub checkpoints: &'a Checkpoints,
}

/// What a successful validation reports back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxValidation {
    pub fee: u64,
    pub is_fusion: bool,
}

/// `ValidateTransaction::validate()` (`ValidateTransaction.cpp:48`).
///
/// `blob` is the serialized transaction as it arrived: its length is what the
/// size, fee and fusion rules measure, so it must be the bytes the block
/// carried and not a re-serialization.
pub fn validate_transaction<C: ChainAccess + ?Sized>(
    tx: &Transaction,
    blob: &[u8],
    state: &mut ValidatorState,
    chain: &C,
    ctx: &TxContext<'_>,
) -> TxResult<TxValidation> {
    validate_transaction_with_threads(tx, blob, state, chain, ctx, wrkz_pow::parallel::default_threads())
}

/// [`validate_transaction`] with the ring-signature thread count named.
///
/// The only thing `threads` changes is how many cores step 10 spreads its ring
/// signature verification over; `1` is the sequential loop. It cannot change a
/// verdict or which rule is reported — see
/// [`wrkz_pow::parallel::first_invalid_ring`] for why — so it is a performance
/// knob and nothing else. [`crate::Config::validate_threads`] is what
/// [`crate::ChainState`] passes here.
///
/// This is the **one-transaction** entry point, which is what the mempool
/// wants: it opens a [`BlockRingBatch`] of its own, runs the transaction into
/// it and settles it before returning. A block validator wants
/// [`validate_transaction_deferred`] instead, so that every transaction of the
/// block shares one batch and one parallel pass.
pub fn validate_transaction_with_threads<C: ChainAccess + ?Sized>(
    tx: &Transaction,
    blob: &[u8],
    state: &mut ValidatorState,
    chain: &C,
    ctx: &TxContext<'_>,
    threads: usize,
) -> TxResult<TxValidation> {
    let mut batch = BlockRingBatch::new();
    // Transaction index 0: there is only one transaction here, so every pair
    // the batch can resolve to is `(0, input)` and the input is what is
    // reported — exactly what this function reported before batches existed.
    let outcome = validate_transaction_deferred(tx, blob, state, chain, ctx, 0, &mut batch);
    // The batch first: every check in it sits at or below whatever `outcome`
    // stopped at, and at the same position the batched check comes first. That
    // is the same argument the block validator uses, on a batch of one
    // transaction — see [`BlockRingBatch`].
    if let Some((_, rule)) = batch.first_invalid(threads) {
        return Err(rule.into());
    }
    outcome
}

/// `ValidateTransaction::validate()` with its pure, expensive checks
/// **deferred** into `batch` rather than run here: the key-image domain check,
/// the output `check_key`, a block transaction's proof of work and the ring
/// signatures.
///
/// Every other check — including every stateful one: the key-image set in
/// `state`, the per-transaction ordering, the fee this returns — runs exactly
/// where and when [`validate_transaction`] runs it. Only pure functions of the
/// transaction's own bytes are postponed, and only because they are what is
/// worth spreading over the cores.
///
/// `tx_index` is the transaction's index **within its block**; it is carried
/// through to the batch so that a failure can be reported against the right
/// transaction. The mempool, which validates one transaction at a time, passes
/// 0.
///
/// # The caller's contract
///
/// A caller that batches a whole block must do all four of these, and
/// [`BlockRingBatch`] documents why each one is load-bearing:
///
/// 1. call this for the block's transactions **in index order**, passing the
///    same `batch` and the transaction's own index;
/// 2. **stop** at the first transaction that returns `Err` — the C++ stops the
///    block there — and hold that error back rather than returning it;
/// 3. ask [`BlockRingBatch::first_invalid`] for the lowest failing
///    `(transaction, input)` pair and report
///    [`TxRule::InputInvalidSignatures`] against that transaction if there is
///    one;
/// 4. only if there is none, return the held-back error.
///
/// [`ChainState::add_block`](crate::ChainState::add_block) is the
/// implementation of that contract.
pub fn validate_transaction_deferred<'a, C: ChainAccess + ?Sized>(
    tx: &'a Transaction,
    blob: &[u8],
    state: &mut ValidatorState,
    chain: &C,
    ctx: &TxContext<'_>,
    tx_index: usize,
    batch: &mut BlockRingBatch<'a>,
) -> TxResult<TxValidation> {
    // 1. size (line 183)
    let max_size = ctx.block_median_size * 2 - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64;
    if blob.len() as u64 > max_size {
        return Err(TxRule::SizeTooLarge { size: blob.len(), limit: max_size }.into());
    }

    // 2. inputs (line 200)
    let sum_of_inputs = validate_inputs(tx, state, tx_index, Some(batch))?;

    // 3. outputs (line 318)
    let sum_of_outputs = validate_outputs(tx, ctx.block_height, tx_index, Some(batch))?;

    // 4. fee (line 391)
    let (fee, is_fusion) = validate_fee(tx, blob, sum_of_inputs, sum_of_outputs, ctx.block_height)?;

    // 5. extra (line 477)
    validate_extra(tx, ctx.block_height, ctx.is_pool_transaction)?;

    // 6. unlock time (line 812)
    validate_unlock_time(tx, ctx.block_height, ctx.block_timestamp)?;

    // 7. output count (line 500)
    validate_input_output_ratio(tx, ctx)?;

    // 8. mixin (line 518)
    validate_ring_sizes(&tx.prefix.ring_sizes(), ctx.block_height).map_err(TxRule::InvalidMixin)?;

    // Steps 9 and 10 both work on the serialized transaction prefix: the
    // transaction proof of work hashes it, the ring signatures sign its hash.
    // Inside the checkpoint zone neither runs, and serializing every prefix
    // there would be the largest single cost of replaying the 4.2 million
    // blocks the zone covers, so it is only built when something will read it.
    let in_checkpoint_zone = ctx.checkpoints.is_in_checkpoint_zone(ctx.block_height + 1);
    // `validateTransactionPoW` (line 578) skips inside the zone for block
    // transactions only; pool admission always pays for the hash.
    let pow_applies = ctx.block_height >= TRANSACTION_POW_HEIGHT && (ctx.is_pool_transaction || !in_checkpoint_zone);
    // `validateTransactionInputsExpensive` (line 657) skips inside the zone for
    // pool transactions too: there is no `m_isPoolTransaction` term there.
    let expensive_applies = !in_checkpoint_zone;
    let prefix_bytes = (pow_applies || expensive_applies).then(|| tx.prefix.to_bytes());

    // 9. transaction proof of work (line 561). A pool transaction pays for it
    //    here and now: deferring one transaction's hash gains nothing, and
    //    checking it first keeps a flood of transactions with no work in them
    //    as cheap to refuse as it has always been. A block transaction's hash
    //    joins the block's batch instead.
    if pow_applies {
        let prefix = prefix_bytes.as_deref().expect("built above");
        if ctx.is_pool_transaction {
            validate_transaction_pow(tx, prefix, fee, is_fusion, ctx.block_height)?;
        } else {
            gather_transaction_pow(tx, prefix, fee, is_fusion, ctx.block_height, tx_index, batch);
        }
    }

    // 10. the expensive input checks (line 654), minus the verification
    //     itself, which lands in `batch`.
    if expensive_applies {
        gather_inputs_expensive(tx, prefix_bytes.as_deref().expect("built above"), chain, ctx, tx_index, batch)?;
    }

    Ok(TxValidation { fee, is_fusion })
}

/// `ValidateTransaction::validate()` with every check run **inline, in the C++
/// order, stopping at the first failure**: the entry point for one transaction
/// offered to the pool.
///
/// [`validate_transaction`] is built for blocks. It reads the key image and the
/// ring members of *every* input before it verifies a single signature, and
/// then settles all of them as one batch across the cores. For a block that is
/// right — the block is almost always valid, and the batch is what makes it
/// fast. For a transaction a peer pushed at the pool it is the wrong cost
/// model: the transaction is the adversary's choice, a bad signature on input
/// 0 still costs every ring read, and the key-image domain checks wait until
/// after the transaction proof of work. This is the loop the C++ runs
/// (`ValidateTransaction.cpp:670`, where the jobs check a shared cancel flag):
/// input `i`'s key image is looked up, its ring read and its signature verified
/// before input `i + 1` is touched, and the first failure returns. The domain
/// check (step 2) and the output `check_key` (step 3) run where the C++ runs
/// them, before the proof of work.
///
/// # Same verdict, same rule
///
/// Every check is the *same function* [`validate_transaction_deferred`] calls,
/// in the same order; only when the pure ones run differs. The batched path is
/// argued on [`BlockRingBatch`] to report what the sequential C++ validator
/// reports, and this *is* the sequential validator, so both paths accept the
/// same transactions and name the same rule, transaction and input when they
/// reject. `early_exit_and_batch_paths_agree` pins that over valid and invalid
/// transactions of every shape the rules distinguish.
///
/// The one thing that differs on the accept path is nothing: a transaction
/// that passes leaves exactly its key images in `state`, as it does there.
/// On a reject `state` may hold fewer images than the batched path leaves
/// (it stops sooner); both callers throw a rejected transaction's state away.
///
/// The transaction proof of work is still paid up front, as a pool
/// transaction always has.
pub fn validate_transaction_early_exit<C: ChainAccess + ?Sized>(
    tx: &Transaction,
    blob: &[u8],
    state: &mut ValidatorState,
    chain: &C,
    ctx: &TxContext<'_>,
) -> TxResult<TxValidation> {
    // 1. size (line 183)
    let max_size = ctx.block_median_size * 2 - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64;
    if blob.len() as u64 > max_size {
        return Err(TxRule::SizeTooLarge { size: blob.len(), limit: max_size }.into());
    }

    // 2. inputs (line 200), the key-image domain check inline.
    let sum_of_inputs = validate_inputs(tx, state, 0, None)?;

    // 3. outputs (line 318), `check_key` inline.
    let sum_of_outputs = validate_outputs(tx, ctx.block_height, 0, None)?;

    // 4. fee (line 391)
    let (fee, is_fusion) = validate_fee(tx, blob, sum_of_inputs, sum_of_outputs, ctx.block_height)?;

    // 5. extra (line 477)
    validate_extra(tx, ctx.block_height, ctx.is_pool_transaction)?;

    // 6. unlock time (line 812)
    validate_unlock_time(tx, ctx.block_height, ctx.block_timestamp)?;

    // 7. output count (line 500)
    validate_input_output_ratio(tx, ctx)?;

    // 8. mixin (line 518)
    validate_ring_sizes(&tx.prefix.ring_sizes(), ctx.block_height).map_err(TxRule::InvalidMixin)?;

    // The same two zone decisions as `validate_transaction_deferred`.
    let in_checkpoint_zone = ctx.checkpoints.is_in_checkpoint_zone(ctx.block_height + 1);
    let pow_applies = ctx.block_height >= TRANSACTION_POW_HEIGHT && (ctx.is_pool_transaction || !in_checkpoint_zone);
    let expensive_applies = !in_checkpoint_zone;
    let prefix_bytes = (pow_applies || expensive_applies).then(|| tx.prefix.to_bytes());

    // 9. transaction proof of work (line 561), on the spot.
    if pow_applies {
        validate_transaction_pow(tx, prefix_bytes.as_deref().expect("built above"), fee, is_fusion, ctx.block_height)?;
    }

    // 10. the expensive input checks (line 654), one input at a time: the
    //     chain reads of `gather_ring`, then that input's ring signature, then
    //     the next input.
    if expensive_applies {
        let prefix_hash = wrkz_pow::cn_fast_hash(prefix_bytes.as_deref().expect("built above"));
        for (input_index, input) in tx.prefix.inputs.iter().enumerate() {
            let (image, ring, signatures) = gather_ring(tx, input, input_index, chain, ctx)?;
            if !wrkz_pow::curve::check_ring_signature(&prefix_hash, image, &ring, signatures) {
                return Err(TxRule::InputInvalidSignatures { input: input_index }.into());
            }
        }
    }

    Ok(TxValidation { fee, is_fusion })
}

/// The height-dependent inputs of [`revalidate_after_height_change`], as
/// `Core::validateBlockTemplateTransaction` fills them in (`Core.cpp:4330`).
///
/// Deliberately smaller than [`TxContext`]: the C++ hands the revalidating
/// validator a null cache and a throwaway `TransactionValidatorState`, and
/// `isPoolTransaction` is always `true` there, so there is nothing else to
/// pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevalidateContext {
    /// `blockHeight`. `fillBlockTemplate` passes the **template's** height
    /// (`top + 1`, `Core.cpp:4385`), unlike pool admission which passes
    /// `getTopBlockIndex()` (`Core.cpp:2238`).
    pub block_height: u64,
    /// `Core::blockMedianSize`.
    pub block_median_size: u64,
    /// `chainsLeaves[0]->getLastTimestamps(1)[0]`: the top block's timestamp.
    pub block_timestamp: u64,
}

/// `ValidateTransaction::revalidateAfterHeightChange()`
/// (`ValidateTransaction.cpp:120`).
///
/// The cheap re-check a *pooled* transaction gets when the height moves under
/// it: `Core::fillBlockTemplate` runs it on every candidate at the template's
/// height, and the pool cleaner runs the same rules whenever the tip advances.
///
/// It is deliberately **not** the full validator: there is no chain access, so
/// no ring resolution, no signature check and no key-image lookup — the C++
/// hands it a `nullptr` cache and a throwaway `TransactionValidatorState`
/// (`Core.cpp:4333-4338`). Seven checks in this order — size, extra, mixin,
/// inputs, outputs, unlock time, fee — and the transaction proof of work only
/// inside a 100-block window above `TRANSACTION_POW_HEIGHT`, whose point is to
/// flush transactions admitted before that rule existed rather than to re-hash
/// the whole pool on every block forever.
///
/// Every rule is the *same function* [`validate_transaction`] calls; only the
/// subset and the order differ, which is exactly the difference the C++ has
/// between its two entry points.
///
/// `is_pool_transaction` is `true` throughout, so the extra cap applies at
/// every height and the checkpoint zone never skips the proof of work.
pub fn revalidate_after_height_change(
    tx: &Transaction,
    blob: &[u8],
    ctx: &RevalidateContext,
) -> RuleResult<TxValidation> {
    // 1. size (line 183)
    let max_size = ctx.block_median_size * 2 - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64;
    if blob.len() as u64 > max_size {
        return Err(TxRule::SizeTooLarge { size: blob.len(), limit: max_size });
    }

    // 2. extra (line 477)
    validate_extra(tx, ctx.block_height, true)?;

    // 3. mixin (line 518)
    validate_ring_sizes(&tx.prefix.ring_sizes(), ctx.block_height).map_err(TxRule::InvalidMixin)?;

    // 4. inputs (line 200), against the throwaway state, so the only key-image
    // collisions it can see are inside this one transaction.
    let sum_of_inputs = validate_inputs(tx, &mut ValidatorState::new(), 0, None)?;

    // 5. outputs (line 318)
    let sum_of_outputs = validate_outputs(tx, ctx.block_height, 0, None)?;

    // 6. unlock time (line 812)
    validate_unlock_time(tx, ctx.block_height, ctx.block_timestamp)?;

    // 7. fee (line 391)
    let (fee, is_fusion) = validate_fee(tx, blob, sum_of_inputs, sum_of_outputs, ctx.block_height)?;

    // 8. the transaction proof of work, in the 100-block window above the fork
    // that introduced it (`ValidateTransaction.cpp:165`).
    if (TRANSACTION_POW_HEIGHT..=TRANSACTION_POW_HEIGHT + 100).contains(&ctx.block_height) {
        validate_transaction_pow(tx, &tx.prefix.to_bytes(), fee, is_fusion, ctx.block_height)?;
    }

    Ok(TxValidation { fee, is_fusion })
}

/// `validateTransactionInputs` (line 200). Returns the input sum.
///
/// `deferred` is the block's batch when there is one. The key-image domain
/// check — `l·I == identity`, one full ed25519 scalar multiplication — is then
/// **appended to it instead of run here**; `None` runs it inline, exactly as
/// this did before batches existed. Nothing else moves: the per-transaction
/// key-image set, the ordering, the running sum and every other rule stay on
/// this thread and in this order. See [`BlockRingBatch`] for why deferring it
/// cannot change which rule is reported.
fn validate_inputs<'a>(
    tx: &'a Transaction,
    state: &mut ValidatorState,
    tx_index: usize,
    mut deferred: Option<&mut BlockRingBatch<'a>>,
) -> RuleResult<u64> {
    if tx.prefix.inputs.is_empty() {
        return Err(TxRule::EmptyInputs);
    }
    let mut in_this_tx: HashSet<Hash> = HashSet::with_capacity(tx.prefix.inputs.len());
    let mut sum: u64 = 0;
    for (input_index, input) in tx.prefix.inputs.iter().enumerate() {
        // A `BaseInput` here is `INPUT_UNKNOWN_TYPE`: coinbase transactions are
        // validated by `Core::validateBlock` and never reach this validator.
        let Input::Key { amount, key_offsets, key_image } = input else {
            return Err(TxRule::InputUnknownType);
        };
        if !in_this_tx.insert(*key_image) {
            return Err(TxRule::InputIdenticalKeyImages);
        }
        if key_offsets.is_empty() {
            return Err(TxRule::InputEmptyOutputUsage);
        }
        // `l·I == identity`: the Monero Lab fix, without which a key image
        // outside the prime-order subgroup lets one output be spent several
        // times (`ValidateTransaction.cpp:260`). It is part of
        // `validateTransactionInputs`, **not** of
        // `validateTransactionInputsExpensive`, so it runs inside the
        // checkpoint zone as well: on a linear import it is the single largest
        // cost of every block that carries a spend.
        match deferred.as_deref_mut() {
            Some(batch) => batch.push(PureCheck::KeyImageDomain { tx_index, input_index, image: key_image }),
            None if !wrkz_pow::curve::key_image_in_prime_subgroup(key_image) => {
                return Err(TxRule::InputInvalidDomainKeyImages)
            }
            None => {}
        }
        // Offsets are relative, so only the first may be zero: a later zero
        // would name the previous ring member again.
        if key_offsets[1..].contains(&0) {
            return Err(TxRule::InputIdenticalOutputIndexes);
        }
        if !state.spent_key_images.insert(*key_image) {
            return Err(TxRule::InputKeyImageAlreadySpent { key_image: *key_image });
        }
        sum = sum.checked_add(*amount).ok_or(TxRule::InputsAmountOverflow)?;
    }
    Ok(sum)
}

/// `validateTransactionOutputs` (line 318). Returns the output sum.
///
/// `deferred` carries the per-output `check_key` — an ed25519 point
/// decompression — into the block's batch the same way [`validate_inputs`]
/// carries the domain check; `None` runs it inline.
fn validate_outputs<'a>(
    tx: &'a Transaction,
    block_height: u64,
    tx_index: usize,
    mut deferred: Option<&mut BlockRingBatch<'a>>,
) -> RuleResult<u64> {
    let mut sum: u64 = 0;
    for (output_index, output) in tx.prefix.outputs.iter().enumerate() {
        if output.amount == 0 {
            return Err(TxRule::OutputZeroAmount);
        }
        if block_height >= MAX_OUTPUT_SIZE_HEIGHT && output.amount > MAX_OUTPUT_SIZE_NODE {
            return Err(TxRule::OutputAmountTooLarge { amount: output.amount });
        }
        // Every output of a parsed transaction is a `KeyOutput`
        // (`OUTPUT_UNKNOWN_TYPE` is unreachable after deserialization, which
        // rejects any other tag), so only the key check remains.
        match deferred.as_deref_mut() {
            Some(batch) => batch.push(PureCheck::OutputKey { tx_index, output_index, key: &output.key }),
            None if !wrkz_pow::curve::check_key(&output.key) => return Err(TxRule::OutputInvalidKey),
            None => {}
        }
        sum = sum.checked_add(output.amount).ok_or(TxRule::OutputsAmountOverflow)?;
    }
    Ok(sum)
}

/// `validateTransactionFee` (line 391). Returns `(fee, is_fusion)`.
fn validate_fee(
    tx: &Transaction,
    blob: &[u8],
    sum_of_inputs: u64,
    sum_of_outputs: u64,
    block_height: u64,
) -> RuleResult<(u64, bool)> {
    // `ValidateTransaction.cpp:394` throws on a zero input sum; see TxRule::ZeroInputSum.
    if sum_of_inputs == 0 {
        return Err(TxRule::ZeroInputSum);
    }
    if sum_of_outputs > sum_of_inputs {
        return Err(TxRule::WrongAmount);
    }
    let fee = sum_of_inputs - sum_of_outputs;
    let is_fusion = is_fusion_transaction(&tx.prefix, blob.len(), block_height);
    if is_fusion {
        if !wrkz_primitives::fees::is_valid_fusion_fee(fee, block_height) {
            return Err(TxRule::WrongFee { fee, minimum: FUSION_FEE_V1 });
        }
    } else {
        // The C++ assigns `validFee = fee != 0` and then *overwrites* it in the
        // height ladder, so the non-zero test only survives where no ladder arm
        // fires — which is never, the last arm covers everything at or below
        // 678,501. Every minimum is at least 5, so requiring both is the same
        // rule and says what it means.
        let minimum = wrkz_primitives::fees::required_minimum_fee(blob.len(), block_height);
        if !wrkz_primitives::fees::is_valid_normal_fee(fee, blob.len(), block_height) {
            return Err(TxRule::WrongFee { fee, minimum });
        }
    }
    Ok((fee, is_fusion))
}

/// `validateTransactionExtra` (line 477): pool transactions always, block
/// transactions from `MAX_EXTRA_SIZE_V2_HEIGHT + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW`
/// = 543,040. Note the comparison is `>=`, so 1023 bytes is the maximum.
fn validate_extra(tx: &Transaction, block_height: u64, is_pool_transaction: bool) -> RuleResult<()> {
    let enforce_from = MAX_EXTRA_SIZE_V2_HEIGHT + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW;
    if (is_pool_transaction || block_height >= enforce_from) && tx.prefix.extra.len() >= MAX_EXTRA_SIZE_V2 {
        return Err(TxRule::ExtraTooLarge { size: tx.prefix.extra.len() });
    }
    Ok(())
}

/// `validateTransactionUnlockTime` (line 812), from `H > UNLOCK_TIME_HEIGHT`.
fn validate_unlock_time(tx: &Transaction, block_height: u64, block_timestamp: u64) -> RuleResult<()> {
    if block_height <= UNLOCK_TIME_HEIGHT {
        return Ok(());
    }
    let unlock_time = tx.prefix.unlock_time;
    let minimum = if unlock_time > CRYPTONOTE_MAX_BLOCK_NUMBER {
        // A unix time: at least 15 block times past the block's timestamp.
        block_timestamp.saturating_add(MINIMUM_UNLOCK_TIME_BLOCKS * DIFFICULTY_TARGET)
    } else {
        block_height + MINIMUM_UNLOCK_TIME_BLOCKS
    };
    if unlock_time < minimum {
        return Err(TxRule::UnlockTimeTooSmall { unlock_time, minimum });
    }
    Ok(())
}

/// `validateInputOutputRatio` (line 500): pool always, blocks from 777,777.
fn validate_input_output_ratio(tx: &Transaction, ctx: &TxContext<'_>) -> RuleResult<()> {
    if (ctx.is_pool_transaction || ctx.block_height >= NORMAL_TX_MAX_OUTPUT_COUNT_V1_HEIGHT)
        && tx.prefix.outputs.len() > NORMAL_TX_MAX_OUTPUT_COUNT_V1
    {
        return Err(TxRule::ExcessiveOutputs { count: tx.prefix.outputs.len() });
    }
    Ok(())
}

/// `validateTransactionPoW` (line 561), from `TRANSACTION_POW_HEIGHT`.
///
/// The caller decides whether it applies: skipped for **block** transactions
/// inside the checkpoint zone (the prefix is committed by the transaction hash,
/// which is committed by the checkpointed block hash) and never skipped for the
/// pool.
fn validate_transaction_pow(
    tx: &Transaction,
    prefix_bytes: &[u8],
    fee: u64,
    is_fusion: bool,
    block_height: u64,
) -> RuleResult<()> {
    let difficulty = transaction_pow_difficulty(
        block_height,
        is_fusion,
        tx.prefix.inputs.len() as u64,
        tx.prefix.outputs.len() as u64,
    )
    .expect("the caller only applies this at or above TRANSACTION_POW_HEIGHT");
    // The hash is over the serialized *prefix*, which includes `extra`; the
    // wallet puts the 8-byte nonce last, but the validator hashes whatever
    // prefix it was given (`TransactionPoW.h:58`).
    let hash = wrkz_pow::cn_upx(prefix_bytes);
    if wrkz_pow::check_hash(&hash, difficulty) {
        return Ok(());
    }
    // From 1,500,000 a large enough fee stands in for the work — but not for a
    // fusion transaction, which pays no fee.
    if block_height >= TRANSACTION_POW_PASS_WITH_FEE_HEIGHT && !is_fusion && fee >= TRANSACTION_POW_PASS_WITH_FEE {
        return Ok(());
    }
    Err(TxRule::PowInvalid { difficulty })
}

/// Every pure per-input and per-output check a block's transactions deferred,
/// in the order the C++ validator reaches them, waiting for one parallel pass.
///
/// # What is in it, and what it costs
///
/// Four checks, all pure functions of bytes the block already carries:
///
/// | check | where the C++ runs it | skipped in the checkpoint zone? | cost |
/// | --- | --- | --- | --- |
/// | `l·I == identity` per key input | `validateTransactionInputs` (line 260) | **no** | one scalar multiplication |
/// | `check_key` per output | `validateTransactionOutputs` (line 330) | **no** | one point decompression |
/// | proof of work per transaction | `validateTransactionPoW` (line 561) | yes | one `cn_upx`, about 850 µs |
/// | ring signature per key input | `validateTransactionInputsExpensive` | yes | two scalar multiplications per ring member |
///
/// The proof of work is batched for **block** transactions only; pool
/// admission checks it on the spot (`validate_transaction_deferred`, step 9).
///
/// The first two are the ones that matter to a linear import. Only
/// `validateTransactionInputsExpensive` and `validateTransactionPoW` are
/// skipped below the last checkpoint; the key-image domain check is not, so
/// every key input of every block down there costs a full ed25519 scalar
/// multiplication whatever the checkpoints say. On the reference curve code
/// this port calls that is on the order of 150 µs, against a whole empty block
/// at a few tens of microseconds — so from the height where blocks start
/// carrying spends it is the import's dominant cost, and it grows with
/// transaction volume rather than with height.
///
/// # Why the batch is block-wide and not transaction-wide
///
/// [`first_invalid_ring`](wrkz_pow::parallel::first_invalid_ring) falls back to
/// a sequential loop below
/// [`PARALLEL_THRESHOLD`](wrkz_pow::parallel::PARALLEL_THRESHOLD) rings,
/// because handing four rings to eight threads costs more than it saves. The
/// common transaction on this chain has **two** key inputs, so a batch that
/// stopped at the transaction boundary would leave essentially the whole chain
/// on the sequential path: one measured stretch of 1,001 blocks carries 54,894
/// transactions of two rings each — 109,788 ring signatures — and not one of
/// them was ever verified on a second core. Gathering the whole block instead
/// turns that stretch into 1,001 batches of about 110 rings, every one of them
/// far above the threshold, and costs a block that really does hold a single
/// small transaction nothing: it still falls back.
///
/// # The order the batch is built in
///
/// The block validator walks its transactions in index order, and inside one
/// transaction the C++ runs `validateTransactionInputs` to the end before
/// `validateTransactionOutputs`, both before `validateTransactionPoW`, and all
/// three before `validateTransactionInputsExpensive`. Each of those walks its
/// inputs or outputs in index order. So appending at the four call sites — the
/// domain check in `validate_inputs`, the key check in `validate_outputs`, the
/// proof of work in `gather_transaction_pow`, the ring in
/// `gather_inputs_expensive` — fills the vector in exactly the order the C++
/// validator reaches the checks: batch position is a **strictly increasing**
/// function of `PureCheck::position`, which is `(transaction, phase, item)`
/// lexicographically. `BlockRingBatch::push` asserts that in a debug build.
///
/// # Determinism, in two dimensions
///
/// `first_invalid_check` is documented to return the **lowest failing batch
/// position**, on every run and at every thread count. That is the
/// one-dimensional half of the argument. The other half is the map above: a
/// strictly increasing map carries "lowest position" back to "lowest visit
/// order", so [`BlockRingBatch::first_invalid`] names the check the sequential
/// C++ validator would have stopped at, whatever the thread count and whatever
/// order the workers ran in.
///
/// That settles which *batched* failure is named. What settles it against the
/// other rules is where gathering stops. Call the held-back failure's position
/// `P`. Every check in the batch is at a position **at or below** `P`, and at
/// `P` itself the batched check is the one the C++ reaches first:
///
/// - a rule that precedes the domain check on input `i` — unknown type,
///   identical key images, empty output usage — stops the loop before input
///   `i`'s image is appended, so the batch holds only inputs below `i`;
/// - a rule that follows it on the same input — identical output indexes, an
///   already-spent key image, the input sum overflow — runs after the append,
///   so input `i`'s image **is** in the batch, and the C++ would have run the
///   domain check first;
/// - the same two cases hold for `check_key` against the zero-amount and
///   amount-too-large rules of the same output;
/// - the fee, the extra, the unlock time, the ratio and the mixin (steps 4-8)
///   run after `t`'s domain and key checks and before its proof of work is
///   gathered, so a failure there leaves the proof of work out of the batch;
/// - a rule of step 10 on input `i` — a key image already spent, an unknown or
///   locked ring member, the signature count, a chain read fault — comes after
///   `t`'s proof of work, which is therefore in the batch and which the C++
///   would have run first, and after the rings of `t`'s inputs below `i`;
/// - anything later still — a later transaction, or the block's own reward and
///   checkpoint steps — is above every check transaction `t` contributed.
///
/// So if any batched check fails, the C++ validator would have reached it
/// before it reached `P` and would have reported that check's rule — which is
/// what the caller reports. And if none fails, the C++ reaches `P` and reports
/// the held-back rule or fault — which is what the caller reports then.
/// Nothing above `P` is ever in the batch, so no later transaction's bad
/// signature can jump in front of an earlier transaction's chain fault, and no
/// later chain fault can hide an earlier transaction's bad key image.
///
/// The verdict itself — accept or reject — cannot move either way: both
/// branches reject. The whole argument above is about *which* rule, *which*
/// transaction and *which* input the rejection names.
///
/// # What deferring does to the state the loop builds
///
/// A deferred domain check means `validate_inputs` no longer returns at a bad
/// key image, so it goes on to insert that image into
/// [`ValidatorState::spent_key_images`] and to process the inputs above it.
/// That state is only ever read by `push_block`, on the accept path, which such
/// a block never reaches: the batched failure is returned before it. The extra
/// chain reads the later inputs may do are held back and then overridden by the
/// batched failure, exactly as the third bullet above requires.
///
/// # Splitting the batch changes nothing
///
/// A caller may settle the batch early and start a fresh one (see
/// [`RING_BATCH_MEMBER_LIMIT`]). Settling a prefix and stopping on its lowest
/// failure gives the lowest failure overall, because every check not yet
/// gathered is above every check in the prefix; and a prefix that verifies
/// cleanly has nothing left to report, so dropping it loses nothing. The same
/// reasoning is what lets the batch be dropped altogether once a failure is
/// found.
#[derive(Default)]
pub struct BlockRingBatch<'a> {
    /// Appended in the order the C++ validator visits the checks, and never
    /// reordered — the whole determinism argument rests on that.
    checks: Vec<PureCheck<'a>>,
    members: usize,
}

/// One check the block validator postponed: a pure function of bytes it
/// already holds, carrying the position it was reached at.
///
/// The three of them are the only per-input and per-output work in the
/// validator that is worth a core, and each costs, on the reference curve code
/// this port calls, roughly 150 µs, 12 µs and 300 µs per ring member
/// respectively. The first two run **inside the checkpoint zone**, which is
/// what makes a linear import of the first four million blocks pay for them.
enum PureCheck<'a> {
    /// `l·I == identity` for one key input (`ValidateTransaction.cpp:260`), a
    /// full ed25519 scalar multiplication. Reported as
    /// [`TxRule::InputInvalidDomainKeyImages`].
    KeyImageDomain { tx_index: usize, input_index: usize, image: &'a Hash },
    /// `check_key` on one transaction output (`ValidateTransaction.cpp:330`),
    /// an ed25519 point decompression. Reported as
    /// [`TxRule::OutputInvalidKey`].
    OutputKey { tx_index: usize, output_index: usize, key: &'a Hash },
    /// A block transaction's proof of work (`ValidateTransaction.cpp:561`):
    /// one `cn_upx` of the serialized prefix, as much work as a ring of three.
    /// The fee escape has already been ruled out where it was gathered.
    /// Reported as [`TxRule::PowInvalid`].
    TxPow { tx_index: usize, prefix: Vec<u8>, difficulty: u64 },
    /// One input's ring signature. Reported as
    /// [`TxRule::InputInvalidSignatures`].
    Ring(GatheredRing<'a>),
}

impl PureCheck<'_> {
    /// Roughly what this check costs, in tenths of an ed25519 scalar
    /// multiplication, so that [`first_invalid_check`] can decide whether a
    /// batch is worth a thread at all.
    ///
    /// Measured on the reference curve code this port calls: a scalar
    /// multiplication about 150 µs, a point decompression about 12 µs, and a
    /// ring signature two scalar multiplications per member. The numbers only
    /// have to be right to within a factor of two — they choose between the
    /// sequential loop and the parallel one, and both compute the same answer.
    fn weight(&self) -> usize {
        match self {
            PureCheck::KeyImageDomain { .. } => 10,
            PureCheck::OutputKey { .. } => 1,
            // About 850 µs.
            PureCheck::TxPow { .. } => 57,
            PureCheck::Ring(r) => 20 * r.ring.len().max(1),
        }
    }

    /// Run it. A pure function of the bytes it borrows and of nothing else,
    /// which is what lets a worker thread run it.
    fn verify(&self) -> bool {
        match self {
            PureCheck::KeyImageDomain { image, .. } => wrkz_pow::curve::key_image_in_prime_subgroup(image),
            PureCheck::OutputKey { key, .. } => wrkz_pow::curve::check_key(key),
            PureCheck::TxPow { prefix, difficulty, .. } => wrkz_pow::check_hash(&wrkz_pow::cn_upx(prefix), *difficulty),
            PureCheck::Ring(r) => wrkz_pow::curve::check_ring_signature(&r.prefix_hash, r.image, &r.ring, r.signatures),
        }
    }

    /// Where the sequential C++ validator reaches this check: the transaction,
    /// then the phase inside it — `validateTransactionInputs` before
    /// `validateTransactionOutputs` before `validateTransactionPoW` before
    /// `validateTransactionInputsExpensive` — then the input or output within
    /// that phase.
    ///
    /// The batch is required to be in this order and
    /// [`BlockRingBatch::push`] asserts it in a debug build. Nothing reads it
    /// otherwise: the rules the C++ reports here (`INPUT_INVALID_DOMAIN_KEYIMAGES`
    /// and `OUTPUT_INVALID_KEY`) name no index, so only the position in the
    /// vector matters and only for choosing between failures.
    fn position(&self) -> (usize, u8, usize) {
        match self {
            PureCheck::KeyImageDomain { tx_index, input_index, .. } => (*tx_index, 0, *input_index),
            PureCheck::OutputKey { tx_index, output_index, .. } => (*tx_index, 1, *output_index),
            PureCheck::TxPow { tx_index, .. } => (*tx_index, 2, 0),
            PureCheck::Ring(r) => (r.tx_index, 3, r.input_index),
        }
    }

    /// The transaction this check belongs to, and the rule its failure is.
    fn failure(&self) -> (usize, TxRule) {
        match self {
            PureCheck::KeyImageDomain { tx_index, .. } => (*tx_index, TxRule::InputInvalidDomainKeyImages),
            PureCheck::OutputKey { tx_index, .. } => (*tx_index, TxRule::OutputInvalidKey),
            PureCheck::TxPow { tx_index, difficulty, .. } => {
                (*tx_index, TxRule::PowInvalid { difficulty: *difficulty })
            }
            PureCheck::Ring(r) => (r.tx_index, TxRule::InputInvalidSignatures { input: r.input_index }),
        }
    }
}

/// Work a batch must be worth before [`first_invalid_check`] spends threads on
/// it, in the tenths-of-a-scalar-multiplication units of [`PureCheck::weight`].
///
/// It is [`WORK_PER_WORKER`] twice over: below two workers' worth there is
/// nothing to spread. Below it the sequential loop wins, and it computes the
/// same answer, so this is a performance threshold and nothing more.
const PARALLEL_WORK_THRESHOLD: usize = 2 * WORK_PER_WORKER;

/// Work one worker must be given before it is worth creating, in the same
/// units.
///
/// 60 is six scalar multiplications, about 900 µs, and it is deliberately
/// generous. Creating a thread costs 25-40 µs on Linux but 60 µs to over a
/// millisecond on Windows, and the tail matters more than the mean here
/// because the settle is on the critical path of every block. Measured on this
/// dev host, a batch of 16 cheap checks handed to four workers took **four
/// times** the sequential loop, while a batch of 256 scaled 1.0x / 1.8x / 3.0x
/// / 4.1x over 1, 2, 4 and 8 workers. Sizing the pool by the work rather than
/// by the entry count is what keeps the first case on the sequential path and
/// lets the second have every core it can use.
///
/// It only chooses between two implementations of the same answer, so getting
/// it wrong costs speed and nothing else.
const WORK_PER_WORKER: usize = 60;

/// The index of the **lowest** entry of `checks` that fails, or `None` when
/// they all pass.
///
/// This is [`wrkz_pow::parallel::first_invalid_ring`] over a heterogeneous
/// batch, and the determinism argument is that function's, unchanged: work is
/// claimed in disjoint ascending batches through one `fetch_add`; `lowest`
/// starts at `usize::MAX`, only ever moves down, and only to an index that has
/// actually failed; a worker abandons an index `i` only after observing
/// `lowest < i`, which cannot happen for the lowest failing index because
/// nothing below it fails. So the answer is the index a sequential loop with an
/// early `return` would have produced, at every thread count and in every
/// interleaving.
///
/// It lives here rather than in `wrkz-pow` because the checks it runs are this
/// crate's, not the curve's; `wrkz_pow::parallel` keeps the ring-only version
/// its own tests pin.
fn first_invalid_check(checks: &[PureCheck<'_>], threads: usize) -> Option<usize> {
    let work: usize = checks.iter().map(PureCheck::weight).sum();
    let workers = threads.min(work / WORK_PER_WORKER).min(checks.len() / 2);
    // Creating a thread is not free and is not even predictable: measured at
    // 25-40 µs on Linux and 60-250 µs on Windows, per thread, per settle. A
    // batch has to be worth more than the threads it would ask for, and
    // counting entries is the wrong measure because they differ by two orders
    // of magnitude in cost — three output keys are 36 µs, three rings of four
    // are 3.6 ms. So the guard is on the work, and a batch below it takes the
    // sequential loop, which is what a block with one small transaction wants.
    if checks.len() < wrkz_pow::parallel::PARALLEL_THRESHOLD || workers < 2 || work < PARALLEL_WORK_THRESHOLD {
        return checks.iter().position(|c| !c.verify());
    }
    let batch = checks.len().div_ceil(workers * 4).max(1);
    let next = AtomicUsize::new(0);
    let lowest = AtomicUsize::new(usize::MAX);

    let worker = || loop {
        let start = next.fetch_add(batch, Ordering::Relaxed);
        if start >= checks.len() || lowest.load(Ordering::Acquire) < start {
            return;
        }
        let end = (start + batch).min(checks.len());
        for (offset, check) in checks[start..end].iter().enumerate() {
            let i = start + offset;
            if lowest.load(Ordering::Acquire) < i {
                return;
            }
            if !check.verify() {
                lowest.fetch_min(i, Ordering::AcqRel);
                // Nothing above `i` in this batch can be lower than `i`.
                break;
            }
        }
    };

    std::thread::scope(|scope| {
        for _ in 1..workers {
            scope.spawn(|| {
                worker();
                // A worker that ran a transaction proof of work holds a
                // CryptoNight scratchpad in its thread-local; give it back now
                // rather than leave it to the thread's exit. A no-op when it
                // holds none.
                wrkz_pow::release_thread_scratchpad();
            });
        }
        // The calling thread is a worker too, and must **not** release its
        // scratchpad: it is the thread that hashes the block proof of work.
        worker();
    });

    match lowest.load(Ordering::Acquire) {
        usize::MAX => None,
        i => Some(i),
    }
}

/// When a caller should settle a batch and start a fresh one, counted in ring
/// **members** — the unit that costs memory, since each one is a 32-byte public
/// key copied out of the store.
///
/// 1,048,576 members is 32 MiB of resolved ring, which no real block comes
/// close to: a block is capped at a couple of megabytes of blob and a ring
/// member costs one to three bytes of relative offset there, so even a
/// pathological block stays two orders of magnitude below this. It exists so
/// that the batch's memory is bounded by a constant rather than by the block
/// size rule, and settling early is free of consequence — see "Splitting the
/// batch changes nothing" on [`BlockRingBatch`].
pub const RING_BATCH_MEMBER_LIMIT: usize = 1 << 20;

impl<'a> BlockRingBatch<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many checks are waiting.
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// How many key-image domain checks are waiting: one ed25519 scalar
    /// multiplication each, and the dominant cost of a block that carries
    /// spends inside the checkpoint zone.
    pub fn key_image_checks(&self) -> usize {
        self.checks.iter().filter(|c| matches!(c, PureCheck::KeyImageDomain { .. })).count()
    }

    /// How many output `check_key`s are waiting.
    pub fn output_key_checks(&self) -> usize {
        self.checks.iter().filter(|c| matches!(c, PureCheck::OutputKey { .. })).count()
    }

    /// How many transaction proofs of work are waiting.
    pub fn tx_pow_checks(&self) -> usize {
        self.checks.iter().filter(|c| matches!(c, PureCheck::TxPow { .. })).count()
    }

    /// How many ring signatures are waiting.
    pub fn ring_checks(&self) -> usize {
        self.checks.iter().filter(|c| matches!(c, PureCheck::Ring(_))).count()
    }

    /// How many ring **members** are waiting: what
    /// [`RING_BATCH_MEMBER_LIMIT`] is compared against.
    pub fn members(&self) -> usize {
        self.members
    }

    /// Append one check, in visit order. The only way anything enters the
    /// batch, so "the vector is in visit order" is a local property of the
    /// three call sites.
    fn push(&mut self, check: PureCheck<'a>) {
        debug_assert!(
            self.checks.last().is_none_or(|previous| previous.position() < check.position()),
            "the batch must be appended to in the order the C++ validator visits its checks"
        );
        // A key image and an output key are one borrowed 32-byte value each;
        // a ring owns its resolved members. Charging one apiece keeps
        // `members` an upper bound on what the batch holds.
        self.members += match &check {
            PureCheck::Ring(r) => r.ring.len(),
            _ => 1,
        };
        self.checks.push(check);
    }

    /// Forget everything gathered so far. Only sound once
    /// [`BlockRingBatch::first_invalid`] has returned `None` for it.
    pub fn clear(&mut self) {
        self.checks.clear();
        self.members = 0;
    }

    /// The transaction index and the rule of the **lowest** check in this batch
    /// that fails, or `None` when every one of them passes.
    ///
    /// `threads` is an upper bound on the workers; `1` is the plain sequential
    /// loop, which is the reference this is required to match. See the type's
    /// documentation for why it does.
    pub fn first_invalid(&self, threads: usize) -> Option<(usize, TxRule)> {
        let at = first_invalid_check(&self.checks, threads)?;
        Some(self.checks[at].failure())
    }
}

impl std::fmt::Debug for BlockRingBatch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockRingBatch").field("checks", &self.checks.len()).field("members", &self.members).finish()
    }
}

/// `validateTransactionPoW` (line 561) for a block transaction, minus the hash:
/// the check is appended to `batch` at the transaction's step-9 position.
///
/// The fee escape is decided here, because it needs no hash: where it applies
/// the C++ verdict is "pass" whatever the hash says, so nothing is gathered
/// and no hash is ever computed — the same verdict for less work.
fn gather_transaction_pow(
    tx: &Transaction,
    prefix_bytes: &[u8],
    fee: u64,
    is_fusion: bool,
    block_height: u64,
    tx_index: usize,
    batch: &mut BlockRingBatch<'_>,
) {
    if block_height >= TRANSACTION_POW_PASS_WITH_FEE_HEIGHT && !is_fusion && fee >= TRANSACTION_POW_PASS_WITH_FEE {
        return;
    }
    let difficulty = transaction_pow_difficulty(
        block_height,
        is_fusion,
        tx.prefix.inputs.len() as u64,
        tx.prefix.outputs.len() as u64,
    )
    .expect("the caller only applies this at or above TRANSACTION_POW_HEIGHT");
    batch.push(PureCheck::TxPow { tx_index, prefix: prefix_bytes.to_vec(), difficulty });
}

/// `validateTransactionInputsExpensive` (line 654), minus the verification.
///
/// The caller skips it entirely inside the checkpoint zone — for pool
/// transactions too, unlike the transaction proof of work
/// (`ValidateTransaction.cpp:657` has no `m_isPoolTransaction` term).
///
/// # Two phases, and where the second one went
///
/// The C++ is one loop: for each input in order, check the key image against
/// the chain, resolve the ring, check the unlock times and the signature count,
/// verify the ring signature, and return at the first thing that fails. All of
/// that except the last step reads the chain, and the last step is where
/// essentially all the time goes.
///
/// So the loop is split. This function is **phase one**: it walks the inputs in
/// order and does everything the C++ loop does *except* verify, appending one
/// ring to `batch` per input, and it returns at the first input whose
/// non-signature checks fail — leaving the batch holding exactly the inputs
/// below that one. **Phase two** is [`BlockRingBatch::first_invalid`], which
/// the caller runs once for a whole block rather than once per transaction.
///
/// Every chain read therefore stays on the calling thread and in input order.
/// Resolving ring members inside the workers instead would put the store behind
/// a lock or force `Sync` on every [`ChainAccess`] implementation, for no gain:
/// the reads are a batched `multi_get`, not the bottleneck.
///
/// Why holding the failure back reproduces the C++ order — in both dimensions —
/// is argued on [`BlockRingBatch`].
fn gather_inputs_expensive<'a, C: ChainAccess + ?Sized>(
    tx: &'a Transaction,
    prefix_bytes: &[u8],
    chain: &C,
    ctx: &TxContext<'_>,
    tx_index: usize,
    batch: &mut BlockRingBatch<'a>,
) -> TxResult<()> {
    let prefix_hash = wrkz_pow::cn_fast_hash(prefix_bytes);
    batch.checks.reserve(tx.prefix.inputs.len());
    for (input_index, input) in tx.prefix.inputs.iter().enumerate() {
        let (image, ring, signatures) = gather_ring(tx, input, input_index, chain, ctx)?;
        batch.push(PureCheck::Ring(GatheredRing { tx_index, input_index, prefix_hash, image, ring, signatures }));
    }
    Ok(())
}

/// One input's ring signature, ready to verify and needing nothing but its own
/// bytes: what [`gather_inputs_expensive`] appends to a [`BlockRingBatch`].
struct GatheredRing<'a> {
    /// The index of the transaction this input belongs to, within its block.
    tx_index: usize,
    /// The index of this input within that transaction. The pair
    /// `(tx_index, input_index)` is what a failure is reported against.
    input_index: usize,
    /// The hash of the transaction's serialized prefix: what the ring signs.
    /// Owned, because one batch spans transactions with different prefixes and
    /// a 32-byte copy per ring is nothing beside the ring itself.
    prefix_hash: [u8; 32],
    image: &'a Hash,
    /// The resolved ring members, in the input's own order.
    ring: Vec<wrkz_pow::curve::PublicKey>,
    /// Exactly `ring.len()` signatures — the count rule has already run.
    signatures: &'a [wrkz_pow::curve::Signature],
}

/// Everything `validateTransactionInputsExpensive` does for one input except
/// verify its ring signature: the key image, the global indexes, the unlock
/// times and the signature count. Returns the key image, the resolved ring and
/// the signatures the verification will need.
///
/// Split out of [`gather_inputs_expensive`] so that the checks that read the
/// chain stay on the calling thread and in input order.
fn gather_ring<'a, C: ChainAccess + ?Sized>(
    tx: &'a Transaction,
    input: &'a Input,
    input_index: usize,
    chain: &C,
    ctx: &TxContext<'_>,
) -> TxResult<(&'a Hash, Vec<wrkz_pow::curve::PublicKey>, &'a [wrkz_pow::curve::Signature])> {
    let Input::Key { amount, key_offsets, key_image } = input else {
        // Unreachable: validate_inputs already rejected anything else.
        return Err(TxRule::InputUnknownType.into());
    };
    if chain.key_image_spent(key_image, ctx.block_height)? {
        return Err(TxRule::InputKeyImageAlreadySpent { key_image: *key_image }.into());
    }
    // `globalIndexes[i] = globalIndexes[i-1] + outputIndexes[i]` in
    // `uint32_t`, which wraps; an overflow here cannot name a real output,
    // so it is the same rejection as an unknown index.
    let absolute = relative_offsets_to_absolute(key_offsets)
        .ok_or_else(|| TxError::from(TxRule::InputInvalidGlobalIndex { amount: *amount, global_index: u64::MAX }))?;
    let found = chain.key_outputs(*amount, &absolute)?;
    let mut ring = Vec::with_capacity(absolute.len());
    for (global_index, record) in absolute.iter().zip(found) {
        // `DatabaseBlockchainCache::extractKeyOutputs` never returns
        // `INVALID_GLOBAL_INDEX`: a missing record simply drops out of the
        // batch result, so the C++ ends up with a short ring and fails the
        // signature-count check or the ring signature instead. The block is
        // rejected either way; naming the real cause is more useful than
        // reproducing which of the two later checks happened to fire.
        let record = record.ok_or_else(|| {
            TxError::from(TxRule::InputInvalidGlobalIndex { amount: *amount, global_index: *global_index })
        })?;
        if !is_spend_time_unlocked(chain, record.unlock_time, ctx.block_height) {
            return Err(TxRule::InputSpendLockedOut {
                amount: *amount,
                global_index: *global_index,
                unlock_time: record.unlock_time,
            }
            .into());
        }
        ring.push(record.public_key);
    }
    let signatures = tx.signatures.get(input_index).map(Vec::as_slice).unwrap_or(&[]);
    if ctx.is_pool_transaction || ctx.block_height >= TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT {
        if ring.len() != signatures.len() {
            return Err(TxRule::InputInvalidSignaturesCount { expected: ring.len(), got: signatures.len() }.into());
        }
    } else if signatures.len() < ring.len() {
        // Below 543,000 the count is not checked, and blocks with a wrong
        // count exist on chain. `checkRingSignature` reads `pubs.size()`
        // signatures, so a *short* vector is an out-of-bounds read in the
        // C++; refusing it is the only safe reading and no such block is on
        // the majority chain (it would have crashed the nodes that saw it).
        return Err(TxRule::InputInvalidSignaturesCount { expected: ring.len(), got: signatures.len() }.into());
    }
    // The two branches above both guarantee `signatures.len() >= ring.len()`,
    // so the trim below never panics and never shortens a ring the C++ would
    // have verified in full.
    let members = ring.len();
    Ok((key_image, ring, &signatures[..members]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_primitives::tx::{Output, TransactionPrefix};

    struct Empty;
    impl ChainAccess for Empty {
        fn key_image_spent(&self, _: &Hash, _: u64) -> crate::Result<bool> {
            Ok(false)
        }
        fn key_output(&self, _: u64, _: u64) -> crate::Result<Option<OutputRecord>> {
            Ok(None)
        }
        fn top_block_timestamp(&self) -> u64 {
            1_788_929_495
        }
        fn now(&self) -> u64 {
            1_788_929_495
        }
    }

    fn ctx(cp: &Checkpoints, height: u64) -> TxContext<'_> {
        TxContext {
            block_height: height,
            block_median_size: 100_000,
            block_timestamp: 1_788_929_495,
            is_pool_transaction: false,
            checkpoints: cp,
        }
    }

    // -----------------------------------------------------------------------
    // the deferred checks report in the order the sequential validator would
    // -----------------------------------------------------------------------

    /// A key image that decompresses but is **not** in the prime-order
    /// subgroup, so `l * I` is not the identity and the domain check rejects
    /// it. Searched for rather than hard-coded, so that what makes it fail is
    /// visible and checked.
    fn bad_key_image() -> Hash {
        for seed in 0u8..=255 {
            let candidate = wrkz_pow::cn_fast_hash(&[seed, 0xD1]);
            if wrkz_pow::curve::check_key(&candidate) && !wrkz_pow::curve::key_image_in_prime_subgroup(&candidate) {
                return candidate;
            }
        }
        panic!("no torsion key image found in the search space");
    }

    /// An output key that does not decompress, so `check_key` rejects it.
    const BAD_OUTPUT_KEY: Hash = [0xff; 32];

    /// `inputs` valid key inputs and one good output: a transaction with
    /// nothing wrong with it, for a test to break in one specific place.
    fn ordered_tx(inputs: usize, tag: u8) -> Transaction {
        let mut prefix = TransactionPrefix {
            version: 1,
            unlock_time: 0,
            inputs: Vec::new(),
            outputs: vec![Output { amount: 1_000, key: wrkz_pow::curve::generate_keys().1 }],
            extra: Vec::new(),
        };
        for i in 0..inputs {
            let (sec, pk) = wrkz_pow::curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(&[tag, i as u8]));
            prefix.inputs.push(Input::Key {
                amount: 2_000,
                key_offsets: vec![1, 1],
                key_image: wrkz_pow::curve::generate_key_image(&pk, &sec),
            });
        }
        Transaction { prefix, signatures: Vec::new() }
    }

    /// The height every ordering test judges at. Above every fork, so the
    /// amount cap and the rest of the height-dependent rules are all in force.
    const ORDER_HEIGHT: u64 = 4_213_649;

    /// `validateTransactionInputs` then `validateTransactionOutputs`, with the
    /// domain check and the key check **deferred** and settled the way the
    /// block validator settles them: the batch first, the held-back rule only
    /// if the batch is clean.
    fn deferred_rule_at(tx: &Transaction, threads: usize) -> std::result::Result<(), TxRule> {
        let mut state = ValidatorState::new();
        let mut batch = BlockRingBatch::new();
        let held = validate_inputs(tx, &mut state, 0, Some(&mut batch))
            .and_then(|_| validate_outputs(tx, ORDER_HEIGHT, 0, Some(&mut batch)));
        match batch.first_invalid(threads) {
            Some((_, rule)) => Err(rule),
            None => held.map(|_| ()),
        }
    }

    /// [`deferred_rule_at`] at every thread count, insisting they agree.
    fn deferred_rule(tx: &Transaction) -> std::result::Result<(), TxRule> {
        let sequential = deferred_rule_at(tx, 1);
        for threads in [2usize, 3, 4, 8, 16, 64] {
            assert_eq!(
                deferred_rule_at(tx, threads),
                sequential,
                "{threads} threads disagreed with the sequential loop"
            );
        }
        sequential
    }

    /// The same two functions with the checks run **inline**: the code as it
    /// stood before they were batched, and the reference every case below is
    /// required to match.
    fn inline_rule(tx: &Transaction) -> std::result::Result<(), TxRule> {
        let mut state = ValidatorState::new();
        validate_inputs(tx, &mut state, 0, None)?;
        validate_outputs(tx, ORDER_HEIGHT, 0, None)?;
        Ok(())
    }

    /// Assert the deferred path names `expected`, that every thread count
    /// agrees, and that the inline path — the pre-change code — names the same.
    #[track_caller]
    fn same_rule(tx: &Transaction, expected: std::result::Result<(), TxRule>) {
        assert_eq!(deferred_rule(tx), expected, "deferred");
        assert_eq!(inline_rule(tx), expected, "inline (the pre-change reference)");
    }

    /// Every rule that sits **before** the domain check on the same input must
    /// still win, and every rule that sits **after** it must still lose.
    ///
    /// The list is `validateTransactionInputs` in its own order — unknown type,
    /// identical key images, empty output usage, **the domain check**,
    /// identical output indexes, already spent, input sum overflow — and each
    /// case puts the competing rule on the *same* input as a bad key image, so
    /// the two really are at the same position and only their order decides.
    #[test]
    fn the_domain_check_keeps_its_place_among_the_input_rules() {
        let bad = bad_key_image();

        // --- before the domain check, so they win ---------------------------
        let mut tx = ordered_tx(2, 1);
        tx.prefix.inputs[1] = Input::Base { block_index: 7 };
        same_rule(&tx, Err(TxRule::InputUnknownType));

        let mut tx = ordered_tx(2, 2);
        tx.prefix.inputs[1] = tx.prefix.inputs[0].clone();
        same_rule(&tx, Err(TxRule::InputIdenticalKeyImages));

        let mut tx = ordered_tx(2, 3);
        if let Input::Key { key_offsets, key_image, .. } = &mut tx.prefix.inputs[1] {
            key_offsets.clear();
            *key_image = bad;
        }
        same_rule(&tx, Err(TxRule::InputEmptyOutputUsage));

        // --- after the domain check, so they lose to it ---------------------
        let mut tx = ordered_tx(2, 4);
        if let Input::Key { key_offsets, key_image, .. } = &mut tx.prefix.inputs[1] {
            *key_offsets = vec![1, 0];
            *key_image = bad;
        }
        same_rule(&tx, Err(TxRule::InputInvalidDomainKeyImages));

        let mut tx = ordered_tx(2, 5);
        if let Input::Key { amount, key_image, .. } = &mut tx.prefix.inputs[1] {
            *amount = u64::MAX;
            *key_image = bad;
        }
        same_rule(&tx, Err(TxRule::InputInvalidDomainKeyImages));

        // The already-spent rule, which also follows the domain check: input 1
        // repeats input 0's image, but `in_this_tx` catches that first, so the
        // state collision needs a state that already holds the image.
        let tx = ordered_tx(1, 6);
        let Input::Key { key_image, .. } = &tx.prefix.inputs[0] else { panic!("a key input") };
        let mut state = ValidatorState::new();
        state.spent_key_images.insert(*key_image);
        let mut batch = BlockRingBatch::new();
        let held = validate_inputs(&tx, &mut state, 0, Some(&mut batch));
        assert!(matches!(held, Err(TxRule::InputKeyImageAlreadySpent { .. })), "{held:?}");
        assert_eq!(batch.key_image_checks(), 1, "the image was batched before the state collision was found");

        // --- an earlier input beats a later one -----------------------------
        let mut tx = ordered_tx(2, 7);
        if let Input::Key { key_image, .. } = &mut tx.prefix.inputs[0] {
            *key_image = bad;
        }
        if let Input::Key { key_offsets, .. } = &mut tx.prefix.inputs[1] {
            key_offsets.clear();
        }
        same_rule(&tx, Err(TxRule::InputInvalidDomainKeyImages));
    }

    /// The same for `check_key` among the output rules, and for the two phases
    /// against each other: the whole of `validateTransactionInputs` runs before
    /// `validateTransactionOutputs`, so any bad key image beats any bad output
    /// key.
    #[test]
    fn the_output_key_check_keeps_its_place() {
        assert!(!wrkz_pow::curve::check_key(&BAD_OUTPUT_KEY), "the test key must not decompress");

        // Before it, on the same output: the zero amount and the amount cap.
        let mut tx = ordered_tx(1, 10);
        tx.prefix.outputs = vec![Output { amount: 0, key: BAD_OUTPUT_KEY }];
        same_rule(&tx, Err(TxRule::OutputZeroAmount));

        let mut tx = ordered_tx(1, 11);
        tx.prefix.outputs = vec![Output { amount: MAX_OUTPUT_SIZE_NODE + 1, key: BAD_OUTPUT_KEY }];
        same_rule(&tx, Err(TxRule::OutputAmountTooLarge { amount: MAX_OUTPUT_SIZE_NODE + 1 }));

        // After it, on the same output: the output sum overflow. Judged below
        // `MAX_OUTPUT_SIZE_HEIGHT`, since the amount cap would otherwise catch
        // an amount large enough to overflow the sum before `check_key` runs —
        // which is itself the case above.
        let mut tx = ordered_tx(1, 12);
        tx.prefix.outputs =
            vec![Output { amount: u64::MAX, key: BAD_OUTPUT_KEY }, Output { amount: 1, key: BAD_OUTPUT_KEY }];
        let deferred = {
            let mut batch = BlockRingBatch::new();
            let held = validate_outputs(&tx, 0, 0, Some(&mut batch));
            match batch.first_invalid(1) {
                Some((_, rule)) => Err(rule),
                None => held.map(|_| ()),
            }
        };
        assert_eq!(deferred, Err(TxRule::OutputInvalidKey), "output 0's key check comes before the sum overflow");
        assert_eq!(validate_outputs(&tx, 0, 0, None).map(|_| ()), deferred, "and the inline path agrees");

        // An earlier output beats a later one.
        let mut tx = ordered_tx(1, 13);
        tx.prefix.outputs = vec![
            Output { amount: 1_000, key: BAD_OUTPUT_KEY },
            Output { amount: 0, key: wrkz_pow::curve::generate_keys().1 },
        ];
        same_rule(&tx, Err(TxRule::OutputInvalidKey));

        // The input phase beats the output phase.
        let mut tx = ordered_tx(2, 14);
        if let Input::Key { key_image, .. } = &mut tx.prefix.inputs[1] {
            *key_image = bad_key_image();
        }
        tx.prefix.outputs = vec![Output { amount: 1_000, key: BAD_OUTPUT_KEY }];
        same_rule(&tx, Err(TxRule::InputInvalidDomainKeyImages));
    }

    /// Across a block: the lowest failing transaction wins, and inside one
    /// transaction the input phase wins — at every thread count, and matching
    /// what the inline validator would have said.
    #[test]
    fn across_transactions_the_lowest_failure_wins() {
        let bad = bad_key_image();
        let block = |images: &[usize], keys: &[usize]| -> Vec<Transaction> {
            (0..5usize)
                .map(|t| {
                    let mut tx = ordered_tx(1, 20 + t as u8);
                    if images.contains(&t) {
                        if let Input::Key { key_image, .. } = &mut tx.prefix.inputs[0] {
                            *key_image = bad;
                        }
                    }
                    if keys.contains(&t) {
                        tx.prefix.outputs = vec![Output { amount: 1_000, key: BAD_OUTPUT_KEY }];
                    }
                    tx
                })
                .collect()
        };

        // The block validator's contract, over the two phases these checks live
        // in: gather in index order, stop at the first held-back failure, let
        // the batch decide.
        let run = |txs: &[Transaction], threads: usize| -> std::result::Result<(), (usize, TxRule)> {
            let mut state = ValidatorState::new();
            let mut batch = BlockRingBatch::new();
            let mut held: Option<(usize, TxRule)> = None;
            for (i, tx) in txs.iter().enumerate() {
                let outcome = validate_inputs(tx, &mut state, i, Some(&mut batch))
                    .and_then(|_| validate_outputs(tx, ORDER_HEIGHT, i, Some(&mut batch)));
                if let Err(e) = outcome {
                    held = Some((i, e));
                    break;
                }
            }
            match batch.first_invalid(threads) {
                Some(f) => Err(f),
                None => match held {
                    Some(h) => Err(h),
                    None => Ok(()),
                },
            }
        };

        for threads in [1usize, 2, 3, 4, 8, 16, 64] {
            assert_eq!(
                run(&block(&[2], &[3]), threads),
                Err((2, TxRule::InputInvalidDomainKeyImages)),
                "{threads} threads: transaction 2 is below transaction 3"
            );
            assert_eq!(
                run(&block(&[3], &[1]), threads),
                Err((1, TxRule::OutputInvalidKey)),
                "{threads} threads: transaction 1 is below transaction 3"
            );
            assert_eq!(
                run(&block(&[1, 3], &[2, 4]), threads),
                Err((1, TxRule::InputInvalidDomainKeyImages)),
                "{threads} threads: the lowest of four failures"
            );
            assert_eq!(run(&block(&[], &[]), threads), Ok(()), "{threads} threads: a clean block");
        }
    }

    /// Settling the batch in pieces, which is what the
    /// [`RING_BATCH_MEMBER_LIMIT`] guard does, reports the same failure as
    /// settling it whole.
    #[test]
    fn settling_the_deferred_checks_in_pieces_reports_the_same_rule() {
        let bad = bad_key_image();
        let mut txs: Vec<Transaction> = (0..6).map(|t| ordered_tx(1, 40 + t as u8)).collect();
        if let Input::Key { key_image, .. } = &mut txs[4].prefix.inputs[0] {
            *key_image = bad;
        }
        for threads in [1usize, 4, 16] {
            let mut state = ValidatorState::new();
            let mut batch = BlockRingBatch::new();
            let mut found = None;
            for (i, tx) in txs.iter().enumerate() {
                validate_inputs(tx, &mut state, i, Some(&mut batch)).expect("only a key image is wrong here");
                validate_outputs(tx, ORDER_HEIGHT, i, Some(&mut batch)).expect("the outputs are fine");
                if let Some(f) = batch.first_invalid(threads) {
                    found = Some(f);
                    break;
                }
                batch.clear();
                assert!(batch.is_empty() && batch.members() == 0);
            }
            assert_eq!(
                found,
                Some((4, TxRule::InputInvalidDomainKeyImages)),
                "{threads} threads, settled per transaction"
            );
        }
    }

    /// Inside the checkpoint zone the ring signatures and the transaction proof
    /// of work are skipped — the domain check and the output key check are
    /// **not**, because they belong to `validateTransactionInputs` and
    /// `validateTransactionOutputs`, which the zone does not touch.
    ///
    /// This is the fact the whole diagnosis rests on, so it is pinned here: a
    /// transaction validated deep inside the zone still batches one scalar
    /// multiplication per key input and one point decompression per output, and
    /// a bad key image is still rejected there.
    #[test]
    fn the_checkpoint_zone_does_not_skip_the_domain_check() {
        let zone = Checkpoints::from_csv("9000000,00000000000000000000000000000000000000000000000000000000000000ff")
            .expect("one checkpoint");
        assert!(zone.is_in_checkpoint_zone(ORDER_HEIGHT + 1), "the transaction must be inside the zone");

        let tx = ordered_tx(3, 50);
        let ctx = ctx(&zone, ORDER_HEIGHT);
        let mut state = ValidatorState::new();
        let mut batch = BlockRingBatch::new();
        let _ = validate_transaction_deferred(&tx, &[0u8; 300], &mut state, &Empty, &ctx, 0, &mut batch);
        assert_eq!(batch.key_image_checks(), 3, "one scalar multiplication per key input, zone or no zone");
        assert_eq!(batch.output_key_checks(), 1, "one point decompression per output");
        assert_eq!(batch.ring_checks(), 0, "ring signatures ARE skipped inside the zone");

        let mut bad = ordered_tx(1, 51);
        if let Input::Key { key_image, .. } = &mut bad.prefix.inputs[0] {
            *key_image = bad_key_image();
        }
        same_rule(&bad, Err(TxRule::InputInvalidDomainKeyImages));
    }

    /// A one-input spend with a ring of `offsets.len()`. The mixin tier from
    /// 1,000,000 is min 1 max 1, so the default ring here is exactly 2.
    fn spend(image: Hash, offsets: Vec<u64>) -> Transaction {
        let ring = offsets.len();
        let mut tx = Transaction::default();
        tx.prefix.version = 1;
        tx.prefix.unlock_time = 4_213_665;
        tx.prefix.inputs.push(Input::Key { amount: 100_000, key_offsets: offsets, key_image: image });
        tx.prefix.outputs.push(Output { amount: 50_000, key: [0; 32] });
        tx.signatures.push(vec![[0u8; 64]; ring]);
        tx
    }

    /// A key image that is on the curve and in the prime-order subgroup: the
    /// image of a real key pair.
    fn real_image() -> Hash {
        let (sec, pk) = wrkz_pow::curve::generate_keys();
        wrkz_pow::curve::generate_key_image(&pk, &sec)
    }

    #[test]
    fn input_rules_fire_in_the_cpp_order() {
        let cp = Checkpoints::none();
        let image = real_image();
        let mut tx = spend(image, vec![7, 1]);
        // A valid-looking output key is needed to get past rule 3.
        let (_, key) = wrkz_pow::curve::generate_keys();
        tx.prefix.outputs[0].key = key;

        // Empty inputs.
        let mut empty = tx.clone();
        empty.prefix.inputs.clear();
        assert_eq!(
            validate_transaction(&empty, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::EmptyInputs
        );

        // A BaseInput outside a coinbase.
        let mut base = tx.clone();
        base.prefix.inputs = vec![Input::Base { block_index: 1 }];
        assert_eq!(
            validate_transaction(&base, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputUnknownType
        );

        // Two inputs with the same key image.
        let mut dup = tx.clone();
        dup.prefix.inputs.push(dup.prefix.inputs[0].clone());
        dup.signatures.push(vec![[0u8; 64]]);
        assert_eq!(
            validate_transaction(&dup, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputIdenticalKeyImages
        );

        // A key image outside the prime-order subgroup.
        let mut bad = tx.clone();
        bad.prefix.inputs = vec![Input::Key { amount: 100_000, key_offsets: vec![7, 1], key_image: [1u8; 32] }];
        assert_eq!(
            validate_transaction(&bad, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputInvalidDomainKeyImages
        );

        // A zero relative offset after the first.
        let zero = spend(image, vec![7, 1, 0]);
        assert_eq!(
            validate_transaction(&zero, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputIdenticalOutputIndexes
        );

        // The same key image twice in one block: caught by the validator state.
        let mut state = ValidatorState::new();
        let _ = validate_transaction(&tx, &[0; 300], &mut state, &Empty, &ctx(&cp, 4_213_649));
        assert_eq!(
            validate_transaction(&tx, &[0; 300], &mut state, &Empty, &ctx(&cp, 4_213_649)).unwrap_err().rule().unwrap(),
            &TxRule::InputKeyImageAlreadySpent { key_image: image }
        );
    }

    #[test]
    fn the_checkpoint_zone_skips_the_expensive_checks_and_the_tx_pow() {
        let image = real_image();
        let (_, key) = wrkz_pow::curve::generate_keys();
        let mut tx = spend(image, vec![7, 1]);
        tx.prefix.outputs[0].key = key;
        // Fee 50000 at height 4,213,649 with a 300-byte blob: the minimum is 30.
        let blob = vec![0u8; 300];

        // Outside the zone, the ring member does not exist.
        let cp = Checkpoints::none();
        assert_eq!(
            validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 }
        );

        // Inside the zone, nothing is looked up and nothing is hashed.
        let mut cp = Checkpoints::none();
        cp.add(4_213_700, [0; 32]);
        let v = validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649)).unwrap();
        assert_eq!(v.fee, 50_000);
        assert!(!v.is_fusion);
    }

    #[test]
    fn fee_extra_unlock_and_output_count_rules() {
        let cp = Checkpoints::none();
        let image = real_image();
        let (_, key) = wrkz_pow::curve::generate_keys();
        let mut tx = spend(image, vec![7, 1]);
        tx.prefix.outputs[0].key = key;

        // Fee below the ladder minimum: 2000 bytes needs 16 chunks * 10 = 160.
        let mut poor = tx.clone();
        poor.prefix.outputs[0].amount = 99_900;
        assert_eq!(
            validate_transaction(&poor, &vec![0; 2000], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::WrongFee { fee: 100, minimum: 160 }
        );

        // Outputs exceed inputs.
        let mut rich = tx.clone();
        rich.prefix.outputs[0].amount = 100_001;
        assert_eq!(
            validate_transaction(&rich, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::WrongAmount
        );

        // extra >= 1024 (strict).
        let mut fat = tx.clone();
        fat.prefix.extra = vec![0xff; 1024];
        assert_eq!(
            validate_transaction(&fat, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::ExtraTooLarge { size: 1024 }
        );
        fat.prefix.extra = vec![0xff; 1023];
        // 1023 bytes passes the extra rule; what stops this transaction now is
        // the ring member that does not exist, one check further down.
        assert_eq!(
            validate_transaction(&fat, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 }
        );

        // unlock time below H + 15.
        let mut soon = tx.clone();
        soon.prefix.unlock_time = 4_213_663;
        assert_eq!(
            validate_transaction(&soon, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::UnlockTimeTooSmall { unlock_time: 4_213_663, minimum: 4_213_664 }
        );

        // 91 outputs.
        let mut many = tx.clone();
        many.prefix.outputs = (0..91).map(|_| Output { amount: 1, key }).collect();
        assert_eq!(
            validate_transaction(&many, &[0; 300], &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::ExcessiveOutputs { count: 91 }
        );
    }

    #[test]
    fn revalidation_runs_the_cpp_subset_and_nothing_that_needs_the_chain() {
        let cp = Checkpoints::none();
        let image = real_image();
        let (_, key) = wrkz_pow::curve::generate_keys();
        let mut tx = spend(image, vec![7, 1]);
        tx.prefix.outputs[0].key = key;
        let blob = vec![0u8; 300];
        let rctx =
            RevalidateContext { block_height: 4_213_649, block_median_size: 100_000, block_timestamp: 1_788_929_495 };

        // The full validator stops on the ring member that does not exist;
        // the revalidation never looks one up, so the same transaction passes
        // and reports the same fee.
        assert_eq!(
            validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &ctx(&cp, 4_213_649))
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 }
        );
        assert_eq!(
            revalidate_after_height_change(&tx, &blob, &rctx).unwrap(),
            TxValidation { fee: 50_000, is_fusion: false }
        );

        // The rules it does run are the same functions, in the C++ order:
        // size first,
        let small = RevalidateContext { block_median_size: 400, ..rctx };
        assert_eq!(
            revalidate_after_height_change(&tx, &blob, &small).unwrap_err(),
            TxRule::SizeTooLarge { size: 300, limit: 200 }
        );
        // then extra — always capped here, because the C++ builds this
        // validator with `isPoolTransaction = true`,
        let mut fat = tx.clone();
        fat.prefix.extra = vec![0xff; MAX_EXTRA_SIZE_V2];
        assert_eq!(
            revalidate_after_height_change(&fat, &blob, &rctx).unwrap_err(),
            TxRule::ExtraTooLarge { size: MAX_EXTRA_SIZE_V2 }
        );
        // then the mixin, which is the rule the pool cleaner exists for,
        let wide = spend(image, vec![7, 1, 1, 1]);
        assert!(matches!(revalidate_after_height_change(&wide, &blob, &rctx).unwrap_err(), TxRule::InvalidMixin(_)));
        // then the inputs, against a throwaway state, so a key image the block
        // being built already spent is *not* seen here,
        let mut twice = tx.clone();
        twice.prefix.inputs.push(twice.prefix.inputs[0].clone());
        assert_eq!(revalidate_after_height_change(&twice, &blob, &rctx).unwrap_err(), TxRule::InputIdenticalKeyImages);
        assert!(revalidate_after_height_change(&tx, &blob, &rctx).is_ok(), "no state carries over between calls");
        // and last the fee, at the new height.
        let mut poor = tx.clone();
        poor.prefix.outputs[0].amount = 99_900;
        assert_eq!(
            revalidate_after_height_change(&poor, &vec![0; 2000], &rctx).unwrap_err(),
            TxRule::WrongFee { fee: 100, minimum: 160 }
        );
    }

    /// The transaction the two proof-of-work tests below share: one input, one
    /// output, every key derived from a fixed seed, so its prefix — and
    /// therefore its `cn_upx` hash for a given nonce — is the same on every
    /// machine.
    ///
    /// The height is 1,150,000: above `TRANSACTION_POW_HEIGHT` (1,123,000), so
    /// the rule runs, and below `TRANSACTION_POW_PASS_WITH_FEE_HEIGHT`
    /// (1,500,000), so the fee escape every other test here takes does not
    /// exist yet and the work is the only way through. Below
    /// `UNLOCK_TIME_HEIGHT` (1,200,000) the unlock rule does not run, and the
    /// mixin tier is min 1 max 1, so the ring of two is right.
    const TX_POW_HEIGHT: u64 = 1_150_000;

    fn tx_pow_transaction() -> (Transaction, Hash) {
        let seed = |tag: &[u8]| wrkz_pow::curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(tag));
        let (spend_secret, spend_public) = seed(b"wrkz tx pow spend");
        let image = wrkz_pow::curve::generate_key_image(&spend_public, &spend_secret);
        let (_, out_key) = seed(b"wrkz tx pow out");
        let (_, tx_public) = seed(b"wrkz tx pow key");
        let mut tx = spend(image, vec![7, 1]);
        tx.prefix.outputs[0].key = out_key;
        tx.prefix.unlock_time = 0;
        (tx, tx_public)
    }

    fn tx_pow_context(cp: &Checkpoints) -> TxContext<'_> {
        let mut c = ctx(cp, TX_POW_HEIGHT);
        c.is_pool_transaction = true;
        c
    }

    #[test]
    fn a_transaction_proof_of_work_is_the_only_way_past_the_rule_before_the_fee_escape() {
        let difficulty = transaction_pow_difficulty(TX_POW_HEIGHT, false, 1, 1).expect("above TRANSACTION_POW_HEIGHT");
        assert_eq!(difficulty, TRANSACTION_POW_DIFFICULTY);
        const { assert!(TX_POW_HEIGHT < TRANSACTION_POW_PASS_WITH_FEE_HEIGHT) };

        let cp = Checkpoints::none();
        let ctx = tx_pow_context(&cp);
        let (mut tx, tx_public) = tx_pow_transaction();
        let blob = vec![0u8; 300];

        // The transaction pays a fee of 50,000 — five times
        // `TRANSACTION_POW_PASS_WITH_FEE` — and is still rejected, because that
        // escape only exists from 1,500,000.
        tx.prefix.extra = wrkz_primitives::tx::build_extra(&tx_public, None, None).expect("extra");
        assert_eq!(
            validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &ctx).unwrap_err().rule().unwrap(),
            &TxRule::PowInvalid { difficulty }
        );

        // 16,903 is a nonce the search in the test below found for this exact
        // transaction: the wallet's 8-byte proof-of-work nonce, last in `extra`
        // (`TransactionPoW.cpp:96`). Hard-coded so that the rule is checked in
        // one `cn_upx` rather than the ~20,000 a search costs.
        tx.prefix.extra =
            wrkz_primitives::tx::build_extra(&tx_public, None, Some(&16_903u64.to_le_bytes())).expect("extra");
        assert!(
            wrkz_pow::check_hash(&wrkz_pow::cn_upx(&tx.prefix.to_bytes()), difficulty),
            "the recorded nonce is still the work this prefix needs"
        );
        // The proof of work passes now; what stops this transaction is the ring
        // member that does not exist, the next check along.
        assert_eq!(
            validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &ctx).unwrap_err().rule().unwrap(),
            &TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 }
        );
    }

    /// In a **block**, the transaction proof of work is deferred into the batch
    /// with the ring signatures, and is still reported ahead of the step-10
    /// rule that follows it — as the sequential validator reports it — at every
    /// thread count.
    #[test]
    fn a_block_transactions_proof_of_work_is_batched_and_still_reported_first() {
        let difficulty = transaction_pow_difficulty(TX_POW_HEIGHT, false, 1, 1).expect("above TRANSACTION_POW_HEIGHT");
        let cp = Checkpoints::none();
        let block_ctx = ctx(&cp, TX_POW_HEIGHT);
        assert!(!block_ctx.is_pool_transaction);
        let (mut tx, tx_public) = tx_pow_transaction();
        let blob = vec![0u8; 300];
        let step_10 = TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 };

        // No nonce: the work fails, and the ring member that does not exist
        // (step 10) fails after it. The C++ stops at the work.
        tx.prefix.extra = wrkz_primitives::tx::build_extra(&tx_public, None, None).expect("extra");
        let mut state = ValidatorState::new();
        let mut batch = BlockRingBatch::new();
        let held = validate_transaction_deferred(&tx, &blob, &mut state, &Empty, &block_ctx, 0, &mut batch);
        assert_eq!(held.unwrap_err().rule().unwrap(), &step_10, "step 10 is held back");
        assert_eq!(batch.tx_pow_checks(), 1, "the work was deferred, not run");
        for threads in [1, 2, 8] {
            assert_eq!(batch.first_invalid(threads), Some((0, TxRule::PowInvalid { difficulty })), "{threads} threads");
        }
        // The one-transaction entry point settles the batch first, so it says
        // the same.
        assert_eq!(
            validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &block_ctx)
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::PowInvalid { difficulty }
        );

        // With the recorded nonce the work passes and step 10 is what is left.
        tx.prefix.extra =
            wrkz_primitives::tx::build_extra(&tx_public, None, Some(&16_903u64.to_le_bytes())).expect("extra");
        assert_eq!(
            validate_transaction(&tx, &blob, &mut ValidatorState::new(), &Empty, &block_ctx)
                .unwrap_err()
                .rule()
                .unwrap(),
            &step_10
        );
    }

    #[test]
    #[ignore = "a real 20,000-difficulty search, 4-20 s; the nonce it finds is pinned in the test above"]
    fn a_transaction_proof_of_work_found_by_searching_is_accepted() {
        let difficulty = transaction_pow_difficulty(TX_POW_HEIGHT, false, 1, 1).expect("above TRANSACTION_POW_HEIGHT");
        let cp = Checkpoints::none();
        let ctx = tx_pow_context(&cp);
        let (mut tx, tx_public) = tx_pow_transaction();

        // One `cn_upx` of the serialized prefix per try. `cn_upx` is memory
        // hard, so the cores share the memory bandwidth rather than multiply
        // the rate; this is minutes of work on one core either way.
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16) as u64;
        let found = std::sync::atomic::AtomicU64::new(u64::MAX);
        std::thread::scope(|scope| {
            for t in 0..threads {
                let (found, tx, tx_public) = (&found, &tx, &tx_public);
                scope.spawn(move || {
                    let mut candidate = tx.clone();
                    let mut n = t;
                    while n < 1_000_000 {
                        if found.load(std::sync::atomic::Ordering::Relaxed) != u64::MAX {
                            return;
                        }
                        candidate.prefix.extra =
                            wrkz_primitives::tx::build_extra(tx_public, None, Some(&n.to_le_bytes())).expect("extra");
                        if wrkz_pow::check_hash(&wrkz_pow::cn_upx(&candidate.prefix.to_bytes()), difficulty) {
                            found.fetch_min(n, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        n += threads;
                    }
                });
            }
        });
        let nonce = found.load(std::sync::atomic::Ordering::Relaxed);
        assert_ne!(nonce, u64::MAX, "no nonce satisfied difficulty {difficulty} below 1,000,000");

        tx.prefix.extra =
            wrkz_primitives::tx::build_extra(&tx_public, None, Some(&nonce.to_le_bytes())).expect("extra");
        assert_eq!(
            validate_transaction(&tx, &vec![0u8; 300], &mut ValidatorState::new(), &Empty, &ctx)
                .unwrap_err()
                .rule()
                .unwrap(),
            &TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 },
            "a searched nonce gets the transaction past the proof of work"
        );
    }

    /// A chain that answers with the outputs it was given and nothing else.
    struct Outputs(std::collections::HashMap<(u64, u64), OutputRecord>);

    impl ChainAccess for Outputs {
        fn key_image_spent(&self, _: &Hash, _: u64) -> crate::Result<bool> {
            Ok(false)
        }
        fn key_output(&self, amount: u64, global_index: u64) -> crate::Result<Option<OutputRecord>> {
            Ok(self.0.get(&(amount, global_index)).copied())
        }
        fn top_block_timestamp(&self) -> u64 {
            1_788_929_495
        }
        fn now(&self) -> u64 {
            1_788_929_495
        }
    }

    /// A transaction with `inputs` key inputs, each a ring of two, every ring
    /// member a real key and every signature a real signature over this
    /// transaction's own prefix hash — so it passes step 10 as it stands and a
    /// test can then break exactly the inputs it wants to break.
    ///
    /// The rings live at global indexes `(4*i, 4*i + 1)` of amount 100,000, so
    /// no two inputs share a ring member and the relative offsets are legal.
    fn signed_spend(inputs: usize) -> (Transaction, Outputs) {
        let mut outputs = std::collections::HashMap::new();
        let mut secrets = Vec::with_capacity(inputs);
        let mut tx = Transaction::default();
        tx.prefix.version = 1;
        tx.prefix.unlock_time = 4_213_665;
        for i in 0..inputs as u64 {
            let (sec, pk) = wrkz_pow::curve::generate_keys();
            let (_, decoy) = wrkz_pow::curve::generate_keys();
            let image = wrkz_pow::curve::generate_key_image(&pk, &sec);
            let record = |key| OutputRecord {
                public_key: key,
                unlock_time: 0,
                transaction_hash: [0; 32],
                output_index: 0,
                block_index: 1,
            };
            outputs.insert((100_000, 4 * i), record(pk));
            outputs.insert((100_000, 4 * i + 1), record(decoy));
            tx.prefix.inputs.push(Input::Key { amount: 100_000, key_offsets: vec![4 * i, 1], key_image: image });
            secrets.push((sec, vec![pk, decoy], image));
        }
        let (_, out_key) = wrkz_pow::curve::generate_keys();
        tx.prefix.outputs.push(Output { amount: 100_000 * inputs as u64 - 50_000, key: out_key });

        // The prefix is final now, so its hash is what every ring signs.
        let prefix_hash = wrkz_pow::cn_fast_hash(&tx.prefix.to_bytes());
        for (sec, ring, image) in &secrets {
            tx.signatures.push(
                wrkz_pow::curve::generate_ring_signature(&prefix_hash, image, ring, sec, 0).expect("a real ring signs"),
            );
        }
        (tx, Outputs(outputs))
    }

    /// Break input `i`'s signature so the ring no longer verifies.
    fn break_signature(tx: &mut Transaction, i: usize) {
        tx.signatures[i][0][0] ^= 0x01;
    }

    fn run(tx: &Transaction, chain: &Outputs, cp: &Checkpoints, threads: usize) -> std::result::Result<(), TxRule> {
        validate_transaction_with_threads(
            tx,
            &[0u8; 300],
            &mut ValidatorState::new(),
            chain,
            &ctx(cp, 4_213_649),
            threads,
        )
        .map(|_| ())
        .map_err(|e| e.rule().cloned().expect("a rule, not a chain fault"))
    }

    /// The whole point of the parallel path: eight inputs, three of them
    /// signed wrongly, and every thread count names the same one — the lowest,
    /// which is where the sequential loop would have stopped.
    #[test]
    fn several_invalid_signatures_report_the_sequential_index_at_every_thread_count() {
        let cp = Checkpoints::none();
        let (mut tx, chain) = signed_spend(8);
        assert_eq!(run(&tx, &chain, &cp, 1), Ok(()), "the batch verifies before anything is broken");
        assert_eq!(run(&tx, &chain, &cp, 8), Ok(()));

        for i in [3usize, 5, 6] {
            break_signature(&mut tx, i);
        }
        // `threads: 1` is the reference: the plain sequential loop.
        assert_eq!(run(&tx, &chain, &cp, 1), Err(TxRule::InputInvalidSignatures { input: 3 }));
        for threads in 2..=16 {
            assert_eq!(
                run(&tx, &chain, &cp, threads),
                Err(TxRule::InputInvalidSignatures { input: 3 }),
                "{threads} threads must name the same input as the sequential path"
            );
        }
    }

    /// The gather/verify split must not let a later input's chain failure jump
    /// the queue in front of an earlier input's bad signature, nor the other
    /// way round.
    #[test]
    fn a_bad_signature_and_a_chain_failure_keep_their_cpp_order() {
        let cp = Checkpoints::none();

        // Input 2 is signed wrongly and input 5 names an output that does not
        // exist. The C++ loop reaches 2 first.
        let (mut tx, mut chain) = signed_spend(8);
        break_signature(&mut tx, 2);
        chain.0.remove(&(100_000, 4 * 5));
        for threads in 1..=8 {
            assert_eq!(
                run(&tx, &chain, &cp, threads),
                Err(TxRule::InputInvalidSignatures { input: 2 }),
                "{threads} threads"
            );
        }

        // The other way round: input 2 names a missing output and input 5 is
        // signed wrongly, so the missing output wins and input 5's signature is
        // never reported — even though it was gathered and could have been
        // verified first.
        let (mut tx, mut chain) = signed_spend(8);
        break_signature(&mut tx, 5);
        chain.0.remove(&(100_000, 4 * 2));
        for threads in 1..=8 {
            assert_eq!(
                run(&tx, &chain, &cp, threads),
                Err(TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 8 }),
                "{threads} threads"
            );
        }
    }

    /// A transaction below the parallel threshold still goes through the same
    /// code and reports the same thing.
    #[test]
    fn a_two_input_transaction_is_unaffected() {
        let cp = Checkpoints::none();
        let (mut tx, chain) = signed_spend(2);
        for threads in [1usize, 4, 16] {
            assert_eq!(run(&tx, &chain, &cp, threads), Ok(()), "{threads} threads");
        }
        break_signature(&mut tx, 1);
        for threads in [1usize, 4, 16] {
            assert_eq!(
                run(&tx, &chain, &cp, threads),
                Err(TxRule::InputInvalidSignatures { input: 1 }),
                "{threads} threads"
            );
        }
    }

    // -- the early-exit pool path ------------------------------------------

    /// A chain for comparing the early-exit path with the batched one:
    /// [`Outputs`], plus key images it reports as spent, one global index whose
    /// read faults, and a count of the ring reads made.
    struct Probe {
        outputs: Outputs,
        spent: HashSet<Hash>,
        fault_at: Option<u64>,
        ring_reads: std::cell::Cell<usize>,
    }

    impl ChainAccess for Probe {
        fn key_image_spent(&self, image: &Hash, _: u64) -> crate::Result<bool> {
            Ok(self.spent.contains(image))
        }
        fn key_output(&self, amount: u64, global_index: u64) -> crate::Result<Option<OutputRecord>> {
            if self.fault_at == Some(global_index) {
                return Err(crate::ChainError::Corrupt("probe fault".into()));
            }
            self.outputs.key_output(amount, global_index)
        }
        fn key_outputs(&self, amount: u64, global_indexes: &[u64]) -> crate::Result<Vec<Option<OutputRecord>>> {
            self.ring_reads.set(self.ring_reads.get() + 1);
            global_indexes.iter().map(|i| self.key_output(amount, *i)).collect()
        }
        fn top_block_timestamp(&self) -> u64 {
            1_788_929_495
        }
        fn now(&self) -> u64 {
            1_788_929_495
        }
    }

    fn probe(outputs: Outputs) -> Probe {
        Probe { outputs, spent: HashSet::new(), fault_at: None, ring_reads: std::cell::Cell::new(0) }
    }

    /// What a validation said: the rule, or `None` for a chain fault.
    type Verdict = std::result::Result<(), Option<TxRule>>;

    /// Run the early-exit path and the batched path (sequential and parallel)
    /// over one transaction, insist they agree — on the verdict, the rule and,
    /// on accept, the key images left in the state — and return the verdict.
    #[track_caller]
    fn both_paths(tx: &Transaction, blob: &[u8], chain: &Probe, ctx: &TxContext<'_>) -> Verdict {
        let mut early_state = ValidatorState::new();
        let early = validate_transaction_early_exit(tx, blob, &mut early_state, chain, ctx);
        let early_verdict: Verdict = early.as_ref().map(|_| ()).map_err(|e| e.rule().cloned());
        for threads in [1usize, 8] {
            let mut state = ValidatorState::new();
            let batch = validate_transaction_with_threads(tx, blob, &mut state, chain, ctx, threads);
            let batch_verdict: Verdict = batch.as_ref().map(|_| ()).map_err(|e| e.rule().cloned());
            assert_eq!(early_verdict, batch_verdict, "{threads} threads");
            if let (Ok(a), Ok(b)) = (&early, &batch) {
                assert_eq!(a, b, "the same fee and fusion flag");
                assert_eq!(early_state.spent_key_images, state.spent_key_images, "the same key images claimed");
            }
        }
        early_verdict
    }

    fn image_of(tx: &Transaction, i: usize) -> Hash {
        match &tx.prefix.inputs[i] {
            Input::Key { key_image, .. } => *key_image,
            Input::Base { .. } => unreachable!("signed_spend builds key inputs"),
        }
    }

    /// The pool's early-exit path and the block's batched path give the same
    /// verdict and name the same rule for every transaction here: valid ones,
    /// and ones broken at every step of the validator — alone, and in pairs
    /// whose C++ order decides which one is reported.
    #[test]
    fn early_exit_and_batch_paths_agree() {
        const H: u64 = 4_213_649;
        let cp = Checkpoints::none();
        let block_ctx = ctx(&cp, H);
        let pool_ctx = TxContext { is_pool_transaction: true, ..block_ctx };
        // Inside the zone step 10 does not run for either path.
        let mut zone = Checkpoints::none();
        zone.add((H + 51).try_into().expect("a block index"), [0; 32]);
        let zone_ctx = TxContext { is_pool_transaction: true, ..ctx(&zone, H) };
        let blob = [0u8; 300];
        fn sig(input: usize) -> Verdict {
            Err(Some(TxRule::InputInvalidSignatures { input }))
        }

        type Mutation = fn(&mut Transaction, &mut Probe);
        type Expected = fn(&Transaction) -> Verdict;
        let cases: Vec<(&str, Mutation, Expected)> = vec![
            ("valid", |_, _| {}, |_| Ok(())),
            ("bad signature on input 0", |tx, _| break_signature(tx, 0), |_| sig(0)),
            ("bad signature on the last input", |tx, _| break_signature(tx, 7), |_| sig(7)),
            (
                "bad signatures on inputs 2 and 5",
                |tx, _| {
                    break_signature(tx, 2);
                    break_signature(tx, 5);
                },
                |_| sig(2),
            ),
            (
                "a missing ring member below a bad signature",
                |tx, c| {
                    c.outputs.0.remove(&(100_000, 4 * 2));
                    break_signature(tx, 5);
                },
                |_| Err(Some(TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 8 })),
            ),
            (
                "a bad signature below a missing ring member",
                |tx, c| {
                    break_signature(tx, 2);
                    c.outputs.0.remove(&(100_000, 4 * 5));
                },
                |_| sig(2),
            ),
            (
                "a key image spent on chain below a bad signature",
                |tx, c| {
                    c.spent.insert(image_of(tx, 4));
                    break_signature(tx, 6);
                },
                |tx| Err(Some(TxRule::InputKeyImageAlreadySpent { key_image: image_of(tx, 4) })),
            ),
            (
                "a bad signature below a key image spent on chain",
                |tx, c| {
                    break_signature(tx, 1);
                    c.spent.insert(image_of(tx, 4));
                },
                |_| sig(1),
            ),
            (
                "a chain fault below a bad signature",
                |tx, c| {
                    c.fault_at = Some(4 * 3);
                    break_signature(tx, 5);
                },
                |_| Err(None),
            ),
            (
                "a bad signature below a chain fault",
                |tx, c| {
                    break_signature(tx, 1);
                    c.fault_at = Some(4 * 3);
                },
                |_| sig(1),
            ),
            (
                "a locked ring member below a bad signature",
                |tx, c| {
                    c.outputs.0.get_mut(&(100_000, 4 * 2 + 1)).expect("a decoy").unlock_time = 9_000_000;
                    break_signature(tx, 6);
                },
                |_| Err(Some(TxRule::InputSpendLockedOut { amount: 100_000, global_index: 9, unlock_time: 9_000_000 })),
            ),
            (
                "a short signature vector",
                |tx, _| {
                    tx.signatures[3].pop();
                },
                |_| Err(Some(TxRule::InputInvalidSignaturesCount { expected: 2, got: 1 })),
            ),
            (
                "a bad signature below a short signature vector",
                |tx, _| {
                    break_signature(tx, 1);
                    tx.signatures[3].pop();
                },
                |_| sig(1),
            ),
            // The rest change the prefix, so every ring signature fails too;
            // the earlier step must still be the one reported.
            (
                "a key image outside the prime-order subgroup",
                |tx, _| {
                    if let Input::Key { key_image, .. } = &mut tx.prefix.inputs[6] {
                        *key_image = bad_key_image();
                    }
                },
                |_| Err(Some(TxRule::InputInvalidDomainKeyImages)),
            ),
            (
                "an output key that does not decompress",
                |tx, _| tx.prefix.outputs[0].key = BAD_OUTPUT_KEY,
                |_| Err(Some(TxRule::OutputInvalidKey)),
            ),
            (
                "a fee below the ladder",
                |tx, _| tx.prefix.outputs[0].amount = 8 * 100_000 - 1,
                |_| {
                    Err(Some(TxRule::WrongFee { fee: 1, minimum: wrkz_primitives::fees::required_minimum_fee(300, H) }))
                },
            ),
            (
                "an unlock time too soon",
                |tx, _| tx.prefix.unlock_time = H + 1,
                |_| Err(Some(TxRule::UnlockTimeTooSmall { unlock_time: H + 1, minimum: H + 15 })),
            ),
        ];

        for (name, mutate, expected) in cases {
            let (mut tx, outputs) = signed_spend(8);
            let mut chain = probe(outputs);
            mutate(&mut tx, &mut chain);
            let want = expected(&tx);
            assert_eq!(both_paths(&tx, &blob, &chain, &pool_ctx), want, "{name}, pool");
            assert_eq!(both_paths(&tx, &blob, &chain, &block_ctx), want, "{name}, block");
            // Inside the zone only the verdicts are compared: step 10 is
            // skipped, so the step-10 cases above are accepted there.
            let _ = both_paths(&tx, &blob, &chain, &zone_ctx);
        }

        // The transaction proof of work, where the fee cannot stand in for it.
        let empty = probe(Outputs(Default::default()));
        let pow_ctx = tx_pow_context(&cp);
        let difficulty = transaction_pow_difficulty(TX_POW_HEIGHT, false, 1, 1).expect("above TRANSACTION_POW_HEIGHT");
        let (mut tx, tx_public) = tx_pow_transaction();
        tx.prefix.extra = wrkz_primitives::tx::build_extra(&tx_public, None, None).expect("extra");
        for c in [pow_ctx, ctx(&cp, TX_POW_HEIGHT)] {
            assert_eq!(both_paths(&tx, &blob, &empty, &c), Err(Some(TxRule::PowInvalid { difficulty })));
        }
        tx.prefix.extra =
            wrkz_primitives::tx::build_extra(&tx_public, None, Some(&16_903u64.to_le_bytes())).expect("extra");
        for c in [pow_ctx, ctx(&cp, TX_POW_HEIGHT)] {
            assert_eq!(
                both_paths(&tx, &blob, &empty, &c),
                Err(Some(TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 7 }))
            );
        }
    }

    /// The point of the early-exit path: a bad signature on input 0 of eight
    /// costs one ring read, not eight.
    #[test]
    fn early_exit_stops_reading_rings_at_the_first_bad_signature() {
        let cp = Checkpoints::none();
        let pool_ctx = TxContext { is_pool_transaction: true, ..ctx(&cp, 4_213_649) };
        let (mut tx, outputs) = signed_spend(8);
        break_signature(&mut tx, 0);
        let chain = probe(outputs);

        let early = validate_transaction_early_exit(&tx, &[0u8; 300], &mut ValidatorState::new(), &chain, &pool_ctx);
        assert_eq!(early.unwrap_err().rule(), Some(&TxRule::InputInvalidSignatures { input: 0 }));
        assert_eq!(chain.ring_reads.get(), 1, "input 0's ring, and nothing after it");

        chain.ring_reads.set(0);
        let batched = validate_transaction(&tx, &[0u8; 300], &mut ValidatorState::new(), &chain, &pool_ctx);
        assert_eq!(batched.unwrap_err().rule(), Some(&TxRule::InputInvalidSignatures { input: 0 }));
        assert_eq!(chain.ring_reads.get(), 8, "the batched path reads every ring before it verifies one");
    }

    #[test]
    fn unlock_semantics() {
        let chain = Empty;
        // Block index branch: a coinbase at index 100 unlocks at 140, so it is
        // spendable in a block whose previous index is 139.
        assert!(!is_spend_time_unlocked(&chain, 140, 138));
        assert!(is_spend_time_unlocked(&chain, 140, 139));
        // Unix time branch from 600,000: the *tip* timestamp plus 60.
        assert!(is_spend_time_unlocked(&chain, 1_788_929_555, 4_213_649));
        assert!(!is_spend_time_unlocked(&chain, 1_788_929_556, 4_213_649));
    }

    // -- block-level batching ----------------------------------------------

    /// `txs` transactions of `inputs` key inputs each, every ring a real ring
    /// of two and every signature a real signature over its own transaction's
    /// prefix hash — a block that validates as it stands, so a test can break
    /// exactly the transactions and inputs it wants to break.
    ///
    /// Ring members are laid out four global indexes apart across the *whole
    /// block*, so no two inputs of any two transactions share one.
    fn signed_block(txs: usize, inputs: usize) -> (Vec<Transaction>, Outputs) {
        let mut outputs = std::collections::HashMap::new();
        let mut built = Vec::with_capacity(txs);
        let mut next: u64 = 0;
        for _ in 0..txs {
            let mut secrets = Vec::with_capacity(inputs);
            let mut tx = Transaction::default();
            tx.prefix.version = 1;
            tx.prefix.unlock_time = 4_213_665;
            for _ in 0..inputs {
                let (sec, pk) = wrkz_pow::curve::generate_keys();
                let (_, decoy) = wrkz_pow::curve::generate_keys();
                let image = wrkz_pow::curve::generate_key_image(&pk, &sec);
                let record = |key| OutputRecord {
                    public_key: key,
                    unlock_time: 0,
                    transaction_hash: [0; 32],
                    output_index: 0,
                    block_index: 1,
                };
                outputs.insert((100_000, next), record(pk));
                outputs.insert((100_000, next + 1), record(decoy));
                tx.prefix.inputs.push(Input::Key { amount: 100_000, key_offsets: vec![next, 1], key_image: image });
                secrets.push((sec, vec![pk, decoy], image));
                next += 4;
            }
            let (_, out_key) = wrkz_pow::curve::generate_keys();
            tx.prefix.outputs.push(Output { amount: 100_000 * inputs as u64 - 50_000, key: out_key });

            // The prefix is final now, so its hash is what every ring signs.
            let prefix_hash = wrkz_pow::cn_fast_hash(&tx.prefix.to_bytes());
            for (sec, ring, image) in &secrets {
                tx.signatures.push(
                    wrkz_pow::curve::generate_ring_signature(&prefix_hash, image, ring, sec, 0)
                        .expect("a real ring signs"),
                );
            }
            built.push(tx);
        }
        (built, Outputs(outputs))
    }

    /// The contract [`crate::ChainState::add_block`] implements, in miniature:
    /// gather every transaction of the block into one batch in index order,
    /// stop at the first non-signature failure and hold it back, then let the
    /// lowest failing `(transaction, input)` decide what is reported.
    ///
    /// Returns the reported `(transaction index, rule)`.
    fn run_block(
        txs: &[Transaction],
        chain: &Outputs,
        cp: &Checkpoints,
        threads: usize,
    ) -> std::result::Result<(), (usize, TxRule)> {
        let ctx = ctx(cp, 4_213_649);
        let mut state = ValidatorState::new();
        let mut batch = BlockRingBatch::new();
        let mut held_back: Option<(usize, TxRule)> = None;
        for (i, tx) in txs.iter().enumerate() {
            if let Err(e) = validate_transaction_deferred(tx, &[0u8; 300], &mut state, chain, &ctx, i, &mut batch) {
                held_back = Some((i, e.rule().cloned().expect("a rule, not a chain fault")));
                break;
            }
        }
        if let Some(failure) = batch.first_invalid(threads) {
            return Err(failure);
        }
        match held_back {
            Some(h) => Err(h),
            None => Ok(()),
        }
    }

    /// The point of the block-wide batch: six transactions of two inputs each
    /// — the shape that used to fall below the parallel threshold every single
    /// time — verified as one batch of twelve rings.
    #[test]
    fn a_block_of_two_input_transactions_verifies_as_one_batch() {
        let cp = Checkpoints::none();
        let (txs, chain) = signed_block(6, 2);
        for threads in [1usize, 2, 4, 8, 16] {
            assert_eq!(run_block(&txs, &chain, &cp, threads), Ok(()), "{threads} threads");
        }
    }

    /// Several transactions of the block are signed wrongly. Every thread
    /// count must name the **lowest** `(transaction, input)` pair, which is
    /// where the sequential C++ loop would have stopped — and must agree with
    /// `threads: 1`, the sequential reference.
    #[test]
    fn a_block_reports_the_lowest_transaction_and_input_at_every_thread_count() {
        let cp = Checkpoints::none();
        let (mut txs, chain) = signed_block(5, 3);
        assert_eq!(run_block(&txs, &chain, &cp, 8), Ok(()), "the block verifies before anything is broken");

        // Break (1, 2), (3, 0) and (3, 1). The lowest pair is (1, 2).
        break_signature(&mut txs[1], 2);
        break_signature(&mut txs[3], 0);
        break_signature(&mut txs[3], 1);
        let reference = run_block(&txs, &chain, &cp, 1);
        assert_eq!(reference, Err((1, TxRule::InputInvalidSignatures { input: 2 })), "the sequential reference");
        for threads in 2..=16 {
            assert_eq!(run_block(&txs, &chain, &cp, threads), reference, "{threads} threads");
        }

        // Every single-failure position in turn, against the sequential path:
        // a failure in the first transaction lets the workers abandon almost
        // everything, one in the last lets them abandon nothing.
        for tx_index in 0..5 {
            for input in 0..3 {
                let (mut txs, chain) = signed_block(5, 3);
                break_signature(&mut txs[tx_index], input);
                let reference = run_block(&txs, &chain, &cp, 1);
                assert_eq!(
                    reference,
                    Err((tx_index, TxRule::InputInvalidSignatures { input })),
                    "the sequential reference for ({tx_index}, {input})"
                );
                for threads in [2usize, 3, 8, 16] {
                    assert_eq!(run_block(&txs, &chain, &cp, threads), reference, "({tx_index}, {input}), {threads}");
                }
            }
        }
    }

    /// A chain fault in one transaction against a bad signature in an earlier
    /// and in a later one. The C++ order decides both, and the block-wide batch
    /// must not let a later transaction's signature jump the queue.
    #[test]
    fn a_block_mixing_a_chain_fault_and_bad_signatures_keeps_the_cpp_order() {
        let cp = Checkpoints::none();

        // Transaction 2's input 1 names an output that does not exist, and
        // transaction 0's input 1 is signed wrongly. The C++ loop reaches
        // (0, 1) first, so that is what is reported — even though the missing
        // output was discovered while gathering, before a single signature was
        // verified.
        let (mut txs, mut chain) = signed_block(4, 2);
        break_signature(&mut txs[0], 1);
        // Transaction 2, input 1: global indexes 4*5 and 4*5 + 1.
        chain.0.remove(&(100_000, 4 * 5 + 1));
        for threads in 1..=8 {
            assert_eq!(
                run_block(&txs, &chain, &cp, threads),
                Err((0, TxRule::InputInvalidSignatures { input: 1 })),
                "{threads} threads"
            );
        }

        // The other way round: the missing output is still in transaction 2,
        // but the bad signature is now in transaction 3, above it. The C++
        // loop stops at transaction 2 and never looks at transaction 3, so the
        // missing output wins and transaction 3's signature is never reported
        // — it is never even gathered.
        let (mut txs, mut chain) = signed_block(4, 2);
        break_signature(&mut txs[3], 0);
        chain.0.remove(&(100_000, 4 * 5 + 1));
        for threads in 1..=8 {
            assert_eq!(
                run_block(&txs, &chain, &cp, threads),
                Err((2, TxRule::InputInvalidGlobalIndex { amount: 100_000, global_index: 4 * 5 + 1 })),
                "{threads} threads"
            );
        }
    }

    /// Settling the batch in pieces — what the [`RING_BATCH_MEMBER_LIMIT`]
    /// guard does on a block too large to hold in one — reports the same pair
    /// as settling it whole.
    #[test]
    fn settling_the_batch_in_pieces_reports_the_same_pair() {
        let cp = Checkpoints::none();
        let ctx = ctx(&cp, 4_213_649);
        let (mut txs, chain) = signed_block(6, 2);
        break_signature(&mut txs[4], 1);
        assert_eq!(
            run_block(&txs, &chain, &cp, 8),
            Err((4, TxRule::InputInvalidSignatures { input: 1 })),
            "settled whole"
        );

        // The same block, settled after every transaction.
        for threads in [1usize, 4, 8] {
            let mut state = ValidatorState::new();
            let mut batch = BlockRingBatch::new();
            let mut found = None;
            for (i, tx) in txs.iter().enumerate() {
                validate_transaction_deferred(tx, &[0u8; 300], &mut state, &chain, &ctx, i, &mut batch)
                    .expect("nothing but a signature is wrong here");
                if let Some(pair) = batch.first_invalid(threads) {
                    found = Some(pair);
                    break;
                }
                batch.clear();
                assert!(batch.is_empty() && batch.members() == 0);
            }
            assert_eq!(
                found,
                Some((4, TxRule::InputInvalidSignatures { input: 1 })),
                "{threads} threads, settled per transaction"
            );
        }
    }

    /// Not a test of speed — a measurement, in the shape the operator's slow
    /// stretch actually has: 500 transactions of two key inputs each, 1,000
    /// ring signatures, which used to be 500 separate batches of two and so
    /// 500 trips down the sequential fallback.
    ///
    /// It asserts only that every thread count agrees; the timings are printed
    /// for whoever ran it.
    #[test]
    #[ignore = "a measurement, not a test; run with --release --ignored --nocapture"]
    fn a_block_of_500_two_input_transactions() {
        use std::time::Instant;

        const TXS: usize = 500;
        const INPUTS: usize = 2;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let threads = wrkz_pow::parallel::default_threads();
        let cp = Checkpoints::none();
        let ctx = ctx(&cp, 4_213_649);
        let (txs, chain) = signed_block(TXS, INPUTS);

        // Phase one, on this thread, exactly as `add_block` runs it.
        let mut state = ValidatorState::new();
        let mut batch = BlockRingBatch::new();
        let t = Instant::now();
        for (i, tx) in txs.iter().enumerate() {
            validate_transaction_deferred(tx, &[0u8; 300], &mut state, &chain, &ctx, i, &mut batch)
                .expect("this block validates");
        }
        let gather_secs = t.elapsed().as_secs_f64();
        // The batch also holds the domain, output-key and transaction
        // proof-of-work checks; the rings are what this measures.
        assert_eq!(batch.ring_checks(), TXS * INPUTS);

        let t = Instant::now();
        let sequential = batch.first_invalid(1);
        let seq_secs = t.elapsed().as_secs_f64();
        assert_eq!(sequential, None, "every ring in this block is valid");

        // What the old code did: one batch per transaction, every one of them
        // below the parallel threshold and so on the sequential path.
        let per_tx_secs = {
            let t = Instant::now();
            for (i, tx) in txs.iter().enumerate() {
                let mut one = BlockRingBatch::new();
                let mut state = ValidatorState::new();
                validate_transaction_deferred(tx, &[0u8; 300], &mut state, &chain, &ctx, i, &mut one)
                    .expect("this block validates");
                assert_eq!(one.first_invalid(threads), None);
            }
            t.elapsed().as_secs_f64()
        } - gather_secs;

        println!("\n== one block: {TXS} transactions of {INPUTS} key inputs, ring size 2 ==");
        println!("{cores} logical cores, {threads} validation threads by default");
        println!(
            "{} rings, {} ring members, {} checks in all ({} key-image domain, {} output key, {} transaction \
             proof of work), gathered in {gather_secs:.3}s",
            batch.ring_checks(),
            batch.members(),
            batch.len(),
            batch.key_image_checks(),
            batch.output_key_checks(),
            batch.tx_pow_checks()
        );
        println!("\n{:>9}  {:>9}  {:>11}  {:>8}", "threads", "seconds", "rings/s", "speedup");
        let rings = batch.ring_checks() as f64;
        let row = |what: &str, secs: f64| {
            println!("{what:>9}  {secs:>9.3}  {:>11.0}  {:>7.2}x", rings / secs.max(1e-9), seq_secs / secs.max(1e-9));
        };
        row("per tx", per_tx_secs);
        row("1", seq_secs);
        let mut n = 2;
        while n < threads {
            let t = Instant::now();
            let r = batch.first_invalid(n);
            let secs = t.elapsed().as_secs_f64();
            assert_eq!(r, sequential, "{n} threads must agree with the sequential path");
            row(&n.to_string(), secs);
            n *= 2;
        }
        let t = Instant::now();
        let parallel = batch.first_invalid(threads);
        let par_secs = t.elapsed().as_secs_f64();
        assert_eq!(parallel, sequential, "the two paths must agree");
        row(&threads.to_string(), par_secs);
        println!(
            "\n\"per tx\" is the old shape: one batch per transaction, two rings each, every one of\n\
             them below the parallel threshold and so verified on this thread alone. The gather\n\
             phase is subtracted from it, so the three rows measure the same work.\n"
        );
    }
}
