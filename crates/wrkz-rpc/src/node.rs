// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`ChainNode`]: [`NodeApi`] over a real [`ChainState`] and
//! [`TransactionPool`] — the `CryptoNote::Core` half of the C++ handlers.
//!
//! Everything here is a read of `wrkz-chain` or `wrkz-mempool` shaped into what
//! `RpcServer.cpp` prints. The P2P numbers `/info` reports come from a
//! [`P2pSnapshot`] the owner supplies, so this crate does not depend on
//! `wrkz-node` and a node without a P2P layer still answers `/info`
//! ([`ChainNode::standalone`]).
//!
//! # What needs which record
//!
//! | Endpoint | Needs |
//! | --- | --- |
//! | `/info`, `/height`, `getblockcount` | block infos only |
//! | headers, `f_block_json`, `/getwalletsyncdata`, `/getrawblocks`, `queryblockslite` | [`Config::store_raw_blocks`] |
//! | `/get_global_indexes_for_range`, `/get_o_indexes` | the per-block output-reference record, which [`Config::unwind_history`] prunes |
//!
//! A node that serves wallets therefore runs with `store_raw_blocks: true` and
//! `unwind_history: u32::MAX` ([`serving_config`]). With a smaller
//! `unwind_history` the global-index endpoints answer only for the recent
//! window and report the C++'s own "failed to getTransactionGlobalIndexes"
//! below it.

use crate::api::*;
use crate::events::{AppliedBlock, Events};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use wrkz_chain::records::BlockInfo;
use wrkz_chain::{ChainState, Config};
use wrkz_mempool::{PoolSource, PoolWithChain, TemplateOptions, TransactionPool};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::*;
use wrkz_primitives::tx::{
    parse_extra_wallet, relative_offsets_to_absolute, Input, PaymentId, Transaction, TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_storage::KvStore;

/// `BLOCKS_SYNCHRONIZING_MAX_RESPONSE_BYTES` (`config/CryptoNoteConfig.h:489`).
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// `ESTIMATED_WALLET_SYNC_TRANSACTION_BYTES`
/// (`DatabaseBlockchainCache.cpp:242`).
pub const ESTIMATED_WALLET_SYNC_TRANSACTION_BYTES: u64 = 512;

/// The `sizeof`s `WalletTypes::WalletBlockInfo::memoryUsage` adds up
/// (`include/WalletTypes.h:49`), on the x86-64 libstdc++ the daemon is built
/// with. They decide only *where a response is cut off* at the 8 MiB budget, so
/// a platform with different `sizeof`s would cut a large response one block
/// earlier or later; nothing about a block that is returned changes.
mod sizes {
    /// `sizeof(WalletTypes::KeyOutput)`: a 32-byte key, a `uint64_t` and an
    /// `optional<uint64_t>`.
    pub const KEY_OUTPUT: u64 = 56;
    /// `sizeof(CryptoNote::KeyInput)`: `uint64_t`, `vector<uint32_t>`, key image.
    pub const KEY_INPUT: u64 = 64;
    pub const VECTOR: u64 = 24;
    pub const STRING: u64 = 32;
    /// `sizeof(std::optional<RawCoinbaseTransaction>)`.
    pub const OPTIONAL_COINBASE: u64 = 104;
    /// `sizeof(hash) + sizeof(transactionPublicKey) + sizeof(unlockTime)`.
    pub const COINBASE_FIXED: u64 = 32 + 32 + 8;
    /// `sizeof(blockHeight) + sizeof(blockHash) + sizeof(blockTimestamp)`.
    pub const BLOCK_FIXED: u64 = 8 + 32 + 8;
}

/// `CryptoNote::parseTransactionExtra` (`TransactionExtra.cpp`) reduced to the
/// two fields `Core::getTransactionDetails` puts in `TransactionExtraDetails`:
/// the transaction public key and the first extra-nonce field's bytes.
///
/// Strict, unlike `Utilities::parseExtra`: the fields are read as a tag stream
/// from the start, a padding run of `0x00` ends the walk, and an unknown tag or
/// a truncated field stops it rather than being re-scanned from the next byte.
/// The two parsers agree on every well-formed transaction; the C++ uses this
/// one for `publicKey` and `nonce` and the loose one for `paymentId`, and so
/// does [`ChainNode::transaction_details`].
fn parse_extra_strict(extra: &[u8]) -> (Option<Hash>, Vec<u8>) {
    const TAG_PADDING: u8 = 0x00;
    const TAG_PUBKEY: u8 = 0x01;
    const TAG_NONCE: u8 = 0x02;
    let mut public_key = None;
    let mut nonce = Vec::new();
    let mut at = 0usize;
    while at < extra.len() {
        match extra[at] {
            TAG_PADDING => break,
            TAG_PUBKEY => {
                let Some(bytes) = extra.get(at + 1..at + 33) else { break };
                if public_key.is_none() {
                    public_key = Some(Hash::try_from(bytes).expect("32 bytes"));
                }
                at += 33;
            }
            TAG_NONCE => {
                // A one-byte length: the C++ serialiser writes the nonce size as
                // a varint, and a nonce is capped at 255 bytes
                // (`TX_EXTRA_NONCE_MAX_COUNT`), so one byte is the whole varint.
                let Some(&len) = extra.get(at + 1) else { break };
                let len = len as usize;
                let Some(bytes) = extra.get(at + 2..at + 2 + len) else { break };
                if nonce.is_empty() {
                    nonce = bytes.to_vec();
                }
                at += 2 + len;
            }
            // Merge-mining and anything else: the strict parser cannot skip a
            // field it does not know the length of, so it stops.
            _ => break,
        }
    }
    (public_key, nonce)
}

/// A [`Config`] that can serve every endpoint of this crate: raw blocks kept,
/// and the per-block output references never pruned.
pub fn serving_config() -> Config {
    Config { store_raw_blocks: true, unwind_history: u32::MAX, recent_window: 256, ..Config::default() }
}

/// The numbers `/info` and the sync gate read from the P2P and sync layers.
///
/// Supplied by the owner of the node, so this crate does not depend on
/// `wrkz-node`. [`P2pSnapshot::standalone`] is what a node with no peers
/// reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct P2pSnapshot {
    /// `get_connections_count()`.
    pub total_connections: u64,
    /// `get_outgoing_connections_count()`.
    pub outgoing_connections: u64,
    pub white_peers: Vec<String>,
    pub gray_peers: Vec<String>,
    pub seed_nodes_count: u64,
    pub last_seed_bootstrap: u64,
    /// `CryptoNoteProtocolHandler::getObservedHeight()` — a **count**; `/info`
    /// reports `max(1, it) - 1`.
    pub observed_height: u64,
    /// `getBlockchainHeight()` — a **count**. `0` means "no P2P layer here":
    /// [`ChainNode`] then reports its own height, so a standalone node reads as
    /// synced instead of permanently one block behind an unknown network.
    pub blockchain_height: u64,
    /// `isSynchronized()`.
    pub synchronized: bool,
    pub pruned: bool,
    pub prune_depth: u64,
    pub prune_capability_active: bool,
    pub lite_start_height: u64,
    pub sync_active_peers: u64,
    pub sync_avg_batch_size: u64,
    pub sync_demoted_peers: u64,
}

impl P2pSnapshot {
    /// No peers, and this node is its own network height.
    pub fn standalone() -> Self {
        Self { synchronized: true, prune_depth: 10080, ..Default::default() }
    }
}

/// The chain, shared with whoever else holds it — the P2P engine, in a running
/// daemon.
///
/// An `RwLock` and not a `Mutex`, because that is exactly what `Core` does with
/// `m_chainMutex`: every read path takes a `std::shared_lock` and only
/// `addBlock` takes the unique one. Several RPC reads therefore run at once,
/// and a block arriving waits only for the reads already in flight — a slow
/// `/getwalletsyncdata` delays the next block by its own duration and nothing
/// more. The lock is never held across a socket write: a handler assembles its
/// value, drops the guard, and the server serialises it afterwards.
pub type SharedChain<S> = Arc<RwLock<ChainState<S>>>;

/// The pool, shared with the P2P layer, which adds relayed transactions to the
/// same one the RPC and the template builder read.
pub type SharedPool = Arc<Mutex<TransactionPool>>;

/// How `/getrandom_outs` picks the decoys a wallet puts in its rings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecoySelection {
    /// Uniformly over every unlocked output of the amount: `Core::getRandomOutputs`
    /// with its `ShuffleGenerator`, what every C++ node does. The default.
    #[default]
    Uniform,
    /// Weighted towards recent outputs, the way real spends are: an output's
    /// age is drawn from Monero's gamma distribution (`recent_picks`).
    /// Off unless asked for, because a node that hands out a different
    /// distribution from the rest of the network makes its own users' rings
    /// stand out until the C++ node does the same.
    Recent,
}

impl DecoySelection {
    /// `uniform` or `recent`, for the command line.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "uniform" => Some(Self::Uniform),
            "recent" => Some(Self::Recent),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::Recent => "recent",
        }
    }
}

/// A block `submitblock` added, as the P2P layer announces it: the block blob
/// and the blobs of the transactions it names, in block order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinedBlock {
    pub block: Vec<u8>,
    pub transactions: Vec<Vec<u8>>,
}

/// A [`NodeApi`] over a real chain and pool.
pub struct ChainNode<S: KvStore> {
    chain: SharedChain<S>,
    pool: SharedPool,
    /// Transactions this node accepted through `/sendrawtransaction` and would
    /// have relayed. A daemon with a P2P layer drains this every tick.
    relay_queue: Mutex<Vec<Vec<u8>>>,
    /// Blocks `submitblock` added to the main chain, waiting for the P2P layer
    /// to announce them (`m_syncManager->relayBlock`, `RpcServer.cpp:1789`).
    /// Kept apart from `relay_queue`: a block sent on as a transaction is
    /// refused by every peer, and the network then learns of it only at its
    /// next timed sync — long enough for someone else's block to win.
    block_relay_queue: Mutex<Vec<MinedBlock>>,
    /// Called once a block has been queued, so the daemon loop can announce it
    /// now rather than at its next tick.
    on_mined_block: Option<Box<dyn Fn() + Send + Sync>>,
    p2p: Box<dyn Fn() -> P2pSnapshot + Send + Sync>,
    start_time: u64,
    /// Fixed template options, so a test can reproduce a template byte for
    /// byte. The default is the C++ behaviour: a fresh key pair and
    /// `time(nullptr)`.
    template_options: TemplateOptions,
    /// The tip-derived half of `getblocktemplate`, held for as long as the tip
    /// is. A pool polls this route once a second per worker and the difficulty
    /// window alone is 61 header reads; none of it can change until a block
    /// arrives. Every per-call part of a template — the pool walk, the
    /// coinbase key, the timestamp — is still done per call.
    template_context_cache: wrkz_mempool::TemplateContextCache,
    /// `--decoy-selection`.
    decoys: DecoySelection,
    /// Where the blocks `submitblock` adds and the transactions
    /// `/sendrawtransaction` admits are published ([`crate::events`]).
    events: Events,
}

impl<S: KvStore> ChainNode<S> {
    /// A node with no P2P layer: no peers, its own height as the network
    /// height, and therefore synced.
    pub fn standalone(chain: ChainState<S>, pool: TransactionPool) -> Self {
        Self::shared(Arc::new(RwLock::new(chain)), Arc::new(Mutex::new(pool)), Box::new(P2pSnapshot::standalone))
    }

    /// Over state someone else owns as well — the P2P engine in a daemon.
    pub fn shared(chain: SharedChain<S>, pool: SharedPool, p2p: Box<dyn Fn() -> P2pSnapshot + Send + Sync>) -> Self {
        Self {
            chain,
            pool,
            relay_queue: Mutex::new(Vec::new()),
            block_relay_queue: Mutex::new(Vec::new()),
            on_mined_block: None,
            p2p,
            start_time: now(),
            template_options: TemplateOptions::default(),
            template_context_cache: wrkz_mempool::TemplateContextCache::new(),
            decoys: DecoySelection::default(),
            events: Events::default(),
        }
    }

    /// Publish what this node's writers change to `events`' listeners.
    pub fn with_events(mut self, events: Events) -> Self {
        self.events = events;
        self
    }

    /// Fix the coinbase key pair and the clock the template builder uses.
    pub fn with_template_options(mut self, options: TemplateOptions) -> Self {
        self.template_options = options;
        self
    }

