// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`NodeApi`]: everything the handlers ask the node for, and the value types
//! they get back.
//!
//! The C++ handlers call straight into `CryptoNote::Core`, `NodeServer` and
//! `CryptoNoteProtocolHandler`. This trait is that surface, narrowed to what
//! `RpcServer.cpp` actually reads, so that:
//!
//! - the handlers are pure functions of a snapshot and never touch a lock or a
//!   database themselves;
//! - a test can serve every route from a hand-built fake (`tests/fake.rs`);
//! - `wrkz-rpc-diff` can put our JSON next to the live daemon's for the same
//!   request without a 4.2-million-block database behind it.
//!
//! Methods that need an index this port's chain state does not keep have a
//! default that reports [`ApiError::Unsupported`]; the handler turns that into
//! the same status the C++ gives when its own lookup fails, and the crate docs
//! list them as follow-ups.

use wrkz_primitives::tx::TransactionPrefix;
use wrkz_primitives::Hash;

/// Why the node could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// The chain moved under the read, or a record the answer needs is missing
    /// mid-reorganisation. `/info` and `getlastblockheader` have their own
    /// wording for this (`RpcServer.cpp:988`, `:1800`); everywhere else it is a
    /// 500.
    Busy(String),
    /// The state could not be read.
    Internal(String),
    /// This port keeps no index that could answer. Listed as a follow-up in the
    /// crate docs; the handler answers as the C++ does when its own lookup
    /// comes back empty.
    Unsupported(&'static str),
    /// The node does not hold what the caller asked about, because of the body
    /// policy it runs under: a lite node's index-only region, or a pruned
    /// node's dropped tail ([`BodyPolicy`]).
    ///
    /// Distinct from [`ApiError::Internal`] on purpose. A missing record on a
    /// full node is a fault and reads as one; a height a lite node was never
    /// going to hold is the node working as configured, and the caller needs to
    /// be told *that*, not handed a wrong answer or a mystery. It carries the
    /// C++'s own wording for the one place the C++ words it — HTTP **400**,
    /// `"This node is a lite node and stores no transaction data below height
    /// H"` (`RpcServer.cpp:1509-1518`).
    NotHeld(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Busy(m) => write!(f, "{m}"),
            ApiError::Internal(m) => write!(f, "{m}"),
            ApiError::Unsupported(m) => write!(f, "not supported by this node: {m}"),
            ApiError::NotHeld(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub type Result<T> = std::result::Result<T, ApiError>;

/// Everything `RpcServer::info` reads (`RpcServer.cpp:891`), before the
/// handler derives `hashrate`, `synced`, `lite`, `upgrade_heights` and
/// `sync_features` from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoSnapshot {
    /// `getTopBlockIndex() + 1` — a **count**, not an index (spec/09).
    pub height: u64,
    pub top_block_hash: Hash,
    /// `getDifficultyForNextBlock()`.
    pub difficulty: u64,
    /// `getBlockchainTransactionCount() - height`: coinbases excluded.
    pub tx_count: u64,
    pub tx_pool_size: u64,
    pub alt_blocks_count: u64,
    pub outgoing_connections_count: u64,
    /// `get_connections_count() - get_outgoing_connections_count()`.
    pub incoming_connections_count: u64,
    pub white_peerlist_size: u64,
    pub grey_peerlist_size: u64,
    pub seed_nodes_count: u64,
    pub last_seed_bootstrap: u64,
    /// `max(1, getObservedHeight()) - 1` — an **index**.
    pub last_known_block_index: u64,
    /// `max(1, getBlockchainHeight())` — a **count**.
    pub network_height: u64,
    pub pruned: bool,
    pub prune_depth: u64,
    pub prune_capability_active: bool,
    /// 0 on a full node; wallets floor their scan height at it.
    pub lite_start_height: u64,
    pub sync_active_peers: u64,
    pub sync_avg_batch_size: u64,
    pub sync_demoted_peers: u64,
    /// The **top block's** versions, 0/0 when they could not be read
    /// (`RpcServer.cpp:911`).
    pub major_version: u8,
    pub minor_version: u8,
    pub version: String,
    pub start_time: u64,
}

/// `RpcServer::height` (`RpcServer.cpp:1002`). Both fields are counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeightSnapshot {
    pub height: u64,
    pub network_height: u64,
}

/// `RpcServer::peers` (`RpcServer.cpp:1016`): `ip:port` strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerLists {
    pub white: Vec<String>,
    pub gray: Vec<String>,
}

