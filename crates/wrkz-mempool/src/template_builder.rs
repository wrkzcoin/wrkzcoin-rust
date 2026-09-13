// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Block templates: `Core::getBlockTemplate` (`Core.cpp:2313`),
//! `Core::fillBlockTemplate` (`Core.cpp:4353`), the coinbase rebuild loop
//! (`Core.cpp:2472-2560`), the `reserved_offset` search
//! (`RpcServer.cpp:1656-1682`) and `Core::submitBlock` (`Core.cpp:1952`).
//!
//! The bytes matter. A pool that can tell our templates from the C++
//! daemon's can tell our node from a C++ one. The quirk is listed in spec/12
//! ("Things that look like bugs and are not"); the bytes are these: the parent
//! block of a daemon template carries major version **0** (`Core.cpp:2365`
//! overwrites the 1 assigned on the line above with `BLOCK_MINOR_VERSION_0`),
//! minor 0, `transactionCount = 1`, empty branches, and a
//! *default-constructed* coinbase — version 0, unlock 0, no inputs, no outputs
//! — whose whole `extra` is an empty merge-mining tag (depth 0, a zero root)
//! for the miner to fix up.

use crate::chain::{ContextError, PoolChain, TemplateContext, TemplateContextCache};
use crate::coinbase::{construct_miner_tx, MinerTxError};
use crate::pool::{spends_on_chain, template_revalidate_context, valid_for_block_template, TransactionPool};
use std::collections::HashSet;
use wrkz_chain::{AddOutcome, ChainState, Rule};
use wrkz_primitives::base58;
use wrkz_primitives::block::{BlockTemplate, ParentBlock, BLOCK_MAJOR_VERSION_2};
use wrkz_primitives::constants::*;
use wrkz_primitives::tx::{
    append_merge_mining_tag, BaseTransaction, Input, MergeMiningTag, Transaction, TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_storage::KvStore;

/// `TRIES_COUNT` (`Core.cpp:2473`).
pub const COINBASE_REBUILD_TRIES: usize = 10;
/// `maxOuts` for a block coinbase (`Core.cpp:2461`).
pub const COINBASE_MAX_OUTS: usize = COINBASE_MAX_OUTPUTS;
/// `TX_EXTRA_NONCE_MAX_COUNT`, and the RPC's own `reserve_size` cap
/// (`RpcServer.cpp:1597`).
pub const MAX_RESERVE_SIZE: usize = 255;

/// One pool transaction chosen for a template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateTransaction {
    pub hash: Hash,
    pub blob: Vec<u8>,
    pub fee: u64,
}

/// Knobs that let a template be reproduced byte for byte.
///
/// Neither exists in the C++, which calls `generateKeyPair()` and
/// `time(nullptr)` directly. Both default to exactly that.
#[derive(Clone, Debug, Default)]
pub struct TemplateOptions {
    /// The coinbase transaction key pair `(secret, public)`. `None` generates
    /// one, as `Currency::constructMinerTx` does.
    pub tx_key: Option<(Hash, Hash)>,
    /// `time(nullptr)`. Still clamped up to the timestamp median.
    pub now: Option<u64>,
}

/// What `getblocktemplate` answers with, plus the parsed template.
#[derive(Clone, Debug)]
pub struct BlockTemplateResult {
    /// `blocktemplate_blob`.
    pub blob: Vec<u8>,
    /// The same bytes, parsed.
    pub block: BlockTemplate,
    /// `difficulty`.
    pub difficulty: u64,
    /// `height`.
    pub height: u64,
    /// `reserved_offset`: the byte offset of the reserved bytes inside `blob`,
    /// or 0 when nothing was reserved.
    pub reserved_offset: u64,
    /// How many bytes were reserved.
    pub reserve_size: usize,
    /// The transaction public key in the coinbase `extra`, which is what the
    /// offset search looks for.
    pub tx_public_key: Hash,
    /// The reward the coinbase pays: base + fees, penalised for size.
    pub reward: u64,
    /// The cumulative block size the reward was computed for.
    pub cumulative_size: u64,
    /// Total fee of the included transactions.
    pub fee: u64,
}