    /// Choose how `/getrandom_outs` picks decoys ([`DecoySelection`]).
    pub fn with_decoy_selection(mut self, decoys: DecoySelection) -> Self {
        self.decoys = decoys;
        self
    }

    /// The shared handles, for a caller that wires the same state into a P2P
    /// engine.
    pub fn handles(&self) -> (SharedChain<S>, SharedPool) {
        (Arc::clone(&self.chain), Arc::clone(&self.pool))
    }

    /// Run `hook` whenever `submitblock` queues a block for announcement.
    pub fn with_mined_block_hook(mut self, hook: Box<dyn Fn() + Send + Sync>) -> Self {
        self.on_mined_block = Some(hook);
        self
    }

    /// Everything `/sendrawtransaction` accepted and would have relayed, taken
    /// away. Transactions only: blocks are in [`ChainNode::take_block_relay_queue`].
    pub fn take_relay_queue(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.relay_queue.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Every block `submitblock` added and the network has not been told of
    /// yet, taken away, oldest first.
    pub fn take_block_relay_queue(&self) -> Vec<MinedBlock> {
        std::mem::take(&mut *self.block_relay_queue.lock().unwrap_or_else(|p| p.into_inner()))
    }

    // A panic inside a handler must not take the whole RPC surface down with
    // it, and the state a poisoned lock guards is still consistent: every write
    // goes through one `write_batch`.
    fn read(&self) -> RwLockReadGuard<'_, ChainState<S>> {
        self.chain.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, ChainState<S>> {
        self.chain.write().unwrap_or_else(|p| p.into_inner())
    }

    fn pool(&self) -> MutexGuard<'_, TransactionPool> {
        self.pool.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Everything a read handler needs, over one read guard.
    fn view<'a>(&self, chain: &'a ChainState<S>) -> View<'a, S> {
        View { chain }
    }
}

/// The chain reads the handlers perform, over one borrowed [`ChainState`].
///
/// Every method here runs under a *read* guard, so several RPC calls answer at
/// once and none of them blocks another.
struct View<'a, S: KvStore> {
    chain: &'a ChainState<S>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn chain_error(e: wrkz_chain::ChainError) -> ApiError {
    ApiError::Internal(e.to_string())
}

fn index_of(v: u64) -> std::result::Result<u32, ApiError> {
    u32::try_from(v).map_err(|_| ApiError::Internal(format!("block index {v} is out of range")))
}

// ---------------------------------------------------------------------------
// chain reads the handlers need
// ---------------------------------------------------------------------------