/// The fields of a `block_header` result, before `depth` is filled in from the
/// top index (`RpcServer.cpp:1800`, `:1878`, `:1938` — identical in all three).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockHeaderInfo {
    pub major_version: u8,
    pub minor_version: u8,
    pub timestamp: u64,
    pub prev_hash: Hash,
    pub nonce: u32,
    pub orphan_status: bool,
    /// A block **index**.
    pub height: u64,
    pub hash: Hash,
    /// `getBlockDifficulty(height)`: this block's own difficulty, the
    /// difference of two cumulative difficulties.
    pub difficulty: u64,
    /// The sum of the coinbase outputs.
    pub reward: u64,
    /// `extraDetails.transactions.size()` — the coinbase counts, so an empty
    /// block reports 1.
    pub num_txes: u64,
    /// `extraDetails.blockSize`: the stored cumulative size.
    pub block_size: u64,
}

/// One row of `f_blocks_list_json` (`RpcServer.cpp:2010`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockListEntry {
    pub cumul_size: u64,
    pub difficulty: u64,
    pub hash: Hash,
    pub height: u64,
    pub timestamp: u64,
    /// `block.transactionHashes.size() + 1` — the coinbase is added by hand.
    pub tx_count: u64,
}

/// One transaction row inside `f_block_json` (`RpcServer.cpp:2121`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockTransactionSummary {
    pub hash: Hash,
    pub fee: u64,
    pub amount_out: u64,
    pub size: u64,
}

/// `f_block_json`'s `block` object (`RpcServer.cpp:2133`).
///
/// Not `Eq`: `penalty` is a `double` in the C++ too (`RpcServer.cpp:2148`) and
/// is the only floating-point field in the whole surface.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockDetails {
    pub header: BlockHeaderInfo,
    pub transactions_cumulative_size: u64,
    /// Emitted as a **string** by the C++ (`std::to_string`).
    pub already_generated_coins: u64,
    pub already_generated_transactions: u64,
    pub size_median: u64,
    pub base_reward: u64,
    pub penalty: f64,
    pub total_fee_amount: u64,
    pub transactions: Vec<BlockTransactionSummary>,
}

/// What `f_transaction_json` prints beyond the transaction itself
/// (`RpcServer.cpp:2171-2309`, `Core::getTransactionDetails`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionDetails {
    /// `result.block`: the block the transaction was mined in, in the same
    /// short shape `f_blocks_list_json` uses (`RpcServer.cpp:2219-2226`).
    pub block: BlockListEntry,
    /// `result.tx`: the transaction as it was serialised.
    pub transaction: wrkz_primitives::tx::Transaction,
    pub hash: Hash,
    /// `totalOutputsAmount`: the sum of the output amounts.
    pub amount_out: u64,
    pub fee: u64,
    /// `Core.cpp:4895-4910`: the **largest** `key_offsets.len()` over the key
    /// inputs — the ring size, not the ring size minus one. A coinbase, which
    /// has no key input, reports 0.
    pub mixin: u64,
    /// The long payment id as 64 hex characters, the ciphertext of an encrypted
    /// short one as 16, or empty.
    pub payment_id: String,
    /// `paymentIdEncrypted`: true when `payment_id` is a short id's ciphertext.
    pub payment_id_encrypted: bool,
    /// `TransactionExtraDetails::publicKey`, from the **strict**
    /// `parseTransactionExtra`, which is what `Core::getTransactionDetails`
    /// fills in — not the looser `Utilities::parseExtra` the payment id comes
    /// from. `None` when the transaction carries no public-key field.
    pub extra_public_key: Option<Hash>,
    /// `TransactionExtraDetails::nonce`: the bytes of the first extra-nonce
    /// field, sub-tag included. Empty when there is none.
    pub extra_nonce: Vec<u8>,
    /// The serialised transaction's length, signatures included.
    pub size: u64,
}