/// Everything `getblocktemplate` can fail with.
#[derive(Debug)]
pub enum TemplateError {
    /// `-4`: `validateAddresses` refused the wallet address.
    BadAddress(base58::Base58Error),
    /// `-3`: "Too big reserved size, maximum allowed is 255".
    ReserveTooBig(usize),
    /// The chain could not answer, or has no difficulty for the next block.
    Context(ContextError),
    /// `constructMinerTx` returned false.
    MinerTx(MinerTxError),
    /// The template did not serialize (a structural impossibility, kept
    /// because `toBinaryArray` can throw).
    Serialization(wrkz_primitives::Error),
    /// The rebuild loop ran out of tries: "Failed to create block template"
    /// (`Core.cpp:2562`).
    CoinbaseUnstable,
    /// The rebuild loop hit one of the two `unexpected case:` bail-outs
    /// (`Core.cpp:2517`, `Core.cpp:2548`).
    CoinbaseSizeMismatch { cumulative_size: u64, transactions_size: u64, coinbase_size: u64 },
    /// `-5`: "not enough space for reserved bytes" (`RpcServer.cpp:1676`).
    ReserveDoesNotFit { reserved_offset: u64, reserve_size: usize, blob_len: usize },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::BadAddress(e) => write!(f, "wallet address: {e:?}"),
            TemplateError::ReserveTooBig(n) => {
                write!(f, "Too big reserved size, maximum allowed is {MAX_RESERVE_SIZE} (got {n})")
            }
            TemplateError::Context(e) => write!(f, "{e}"),
            TemplateError::MinerTx(e) => write!(f, "Failed to construct miner transaction: {e}"),
            TemplateError::Serialization(e) => write!(f, "block template does not serialize: {e}"),
            TemplateError::CoinbaseUnstable => write!(f, "Failed to create block template"),
            TemplateError::CoinbaseSizeMismatch { cumulative_size, transactions_size, coinbase_size } => write!(
                f,
                "unexpected case: cumulative_size={cumulative_size} is not equal \
                 txs_cumulative_size={transactions_size} + get_object_blobsize(b.baseTransaction)={coinbase_size}"
            ),
            TemplateError::ReserveDoesNotFit { .. } => {
                write!(f, "Internal error: failed to create block template, not enough space for reserved bytes")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

impl From<ContextError> for TemplateError {
    fn from(e: ContextError) -> Self {
        TemplateError::Context(e)
    }
}

impl From<MinerTxError> for TemplateError {
    fn from(e: MinerTxError) -> Self {
        TemplateError::MinerTx(e)
    }
}

/// `RpcServer::getBlockTemplate` + `Core::getBlockTemplate`: pick the
/// transactions, build the block, place the reserve.
///
/// `reserve_size` is the RPC's `reserve_size` (that many zero bytes are
/// reserved); `extra_nonce`, when given, is the RPC's `extra_nonce` parameter
/// and wins over `reserve_size`, exactly as the `else if` at
/// `RpcServer.cpp:1593` orders them.
pub fn build_template<C: PoolChain + ?Sized>(
    chain: &C,
    pool: &mut TransactionPool,
    wallet_address: &str,
    reserve_size: usize,
    extra_nonce: Option<&[u8]>,
    options: &TemplateOptions,
) -> Result<BlockTemplateResult, TemplateError> {
    let ctx = TemplateContext::from_chain(chain)?;
    let selected = fill_block_template(pool, chain, &ctx);
    build_template_from_context(&ctx, &selected, wallet_address, reserve_size, extra_nonce, options)
}

/// [`build_template`], reading the chain's tip-derived numbers from `cache`
/// instead of off the chain when the tip has not moved.
///
/// The answer is the one [`build_template`] gives: the cache holds a
/// [`TemplateContext`], which is a function of the tip alone, and everything
/// that varies per call — the transaction selection, the coinbase key, the
/// timestamp — is still done here. See [`TemplateContextCache`].
pub fn build_template_cached<C: PoolChain + ?Sized>(
    chain: &C,
    pool: &mut TransactionPool,
    cache: &TemplateContextCache,
    wallet_address: &str,
    reserve_size: usize,
    extra_nonce: Option<&[u8]>,
    options: &TemplateOptions,
) -> Result<BlockTemplateResult, TemplateError> {
    let ctx = cache.context(chain)?;
    let selected = fill_block_template(pool, chain, &ctx);
    build_template_from_context(&ctx, &selected, wallet_address, reserve_size, extra_nonce, options)
}

/// `Core::fillBlockTemplate` (`Core.cpp:4353`).
///
/// Fee-paying transactions in priority order first, then the fee-less ones,
/// while the total stays inside
/// `min(1.25 · medianSize, maxBlockCumulativeSize) − 600` bytes; each is
/// revalidated at the **template's** height and dropped from the pool if it
/// fails, or if the chain has already spent one of its key images; and no two
/// included transactions may spend the same key image.
pub fn fill_block_template<C: PoolChain + ?Sized>(
    pool: &mut TransactionPool,
    chain: &C,
    ctx: &TemplateContext,
) -> Vec<TemplateTransaction> {
    // `(125 * medianSize) / 100`, then min with the hard cap, then the coinbase
    // reservation. The C++ subtracts on a `size_t`; a median small enough to
    // make this wrap cannot occur, because the median is at least the granted
    // full reward zone (10,000 at its smallest).
    let max_total_size = ((125 * ctx.median_size) / 100)
        .min(ctx.max_cumulative_size)
        .saturating_sub(CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64);

    let rctx = template_revalidate_context(chain, ctx.height);
    let mut selected: Vec<TemplateTransaction> = Vec::new();
    let mut invalid: Vec<Hash> = Vec::new();
    // `TransactionSpentInputsChecker`: note that it inserts as it walks, so a
    // transaction rejected on its second input has already contributed its
    // first (`Core.cpp:108`).
    let mut already_spent: HashSet<Hash> = HashSet::new();
    let mut transactions_size: u64 = 0;

    {
        let (regular, fusion) = pool.for_block_template();
        for entry in regular.into_iter().chain(fusion) {
            if transactions_size + entry.size() as u64 > max_total_size {
                continue;
            }
            if !valid_for_block_template(entry, &rctx) {
                invalid.push(entry.hash);
                continue;
            }
            // Not in the C++, whose revalidation reads no chain state
            // (`Core.cpp:4333`): a key image the chain already holds as spent
            // at or below the template's parent makes the block invalid, so
            // the transaction is dropped from the pool like any other that
            // fails here. This is the second line of defence behind
            // `TransactionPool::remove_spent_in_chain`, which a chain switch
            // should already have run. A read that fails leaves the
            // transaction in the pool but out of this template — a template
            // must not carry what cannot be confirmed.
            match spends_on_chain(chain, &entry.transaction, ctx.height.saturating_sub(1)) {
                Ok(false) => {}
                Ok(true) => {
                    invalid.push(entry.hash);
                    continue;
                }
                Err(_) => continue,
            }
            let mut collides = false;
            for input in &entry.transaction.prefix.inputs {
                if let Input::Key { key_image, .. } = input {
                    if !already_spent.insert(*key_image) {
                        collides = true;
                        break;
                    }
                }
            }
            if collides {
                continue;
            }
            transactions_size += entry.size() as u64;
            selected.push(TemplateTransaction { hash: entry.hash, blob: entry.blob.clone(), fee: entry.fee });
        }
    }

    for hash in invalid {
        pool.remove(&hash);
    }
    selected
}

/// The half of `Core::getBlockTemplate` that does not touch the pool, over an
/// explicit [`TemplateContext`].
///
/// Split out so that a template can be built for a height whose chain state we
/// do not hold — which is what the byte comparison against the live daemon
/// needs.
pub fn build_template_from_context(
    ctx: &TemplateContext,
    transactions: &[TemplateTransaction],
    wallet_address: &str,
    reserve_size: usize,
    extra_nonce: Option<&[u8]>,
    options: &TemplateOptions,
) -> Result<BlockTemplateResult, TemplateError> {
    let address = base58::parse_address(wallet_address).map_err(TemplateError::BadAddress)?;

    // `RpcServer.cpp:1560-1611`: `extra_nonce` (hex) wins over `reserve_size`,
    // and either way `blobReserve` is what ends up in the coinbase.
    let reserve: Vec<u8> = match extra_nonce {
        Some(n) => n.to_vec(),
        None => vec![0u8; reserve_size],
    };
    if reserve.len() > MAX_RESERVE_SIZE {
        return Err(TemplateError::ReserveTooBig(reserve.len()));
    }

    let tx_keys = options.tx_key.unwrap_or_else(wrkz_pow::curve::generate_keys);
    let now = options.now.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    });