impl<S: KvStore> View<'_, S> {
    fn tip(&self) -> u64 {
        self.chain.tip_index().unwrap_or(0) as u64
    }

    /// What this state will answer a body question for.
    fn body_policy(&self) -> BodyPolicy {
        let cfg = self.chain.config();
        BodyPolicy {
            lite_start_height: u64::from(cfg.lite_start_height),
            prune_depth: cfg.prune_depth.map(u64::from),
            floor: u64::from(self.chain.body_floor().unwrap_or(0)),
            transactions_from: u64::from(self.chain.transactions_floor()),
        }
    }

    /// The answer for a transaction the index does not hold: "no such
    /// transaction" on a node that keeps every transaction record, and the lite
    /// refusal on one imported from a snapshot, which keeps none below its
    /// height and so cannot tell a transaction mined there from one never mined.
    fn missing_transaction<T>(&self) -> Result<Option<T>> {
        match self.body_policy().transaction_refusal() {
            Some(message) => Err(ApiError::NotHeld(message)),
            None => Ok(None),
        }
    }

    /// The error for a height whose body this node does not have.
    ///
    /// A lite or pruned node says which it is and where its data starts; a full
    /// node that has somehow lost a body reports the missing record, which is a
    /// different kind of problem and must not read as a mode.
    fn no_body(&self, index: u64) -> ApiError {
        let policy = self.body_policy();
        match policy.refusal(index) {
            Some(message) => ApiError::NotHeld(message),
            None => ApiError::Internal(format!("no block body is stored at height {index}")),
        }
    }

    /// The lowest height a body-reading sync may start at. The C++ raises a
    /// caller's `startHeight` to `lite_start_height` and clears its timestamp
    /// rather than erroring (`RpcServer.cpp:1248-1253` and `:3006-3011`): a
    /// wallet has already been told the floor by `/info`, and a short answer it
    /// can act on beats an error it cannot.
    fn sync_floor(&self) -> u64 {
        self.body_policy().floor
    }

    fn info_at(&self, index: u64) -> Result<BlockInfo> {
        let index = index_of(index)?;
        self.chain
            .block_info(index)
            .map_err(chain_error)?
            .ok_or_else(|| ApiError::Busy(format!("block info for index {index} is missing")))
    }

    /// The block blob and its transaction blobs, parsed.
    fn block_at(&self, index: u64) -> Result<(BlockTemplate, Vec<Vec<u8>>, Vec<u8>)> {
        let idx = index_of(index)?;
        let (blob, txs) = match self.chain.raw_block(idx).map_err(chain_error)? {
            Some(v) => v,
            None => return Err(self.no_body(index)),
        };
        let block = BlockTemplate::from_bytes(&blob)
            .map_err(|e| ApiError::Internal(format!("stored block {index} does not parse: {e}")))?;
        Ok((block, txs, blob))
    }

    /// `getBlockDifficulty(index)` (`Core.cpp:1408`): the difference of the two
    /// cumulative difficulties, or the single one at index 0.
    fn block_difficulty(&self, index: u64) -> Result<u64> {
        let this = self.info_at(index)?.cumulative_difficulty;
        if index == 0 {
            return Ok(this);
        }
        Ok(this.saturating_sub(self.info_at(index - 1)?.cumulative_difficulty))
    }

    /// `getLastBlocksSizes(count, index, useGenesis=true)`.
    fn last_block_sizes(&self, count: u64, index: u64) -> Result<Vec<u64>> {
        let from = (index + 1).saturating_sub(count);
        let mut sizes = Vec::with_capacity((index - from + 1) as usize);
        for i in from..=index {
            sizes.push(self.info_at(i)?.block_size as u64);
        }
        Ok(sizes)
    }

    fn header_at(&self, index: u64) -> Result<BlockHeaderInfo> {
        let info = self.info_at(index)?;
        let (block, _, blob) = self.block_at(index)?;
        let reward = block.coinbase_output_total().unwrap_or(0);
        let num_txes = self.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?.len() as u64;
        // `extraDetails.blockSize` (`Core.cpp:4751`):
        // `blockBlobSize + storedCumulativeSize - coinbaseSize`.
        let coinbase_size = block.base_transaction.to_bytes().map(|b| b.len() as u64).unwrap_or(0);
        let block_size = blob.len() as u64 + info.block_size as u64 - coinbase_size;
        Ok(BlockHeaderInfo {
            major_version: block.major_version,
            minor_version: block.minor_version,
            timestamp: block.timestamp,
            prev_hash: block.previous_block_hash,
            nonce: block.nonce,
            // Only main-chain blocks are reachable from here.
            orphan_status: false,
            height: index,
            hash: info.block_hash,
            difficulty: self.block_difficulty(index)?,
            reward,
            num_txes,
            block_size,
        })
    }

    /// `getBlockHeaderByHash` for a block on an alternative chain
    /// (`RpcServer.cpp:1848-1912`): the block's own fields, `orphan_status`
    /// set, and the one number the C++ borrows — `difficulty` is
    /// `getBlockDifficulty(height)`, the **main** chain's block at that height.
    fn alternative_header(&self, index: u32, info: &BlockInfo, blob: &[u8]) -> Result<BlockHeaderInfo> {
        let block = BlockTemplate::from_bytes(blob)
            .map_err(|e| ApiError::Internal(format!("held alternative block does not parse: {e}")))?;
        let coinbase_size = block.base_transaction.to_bytes().map(|b| b.len() as u64).unwrap_or(0);
        let height = u64::from(index);
        Ok(BlockHeaderInfo {
            major_version: block.major_version,
            minor_version: block.minor_version,
            timestamp: block.timestamp,
            prev_hash: block.previous_block_hash,
            nonce: block.nonce,
            orphan_status: true,
            height,
            hash: info.block_hash,
            difficulty: self.block_difficulty(height)?,
            reward: block.coinbase_output_total().unwrap_or(0),
            // `extraDetails.transactions.size()`: the coinbase counts.
            num_txes: block.transaction_hashes.len() as u64 + 1,
            block_size: (blob.len() as u64 + info.block_size as u64).saturating_sub(coinbase_size),
        })
    }

    /// `Core::findBlockchainSupplement` (`Core.cpp:1560`): the index of the
    /// first hash this chain knows. An empty list is index 0; a list where
    /// nothing is known **throws** in the C++, which the sync endpoints turn
    /// into a 500.
    fn find_blockchain_supplement(&self, hashes: &[Hash]) -> Result<u64> {
        if hashes.is_empty() {
            return Ok(0);
        }
        for hash in hashes {
            if let Some(index) = self.chain.block_index_by_hash(hash).map_err(chain_error)? {
                return Ok(index as u64);
            }
        }
        Err(ApiError::Internal("Genesis block hash was not found.".into()))
    }

    /// `DatabaseBlockchainCache::getBlockHeightForTimestamp`
    /// (`DatabaseBlockchainCache.cpp:2224`): the first block of the UTC day
    /// `timestamp` falls in, or `None` when no block carries that day.
    ///
    /// The C++ reads it from a per-day index its `pushBlock` fills — for each
    /// midnight, the first block pushed whose timestamp rounds to it
    /// (`:1599`). That is the lowest block index whose timestamp lies in the
    /// day, and [`first_block_of_day`] finds exactly that from the block infos
    /// this state already has, so the answer is the C++'s without a stored
    /// index.
    fn block_index_for_timestamp(&self, timestamp: u64) -> Result<Option<u64>> {
        first_block_of_day(self.tip(), midnight(timestamp), |i| Ok(self.info_at(i)?.timestamp))
    }

    /// `DatabaseBlockchainCache::getTimestampLowerBoundBlockIndex` (`:2246`):
    /// the first block of the latest day at or before `timestamp`'s that holds
    /// a block, and 0 when none does. `queryblockslite` takes its `fullOffset`
    /// from it.
    fn timestamp_lower_bound(&self, timestamp: u64) -> Result<u64> {
        timestamp_lower_bound(self.tip(), timestamp, |i| Ok(self.info_at(i)?.timestamp))
    }

    /// The `(amount, global index)` pairs a block created, in creation order:
    /// the coinbase's outputs first, then each transaction's, which is exactly
    /// the order `push_outputs` wrote them in (`ChainState::push_block`).
    ///
    /// Through [`ChainState::block_output_refs`], which rebuilds the record from
    /// the block body where a state imported with a short unwind history has
    /// dropped it — so a node on such an import still answers
    /// `/get_global_indexes_for_range` for every height it holds a body for.
    fn block_output_refs(&self, index: u64) -> Result<Option<Vec<(u64, u32)>>> {
        self.chain.block_output_refs(index_of(index)?).map_err(chain_error)
    }

    /// Per-transaction global indexes for one block, in `(hash, indexes)` pairs.
    fn global_indexes_of_block(&self, index: u64) -> Result<Vec<(Hash, Vec<u64>)>> {
        let Some(refs) = self.block_output_refs(index)? else {
            return Ok(Vec::new());
        };
        let (block, txs, _) = self.block_at(index)?;
        let hashes = self.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
        let mut counts: Vec<(Hash, usize)> = Vec::with_capacity(txs.len() + 1);
        counts.push((*hashes.first().unwrap_or(&[0; 32]), block.base_transaction.prefix.outputs.len()));
        for (blob, hash) in txs.iter().zip(hashes.iter().skip(1)) {
            let tx = Transaction::from_bytes(blob)
                .map_err(|e| ApiError::Internal(format!("stored transaction does not parse: {e}")))?;
            counts.push((*hash, tx.prefix.outputs.len()));
        }
        let mut out = Vec::with_capacity(counts.len());
        let mut at = 0usize;
        for (hash, n) in counts {
            let slice = refs.get(at..at + n).unwrap_or(&[]);
            out.push((hash, slice.iter().map(|(_, gi)| *gi as u64).collect()));
            at += n;
        }
        Ok(out)
    }

    /// `sizeMedian` and the coins generated before `index`: what
    /// `Core::getBlockDetails` feeds the reward function (`Core.cpp:4757-4765`).
    fn reward_inputs(&self, index: u64) -> Result<(u64, u64)> {
        if index == 0 {
            return Ok((0, 0));
        }
        let mut sizes = self.last_block_sizes(CRYPTONOTE_REWARD_BLOCKS_WINDOW as u64, index - 1)?;
        Ok((wrkz_chain::reward::median_value(&mut sizes), self.info_at(index - 1)?.already_generated_coins))
    }

    /// `Core::getBlockDetails` (`Core.cpp:4715-4817`) for a main-chain block,
    /// as `/queryblocksdetailed` prints it.
    fn detailed_block(&self, index: u64) -> Result<DetailedBlock> {
        let info = self.info_at(index)?;
        let (block, tx_blobs, blob) = self.block_at(index)?;
        let hashes = self.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
        let transactions_cumulative_size = info.block_size as u64;
        let coinbase_blob = block
            .base_transaction
            .to_bytes()
            .map_err(|e| ApiError::Internal(format!("stored coinbase does not serialise: {e}")))?;
        let (size_median, previous_coins) = self.reward_inputs(index)?;
        let base_reward =
            wrkz_chain::reward::get_block_reward(block.major_version, size_median, 0, previous_coins, 0, index)
                .map(|r| r.reward)
                .unwrap_or(0);
        // Per-transaction global indexes; a transaction the node cannot place
        // gets zeros, as `Core.cpp:4962` does.
        let global_indexes = self.global_indexes_of_block(index)?;
        let indexes_of = |i: usize| global_indexes.get(i).map(|(_, v)| v.as_slice()).unwrap_or(&[]);

        let mut transactions = Vec::with_capacity(tx_blobs.len() + 1);
        let coinbase = Transaction { prefix: block.base_transaction.prefix.clone(), signatures: Vec::new() };
        let coinbase_hash = *hashes.first().unwrap_or(&[0; 32]);
        transactions.push(self.detailed_transaction(
            &coinbase,
            coinbase_hash,
            coinbase_blob.len() as u64,
            info.timestamp,
            indexes_of(0),
        )?);
        let mut total_fee_amount = 0u64;
        for (i, (raw, hash)) in tx_blobs.iter().zip(hashes.iter().skip(1)).enumerate() {
            let tx = Transaction::from_bytes(raw)
                .map_err(|e| ApiError::Internal(format!("stored transaction does not parse: {e}")))?;
            let details = self.detailed_transaction(&tx, *hash, raw.len() as u64, info.timestamp, indexes_of(i + 1))?;
            total_fee_amount = total_fee_amount.wrapping_add(details.fee);
            transactions.push(details);
        }

        Ok(DetailedBlock {
            major_version: block.major_version,
            minor_version: block.minor_version,
            timestamp: block.timestamp,
            prev_hash: block.previous_block_hash,
            index,
            hash: info.block_hash,
            difficulty: self.block_difficulty(index)?,
            reward: block.coinbase_output_total().unwrap_or(0),
            block_size: (blob.len() as u64 + transactions_cumulative_size).saturating_sub(coinbase_blob.len() as u64),
            transactions_cumulative_size,
            already_generated_coins: info.already_generated_coins,
            already_generated_transactions: info.already_generated_transactions,
            size_median,
            base_reward,
            nonce: block.nonce,
            total_fee_amount,
            transactions,
        })
    }

    /// `Core::getTransactionDetails` for a transaction mined in a main-chain
    /// block (`Core.cpp:4833-4980`).
    fn detailed_transaction(
        &self,
        tx: &Transaction,
        hash: Hash,
        size: u64,
        timestamp: u64,
        global_indexes: &[u64],
    ) -> Result<DetailedTransaction> {
        let total_outputs_amount = tx.prefix.sum_outputs().unwrap_or(0);
        let mut total_inputs_amount = 0u64;
        let mut mixin = 0u64;
        let mut inputs = Vec::with_capacity(tx.prefix.inputs.len());
        for input in &tx.prefix.inputs {
            match input {
                Input::Base { block_index } => {
                    inputs.push(DetailedInput::Base { block_index: *block_index, amount: total_outputs_amount })
                }
                Input::Key { amount, key_offsets, key_image } => {
                    total_inputs_amount = total_inputs_amount.wrapping_add(*amount);
                    mixin = mixin.max(key_offsets.len() as u64);
                    // `extractKeyOtputReferences(...).back()`: the ring's last
                    // member, the highest global index.
                    let last = relative_offsets_to_absolute(key_offsets).and_then(|a| a.last().copied());
                    let record = match last {
                        Some(gi) => wrkz_chain::validate::ChainAccess::key_output(self.chain, *amount, gi)
                            .map_err(chain_error)?,
                        None => None,
                    };
                    let Some(record) = record else {
                        return Err(ApiError::Internal(format!(
                            "a ring member of transaction {} is not in the chain",
                            hex::encode(hash)
                        )));
                    };
                    // Below a snapshot import's line the record's transaction
                    // hash is the zero the snapshot carries, not the real one.
                    if record.block_index < self.chain.transactions_floor() {
                        let refusal = self.body_policy().transaction_refusal().unwrap_or_default();
                        return Err(ApiError::NotHeld(refusal));
                    }
                    inputs.push(DetailedInput::Key {
                        amount: *amount,
                        key_offsets: key_offsets.clone(),
                        key_image: *key_image,
                        mixin: key_offsets.len() as u64,
                        output_transaction_hash: record.transaction_hash,
                        output_number: u64::from(record.output_index),
                    });
                }
            }
        }
        let outputs = tx
            .prefix
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| DetailedOutput {
                amount: o.amount,
                key: o.key,
                global_index: global_indexes.get(i).copied().unwrap_or(0),
            })
            .collect();
        let (extra_public_key, extra_nonce) = parse_extra_strict(&tx.prefix.extra);
        // `getPaymentIdFromTransactionExtraNonce`: the nonce is exactly the
        // payment-id sub-tag and 32 bytes.
        let payment_id = match extra_nonce.as_slice() {
            [0x00, id @ ..] if id.len() == 32 => id.try_into().expect("32 bytes"),
            _ => [0u8; 32],
        };
        Ok(DetailedTransaction {
            hash,
            timestamp,
            size,
            // `CachedTransaction::getTransactionFee`: 0 the moment it sees a
            // base input.
            fee: tx.fee().unwrap_or(0),
            unlock_time: tx.prefix.unlock_time,
            total_inputs_amount,
            total_outputs_amount,
            mixin,
            payment_id,
            extra_public_key: extra_public_key.unwrap_or([0; 32]),
            extra_nonce,
            extra_raw: tx.prefix.extra.clone(),
            inputs,
            outputs,
            signatures: tx.signatures.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// wallet sync
// ---------------------------------------------------------------------------

/// `Utilities::parseExtra(...).paymentID`: 64 hex characters for a plaintext
/// long id, 16 for the ciphertext of an encrypted short id, empty otherwise.
/// The daemon cannot decrypt a short id, and never reports a legacy plaintext
/// one (spec/09).
fn payment_id_string(extra: &[u8]) -> String {
    match wrkz_primitives::tx::parse_extra_wallet(extra).payment_id {
        Some(PaymentId::Long(id)) => hex::encode(id),
        Some(PaymentId::EncryptedShort(id)) => hex::encode(id),
        None => String::new(),
    }
}

/// `Core::getRawCoinbaseTransaction` (`Core.cpp:1271`). The public key comes
/// from `getTransactionPublicKeyFromExtra`, which is the consensus parser.
fn raw_coinbase(prefix: &TransactionPrefix, hash: Hash) -> SyncTransaction {
    SyncTransaction {
        hash,
        outputs: prefix
            .outputs
            .iter()
            .map(|o| SyncOutput { amount: o.amount, key: o.key, global_index: None })
            .collect(),
        tx_public_key: wrkz_primitives::tx::parse_extra(&prefix.extra).public_key.unwrap_or([0; 32]),
        unlock_time: prefix.unlock_time,
        payment_id: String::new(),
        inputs: Vec::new(),
    }
}

/// `Core::getRawTransaction` (`Core.cpp:1295`). Here the public key and the
/// payment id come from the **wallet** parser `Utilities::parseExtra`.
fn raw_transaction(tx: &Transaction, hash: Hash) -> SyncTransaction {
    let parsed = wrkz_primitives::tx::parse_extra_wallet(&tx.prefix.extra);
    SyncTransaction {
        hash,
        outputs: tx
            .prefix
            .outputs
            .iter()
            .map(|o| SyncOutput { amount: o.amount, key: o.key, global_index: None })
            .collect(),
        tx_public_key: parsed.public_key.unwrap_or([0; 32]),
        unlock_time: tx.prefix.unlock_time,
        payment_id: payment_id_string(&tx.prefix.extra),
        inputs: tx
            .prefix
            .inputs
            .iter()
            .filter_map(|i| match i {
                wrkz_primitives::tx::Input::Key { amount, key_offsets, key_image } => {
                    Some(SyncInput { amount: *amount, key_image: *key_image, key_offsets: key_offsets.clone() })
                }
                wrkz_primitives::tx::Input::Base { .. } => None,
            })
            .collect(),
    }
}

/// `WalletTypes::WalletBlockInfo::memoryUsage()` (`include/WalletTypes.h:92`).
fn block_memory_usage(block: &SyncBlock) -> u64 {
    let tx_usage = |t: &SyncTransaction, coinbase: bool| -> u64 {
        let base = t.outputs.len() as u64 * sizes::KEY_OUTPUT + sizes::VECTOR + sizes::COINBASE_FIXED;
        if coinbase {
            base
        } else {
            t.payment_id.len() as u64 + sizes::STRING + t.inputs.len() as u64 * sizes::KEY_INPUT + sizes::VECTOR + base
        }
    };
    let coinbase = block.coinbase.as_ref().map(|c| tx_usage(c, true)).unwrap_or(sizes::OPTIONAL_COINBASE);
    let txs = block.transactions.iter().fold(sizes::VECTOR, |a, t| a + tx_usage(t, false));
    coinbase + txs + sizes::BLOCK_FIXED
}

impl<S: KvStore> View<'_, S> {
    /// The head of `Core::getWalletSyncData` / `Core::getRawBlocks`
    /// (`Core.cpp:938-1000`), which both share: resolve the start index, or
    /// report that the caller is already at the top.
    ///
    /// `Ok(Err(top))` is the "return the top block and nothing else" case.
    #[allow(clippy::type_complexity)]
    fn resolve_sync_start(&self, r: &SyncRequest) -> Result<std::result::Result<(u64, u64, u64), TopBlock>> {
        let current_index = self.tip();
        let current_hash = self.info_at(current_index)?.block_hash;
        let top = TopBlock { hash: current_hash, height: current_index };

        let actual_block_count =
            if r.block_count == 0 { BLOCKS_SYNCHRONIZING_DEFAULT_COUNT as u64 } else { r.block_count }
                .min(BLOCKS_SYNCHRONIZING_MAX_COUNT as u64);

        let timestamp_index = if r.start_timestamp == 0 {
            0
        } else {
            match self.block_index_for_timestamp(r.start_timestamp)? {
                Some(i) => i,
                // `startTimestamp != 0 && !success`: the node is not synced far
                // enough, so it answers with the top block and no blocks.
                None => return Ok(Err(top)),
            }
        };

        // `Core::resolveWalletSyncStartIndex` (`Core.cpp:839`), then the lite
        // and prune clamp: `startHeight = liteStartHeight; startTimestamp = 0`
        // (`RpcServer.cpp:1248-1253`). Raising the resolved height is the same
        // thing, and covers the prune floor with it.
        let first_block_height =
            if r.start_height == 0 { timestamp_index } else { r.start_height }.max(self.sync_floor());
        let last_known = self.find_blockchain_supplement(&r.block_hash_checkpoints)?;
        let start_index = std::cmp::max(if last_known == 0 { 0 } else { last_known + 1 }, first_block_height);

        if current_index < start_index {
            return Ok(Err(top));
        }
        let block_difference = current_index.abs_diff(start_index);
        Ok(Ok((start_index, actual_block_count, block_difference)))
    }
}

// ---------------------------------------------------------------------------
// the timestamp index, answered from block infos
// ---------------------------------------------------------------------------

/// `ONE_DAY_SECONDS`: the width of a bucket of the C++ timestamp index.
const ONE_DAY: u64 = 86_400;

/// How many blocks around a timestamp crossing are searched for one that
/// crosses earlier (or, for [`first_block_of_day`], later).
///
/// Block timestamps are only nearly ordered: a block must be above the median
/// of the last 60 and at most a few minutes ahead of the validating node's
/// clock, so the sequence can dither around a given second for a handful of
/// blocks and no more. 720 blocks is twelve hours of them, far beyond anything
/// that rule lets through, and costs 720 record reads on a call a wallet makes
/// once, when it is created.
const TIMESTAMP_INVERSION_WINDOW: u64 = 720;

/// `roundToMidnight` (`DatabaseBlockchainCache.cpp:184`), UTC.
fn midnight(timestamp: u64) -> u64 {
    timestamp / ONE_DAY * ONE_DAY
}

/// The lowest block index in `0..=tip` whose timestamp is at or after `t`, or
/// `None` when there is none.
///
/// A binary search finds a crossing — a block at or after `t` right after one
/// before it — and then the window below the crossing is scanned for an
/// earlier block already past `t`, which the near-ordering of timestamps
/// confines to that window.
fn first_at_or_after(tip: u64, t: u64, mut timestamp_at: impl FnMut(u64) -> Result<u64>) -> Result<Option<u64>> {
    let (mut low, mut high) = (0u64, tip + 1);
    while low < high {
        let mid = low + (high - low) / 2;
        if timestamp_at(mid)? >= t {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    for i in low.saturating_sub(TIMESTAMP_INVERSION_WINDOW)..low.min(tip + 1) {
        if timestamp_at(i)? >= t {
            return Ok(Some(i));
        }
    }
    Ok((low <= tip).then_some(low))
}

/// The lowest block index whose timestamp lies in the UTC day starting at
/// `day`: what the C++ per-day index holds for that midnight.
///
/// Usually that is the first block at or after midnight. When that block is
/// already stamped a *later* day, the day's first block — if it has one —
/// arrived after it, and is looked for in the window that follows.
fn first_block_of_day(tip: u64, day: u64, mut timestamp_at: impl FnMut(u64) -> Result<u64>) -> Result<Option<u64>> {
    let Some(first) = first_at_or_after(tip, day, &mut timestamp_at)? else { return Ok(None) };
    let end = day.saturating_add(ONE_DAY);
    if timestamp_at(first)? < end {
        return Ok(Some(first));
    }
    for i in first + 1..=tip.min(first + TIMESTAMP_INVERSION_WINDOW) {
        let t = timestamp_at(i)?;
        if t >= day && t < end {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// `getTimestampLowerBoundBlockIndex`: the first block of the latest day at or
/// before `timestamp`'s day that holds a block; 0 when none does.
///
/// The C++ walks back one midnight at a time until its index has an entry,
/// one read per day. The latest earlier day with a block is simply the day of
/// the latest timestamp before that day's midnight, which sits just below the
/// first block at or after it, so this jumps there in one step instead.
fn timestamp_lower_bound(tip: u64, timestamp: u64, mut timestamp_at: impl FnMut(u64) -> Result<u64>) -> Result<u64> {
    let day = midnight(timestamp);
    if day == 0 {
        // `while (midnight > 0)` never runs, and the answer is 0.
        return Ok(0);
    }
    if let Some(first) = first_block_of_day(tip, day, &mut timestamp_at)? {
        return Ok(first);
    }
    let end = first_at_or_after(tip, day, &mut timestamp_at)?.unwrap_or(tip + 1);
    if end == 0 {
        return Ok(0);
    }
    let mut latest = 0u64;
    for i in end.saturating_sub(TIMESTAMP_INVERSION_WINDOW)..end {
        latest = latest.max(timestamp_at(i)?);
    }
    Ok(first_block_of_day(tip, midnight(latest), &mut timestamp_at)?.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// decoy selection
// ---------------------------------------------------------------------------

/// `ShuffleGenerator<uint32_t>` (`src/common/ShuffleGenerator.h`): a lazy
/// Fisher-Yates over `0..n`, so drawing `k` distinct values out of millions
/// costs `k` map entries rather than an `n`-element array.
struct ShuffleGenerator {
    n: u32,
    remaining: u32,
    swapped: std::collections::HashMap<u32, u32>,
    rng: u64,
}

impl ShuffleGenerator {
    fn new(n: u32, seed: u64) -> Self {
        Self { n, remaining: n, swapped: std::collections::HashMap::new(), rng: seed | 1 }
    }

    /// splitmix64, which is enough for choosing decoys uniformly; the seed
    /// comes from the OS.
    fn next_u64(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// The next distinct value, or `None` once the sequence is exhausted.
    fn next(&mut self) -> Option<u32> {
        if self.remaining == 0 {
            return None;
        }
        let i = (self.next_u64() % self.remaining as u64) as u32;
        let last = self.remaining - 1;
        let value = *self.swapped.get(&i).unwrap_or(&i);
        let tail = *self.swapped.get(&last).unwrap_or(&last);
        self.swapped.insert(i, tail);
        self.swapped.remove(&last);
        self.remaining = last;
        let _ = self.n;
        Some(value)
    }
}

/// Unusable candidates the uniform pick of `/getrandom_outs` may pass over for
/// one amount before it settles for what it has: a floor, plus a multiple of
/// the outputs wanted.
///
/// The C++ (`Core.cpp:2047-2062`) walks the shuffled indexes until it has
/// enough or runs out, with a database read per candidate, so one amount whose
/// outputs are mostly still locked makes one request read every output of
/// that amount. Stopping early answers with fewer, which is what running out
/// already does — "finding fewer is not an error" (`Core.cpp:2054`) — and the
/// wallet handles it the same way. On a denomination where even one output in
/// twenty is usable, the chance of stopping short is below one in a million
/// (for one wanted: 0.95^356 ≈ 10⁻⁸).
const UNIFORM_MISS_FLOOR: u32 = 256;
const UNIFORM_MISSES_PER_WANTED: u32 = 100;

/// The C++'s uniform pick (`Core.cpp:2047-2062`): up to `wanted` distinct
/// usable indexes below `total`, in shuffled order, none of them in `taken`,
/// passing over at most [`UNIFORM_MISS_FLOOR`] +
/// [`UNIFORM_MISSES_PER_WANTED`] · `wanted` unusable ones.
fn uniform_picks(
    total: u32,
    wanted: u32,
    taken: &std::collections::HashSet<u32>,
    seed: u64,
    usable: &dyn Fn(u32) -> Result<bool>,
) -> Result<Vec<u32>> {
    let mut picks = Vec::with_capacity(wanted as usize);
    let mut misses_left = UNIFORM_MISS_FLOOR.saturating_add(wanted.saturating_mul(UNIFORM_MISSES_PER_WANTED));
    let mut generator = ShuffleGenerator::new(total, seed);
    while (picks.len() as u32) < wanted {
        let Some(gi) = generator.next() else { break };
        // The generator never repeats itself, so only the recent draws can
        // already hold this index — and asking them costs no read.
        if taken.contains(&gi) {
            continue;
        }
        if usable(gi)? {
            picks.push(gi);
            continue;
        }
        if misses_left == 0 {
            break;
        }
        misses_left -= 1;
    }
    Ok(picks)
}

fn random_seed() -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        // Falling back to the clock only degrades decoy *diversity*, never
        // correctness; a node that reaches this has no OS entropy source.
        return now().wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    u64::from_le_bytes(b)
}

/// splitmix64 again, as a source of uniform doubles for [`gamma`].
struct SplitMix(u64);

impl SplitMix {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`, 53 bits.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n`, for `n > 0`.
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }

    /// Standard normal (Box–Muller).
    fn normal(&mut self) -> f64 {
        let u1 = self.unit().max(f64::MIN_POSITIVE);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Gamma(`shape`, `scale`) for `shape >= 1` (Marsaglia and Tsang, 2000).
fn gamma(rng: &mut SplitMix, shape: f64, scale: f64) -> f64 {
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x = rng.normal();
        let v = 1.0 + c * x;
        if v <= 0.0 {
            continue;
        }
        let v = v * v * v;
        let u = rng.unit();
        if u < 1.0 - 0.0331 * x * x * x * x || u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
            return d * v * scale;
        }
    }
}

/// Monero's decoy age distribution (`wallet2.cpp`, `GAMMA_SHAPE` and
/// `GAMMA_SCALE`): the natural log of an output's age in seconds is
/// Gamma(19.28, 1/1.61), fitted to the ages of real spends. Its median is
/// about a day and a half.
const DECOY_GAMMA_SHAPE: f64 = 19.28;
const DECOY_GAMMA_SCALE: f64 = 1.0 / 1.61;

/// The first of the outputs `0..limit` whose block is at or above `block`, or
/// `limit` when there is none. Outputs are numbered in chain order, so their
/// blocks never decrease and a binary search is exact.
fn first_output_at_or_after(limit: u32, block: u64, block_of: &dyn Fn(u32) -> Result<u64>) -> Result<u32> {
    let (mut lo, mut hi) = (0u32, limit);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if block_of(mid)? < block {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// Up to `wanted` distinct global indexes of one amount, drawn towards recent
/// outputs ([`DecoySelection::Recent`]).
///
/// Each draw is an age from the gamma above, in blocks of `DIFFICULTY_TARGET`
/// seconds, counted back from `upper_block` (the newest block whose outputs
/// may be offered); the pick is a uniform one among the outputs created in
/// that block, or the first output after it when the block created none of
/// this amount. A draw older than the amount's first output is thrown away, as
/// Monero throws them away. At most `20 × wanted` draws are made: a caller
/// that gets fewer back fills the rest uniformly.
fn recent_picks(
    total: u32,
    wanted: u32,
    upper_block: u64,
    block_of: &dyn Fn(u32) -> Result<u64>,
    rng: &mut SplitMix,
) -> Result<Vec<u32>> {
    // Outputs `0..end` are at or below `upper_block`.
    let end = first_output_at_or_after(total, upper_block.saturating_add(1), block_of)?;
    if end == 0 || wanted == 0 {
        return Ok(Vec::new());
    }
    let oldest = block_of(0)?;
    let mut picks: Vec<u32> = Vec::with_capacity(wanted as usize);
    for _ in 0..wanted.saturating_mul(20) {
        if picks.len() as u32 == wanted {
            break;
        }
        let age_seconds = gamma(rng, DECOY_GAMMA_SHAPE, DECOY_GAMMA_SCALE).exp();
        let age_blocks = (age_seconds / DIFFICULTY_TARGET as f64) as u64;
        let Some(target) = upper_block.checked_sub(age_blocks) else { continue };
        if target < oldest {
            continue;
        }
        let first = first_output_at_or_after(end, target, block_of)?;
        if first >= end {
            continue;
        }
        let next = first_output_at_or_after(end, target + 1, block_of)?;
        let gi = if next > first { first + rng.below(next - first) } else { first };
        if !picks.contains(&gi) {
            picks.push(gi);
        }
    }
    Ok(picks)
}

// ---------------------------------------------------------------------------
// NodeApi
// ---------------------------------------------------------------------------

impl<S: KvStore + Send + Sync> NodeApi for ChainNode<S> {
    fn info(&self) -> Result<InfoSnapshot> {
        let chain = self.read();
        let s = self.view(&chain);
        let p2p = (self.p2p)();
        let policy = s.body_policy();
        let tip = s.tip();
        let height = tip + 1;
        let info = s.info_at(tip)?;
        let difficulty =
            s.chain.main_chain_difficulty_for_next_block(index_of(tip)?).map_err(chain_error)?.unwrap_or(0);
        // `getBlockDetails(getTopBlockIndex())` for the version pair, with the
        // C++'s own fallback of 0/0 when it could not be read
        // (`RpcServer.cpp:911`).
        let (major_version, minor_version) = match s.block_at(tip) {
            Ok((b, _, _)) => (b.major_version, b.minor_version),
            Err(_) => (0, 0),
        };
        let network_height = if p2p.blockchain_height == 0 { height } else { std::cmp::max(1, p2p.blockchain_height) };
        Ok(InfoSnapshot {
            height,
            top_block_hash: info.block_hash,
            difficulty,
            // `getBlockchainTransactionCount() - height`: the running count
            // includes one coinbase per block, which this subtracts.
            tx_count: info.already_generated_transactions.saturating_sub(height),
            tx_pool_size: self.pool().len() as u64,
            alt_blocks_count: s.chain.alternative_block_count() as u64,
            outgoing_connections_count: p2p.outgoing_connections,
            incoming_connections_count: p2p.total_connections.saturating_sub(p2p.outgoing_connections),
            white_peerlist_size: p2p.white_peers.len() as u64,
            grey_peerlist_size: p2p.gray_peers.len() as u64,
            seed_nodes_count: p2p.seed_nodes_count,
            last_seed_bootstrap: p2p.last_seed_bootstrap,
            last_known_block_index: std::cmp::max(1, p2p.observed_height) - 1,
            network_height,
            // `/info`'s three mode fields are answered from the state's own
            // configuration, not from what the P2P layer was told to
            // advertise, so `pruned` and `lite_start_height` describe the
            // database an operator is actually running. The P2P values are
            // taken as a floor: a body-less import that carries no lite marker
            // shows up there and nowhere else (`daemon::lowest_stored_block`).
            pruned: policy.is_pruned() || p2p.pruned,
            prune_depth: policy.prune_depth.unwrap_or(p2p.prune_depth),
            prune_capability_active: p2p.prune_capability_active,
            lite_start_height: policy.lite_start_height.max(p2p.lite_start_height),
            sync_active_peers: p2p.sync_active_peers,
            sync_avg_batch_size: p2p.sync_avg_batch_size,
            sync_demoted_peers: p2p.sync_demoted_peers,
            major_version,
            minor_version,
            version: crate::DAEMON_VERSION.into(),
            start_time: self.start_time,
        })
    }

    fn height(&self) -> HeightSnapshot {
        let chain = self.read();
        let s = self.view(&chain);
        let p2p = (self.p2p)();
        let height = s.tip() + 1;
        let network_height = if p2p.blockchain_height == 0 { height } else { std::cmp::max(1, p2p.blockchain_height) };
        HeightSnapshot { height, network_height }
    }

    fn peers(&self) -> PeerLists {
        let p2p = (self.p2p)();
        PeerLists { white: p2p.white_peers, gray: p2p.gray_peers }
    }

    fn is_synced(&self) -> bool {
        let p2p = (self.p2p)();
        let height = self.view(&self.read()).tip() + 1;
        let network_height = if p2p.blockchain_height == 0 { height } else { std::cmp::max(1, p2p.blockchain_height) };
        p2p.synchronized && height >= network_height
    }

    fn top_index(&self) -> u64 {
        self.view(&self.read()).tip()
    }

    /// `Core::save()`. Takes the *write* guard — the same one `addBlock` takes
    /// — for exactly the length of the commit, so a block arriving waits for
    /// the flush and nothing else. The `sync` afterwards is what makes the
    /// state resumable after a power cut, which is the whole point of the
    /// console's `save`.
    fn save(&self) -> Result<()> {
        let mut chain = self.write();
        chain.flush().map_err(chain_error)?;
        chain.sync().map_err(chain_error)
    }

    fn block_hash_by_index(&self, index: u64) -> Result<Option<Hash>> {
        let chain = self.read();
        let s = self.view(&chain);
        if index > s.tip() {
            return Ok(None);
        }
        Ok(Some(s.info_at(index)?.block_hash))
    }

    fn block_header_by_hash(&self, hash: &Hash) -> Result<Option<BlockHeaderInfo>> {
        let chain = self.read();
        let s = self.view(&chain);
        match s.chain.block_index_by_hash(hash).map_err(chain_error)? {
            Some(index) => s.header_at(index as u64).map(Some),
            // `Core::getBlockByHash` searches every segment, the alternative
            // ones included.
            None => match s.chain.alternative_block(hash) {
                Some((index, info, (blob, _))) => s.alternative_header(index, &info, &blob).map(Some),
                None => Ok(None),
            },
        }
    }

    fn block_header_by_index(&self, index: u64) -> Result<Option<BlockHeaderInfo>> {
        let chain = self.read();
        let s = self.view(&chain);
        if index > s.tip() {
            return Ok(None);
        }
        s.header_at(index).map(Some)
    }

    fn block_list(&self, height: u64) -> Result<Vec<BlockListEntry>> {
        let chain = self.read();
        let s = self.view(&chain);
        // `RpcServer.cpp:1996`: `for (i = height; i >= startHeight; i--)` with
        // `startHeight = height < 30 ? 0 : height - 30`. Below 30 the unsigned
        // loop runs past zero and the block lookup throws, which the middleware
        // answers as a 500 — reproduced rather than quietly fixed, because a
        // caller that sees a 500 there today would see a different answer from
        // a port that "fixed" it.
        if height < 30 {
            return Err(ApiError::Internal("Requested hash wasn't found in main blockchain".into()));
        }
        let start = height - 30;
        let mut out = Vec::with_capacity(31);
        for i in (start..=height).rev() {
            let info = s.info_at(i)?;
            let (block, _, blob) = s.block_at(i)?;
            let coinbase_size = block.base_transaction.to_bytes().map(|b| b.len() as u64).unwrap_or(0);
            out.push(BlockListEntry {
                cumul_size: blob.len() as u64 + info.block_size as u64 - coinbase_size,
                difficulty: s.block_difficulty(i)?,
                hash: info.block_hash,
                height: i,
                timestamp: block.timestamp,
                tx_count: block.transaction_hashes.len() as u64 + 1,
            });
        }
        Ok(out)
    }

    fn block_details(&self, hash: &Hash) -> Result<Option<BlockDetails>> {
        let chain = self.read();
        let s = self.view(&chain);
        let Some(index) = s.chain.block_index_by_hash(hash).map_err(chain_error)? else {
            return Ok(None);
        };
        let index = index as u64;
        let header = s.header_at(index)?;
        let info = s.info_at(index)?;
        let (block, tx_blobs, _) = s.block_at(index)?;

        // `Core.cpp:4748`: `transactionsCumulativeSize` is the *stored* size.
        let transactions_cumulative_size = info.block_size as u64;

        let (size_median, previous_coins) = if index > 0 {
            let mut sizes = s.last_block_sizes(CRYPTONOTE_REWARD_BLOCKS_WINDOW as u64, index - 1)?;
            (wrkz_chain::reward::median_value(&mut sizes), s.info_at(index - 1)?.already_generated_coins)
        } else {
            (0, 0)
        };

        let reward_at = |current_size: u64| -> u64 {
            wrkz_chain::reward::get_block_reward(
                block.major_version,
                size_median,
                current_size,
                previous_coins,
                0,
                index,
            )
            .map(|r| r.reward)
            .unwrap_or(0)
        };
        let base_reward = reward_at(0);
        let current_reward = reward_at(transactions_cumulative_size);
        let penalty = if base_reward == 0 && current_reward == 0 {
            0.0
        } else {
            (base_reward.saturating_sub(current_reward)) as f64 / base_reward as f64
        };

        let hashes = s.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
        let mut transactions = Vec::with_capacity(tx_blobs.len() + 1);
        let coinbase_blob = block.base_transaction.to_bytes().unwrap_or_default();
        transactions.push(BlockTransactionSummary {
            hash: hashes.first().copied().unwrap_or([0; 32]),
            fee: 0,
            amount_out: block.coinbase_output_total().unwrap_or(0),
            size: coinbase_blob.len() as u64,
        });
        let mut total_fee_amount = 0u64;
        for (blob, hash) in tx_blobs.iter().zip(hashes.iter().skip(1)) {
            let tx = Transaction::from_bytes(blob)
                .map_err(|e| ApiError::Internal(format!("stored transaction does not parse: {e}")))?;
            let fee = tx.fee().unwrap_or(0);
            total_fee_amount = total_fee_amount.wrapping_add(fee);
            transactions.push(BlockTransactionSummary {
                hash: *hash,
                fee,
                amount_out: tx.prefix.sum_outputs().unwrap_or(0),
                size: blob.len() as u64,
            });
        }

        Ok(Some(BlockDetails {
            header,
            transactions_cumulative_size,
            already_generated_coins: info.already_generated_coins,
            already_generated_transactions: info.already_generated_transactions,
            size_median,
            base_reward,
            penalty,
            total_fee_amount,
            transactions,
        }))
    }

    fn wallet_sync_start_index(&self, request: &SyncRequest) -> Result<Option<u64>> {
        let chain = self.read();
        let s = self.view(&chain);
        // The resolution `wallet_sync_data` runs, so the key names exactly the
        // start its answer will have. An error is the unknown-checkpoints case,
        // which the full call reports; for the cache it only means there is
        // nothing to look up.
        Ok(match s.resolve_sync_start(request) {
            Ok(Ok((start_index, _, _))) => Some(start_index),
            Ok(Err(_)) | Err(_) => None,
        })
    }

    fn wallet_sync_data(&self, request: &SyncRequest) -> Result<WalletSyncData> {
        let chain = self.read();
        let s = self.view(&chain);
        let (start_index, actual_block_count, block_difference) = match s.resolve_sync_start(request)? {
            Ok(v) => v,
            Err(top) => return Ok(WalletSyncData { top_block: Some(top), ..Default::default() }),
        };
        let current_index = s.tip();
        let current_hash = s.info_at(current_index)?.block_hash;

        let mut end_index = std::cmp::min(actual_block_count, block_difference + 1) + start_index;

        // `Core.cpp:1005`: widening only applies when both flags are set.
        let skip_empty = request.skip_empty_blocks && request.skip_coinbase_transactions;
        let mut scan_count = end_index - start_index;
        if skip_empty {
            let widened = std::cmp::min(
                actual_block_count.saturating_mul(BLOCKS_SYNCHRONIZING_SKIP_EMPTY_SCAN_MULTIPLIER),
                BLOCKS_SYNCHRONIZING_SKIP_EMPTY_MAX_SCAN,
            );
            scan_count = std::cmp::min(widened, block_difference + 1);
        }
        let mut scan_end_index = start_index + std::cmp::max(scan_count, end_index - start_index);

        if request.end_height != 0 {
            end_index = std::cmp::min(end_index, request.end_height);
            scan_end_index = std::cmp::min(scan_end_index, request.end_height);
        }
        if end_index <= start_index {
            // `Core.cpp:1035`: the window is entirely behind the caller, and the
            // C++ returns before it would have set `topBlock`.
            return Ok(WalletSyncData::default());
        }
        if !skip_empty || scan_end_index < end_index {
            scan_end_index = end_index;
        }

        // `DatabaseBlockchainCache::getWalletSyncBlocks` (`:2830`): decide which
        // heights the response covers from the transaction *counts* alone,
        // before reading a single block body.
        let block_limit = end_index - start_index;
        let transaction_budget = std::cmp::max(1, MAX_RESPONSE_BYTES / ESTIMATED_WALLET_SYNC_TRANSACTION_BYTES);
        let mut selected: Vec<u64> = Vec::new();
        let mut transactions_so_far = 0u64;
        let mut last_scanned = 0u64;
        let mut scanned_any = false;
        for index in start_index..scan_end_index {
            if index > current_index {
                break;
            }
            let hashes = s.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
            if hashes.is_empty() {
                break;
            }
            let block_transactions = hashes.len() as u64;
            if !selected.is_empty() && transactions_so_far + block_transactions > transaction_budget {
                break;
            }
            scanned_any = true;
            last_scanned = index;
            if skip_empty && block_transactions <= 1 {
                continue;
            }
            transactions_so_far += block_transactions;
            selected.push(index);
            if selected.len() as u64 >= block_limit {
                break;
            }
        }
        if !scanned_any {
            // Nothing at all was readable: the C++ returns an empty list, and
            // the caller below fills in `topBlock`.
            return Ok(WalletSyncData {
                top_block: Some(TopBlock { hash: current_hash, height: current_index }),
                ..Default::default()
            });
        }
        if selected.last() != Some(&last_scanned) {
            selected.push(last_scanned);
        }

        let mut items = Vec::with_capacity(selected.len());
        let mut scanned_to_height = 0u64;
        let mut response_bytes = 0u64;
        for index in selected {
            let hashes = s.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
            let (block, tx_blobs, _) = s.block_at(index)?;
            let info = s.info_at(index)?;
            let coinbase = if request.skip_coinbase_transactions {
                None
            } else {
                Some(raw_coinbase(&block.base_transaction.prefix, hashes.first().copied().unwrap_or([0; 32])))
            };
            let mut transactions = Vec::with_capacity(tx_blobs.len());
            for (blob, hash) in tx_blobs.iter().zip(hashes.iter().skip(1)) {
                let tx = Transaction::from_bytes(blob)
                    .map_err(|e| ApiError::Internal(format!("stored transaction does not parse: {e}")))?;
                transactions.push(raw_transaction(&tx, *hash));
            }
            let item = SyncBlock {
                block_hash: info.block_hash,
                block_height: index,
                block_timestamp: block.timestamp,
                coinbase,
                transactions,
            };
            response_bytes += block_memory_usage(&item);
            items.push(item);
            // Coverage is claimed only as blocks actually go out.
            scanned_to_height = index;
            if response_bytes >= MAX_RESPONSE_BYTES {
                break;
            }
        }

        let top_block =
            if items.is_empty() { Some(TopBlock { hash: current_hash, height: current_index }) } else { None };
        Ok(WalletSyncData { items, top_block, scanned_to_height })
    }

    fn raw_blocks(&self, request: &SyncRequest) -> Result<RawBlocks> {
        let chain = self.read();
        let s = self.view(&chain);
        let (start_index, actual_block_count, block_difference) = match s.resolve_sync_start(request)? {
            Ok(v) => v,
            Err(top) => return Ok(RawBlocks { items: Vec::new(), top_block: Some(top) }),
        };
        let current_index = s.tip();
        let current_hash = s.info_at(current_index)?.block_hash;
        let end_index = std::cmp::min(actual_block_count, block_difference + 1) + start_index;

        let mut items: Vec<RawBlockItem> = Vec::new();
        let mut response_bytes = 0u64;
        if request.skip_coinbase_transactions {
            // `getNonEmptyBlocks(startIndex, count)`: up to `count` blocks at or
            // after `startIndex` that hold more than their coinbase.
            let mut taken = 0u64;
            for index in start_index..=current_index {
                if taken >= actual_block_count {
                    break;
                }
                let hashes = s.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
                if hashes.len() <= 1 {
                    continue;
                }
                let (_, txs, blob) = s.block_at(index)?;
                response_bytes += blob.len() as u64 + txs.iter().map(|t| t.len() as u64).sum::<u64>();
                items.push(RawBlockItem { block: blob, transactions: txs });
                taken += 1;
                if response_bytes >= MAX_RESPONSE_BYTES {
                    break;
                }
            }
        } else {
            for index in start_index..end_index {
                if index > current_index {
                    break;
                }
                let (_, txs, blob) = s.block_at(index)?;
                response_bytes += blob.len() as u64 + txs.iter().map(|t| t.len() as u64).sum::<u64>();
                items.push(RawBlockItem { block: blob, transactions: txs });
                // Checked after appending, so at least one block always goes out.
                if response_bytes >= MAX_RESPONSE_BYTES {
                    break;
                }
            }
        }

        let top_block =
            if items.is_empty() { Some(TopBlock { hash: current_hash, height: current_index }) } else { None };
        Ok(RawBlocks { items, top_block })
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> Result<Vec<(Hash, Vec<u64>)>> {
        let chain = self.read();
        let s = self.view(&chain);
        // `RpcServer.cpp:1509-1518`: a lite node refuses this outright rather
        // than answering for a region it does not index. This port keeps the
        // per-block output records at every height, so it *could* answer — but
        // a caller that gets a list here and a 400 from a C++ lite node for the
        // same request has no way to write one client for both, so the refusal
        // is reproduced. A pruned node is not refused: pruning drops bodies and
        // keeps every index, in this port as in the C++.
        let policy = s.body_policy();
        if policy.is_lite() && start < policy.lite_start_height {
            return Err(ApiError::NotHeld(format!(
                "This node is a lite node and stores no transaction data below height {}",
                policy.lite_start_height
            )));
        }
        let tip = s.tip();
        let mut out = Vec::new();
        for index in start..end {
            if index > tip {
                break;
            }
            out.extend(s.global_indexes_of_block(index)?);
        }
        Ok(out)
    }

    fn transaction_global_indexes(&self, hash: &Hash) -> Result<Option<Vec<u32>>> {
        let chain = self.read();
        let s = self.view(&chain);
        let Some(index) = s.chain.transaction_block_index(hash).map_err(chain_error)? else {
            return s.missing_transaction();
        };
        for (tx_hash, indexes) in s.global_indexes_of_block(index as u64)? {
            if tx_hash == *hash {
                return Ok(Some(indexes.into_iter().map(|i| i as u32).collect()));
            }
        }
        Ok(None)
    }

    fn random_outputs(&self, amount: u64, count: u16) -> Result<std::result::Result<Vec<(u32, Hash)>, String>> {
        let chain = self.read();
        let s = self.view(&chain);
        if count == 0 {
            return Ok(Ok(Vec::new()));
        }
        let top = s.tip();
        // `Core.cpp:2036`: the C++ computes `top - 40` in unsigned arithmetic
        // and then compares it with 40, so a chain below 80 blocks is refused
        // (below 40 it wraps to a huge number and passes, which cannot happen
        // on a chain that has any outputs to serve).
        let upper_block_limit = top.wrapping_sub(CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW);
        if upper_block_limit < CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW {
            return Ok(Err("Blockchain height is less than mined unlock window".into()));
        }
        let total = s.chain.output_count_for_amount(amount).map_err(chain_error)?;
        if total == 0 {
            return Ok(Ok(Vec::new()));
        }
        let wanted = std::cmp::min(count as u32, total);
        // `Core.cpp:2047-2062`: an output is a candidate once it is unlocked
        // and its block is below the mined-money unlock window.
        let usable = |gi: u32| -> Result<bool> {
            let Some(record) =
                wrkz_chain::validate::ChainAccess::key_output(s.chain, amount, gi as u64).map_err(chain_error)?
            else {
                return Ok(false);
            };
            Ok(wrkz_chain::validate::is_spend_time_unlocked(s.chain, record.unlock_time, top)
                && record.block_index as u64 <= upper_block_limit)
        };
        let mut chosen: Vec<u32> = Vec::with_capacity(wanted as usize);
        if self.decoys == DecoySelection::Recent {
            let block_of = |gi: u32| -> Result<u64> {
                match wrkz_chain::validate::ChainAccess::key_output(s.chain, amount, gi as u64).map_err(chain_error)? {
                    Some(record) => Ok(u64::from(record.block_index)),
                    None => Err(ApiError::Internal(format!("output {gi} of amount {amount} is missing"))),
                }
            };
            let mut rng = SplitMix::new(random_seed());
            for gi in recent_picks(total, wanted, upper_block_limit, &block_of, &mut rng)? {
                if usable(gi)? {
                    chosen.push(gi);
                }
            }
        }
        // The C++'s uniform pick — everything when `Uniform`, and whatever the
        // recent draws could not supply otherwise. A set, not `chosen` itself,
        // answers "already picked?", so the check is not a scan per candidate.
        let taken: std::collections::HashSet<u32> = chosen.iter().copied().collect();
        let still_wanted = wanted.saturating_sub(chosen.len() as u32);
        chosen.extend(uniform_picks(total, still_wanted, &taken, random_seed(), &usable)?);
        // `Core.cpp:2065`.
        chosen.sort_unstable();
        let mut out = Vec::with_capacity(chosen.len());
        for gi in chosen {
            match wrkz_chain::validate::ChainAccess::key_output(s.chain, amount, gi as u64).map_err(chain_error)? {
                Some(r) => out.push((gi, r.public_key)),
                None => return Ok(Err("Invalid global index is given".into())),
            }
        }
        Ok(Ok(out))
    }

    fn add_transaction_to_pool(&self, blob: &[u8]) -> std::result::Result<(), String> {
        // A read guard is enough: the pool validates against the chain and
        // writes only to itself.
        let chain = self.read();
        let status = self.pool().add(blob, &*chain, PoolSource::Rpc);
        drop(chain);
        if status.accepted() {
            self.relay_queue.lock().unwrap_or_else(|p| p.into_inner()).push(blob.to_vec());
            if self.events.is_listening() {
                if let Some(hash) = Transaction::from_bytes(blob).ok().and_then(|tx| tx.hash().ok()) {
                    self.events.pool_added(&[hash]);
                }
            }
            Ok(())
        } else {
            Err(status.message())
        }
    }

    fn transactions_status(&self, hashes: &[Hash]) -> Result<TransactionsStatus> {
        let chain = self.read();
        let s = self.view(&chain);
        // The C++ collects the request into an `unordered_set` first, so a
        // repeated hash is answered once.
        let pool = self.pool();
        let mut unique: Vec<Hash> = Vec::with_capacity(hashes.len());
        let mut seen = std::collections::HashSet::new();
        for hash in hashes {
            if seen.insert(*hash) {
                unique.push(*hash);
            }
        }
        // One `multi_get` for the whole request rather than one read per hash:
        // a wallet checking a batch of its own sends asks about dozens at once.
        let indexes = s.chain.transaction_block_indexes(&unique).map_err(chain_error)?;
        let mut out = TransactionsStatus::default();
        for (hash, index) in unique.iter().zip(indexes) {
            if pool.contains(hash) {
                out.in_pool.push(*hash);
            } else if index.is_some() {
                out.in_block.push(*hash);
            } else {
                out.unknown.push(*hash);
            }
        }
        Ok(out)
    }

    fn pool_transactions(&self) -> Result<Vec<PoolTransactionSummary>> {
        Ok(self
            .pool()
            .by_priority()
            .into_iter()
            .map(|e| PoolTransactionSummary { hash: e.hash, fee: e.fee, amount_out: e.amount, size: e.size() as u64 })
            .collect())
    }

    fn pool_changes_lite(&self, tail_block_id: &Hash, known: &[Hash]) -> Result<PoolChanges> {
        let chain = self.read();
        let s = self.view(&chain);
        let pool = self.pool();
        let known: std::collections::HashSet<Hash> = known.iter().copied().collect();
        // One priority-ordered snapshot, read by both the set and the loop:
        // `hashes()` sorts the whole pool, and once is enough.
        let hashes = pool.hashes();
        let held: std::collections::HashSet<Hash> = hashes.iter().copied().collect();
        let mut added = Vec::new();
        for hash in hashes {
            if known.contains(&hash) {
                continue;
            }
            if let Some(entry) = pool.get(&hash) {
                added.push(TxPrefixInfo { hash, prefix: entry.transaction.prefix.clone() });
            }
        }
        let deleted = known.iter().filter(|h| !held.contains(*h)).copied().collect();
        let top = s.info_at(s.tip())?.block_hash;
        Ok(PoolChanges { added, deleted, is_tail_block_actual: top == *tail_block_id })
    }

    fn query_blocks_lite(&self, known: &[Hash], timestamp: u64) -> Result<QueryBlocksLite> {
        let chain = self.read();
        let s = self.view(&chain);
        let current_index = s.tip();
        let start_index = s.find_blockchain_supplement(known)?;
        // `Core.cpp:664`: a caller that knows a block but no timestamp gets the
        // timestamp of the block it named.
        let timestamp = if start_index > 0 && timestamp == 0 && start_index <= current_index {
            s.info_at(start_index)?.timestamp
        } else {
            timestamp
        };
        // `fullOffset = getTimestampLowerBoundBlockIndex(timestamp)`, raised to
        // the start (`Core.cpp:617`). Then the same clamp the wallet-sync
        // endpoints take (`RpcServer.cpp:3006`): below the floor this node has
        // only hashes, and hash-only entries are exactly what `pushBlockHashes`
        // emits below `full_offset` anyway.
        let full_offset = std::cmp::max(s.timestamp_lower_bound(timestamp)?, start_index).max(s.sync_floor());

        // `pushBlockHashes(startIndex, fullOffset, BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT)`:
        // hash-only entries for everything below the timestamp bound.
        let mut items = Vec::new();
        let mut pushed = 0u64;
        for index in start_index..full_offset {
            if pushed >= BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT as u64 || index > current_index {
                break;
            }
            items.push(BlockShortInfo {
                block_id: s.info_at(index)?.block_hash,
                block: Vec::new(),
                tx_prefixes: Vec::new(),
            });
            pushed += 1;
        }
        if start_index + pushed != full_offset {
            return Ok(QueryBlocksLite { start_index, current_index, full_offset, items });
        }

        // `fillQueryBlockShortInfo(fullOffset, currentIndex, BLOCKS_SYNCHRONIZING_DEFAULT_COUNT)`
        // (`Core.cpp:4212`): `min(maxItemsCount, currentIndex - fullOffset + 1)`
        // *full* entries after the hash-only ones, however many of those there
        // were. Counting the hash-only entries against the limit, as this once
        // did, handed a catching-up wallet fewer blocks than the C++ does.
        let full = (BLOCKS_SYNCHRONIZING_DEFAULT_COUNT as u64).min((current_index + 1).saturating_sub(full_offset));
        for index in full_offset..full_offset + full {
            let (block, tx_blobs, blob) = s.block_at(index)?;
            let hashes = s.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
            let mut prefixes = Vec::with_capacity(tx_blobs.len());
            for (raw, hash) in tx_blobs.iter().zip(hashes.iter().skip(1)) {
                let tx = Transaction::from_bytes(raw)
                    .map_err(|e| ApiError::Internal(format!("stored transaction does not parse: {e}")))?;
                prefixes.push(TxPrefixInfo { hash: *hash, prefix: tx.prefix });
            }
            let _ = &block;
            items.push(BlockShortInfo { block_id: s.info_at(index)?.block_hash, block: blob, tx_prefixes: prefixes });
        }
        Ok(QueryBlocksLite { start_index, current_index, full_offset, items })
    }

    fn query_blocks_detailed(&self, known: &[Hash], timestamp: u64, block_count: u32) -> Result<QueryBlocksDetailed> {
        // `Core.cpp:726-742`: 0 is the default, 1 is raised to 2 so that a
        // caller stepping one block at a time still advances, and anything
        // above `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT` is cut to it.
        let limit = BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT as u64;
        let block_count = match u64::from(block_count) {
            0 => limit,
            1 => 2,
            n => n.min(limit),
        };
        let chain = self.read();
        let s = self.view(&chain);
        let current_index = s.tip();
        let start_index = s.find_blockchain_supplement(known)?;
        // The timestamp and offset rules of `queryblockslite` (`Core.cpp:754-768`),
        // with the same body floor.
        let timestamp = if start_index > 0 && timestamp == 0 && start_index <= current_index {
            s.info_at(start_index)?.timestamp
        } else {
            timestamp
        };
        let full_offset = std::cmp::max(s.timestamp_lower_bound(timestamp)?, start_index).max(s.sync_floor());

        // `pushBlockHashes(startIndex, fullOffset, blockCount)` (`Core.cpp:4141`):
        // the blocks below the offset, as hashes alone.
        let mut blocks = Vec::new();
        for index in start_index..start_index + (full_offset - start_index).min(block_count) {
            if index > current_index {
                break;
            }
            blocks.push(DetailedBlock { hash: s.info_at(index)?.block_hash, ..DetailedBlock::default() });
        }
        if start_index + blocks.len() as u64 != full_offset {
            return Ok(QueryBlocksDetailed { start_index, current_index, full_offset, blocks });
        }

        // `fillQueryBlockDetails(fullOffset, currentIndex, blockCount)` (`Core.cpp:4255`).
        let full = block_count.min((current_index + 1).saturating_sub(full_offset));
        for index in full_offset..full_offset + full {
            blocks.push(s.detailed_block(index)?);
        }
        Ok(QueryBlocksDetailed { start_index, current_index, full_offset, blocks })
    }

    fn transaction_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        let chain = self.read();
        let s = self.view(&chain);
        if let Some(entry) = self.pool().get(hash) {
            return Ok(Some(entry.blob.clone()));
        }
        let Some(index) = s.chain.transaction_block_index(hash).map_err(chain_error)? else {
            return s.missing_transaction();
        };
        let hashes = s.chain.block_transaction_hashes(index).map_err(chain_error)?;
        let (block, tx_blobs, _) = s.block_at(index as u64)?;
        if hashes.first() == Some(hash) {
            return Ok(block.base_transaction.to_bytes().ok());
        }
        for (blob, h) in tx_blobs.iter().zip(hashes.iter().skip(1)) {
            if h == hash {
                return Ok(Some(blob.clone()));
            }
        }
        Ok(None)
    }

    fn transaction_block_index(&self, hash: &Hash) -> Result<Option<u64>> {
        let chain = self.read();
        let s = self.view(&chain);
        match s.chain.transaction_block_index(hash).map_err(chain_error)? {
            Some(index) => Ok(Some(u64::from(index))),
            None => s.missing_transaction(),
        }
    }

    fn body_policy(&self) -> BodyPolicy {
        let chain = self.read();
        self.view(&chain).body_policy()
    }

    fn transaction_hashes_by_payment_id(&self, payment_id: &Hash) -> Result<Vec<Hash>> {
        let chain = self.read();
        let s = self.view(&chain);
        // A snapshot import holds no payment ids below its line, so any list
        // from it could be missing the head of the answer.
        if let Some(message) = s.body_policy().transaction_refusal() {
            return Err(ApiError::NotHeld(message));
        }
        // `Core.cpp:5002-5017`: the chain index first, then the pool, appended.
        let mut hashes = s.chain.transaction_hashes_by_payment_id(payment_id).map_err(chain_error)?;
        // The C++ pool index keys on a payment id that **defaults to the zero
        // hash** when extraction fails, so a query for 64 zeros matches every
        // pool transaction that carries no long payment id at all
        // (`TransactionPool.cpp:154-158`). `wrkz-mempool` stores `None` for
        // those instead of a zero hash, so that answer cannot arise here and
        // nothing has to be filtered out again.
        hashes.extend(self.pool().hashes_by_payment_id(payment_id));
        Ok(hashes)
    }

    fn transaction_details(&self, hash: &Hash) -> Result<Option<TransactionDetails>> {
        let chain = self.read();
        let s = self.view(&chain);
        // Chain only. `Core::getTransactions` walks the segments and never the
        // pool (`Core.cpp:1356-1406`), so `f_transaction_json` returns "does
        // not exist" for a transaction sitting in the mempool, and so does this.
        let Some(index) = s.chain.transaction_block_index(hash).map_err(chain_error)? else {
            return s.missing_transaction();
        };
        let index = u64::from(index);
        let hashes = s.chain.block_transaction_hashes(index_of(index)?).map_err(chain_error)?;
        // Everything below needs the block body, which a lite or pruned node
        // may not have for this height. Refusing names the mode; it must never
        // read as "no such transaction", because the index just said otherwise.
        let (block, tx_blobs, blob) = s.block_at(index)?;

        let (transaction, size) = if hashes.first() == Some(hash) {
            let coinbase = wrkz_primitives::tx::Transaction {
                prefix: block.base_transaction.prefix.clone(),
                signatures: Vec::new(),
            };
            let bytes = block
                .base_transaction
                .to_bytes()
                .map_err(|e| ApiError::Internal(format!("stored coinbase does not serialise: {e}")))?;
            let len = bytes.len() as u64;
            (coinbase, len)
        } else {
            let mut found = None;
            for (raw, h) in tx_blobs.iter().zip(hashes.iter().skip(1)) {
                if h == hash {
                    let tx = Transaction::from_bytes(raw)
                        .map_err(|e| ApiError::Internal(format!("stored transaction does not parse: {e}")))?;
                    found = Some((tx, raw.len() as u64));
                    break;
                }
            }
            match found {
                Some(v) => v,
                // The index named this block and the block does not hold it.
                None => return Ok(None),
            }
        };

        // `Core.cpp:4895-4910`: the largest ring over the key inputs.
        let mixin = transaction
            .prefix
            .inputs
            .iter()
            .filter_map(|i| match i {
                Input::Key { key_offsets, .. } => Some(key_offsets.len() as u64),
                Input::Base { .. } => None,
            })
            .max()
            .unwrap_or(0);

        // `paymentId` and `paymentIdEncrypted` come from the loose wallet
        // parser (`Utilities::parseExtra`, `RpcServer.cpp:2288`), `publicKey`
        // and `nonce` from the strict one — the same split the C++ makes.
        let parsed = parse_extra_wallet(&transaction.prefix.extra);
        let (payment_id, payment_id_encrypted) = match parsed.payment_id {
            Some(PaymentId::Long(id)) => (hex::encode(id), false),
            Some(PaymentId::EncryptedShort(id)) => (hex::encode(id), true),
            None => (String::new(), false),
        };
        let (extra_public_key, extra_nonce) = parse_extra_strict(&transaction.prefix.extra);

        let info = s.info_at(index)?;
        let coinbase_size = block.base_transaction.to_bytes().map(|b| b.len() as u64).unwrap_or(0);
        let short = BlockListEntry {
            cumul_size: blob.len() as u64 + info.block_size as u64 - coinbase_size,
            difficulty: s.block_difficulty(index)?,
            hash: info.block_hash,
            height: index,
            timestamp: block.timestamp,
            tx_count: block.transaction_hashes.len() as u64 + 1,
        };

        Ok(Some(TransactionDetails {
            block: short,
            hash: *hash,
            amount_out: transaction.prefix.sum_outputs().unwrap_or(0),
            // `CachedTransaction::getTransactionFee` returns 0 the moment it
            // sees a `BaseInput`, which is what makes a coinbase fee 0.
            fee: transaction.fee().unwrap_or(0),
            mixin,
            payment_id,
            payment_id_encrypted,
            extra_public_key,
            extra_nonce,
            size,
            transaction,
        }))
    }

    fn block_template(
        &self,
        wallet_address: &str,
        reserve: &[u8],
    ) -> Result<std::result::Result<BlockTemplateAnswer, String>> {
        let chain = self.read();
        let mut pool = self.pool();
        // `RpcServer.cpp:1560` builds `blobReserve` and hands it to
        // `Core::getBlockTemplate` as the extra nonce, whatever its origin.
        match wrkz_mempool::build_template_cached(
            &*chain,
            &mut pool,
            &self.template_context_cache,
            wallet_address,
            0,
            Some(reserve),
            &self.template_options,
        ) {
            Ok(t) => Ok(Ok(BlockTemplateAnswer {
                blob: t.blob,
                difficulty: t.difficulty,
                height: t.height,
                tx_public_key: t.tx_public_key,
            })),
            Err(e) => Ok(Err(e.to_string())),
        }
    }

    fn submit_block(&self, blob: &[u8]) -> Result<SubmitOutcome> {
        // The one write path: the exclusive guard, held for exactly the
        // `addBlock` the C++ holds its unique lock for.
        let mut chain = self.write();
        let mut pool = self.pool();
        // The full `NOTIFY_NEW_BLOCK` a pre-lite peer needs carries the
        // transactions, and adding the block takes them out of the pool, so
        // they are copied first (`RawBlockLegacy(rawBlob, blockTemplate, core)`).
        let transactions: Vec<Vec<u8>> = BlockTemplate::from_bytes(blob)
            .map(|b| b.transaction_hashes.iter().filter_map(|h| pool.get(h).map(|e| e.blob.clone())).collect())
            .unwrap_or_default();
        let (status, update) = wrkz_mempool::submit_block_update(&mut pool, &mut chain, blob);
        drop(pool);
        if status.is_ok() {
            // Committed now rather than with the engine's next event: a store
            // that batches writes would otherwise hold a mined block back.
            chain.flush().map_err(chain_error)?;
            drop(chain);
            if let Some(update) = &update {
                let applied = AppliedBlock {
                    outcome: &update.outcome,
                    block_blob: blob,
                    lowest_unwound: update.lowest_unwound,
                    pool_removed: &update.removed,
                    pool_restored: &update.restored,
                };
                self.events.block_applied(&applied, |index| self.block_hash_by_index(u64::from(index)).ok().flatten());
            }
            if status.should_relay() {
                self.block_relay_queue
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(MinedBlock { block: blob.to_vec(), transactions });
                if let Some(hook) = &self.on_mined_block {
                    hook();
                }
            }
            Ok(SubmitOutcome::Added { relay: status.should_relay() })
        } else {
            Ok(SubmitOutcome::NotAccepted)
        }
    }
}

/// [`PoolRelay`] over a borrowed pool and chain, which is what
/// `wrkz_mempool::add_block_with_pool` and the P2P relay path want.
///
/// [`PoolRelay`]: wrkz_mempool::PoolRelay
pub fn pool_relay<'a, S: KvStore>(
    pool: &'a mut TransactionPool,
    chain: &'a ChainState<S>,
) -> PoolWithChain<'a, ChainState<S>> {
    PoolWithChain::new(pool, chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state imported from a lite snapshot has no transaction records below
    /// its line, so a transaction lookup that finds nothing cannot say "no
    /// such transaction": it refuses, in the lite wording. A lite node that
    /// synced its own region, and a full node, answer "not found" as before.
    #[test]
    fn a_snapshot_import_refuses_what_it_cannot_know_about_transactions() {
        use wrkz_chain::{keys, Checkpoints};
        use wrkz_storage::{KvStore, MemStore};
        let node = |tag: Option<&str>, lite: u32| {
            let mut store = MemStore::default();
            let version = keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec();
            let mut ops = vec![(keys::meta(keys::META_VERSION), Some(version))];
            if lite != 0 {
                ops.push((keys::meta(keys::META_LITE_HEIGHT), Some(lite.to_le_bytes().to_vec())));
            }
            if let Some(tag) = tag {
                ops.push((keys::meta(keys::META_TAG), Some(tag.as_bytes().to_vec())));
            }
            store.write_batch(ops).unwrap();
            let cfg = Config { lite_start_height: lite, ..serving_config() };
            let chain = ChainState::open_or_genesis(store, cfg, Checkpoints::none()).unwrap();
            ChainNode::standalone(chain, TransactionPool::new(Default::default()))
        };
        let unknown = [0x77; 32];
        let refusal = "This node is a lite node and stores no transaction data below height 1000";

        let imported = node(Some(keys::TAG_LITE_SNAPSHOT), 1000);
        assert_eq!(imported.body_policy().transactions_from, 1000);
        for e in [
            imported.transaction_global_indexes(&unknown).map(|_| ()),
            imported.transaction_blob(&unknown).map(|_| ()),
            imported.transaction_block_index(&unknown).map(|_| ()),
            imported.transaction_details(&unknown).map(|_| ()),
            imported.transaction_hashes_by_payment_id(&unknown).map(|_| ()),
        ] {
            assert!(matches!(&e, Err(ApiError::NotHeld(m)) if m == refusal), "{e:?}");
        }
        // Genesis is still a transaction it holds.
        let coinbase = wrkz_primitives::block::genesis_block().base_transaction.hash().unwrap();
        assert_eq!(imported.transaction_block_index(&coinbase).unwrap(), Some(0));

        for other in [node(None, 1000), node(None, 0)] {
            assert_eq!(other.body_policy().transactions_from, 0);
            assert_eq!(other.transaction_global_indexes(&unknown).unwrap(), None);
            assert_eq!(other.transaction_blob(&unknown).unwrap(), None);
            assert_eq!(other.transaction_block_index(&unknown).unwrap(), None);
            assert!(other.transaction_hashes_by_payment_id(&unknown).unwrap().is_empty());
        }
    }

    /// A closure over a list of block timestamps, index = block index.
    fn at(ts: &[u64]) -> impl FnMut(u64) -> Result<u64> + '_ {
        move |i| Ok(ts[i as usize])
    }

    const D: u64 = 20_000 * ONE_DAY;

    #[test]
    fn the_first_block_of_a_day_is_the_one_the_cpp_index_holds() {
        // Genesis at 0, then three days of blocks.
        let ts = [0, D + 10, D + 70, D + 130, D + ONE_DAY + 5, D + ONE_DAY + 60];
        let tip = ts.len() as u64 - 1;
        assert_eq!(first_block_of_day(tip, midnight(D + 100), at(&ts)).unwrap(), Some(1));
        assert_eq!(first_block_of_day(tip, midnight(D + ONE_DAY + 30), at(&ts)).unwrap(), Some(4));
        assert_eq!(first_block_of_day(tip, D + 2 * ONE_DAY, at(&ts)).unwrap(), None, "no block that day");
        assert_eq!(first_block_of_day(tip, 0, at(&ts)).unwrap(), Some(0), "genesis owns day 0");
    }

    #[test]
    fn an_inversion_below_the_crossing_is_found() {
        // Block 2 is already past midnight; blocks 3 and 4 are stamped before
        // it. A plain binary search can land on 5; the day starts at 2.
        let ts = [0, D - 100, D + 5, D - 10, D - 5, D + 20, D + 80];
        let tip = ts.len() as u64 - 1;
        assert_eq!(first_at_or_after(tip, D, at(&ts)).unwrap(), Some(2));
        assert_eq!(first_block_of_day(tip, D, at(&ts)).unwrap(), Some(2));
    }

    #[test]
    fn a_days_first_block_may_arrive_after_the_next_days() {
        // Block 2 is stamped the next day; block 3 is the first stamped day D,
        // and that is what the C++ index records for D.
        let ts = [0, D - 100, D + ONE_DAY + 5, D + 50, D + ONE_DAY + 60];
        let tip = ts.len() as u64 - 1;
        assert_eq!(first_block_of_day(tip, D, at(&ts)).unwrap(), Some(3));
        assert_eq!(first_block_of_day(tip, D + ONE_DAY, at(&ts)).unwrap(), Some(2));
    }

    #[test]
    fn the_lower_bound_falls_back_to_the_latest_day_with_a_block() {
        // Days D and D+2 have blocks; D+1 has none.
        let ts = [0, D + 5, D + 65, D + 2 * ONE_DAY + 5, D + 2 * ONE_DAY + 90];
        let tip = ts.len() as u64 - 1;
        assert_eq!(timestamp_lower_bound(tip, D + 200, at(&ts)).unwrap(), 1, "a day with blocks is its own answer");
        assert_eq!(timestamp_lower_bound(tip, D + ONE_DAY + 7, at(&ts)).unwrap(), 1, "an empty day falls back");
        assert_eq!(timestamp_lower_bound(tip, D + 9 * ONE_DAY, at(&ts)).unwrap(), 3, "past the tip: the tip's day");
        assert_eq!(timestamp_lower_bound(tip, D - 3 * ONE_DAY, at(&ts)).unwrap(), 0, "before every block but genesis");
        assert_eq!(timestamp_lower_bound(tip, 0, at(&ts)).unwrap(), 0);
        assert_eq!(timestamp_lower_bound(tip, 500, at(&ts)).unwrap(), 0, "day 0 is never searched");
    }

    #[test]
    fn the_shuffle_generator_yields_every_value_once() {
        let mut g = ShuffleGenerator::new(64, 12345);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let v = g.next().expect("not exhausted");
            assert!(v < 64);
            assert!(seen.insert(v), "values are distinct");
        }
        assert_eq!(g.next(), None, "the sequence ends after n draws");
        assert_eq!(seen.len(), 64);
        assert_eq!(ShuffleGenerator::new(0, 1).next(), None);
    }

    #[test]
    fn the_memory_estimate_matches_the_cpp_sizeofs() {
        // An empty block with no coinbase: `sizeof(optional) + sizeof(vector) +
        // 8 + 32 + 8`.
        let empty = SyncBlock {
            block_hash: [0; 32],
            block_height: 1,
            block_timestamp: 0,
            coinbase: None,
            transactions: Vec::new(),
        };
        assert_eq!(block_memory_usage(&empty), 104 + 24 + 48);
        // One coinbase with one output: 56 + 24 + 72 = 152.
        let with_coinbase = SyncBlock {
            coinbase: Some(SyncTransaction {
                hash: [0; 32],
                outputs: vec![SyncOutput { amount: 1, key: [0; 32], global_index: None }],
                tx_public_key: [0; 32],
                unlock_time: 0,
                payment_id: String::new(),
                inputs: Vec::new(),
            }),
            ..empty
        };
        assert_eq!(block_memory_usage(&with_coinbase), 152 + 24 + 48);
    }

    #[test]
    fn a_standalone_snapshot_reads_as_synced() {
        let p = P2pSnapshot::standalone();
        assert!(p.synchronized);
        assert_eq!(p.blockchain_height, 0, "zero means 'use our own height'");
        assert_eq!(p.prune_depth, 10080);
    }

    #[test]
    fn the_decoy_gamma_has_the_mean_it_was_fitted_with() {
        let mut rng = SplitMix::new(7);
        let n = 20_000;
        let mean = (0..n).map(|_| gamma(&mut rng, DECOY_GAMMA_SHAPE, DECOY_GAMMA_SCALE)).sum::<f64>() / n as f64;
        let expected = DECOY_GAMMA_SHAPE * DECOY_GAMMA_SCALE;
        assert!((mean - expected).abs() < 0.1, "mean {mean}, expected {expected}");
    }

    #[test]
    fn recent_decoys_are_recent_distinct_and_in_range() {
        // One output per block over 100,000 blocks.
        let block_of = |gi: u32| -> Result<u64> { Ok(u64::from(gi)) };
        let upper = 99_999u64;
        let mut rng = SplitMix::new(11);
        let mut ages = Vec::new();
        for _ in 0..500 {
            let picks = recent_picks(100_000, 8, upper, &block_of, &mut rng).unwrap();
            assert_eq!(picks.len(), 8);
            let mut distinct = picks.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(distinct.len(), 8, "no output twice in one answer");
            ages.extend(picks.iter().map(|&gi| upper - u64::from(gi)));
        }
        ages.sort_unstable();
        let median = ages[ages.len() / 2];
        // The gamma's median is about e^11.77 s, some 36 hours: about 2,150
        // one-minute blocks. A uniform pick over this chain would be ~50,000.
        assert!((1_000..5_000).contains(&median), "median age {median} blocks");
    }

    #[test]
    fn recent_decoys_stay_at_or_below_the_upper_block() {
        // An amount with one output every 100 blocks; nothing past 50,000 may
        // be offered.
        let sparse = |gi: u32| -> Result<u64> { Ok(u64::from(gi) * 100) };
        let mut rng = SplitMix::new(3);
        for _ in 0..200 {
            for gi in recent_picks(1_000, 4, 50_000, &sparse, &mut rng).unwrap() {
                assert!(u64::from(gi) * 100 <= 50_000, "output {gi} is past the upper block");
            }
        }
        // No output at or below the upper block: nothing to offer, and the
        // caller's uniform pass decides.
        let late = |gi: u32| -> Result<u64> { Ok(u64::from(gi) * 100 + 1_000) };
        assert!(recent_picks(1_000, 4, 50, &late, &mut rng).unwrap().is_empty());
    }

    #[test]
    fn the_uniform_pick_is_distinct_and_skips_what_is_already_taken() {
        let taken: std::collections::HashSet<u32> = [3, 4].into_iter().collect();
        let all = |_gi: u32| -> Result<bool> { Ok(true) };
        let picks = uniform_picks(10, 8, &taken, 5, &all).unwrap();
        let mut distinct = picks.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 8);
        assert!(picks.iter().all(|gi| *gi < 10 && !taken.contains(gi)));
        // More wanted than there are: everything left, and no error.
        assert_eq!(uniform_picks(10, 9, &taken, 5, &all).unwrap().len(), 8);
        // One output in twenty usable is still enough for a full ring.
        let sparse = |gi: u32| -> Result<bool> { Ok(gi.is_multiple_of(20)) };
        assert_eq!(uniform_picks(1_000_000, 8, &Default::default(), 9, &sparse).unwrap().len(), 8);
    }

    #[test]
    fn the_uniform_pick_gives_up_after_its_miss_budget() {
        // Ten million outputs, none usable: the C++ would read every one of
        // them for a single amount of a single request.
        let reads = std::cell::Cell::new(0u32);
        let none = |_gi: u32| -> Result<bool> {
            reads.set(reads.get() + 1);
            Ok(false)
        };
        assert!(uniform_picks(10_000_000, 8, &Default::default(), 1, &none).unwrap().is_empty());
        assert_eq!(reads.get(), UNIFORM_MISS_FLOOR + 8 * UNIFORM_MISSES_PER_WANTED + 1);
    }

    #[test]
    fn decoy_selection_names_round_trip() {
        for d in [DecoySelection::Uniform, DecoySelection::Recent] {
            assert_eq!(DecoySelection::parse(d.name()), Some(d));
        }
        assert_eq!(DecoySelection::default(), DecoySelection::Uniform, "the C++ behaviour unless asked");
        assert_eq!(DecoySelection::parse("gamma"), None);
    }
}