/// One entry of `/queryblocksdetailed` (`RpcServer.cpp:2900-2917`): the
/// `CryptoNote::BlockDetails` that `Core::getBlockDetails` fills
/// (`Core.cpp:4715-4817`). A block below `fullOffset` is sent as its hash
/// alone with every other field at the struct's zero default
/// (`Core::pushBlockHashes`, `Core.cpp:4141`), which is what [`Default`] gives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailedBlock {
    pub major_version: u8,
    pub minor_version: u8,
    pub timestamp: u64,
    pub prev_hash: Hash,
    pub index: u64,
    pub hash: Hash,
    /// This block's own difficulty.
    pub difficulty: u64,
    /// The sum of the coinbase outputs.
    pub reward: u64,
    /// `Core.cpp:4752`: the block blob plus the transactions, the coinbase
    /// counted once.
    pub block_size: u64,
    /// The stored cumulative size (`Core.cpp:4748`).
    pub transactions_cumulative_size: u64,
    pub already_generated_coins: u64,
    pub already_generated_transactions: u64,
    pub size_median: u64,
    /// The reward an empty block would have had at this height.
    pub base_reward: u64,
    pub nonce: u32,
    pub total_fee_amount: u64,
    /// The coinbase first, then the block's transactions in order.
    pub transactions: Vec<DetailedTransaction>,
}

/// One `CryptoNote::TransactionDetails` (`Core.cpp:4833-4980`) as
/// `/queryblocksdetailed` prints it (`RpcServer.cpp:2787-2897`). The block
/// hash and index it prints are the enclosing block's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailedTransaction {
    pub hash: Hash,
    /// The block's timestamp.
    pub timestamp: u64,
    /// The serialised length, signatures included.
    pub size: u64,
    /// 0 for a coinbase.
    pub fee: u64,
    pub unlock_time: u64,
    /// The key inputs' amounts summed; a coinbase input counts 0
    /// (`getTransactionInputAmount`, `TransactionUtils.cpp:49`).
    pub total_inputs_amount: u64,
    pub total_outputs_amount: u64,
    /// The largest ring over the key inputs; 0 for a coinbase.
    pub mixin: u64,
    /// `TransactionImpl::getPaymentId`: a long id that is the whole extra
    /// nonce, else zeros. Encrypted short ids are not reported here.
    pub payment_id: Hash,
    /// The extra's public key, zeros when it has none.
    pub extra_public_key: Hash,
    /// The first extra-nonce field's bytes, sub-tag included.
    pub extra_nonce: Vec<u8>,
    /// The whole extra.
    pub extra_raw: Vec<u8>,
    pub inputs: Vec<DetailedInput>,
    pub outputs: Vec<DetailedOutput>,
    /// One vector per input. A coinbase has none at all: the C++ deserialiser
    /// leaves the vector empty for a lone base input
    /// (`CryptoNoteSerialization.cpp:246`), so `signaturesSize` reads 0.
    pub signatures: Vec<Vec<[u8; 64]>>,
}

/// One input of a [`DetailedTransaction`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetailedInput {
    /// `BaseInputDetails`: `amount` is the coinbase's output total.
    Base { block_index: u64, amount: u64 },
    /// `KeyInputDetails`: `mixin` is this ring's size, and the `output_*`
    /// fields name the transaction and output of the ring's **last** member —
    /// the highest global index (`Core.cpp:4950`) — not the one being spent,
    /// which nobody can tell.
    Key {
        amount: u64,
        key_offsets: Vec<u64>,
        key_image: Hash,
        mixin: u64,
        output_transaction_hash: Hash,
        output_number: u64,
    },
}

/// One output of a [`DetailedTransaction`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailedOutput {
    pub amount: u64,
    pub key: Hash,
    /// 0 when the node cannot say (`Core.cpp:4962`).
    pub global_index: u64,
}

/// `/queryblocksdetailed`'s answer (`Core::queryBlocksDetailed`, `Core.cpp:706`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryBlocksDetailed {
    pub start_index: u64,
    pub current_index: u64,
    pub full_offset: u64,
    pub blocks: Vec<DetailedBlock>,
}