    let transactions_size: u64 = transactions.iter().map(|t| t.blob.len() as u64).sum();
    let fee: u64 = transactions.iter().fold(0u64, |a, t| a.wrapping_add(t.fee));

    let mut block = BlockTemplate {
        major_version: ctx.major_version,
        minor_version: ctx.minor_version,
        timestamp: ctx.clamp_timestamp(now),
        previous_block_hash: ctx.previous_block_hash,
        // `b = BlockTemplate {}` leaves the nonce at 0; the miner fills it in.
        nonce: 0,
        parent_block: None,
        base_transaction: Transaction::default(),
        transaction_hashes: transactions.iter().map(|t| t.hash).collect(),
    };
    if ctx.major_version >= BLOCK_MAJOR_VERSION_2 {
        block.parent_block = Some(daemon_template_parent_block());
    }

    let build = |current_block_size: u64| {
        construct_miner_tx(
            ctx.major_version,
            ctx.height,
            ctx.median_size,
            ctx.already_generated_coins,
            current_block_size,
            fee,
            &address.view_public_key,
            &address.spend_public_key,
            &reserve,
            COINBASE_MAX_OUTS,
            tx_keys,
        )
    };

    // "two-phase miner transaction generation" (`Core.cpp:2443`): the reward
    // depends on the block size, which depends on the coinbase size.
    let (mut coinbase, mut miner) = build(transactions_size)?;
    let mut cumulative_size = transactions_size + coinbase_size(&coinbase);