/// What a node will and will not answer about block bodies.
///
/// Every mode is a statement about *bodies*, never about consensus: a lite or
/// pruned node validates each block it applies exactly as a full one does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyPolicy {
    /// `--lite-height`: the height at and above which full block data is kept.
    /// `0` on a node that is not lite.
    pub lite_start_height: u64,
    /// `--prune-depth`, or `None` when every body is kept.
    pub prune_depth: Option<u64>,
    /// The lowest height a body can be served for right now — the higher of the
    /// lite line and the prune window's floor. `0` on a full node.
    pub floor: u64,
    /// The height below which this node holds no **transaction** records — no
    /// transaction index, no payment ids, no per-block transaction list — and
    /// so cannot tell a transaction mined below it from one that was never
    /// mined. `0` on every node but one imported from a lite snapshot, where
    /// it is the lite height (`wrkz_chain::ChainState::transactions_floor`). A
    /// lite node that synced its own region keeps those records: its `floor`
    /// is its lite height and this is `0`.
    pub transactions_from: u64,
}

impl BodyPolicy {
    pub fn is_lite(&self) -> bool {
        self.lite_start_height != 0
    }

    pub fn is_pruned(&self) -> bool {
        self.prune_depth.is_some()
    }

    /// Whether a body question about `height` can be answered at all. Genesis
    /// always can on a lite node: the C++ exempts index 0 explicitly
    /// (`DatabaseBlockchainCache.h:476`, `blockIndex != 0`).
    pub fn serves(&self, height: u64) -> bool {
        height >= self.floor || (height == 0 && !self.is_pruned())
    }

    /// Why a height cannot be answered for, in the words the mode deserves.
    /// `None` when it can.
    pub fn refusal(&self, height: u64) -> Option<String> {
        if self.serves(height) {
            return None;
        }
        if self.is_lite() && height < self.lite_start_height {
            // `RpcServer.cpp:1509`, the one refusal the C++ words itself.
            Some(format!(
                "This node is a lite node and stores no transaction data below height {}",
                self.lite_start_height
            ))
        } else {
            Some(format!("This node is a pruned node and stores no block data below height {}", self.floor))
        }
    }

    /// Why a transaction this node did not find cannot be reported as absent:
    /// on a node with a [`BodyPolicy::transactions_from`], a transaction mined
    /// below it has no record here, so "not found" would be a guess. The words
    /// are the C++'s lite refusal (`RpcServer.cpp:1509`), which is the same
    /// fact. `None` on a node that holds every transaction record it has a
    /// block for.
    pub fn transaction_refusal(&self) -> Option<String> {
        (self.transactions_from != 0).then(|| {
            format!("This node is a lite node and stores no transaction data below height {}", self.transactions_from)
        })
    }
}

/// One key output as `/getwalletsyncdata` reports it (`WalletTypes.h:20`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutput {
    pub amount: u64,
    pub key: Hash,
    /// The daemon never fills this in; a blockchain-cache API does
    /// (`RpcServer.cpp:1355`). Always `None` here.
    pub global_index: Option<u64>,
}

/// One key input as `/getwalletsyncdata` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncInput {
    pub amount: u64,
    pub key_image: Hash,
    /// The **relative** offsets, as they are in the transaction.
    pub key_offsets: Vec<u64>,
}

/// `WalletTypes::RawTransaction` / `RawCoinbaseTransaction`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncTransaction {
    pub hash: Hash,
    pub outputs: Vec<SyncOutput>,
    pub tx_public_key: Hash,
    pub unlock_time: u64,
    /// The plaintext long id, the 16-hex ciphertext of an encrypted short id,
    /// or empty. A coinbase never carries one.
    pub payment_id: String,
    pub inputs: Vec<SyncInput>,
}

/// `WalletTypes::WalletBlockInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncBlock {
    pub block_hash: Hash,
    /// A block **index**, despite the name.
    pub block_height: u64,
    pub block_timestamp: u64,
    pub coinbase: Option<SyncTransaction>,
    pub transactions: Vec<SyncTransaction>,
}

/// `WalletTypes::TopBlock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopBlock {
    pub hash: Hash,
    /// A block **index**.
    pub height: u64,
}

/// What `Core::getWalletSyncData` fills in (`Core.cpp:908`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalletSyncData {
    pub items: Vec<SyncBlock>,
    /// Present only when `items` is empty, or when a timestamp start could not
    /// be resolved. Absent when the requested window was entirely behind the
    /// caller (`Core.cpp:1035`, the early return).
    pub top_block: Option<TopBlock>,
    /// The highest index the daemon looked at. `0` means "nothing covered", and
    /// the field is then left out of the response entirely.
    pub scanned_to_height: u64,
}

/// One `/getrawblocks` item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlockItem {
    pub block: Vec<u8>,
    pub transactions: Vec<Vec<u8>>,
}

/// What `Core::getRawBlocks` fills in (`Core.cpp:1123`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawBlocks {
    pub items: Vec<RawBlockItem>,
    pub top_block: Option<TopBlock>,
}

/// The request both sync endpoints parse, after the RPC layer's own clamps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncRequest {
    /// Newest first. An empty list means "start from `start_height`".
    pub block_hash_checkpoints: Vec<Hash>,
    pub start_height: u64,
    pub start_timestamp: u64,
    /// Already checked against `--rpc-max-block-count` by the handler.
    pub block_count: u64,
    pub skip_coinbase_transactions: bool,
    pub skip_empty_blocks: bool,
    /// Exclusive; `0` means unbounded.
    pub end_height: u64,
}

/// `TransactionPrefixInfo` (`CoreRpcServerCommandsDefinitions.h`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxPrefixInfo {
    pub hash: Hash,
    pub prefix: TransactionPrefix,
}

/// `BlockShortInfo`. `block` is empty for the hash-only entries
/// `pushBlockHashes` produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockShortInfo {
    pub block_id: Hash,
    pub block: Vec<u8>,
    pub tx_prefixes: Vec<TxPrefixInfo>,
}

/// What `Core::queryBlocksLite` fills in (`Core.cpp:642`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryBlocksLite {
    /// All three are block **indexes**.
    pub start_index: u64,
    pub current_index: u64,
    pub full_offset: u64,
    pub items: Vec<BlockShortInfo>,
}

/// What `Core::getPoolChangesLite` fills in (`Core.cpp:2289`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolChanges {
    pub added: Vec<TxPrefixInfo>,
    pub deleted: Vec<Hash>,
    /// `getTopBlockHash() == lastBlockHash`.
    pub is_tail_block_actual: bool,
}

/// One row of `f_on_transactions_pool_json` (`RpcServer.cpp:2320`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolTransactionSummary {
    pub hash: Hash,
    pub fee: u64,
    pub amount_out: u64,
    pub size: u64,
}

/// `Core::getTransactionsStatus`'s three buckets (`Core.cpp:792`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionsStatus {
    pub in_pool: Vec<Hash>,
    pub in_block: Vec<Hash>,
    pub unknown: Vec<Hash>,
}

/// What `Core::getBlockTemplate` hands back, before `RpcServer::getBlockTemplate`
/// searches the blob for the reserved offset (`RpcServer.cpp:1656`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockTemplateAnswer {
    pub blob: Vec<u8>,
    pub difficulty: u64,
    /// A **count**: `getTopBlockIndex() + 1`.
    pub height: u64,
    /// The coinbase's transaction public key, which is what the offset search
    /// looks for in `blob`.
    pub tx_public_key: Hash,
}

/// `Core::submitBlock`'s outcome, reduced to what `RpcServer::submitBlock`
/// distinguishes (`RpcServer.cpp:1734`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The `BLOCK_ADDED` error condition. `relay` is true for `ADDED_TO_MAIN`
    /// and `ADDED_TO_ALTERNATIVE_AND_SWITCHED` (`RpcServer.cpp:1745`).
    Added { relay: bool },
    /// Anything else: `-7 Block not accepted`.
    NotAccepted,
}