    let mut settled = false;
    for _ in 0..COINBASE_REBUILD_TRIES {
        let built = build(cumulative_size)?;
        coinbase = built.0;
        miner = built.1;
        let blob_size = coinbase_size(&coinbase);
        let budget = cumulative_size - transactions_size;
        if blob_size > budget {
            cumulative_size = transactions_size + blob_size;
            continue;
        }
        if blob_size < budget {
            // Pad `extra` with zeroes to exactly fill the budget. One byte of
            // slack is possible because the extra's own length is a varint.
            let delta = budget - blob_size;
            coinbase.prefix.extra.extend(std::iter::repeat_n(0u8, delta as usize));
            if cumulative_size != transactions_size + coinbase_size(&coinbase) {
                if cumulative_size + 1 != transactions_size + coinbase_size(&coinbase) {
                    return Err(TemplateError::CoinbaseSizeMismatch {
                        cumulative_size,
                        transactions_size,
                        coinbase_size: coinbase_size(&coinbase),
                    });
                }
                coinbase.prefix.extra.pop();
                if cumulative_size != transactions_size + coinbase_size(&coinbase) {
                    // "no luck": the byte removed shrank the length varint too.
                    cumulative_size += delta - 1;
                    continue;
                }
            }
        }
        if cumulative_size != transactions_size + coinbase_size(&coinbase) {
            return Err(TemplateError::CoinbaseSizeMismatch {
                cumulative_size,
                transactions_size,
                coinbase_size: coinbase_size(&coinbase),
            });
        }
        settled = true;
        break;
    }
    if !settled {
        return Err(TemplateError::CoinbaseUnstable);
    }

    block.base_transaction = coinbase;
    let blob = block.to_bytes().map_err(TemplateError::Serialization)?;