/// The node, as the handlers see it.
///
/// Every method is `&self`: an implementation that owns mutable state does its
/// own locking, exactly as `Core` does with `m_chainMutex`.
pub trait NodeApi: Send + Sync {
    // -- status --------------------------------------------------------------

    /// `RpcServer::info`. An error becomes the `"BUSY"` 503 body.
    fn info(&self) -> Result<InfoSnapshot>;

    /// `RpcServer::height`.
    fn height(&self) -> HeightSnapshot;

    /// `RpcServer::peers`.
    fn peers(&self) -> PeerLists;

    /// The middleware's sync gate (`RpcServer.cpp:576`):
    /// `isSynchronized() && height >= networkHeight`. Only `/sendrawtransaction`
    /// is gated on it.
    fn is_synced(&self) -> bool;

    /// `getTopBlockIndex()`, which every `depth` is measured from.
    fn top_index(&self) -> u64;

    /// `Core::save()`: commit whatever the store is holding back and make it
    /// durable. No RPC route calls it — the daemon console's `save` command
    /// does (`DaemonCommandsHandler::save`), and the daemon does it once on the
    /// way out. It is here rather than on the concrete node so the console
    /// reaches the chain through the one handle the RPC uses and cannot end up
    /// flushing a different state.
    ///
    /// The default is `Ok(())`, for an implementation with nothing to commit.
    fn save(&self) -> Result<()> {
        Ok(())
    }

    // -- blocks --------------------------------------------------------------

    /// The main-chain hash at an index, or `None` above the tip.
    fn block_hash_by_index(&self, index: u64) -> Result<Option<Hash>>;

    /// A header by hash, over every segment the node holds. `None` when no
    /// segment has it.
    fn block_header_by_hash(&self, hash: &Hash) -> Result<Option<BlockHeaderInfo>>;

    /// A header by main-chain index.
    fn block_header_by_index(&self, index: u64) -> Result<Option<BlockHeaderInfo>>;

    /// The 31 rows `f_blocks_list_json` returns, newest first
    /// (`RpcServer.cpp:1996`: `height` down to `height - 30`, floored at 0).
    fn block_list(&self, height: u64) -> Result<Vec<BlockListEntry>>;

    /// `getBlockDetails(hash)` reduced to what `f_block_json` prints.
    fn block_details(&self, hash: &Hash) -> Result<Option<BlockDetails>>;

    // -- wallet sync ---------------------------------------------------------

    /// `Core::getWalletSyncData`. `Err` is the 500 the C++ returns when it
    /// answers `false` — which includes the case where none of the caller's
    /// checkpoints is on this chain (`findBlockchainSupplement` throws).
    fn wallet_sync_data(&self, request: &SyncRequest) -> Result<WalletSyncData>;

    /// `Core::getWalletSyncStartIndex` (`Core.cpp:865`): the block index a
    /// `/getwalletsyncdata` call with these parameters starts from, after the
    /// lite and prune clamp. `None` wherever the C++ answers `false` — a
    /// timestamp this node cannot place, checkpoints it does not hold — and
    /// wherever the answer would be the top block alone; neither is worth
    /// caching.
    ///
    /// Only the response cache ([`crate::sync_cache`]) reads it. The default
    /// answers `None`, which leaves the cache off for an implementation that
    /// does not provide it.
    fn wallet_sync_start_index(&self, request: &SyncRequest) -> Result<Option<u64>> {
        let _ = request;
        Ok(None)
    }

    /// `Core::getRawBlocks`.
    fn raw_blocks(&self, request: &SyncRequest) -> Result<RawBlocks>;

    /// `Core::getGlobalIndexesForRange`, `[start, end)`. The C++ returns an
    /// `unordered_map`, so its order is arbitrary; ours is block order, then
    /// transaction order within a block.
    fn global_indexes_for_range(&self, start: u64, end: u64) -> Result<Vec<(Hash, Vec<u64>)>>;

    /// `Core::getTransactionGlobalIndexes`. `None` when no segment holds the
    /// transaction, which is the C++ `false` and a 500.
    fn transaction_global_indexes(&self, hash: &Hash) -> Result<Option<Vec<u32>>>;