    let reserved_offset = if reserve.is_empty() {
        0
    } else {
        // `std::search` for the 32 public key bytes, then past the key and the
        // two extra-nonce bytes (tag and length). A miss leaves `it` at `end()`,
        // which the C++ still uses, so the offset becomes the blob length and
        // the bounds check below rejects it.
        let at = find_subslice(&blob, &miner.tx_public_key).unwrap_or(blob.len());
        let offset = (at + 32 + 2) as u64;
        if offset + reserve.len() as u64 > blob.len() as u64 {
            return Err(TemplateError::ReserveDoesNotFit {
                reserved_offset: offset,
                reserve_size: reserve.len(),
                blob_len: blob.len(),
            });
        }
        offset
    };

    Ok(BlockTemplateResult {
        blob,
        block,
        difficulty: ctx.difficulty,
        height: ctx.height,
        reserved_offset,
        reserve_size: reserve.len(),
        tx_public_key: miner.tx_public_key,
        reward: miner.reward,
        cumulative_size,
        fee,
    })
}

/// The parent block a daemon template carries (`Core.cpp:2364-2377`; the
/// daemon's shape in spec/12, "Things that look like bugs and are not").
///
/// Major version **0**: line 2364 sets `BLOCK_MAJOR_VERSION_1` and line 2365
/// immediately overwrites the same field with `BLOCK_MINOR_VERSION_0`, so the
/// 1 never survives and `minorVersion` is left at its default 0.
/// `previousBlockHash` is *not* set either — it stays 32 zero bytes, not the
/// block's own parent. `transactionCount = 1`, so
/// `tree_depth(1) = 0` and the base transaction branch is empty; the coinbase
/// is default-constructed (version 0, unlock 0, no inputs, no outputs) and its
/// whole `extra` is `03 21 00 ‖ 32 zero bytes`, an empty merge-mining tag whose
/// depth of 0 also makes the blockchain branch empty.
pub fn daemon_template_parent_block() -> ParentBlock {
    let mut extra = Vec::with_capacity(35);
    append_merge_mining_tag(&mut extra, &MergeMiningTag { depth: 0, merkle_root: [0u8; 32] });
    let coinbase = BaseTransaction {
        prefix: TransactionPrefix { version: 0, unlock_time: 0, inputs: Vec::new(), outputs: Vec::new(), extra },
    };
    ParentBlock::new(0, 0, [0u8; 32], 1, Vec::new(), coinbase, Vec::new())
}

/// `getObjectBinarySize(b.baseTransaction)`. A coinbase never carries
/// signatures, so this cannot fail.
fn coinbase_size(tx: &Transaction) -> u64 {
    tx.to_bytes().map(|b| b.len() as u64).unwrap_or(0)
}

/// `std::search(haystack.begin(), haystack.end(), needle...)`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// What `submitblock` answers with.
#[derive(Clone, Debug)]
pub enum SubmitStatus {
    /// The block was added (`ADDED_TO_MAIN`, `ADDED_TO_ALTERNATIVE`,
    /// `ADDED_TO_ALTERNATIVE_AND_SWITCHED`). The RPC answers `{"status":"OK"}`
    /// and relays for the first and the last.
    Added(AddOutcome),
    /// `ALREADY_EXISTS`. The C++ error *condition* `BLOCK_ADDED` includes it
    /// (`AddBlockErrorCondition.h:73`), so the RPC answers OK — but nothing is
    /// relayed.
    AlreadyExists,
    /// `DESERIALIZATION_FAILED`.
    DeserializationFailed,
    /// `TRANSACTION_ABSENT_IN_POOL` (`Core.cpp:1977`): the submitted template
    /// names a transaction this node's pool does not hold, so the block cannot
    /// be assembled at all.
    TransactionAbsentInPool(Hash),
    /// Any other `addBlock` failure. The RPC answers `-7 Block not accepted`.
    NotAccepted(Rule),
}

impl SubmitStatus {
    /// Whether `RpcServer::submitBlock` answers `{"status":"OK"}` — the
    /// `BLOCK_ADDED` error condition (`RpcServer.cpp:1734`).
    pub fn is_ok(&self) -> bool {
        matches!(self, SubmitStatus::Added(_) | SubmitStatus::AlreadyExists)
    }

    /// Whether the node relays the block on (`RpcServer.cpp:1745`).
    pub fn should_relay(&self) -> bool {
        matches!(
            self,
            SubmitStatus::Added(AddOutcome { status: wrkz_chain::AddStatus::Main, .. })
                | SubmitStatus::Added(AddOutcome { status: wrkz_chain::AddStatus::AlternativeAndSwitched, .. })
        )
    }
}

/// `Core::submitBlock(rawBlockTemplate)` (`Core.cpp:1952`) with the pool
/// bookkeeping `Core::addBlock` performs afterwards.
///
/// The transaction blobs are taken **from the pool**, by the hashes the
/// template names — a miner submits the header only, so a template naming a
/// transaction the pool has since dropped cannot be reassembled.
pub fn submit_block<S: KvStore>(
    pool: &mut TransactionPool,
    chain: &mut ChainState<S>,
    block_blob: &[u8],
) -> SubmitStatus {
    submit_block_update(pool, chain, block_blob).0
}

/// [`submit_block`], also returning what adding the block did to the chain and
/// the pool when it was added: what a caller that publishes the change needs.
pub fn submit_block_update<S: KvStore>(
    pool: &mut TransactionPool,
    chain: &mut ChainState<S>,
    block_blob: &[u8],
) -> (SubmitStatus, Option<crate::pool::ChainUpdate>) {
    let Ok(block) = BlockTemplate::from_bytes(block_blob) else {
        return (SubmitStatus::DeserializationFailed, None);
    };
    let mut tx_blobs = Vec::with_capacity(block.transaction_hashes.len());
    for hash in &block.transaction_hashes {
        match pool.get(hash) {
            Some(entry) => tx_blobs.push(entry.blob.clone()),
            None => return (SubmitStatus::TransactionAbsentInPool(*hash), None),
        }
    }
    match crate::pool::add_block_with_pool(pool, chain, block_blob, &tx_blobs) {
        Ok(update) => (SubmitStatus::Added(update.outcome), Some(update)),
        Err(e) => {
            let status = match e.rule() {
                Some(Rule::AlreadyExists) => SubmitStatus::AlreadyExists,
                Some(Rule::DeserializationFailed(_)) => SubmitStatus::DeserializationFailed,
                Some(rule) => SubmitStatus::NotAccepted(rule.clone()),
                None => SubmitStatus::NotAccepted(Rule::DeserializationFailed("chain state fault")),
            };
            (status, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_block_is_the_daemons_exact_shape() {
        let pb = daemon_template_parent_block();
        assert_eq!(pb.major_version, 0, "Core.cpp:2365 overwrites the 1 of line 2364");
        assert_eq!(pb.minor_version, 0);
        assert_eq!(pb.previous_block_hash, [0u8; 32]);
        assert_eq!(pb.transaction_count, 1);
        assert!(pb.base_transaction_branch.is_empty());
        assert!(pb.blockchain_branch.is_empty());
        let cb = pb.base_transaction();
        assert_eq!(cb.prefix.version, 0);
        assert_eq!(cb.prefix.unlock_time, 0);
        assert!(cb.prefix.inputs.is_empty());
        assert!(cb.prefix.outputs.is_empty());
        // `03 ‖ varint(33) ‖ varint(0) ‖ 32 zero bytes`
        assert_eq!(cb.prefix.extra.len(), 35);
        assert_eq!(&cb.prefix.extra[..3], &[0x03, 0x21, 0x00]);
        assert_eq!(&cb.prefix.extra[3..], &[0u8; 32]);
        let tag = pb.merge_mining_tag().expect("tag parses");
        assert_eq!(tag.depth, 0);
        assert_eq!(tag.merkle_root, [0u8; 32]);
    }

    #[test]
    fn subslice_search_matches_std_search() {
        assert_eq!(find_subslice(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_subslice(b"abcdef", b"xy"), None);
        assert_eq!(find_subslice(b"ab", b"abc"), None);
        assert_eq!(find_subslice(b"abcdef", b""), None);
    }
}