    /// `Core::getRandomOutputs`. `Err(String)` is the message the C++ puts in a
    /// `CANT_GET_FAKE_OUTPUTS` 400; an empty vector is the "this denomination
    /// is thin" case, which is a success (`Core.cpp:2054`).
    fn random_outputs(&self, amount: u64, count: u16) -> Result<std::result::Result<Vec<(u32, Hash)>, String>>;

    // -- transactions --------------------------------------------------------

    /// `Core::addTransactionToPool` plus the relay the RPC does on success.
    /// `Err(String)` is the message that lands in `{"status":"Failed","error":…}`.
    fn add_transaction_to_pool(&self, blob: &[u8]) -> std::result::Result<(), String>;

    /// `Core::getTransactionsStatus`.
    fn transactions_status(&self, hashes: &[Hash]) -> Result<TransactionsStatus>;

    /// `Core::getPoolTransactions`, summarised.
    fn pool_transactions(&self) -> Result<Vec<PoolTransactionSummary>>;

    /// `Core::getPoolChangesLite`.
    fn pool_changes_lite(&self, tail_block_id: &Hash, known: &[Hash]) -> Result<PoolChanges>;

    /// `Core::queryBlocksLite`.
    fn query_blocks_lite(&self, known: &[Hash], timestamp: u64) -> Result<QueryBlocksLite>;

    /// `Core::queryBlocksDetailed` (`Core.cpp:706`), with `block_count`
    /// already truncated to the `uint32_t` the C++ takes. The default refuses,
    /// which the handler answers with the C++'s own failure body.
    fn query_blocks_detailed(
        &self,
        _known: &[Hash],
        _timestamp: u64,
        _block_count: u32,
    ) -> Result<QueryBlocksDetailed> {
        Err(ApiError::Unsupported("queryblocksdetailed"))
    }

    /// The raw transaction bytes, from any segment or the pool
    /// (`Core::getTransactions`). Used by `f_transaction_json`.
    fn transaction_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>>;

    /// The block index a transaction was mined in, for `f_transaction_json`.
    /// The default reports [`ApiError::Unsupported`]: `wrkz-chain` keeps no
    /// transaction-hash index yet.
    fn transaction_block_index(&self, _hash: &Hash) -> Result<Option<u64>> {
        Err(ApiError::Unsupported("a transaction-to-block index"))
    }

    /// `Core::getTransactionHashesByPaymentId`, chain **then** pool
    /// (`Core.cpp:5002-5017`), in the order they were mined. The default
    /// reports [`ApiError::Unsupported`], for an implementation with no
    /// payment-id index.
    fn transaction_hashes_by_payment_id(&self, _payment_id: &Hash) -> Result<Vec<Hash>> {
        Err(ApiError::Unsupported("a payment-id index"))
    }

    /// `Core::getTransactionDetails` reduced to what `f_transaction_json`
    /// prints. `Ok(None)` is the C++'s "no segment holds this transaction",
    /// which it answers as `-1 "Block hash specified does not exist!"`.
    ///
    /// **Chain only.** `Core::getTransactions` never consults the pool
    /// (`Core.cpp:1356-1406`), so `f_transaction_json` cannot see a mempool
    /// transaction and neither can this.
    fn transaction_details(&self, _hash: &Hash) -> Result<Option<TransactionDetails>> {
        Err(ApiError::Unsupported("transaction details"))
    }

    /// Which block bodies this node keeps. The default is a full node.
    ///
    /// Cheap by construction — it reads configuration, not the chain — because
    /// every handler that touches a body consults it before it answers.
    fn body_policy(&self) -> BodyPolicy {
        BodyPolicy::default()
    }

    // -- mining --------------------------------------------------------------

    /// `Core::getBlockTemplate`. `Err(String)` becomes `-5 "Failed to create
    /// block template: …"`.
    fn block_template(
        &self,
        wallet_address: &str,
        reserve: &[u8],
    ) -> Result<std::result::Result<BlockTemplateAnswer, String>>;

    /// `Core::submitBlock` plus the relay the RPC does.
    fn submit_block(&self, blob: &[u8]) -> Result<SubmitOutcome>;
}
