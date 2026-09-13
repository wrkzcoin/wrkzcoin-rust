// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The transaction pool (`src/cryptonotecore/TransactionPool.cpp`), its
//! admission path (`Core::addTransactionToPool` / `Core::isTransactionValidForPool`,
//! `Core.cpp:2144-2248`), the post-block sweep
//! (`Core::checkAndRemoveInvalidPoolTransactions`, `Core.cpp:1833`), the
//! reorganisation refill (`Core::copyTransactionsToPool`, `Core.cpp:567`) and
//! the age limit (`TransactionPoolCleanWrapper::clean`,
//! `TransactionPoolCleaner.cpp:118`).
//!
//! None of this is consensus. It is nevertheless written rule for rule,
//! because the pool decides what a template contains and therefore what a
//! block pays.
//!
//! Four things are this port's own and have no C++ counterpart, all of them
//! about what an unwanted transaction costs rather than about which
//! transactions are admitted: the cheap refusals (a key image another pooled
//! transaction already spends, a full pool the offer could not stay in) run
//! **before** the validator rather than after it; the validator is the
//! early-exit one ([`wrkz_chain::validate::validate_transaction_early_exit`]);
//! a transaction proven invalid for a reason that cannot change is remembered
//! and refused on its hash when it is offered again ([`RejectionCategory`]);
//! and [`TransactionPool::remove_spent_in_chain`] closes the gap a chain switch
//! leaves open (`Core.cpp:1717` sweeps the pool against the switching block
//! only).

use crate::chain::PoolChain;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use wrkz_chain::validate::{
    revalidate_after_height_change, validate_transaction_early_exit, RevalidateContext, TxContext, TxError, TxRule,
    ValidatorState,
};
use wrkz_chain::{AddOutcome, AddStatus, ChainState};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::*;
use wrkz_primitives::mixins::validate_ring_sizes;
use wrkz_primitives::tx::{Input, PaymentId, Transaction};
use wrkz_primitives::Hash;
use wrkz_storage::KvStore;

/// Where a transaction reached the pool from. The C++ tracks this only through
/// which call path pushed it; it is explicit here so that the two live times of
/// `CryptoNoteConfig.h` can be told apart (see [`PoolConfig::alt_block_tx_livetime`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolSource {
    /// `NOTIFY_NEW_TRANSACTIONS` from a peer.
    Network,
    /// `/sendrawtransaction`.
    Rpc,
    /// `Core::copyTransactionsToPool` after a chain switch.
    AlternativeBlock,
}

/// The pool's budgets and time limits (`config/CryptoNoteConfig.h`).
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// `CRYPTONOTE_MEMPOOL_MAX_SIZE_BYTES` (64 MiB).
    pub max_size_bytes: u64,
    /// `CRYPTONOTE_MEMPOOL_EVICT_TO_PERCENT` (90).
    pub evict_to_percent: u64,
    /// `CRYPTONOTE_MEMPOOL_TX_LIVETIME` (24 h), the timeout the clean wrapper
    /// is constructed with (`Core.cpp:293`).
    pub tx_livetime: u64,
    /// The live time for transactions that came back from an unwound block.
    ///
    /// `CRYPTONOTE_MEMPOOL_TX_FROM_ALT_BLOCK_LIVETIME` (7 days) is loaded into
    /// `Currency` (`Currency.cpp:870`) and then **never read**: the deployed
    /// cleaner takes one timeout, `mempoolTxLiveTime()`. The default here is
    /// therefore the same 24 h as everything else, which is what the daemon
    /// does; raise it to reinstate what spec/06 describes.
    pub alt_block_tx_livetime: u64,
    /// `FUSION_TX_MAX_POOL_COUNT` (60).
    pub fusion_max_pool_count: usize,
    /// How many rejected transaction hashes the pool remembers
    /// ([`REJECTION_CACHE_CAPACITY`] by default). Not in the C++; `0` turns
    /// the cache off.
    pub rejection_cache_capacity: usize,
    /// How long, in seconds, a remembered rejection is honoured
    /// ([`REJECTION_CACHE_TTL`] by default).
    pub rejection_cache_ttl: u64,
}

/// The default [`PoolConfig::rejection_cache_capacity`]: 10,000 hashes, a
/// few hundred kilobytes.
pub const REJECTION_CACHE_CAPACITY: usize = 10_000;

/// The default [`PoolConfig::rejection_cache_ttl`]: one hour.
pub const REJECTION_CACHE_TTL: u64 = 60 * 60;

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: CRYPTONOTE_MEMPOOL_MAX_SIZE_BYTES as u64,
            evict_to_percent: CRYPTONOTE_MEMPOOL_EVICT_TO_PERCENT as u64,
            tx_livetime: CRYPTONOTE_MEMPOOL_TX_LIVETIME,
            alt_block_tx_livetime: CRYPTONOTE_MEMPOOL_TX_LIVETIME,
            fusion_max_pool_count: FUSION_TX_MAX_POOL_COUNT,
            rejection_cache_capacity: REJECTION_CACHE_CAPACITY,
            rejection_cache_ttl: REJECTION_CACHE_TTL,
        }
    }
}

/// One pooled transaction: the C++ `PendingTransactionInfo` plus the values
/// `CachedTransaction` memoises.
#[derive(Clone, Debug)]
pub struct PoolEntry {
    pub hash: Hash,
    pub transaction: Transaction,
    /// The bytes as they arrived. Every size rule measures these, and this is
    /// what a relay sends on.
    pub blob: Vec<u8>,
    /// `getTransactionFee()`: input sum − output sum, and 0 for a coinbase.
    pub fee: u64,
    /// `getTransactionAmount()`: the output sum.
    pub amount: u64,
    pub is_fusion: bool,
    /// `receiveTime`, a unix timestamp.
    pub receive_time: u64,
    pub source: PoolSource,
    /// The payment id of the wallet-side extra parser, when there is one.
    pub payment_id: Option<Hash>,
}

impl PoolEntry {
    /// `getTransactionBinaryArray().size()`.
    pub fn size(&self) -> usize {
        self.blob.len()
    }
}

/// The result of offering a transaction to the pool.
///
/// The variants are the answers the RPC and the P2P layers need, in the same
/// shape the C++ returns them: a `bool` plus a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolStatus {
    /// Accepted and held. Relay it.
    Added,
    /// `"Could not deserialize transaction"` (`Core.cpp:2151`).
    DeserializationFailed,
    /// `"Transaction already exists in pool"` (`Core.cpp:2179`).
    AlreadyInPool,
    /// The transaction is already in a block. The C++ has no such code on this
    /// path — it lets `INPUT_KEYIMAGE_ALREADY_SPENT` reject it — but the RPC
    /// and `Core::hasTransaction` distinguish the two, and so does this.
    AlreadyInBlockchain,
    /// `TransactionPoolCleanWrapper::pushTransaction` refuses a transaction the
    /// cleaner deleted within the live time (`TransactionPoolCleaner.cpp:37`).
    RecentlyDeleted,
    /// `"Pool already contains the maximum amount of fusion transactions"`
    /// (`Core.cpp:2222`).
    FusionPoolFull,
    /// The pool validator rejected it (`isTransactionValidForPool`).
    Rejected(TxRule),
    /// `hasIntersections(poolState, transactionState)`
    /// (`TransactionPool.cpp:169`): another pooled transaction already spends
    /// this key image.
    KeyImageInPool { key_image: Hash, holder: Hash },
    /// The pool is at its size budget and this is the least profitable thing
    /// offered to it (`TransactionPool.cpp:180`), or the eviction that followed
    /// the insert threw it straight back out (`Core.cpp:2205`).
    PoolFull,
    /// Not in the C++: this exact transaction was refused by the validator a
    /// short while ago, for a reason that cannot have changed since, and is
    /// refused again on its hash without being validated. Carries the rule of
    /// the original rejection, and [`PoolStatus::message`] is the same as for
    /// [`PoolStatus::Rejected`] with that rule.
    ///
    /// Only rejections whose [`RejectionCategory`] is `Invalid`,
    /// `InvalidSignature` or `InvalidAtHeight` are remembered; see there for
    /// how long each is honoured.
    CachedRejection(TxRule),
}

/// Why the pool refused a transaction, in the terms a peer-scoring policy
/// needs: whether an honest node could have sent it.
///
/// The categories also decide what the pool's rejection cache remembers. A
/// rule is only remembered when re-running the validator could not give a
/// different answer, so the cache never refuses a transaction the validator
/// would have accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectionCategory {
    /// The bytes do not deserialize. No node relays these.
    Malformed,
    /// A rule that is a function of the transaction's own bytes failed: no
    /// chain state and no later height makes it valid. An honest node never
    /// relays one. Remembered for [`PoolConfig::rejection_cache_ttl`].
    ///
    /// For rules that only exist from some height (the output amount cap, the
    /// signature count) the argument needs the chain not to fall back below
    /// that height, which the checkpoints guarantee: every such height is far
    /// below the last checkpoint.
    Invalid,
    /// `INPUT_INVALID_SIGNATURES`. Invalid against the ring members *this*
    /// chain holds at the global indexes the inputs name — which only a chain
    /// switch deeper than those outputs can change, so a peer on the other
    /// side of a fork could be honest. Remembered for
    /// [`PoolConfig::rejection_cache_ttl`] or until
    /// [`TransactionPool::remove_spent_in_chain`] (a chain switch), whichever
    /// comes first.
    InvalidSignature,
    /// A rule that depends on the height, the block median or the top
    /// timestamp — the fee ladder, the transaction proof of work, the mixin
    /// tier, the size limit, the unlock time, the output amount cap. It may
    /// pass at another height (a fork can change the fee or the mixin), so a
    /// peer at another height could be honest. Remembered only while the
    /// chain tip it was judged against is still the tip.
    InvalidAtHeight,
    /// It depends on what the chain holds: a key image already spent, a ring
    /// member this node does not have or that is still locked, the transaction
    /// already being in a block, or a state read that failed. A peer ahead of
    /// or beside this node could be honest. Never remembered.
    ChainState,
    /// The pool's own policy: already held, recently deleted, the fee-less
    /// cap, a key image another pooled transaction spends, or no room.
    /// Nothing wrong with the transaction. Never remembered.
    Policy,
}

impl RejectionCategory {
    /// The category of a rule the pool validator reported.
    ///
    /// The pool always validates with `isPoolTransaction = true`, so the
    /// extra-size, output-count and signature-count rules — which blocks only
    /// enforce from some height — apply to it unconditionally and are
    /// functions of the bytes.
    pub fn of_rule(rule: &TxRule) -> Self {
        match rule {
            TxRule::EmptyInputs
            | TxRule::InputUnknownType
            | TxRule::InputIdenticalKeyImages
            | TxRule::InputEmptyOutputUsage
            | TxRule::InputInvalidDomainKeyImages
            | TxRule::InputIdenticalOutputIndexes
            | TxRule::InputsAmountOverflow
            | TxRule::OutputZeroAmount
            | TxRule::OutputInvalidKey
            | TxRule::OutputsAmountOverflow
            | TxRule::WrongAmount
            | TxRule::ZeroInputSum
            | TxRule::ExtraTooLarge { .. }
            | TxRule::ExcessiveOutputs { .. }
            // `ring.len()` is the number of offsets once every member was
            // found (a missing one is `INPUT_INVALID_GLOBAL_INDEX` first), so
            // this compares two counts the bytes carry.
            | TxRule::InputInvalidSignaturesCount { .. } => RejectionCategory::Invalid,
            TxRule::InputInvalidSignatures { .. } => RejectionCategory::InvalidSignature,
            TxRule::SizeTooLarge { .. }
            | TxRule::OutputAmountTooLarge { .. }
            | TxRule::WrongFee { .. }
            | TxRule::UnlockTimeTooSmall { .. }
            | TxRule::InvalidMixin(_)
            | TxRule::PowInvalid { .. } => RejectionCategory::InvalidAtHeight,
            TxRule::InputKeyImageAlreadySpent { .. }
            | TxRule::InputInvalidGlobalIndex { .. }
            | TxRule::InputSpendLockedOut { .. } => RejectionCategory::ChainState,
        }
    }

    /// Whether the peer that sent it is at fault: it relayed bytes no honest
    /// node relays. `InvalidSignature` counts — the fork case that excuses it
    /// is a chain split deeper than the outputs the rings name.
    pub fn is_peer_fault(&self) -> bool {
        matches!(self, RejectionCategory::Malformed | RejectionCategory::Invalid | RejectionCategory::InvalidSignature)
    }
}

impl PoolStatus {
    /// Whether the transaction is now in the pool.
    pub fn accepted(&self) -> bool {
        matches!(self, PoolStatus::Added)
    }

    /// The validator's rule, for [`PoolStatus::Rejected`] and
    /// [`PoolStatus::CachedRejection`].
    pub fn rule(&self) -> Option<&TxRule> {
        match self {
            PoolStatus::Rejected(rule) | PoolStatus::CachedRejection(rule) => Some(rule),
            _ => None,
        }
    }

    /// Whether this was refused from the rejection cache, without validation.
    pub fn is_cached_rejection(&self) -> bool {
        matches!(self, PoolStatus::CachedRejection(_))
    }

    /// Why it was refused, or `None` when it was accepted.
    pub fn category(&self) -> Option<RejectionCategory> {
        Some(match self {
            PoolStatus::Added => return None,
            PoolStatus::DeserializationFailed => RejectionCategory::Malformed,
            PoolStatus::AlreadyInBlockchain => RejectionCategory::ChainState,
            PoolStatus::AlreadyInPool
            | PoolStatus::RecentlyDeleted
            | PoolStatus::FusionPoolFull
            | PoolStatus::KeyImageInPool { .. }
            | PoolStatus::PoolFull => RejectionCategory::Policy,
            PoolStatus::Rejected(rule) | PoolStatus::CachedRejection(rule) => RejectionCategory::of_rule(rule),
        })
    }

    /// The message the C++ returns to the RPC caller, for the variants that
    /// have one.
    pub fn message(&self) -> String {
        match self {
            PoolStatus::Added => String::new(),
            PoolStatus::DeserializationFailed => "Could not deserialize transaction".into(),
            PoolStatus::AlreadyInPool => "Transaction already exists in pool".into(),
            PoolStatus::AlreadyInBlockchain => "Transaction already exists in the blockchain".into(),
            PoolStatus::RecentlyDeleted => "Transaction was not accepted into the pool".into(),
            PoolStatus::FusionPoolFull => "Pool already contains the maximum amount of fusion transactions".into(),
            PoolStatus::Rejected(rule) | PoolStatus::CachedRejection(rule) => rule.to_string(),
            PoolStatus::KeyImageInPool { .. } | PoolStatus::PoolFull => {
                "Transaction was not accepted into the pool".into()
            }
        }
    }
}

impl std::fmt::Display for PoolStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolStatus::Added => write!(f, "added"),
            PoolStatus::KeyImageInPool { key_image, holder } => write!(
                f,
                "key image {} is already spent by pooled transaction {}",
                hex::encode(key_image),
                hex::encode(holder)
            ),
            other => write!(f, "{}", other.message()),
        }
    }
}

/// The keys `TransactionPriorityComparator` compares, in its order
/// (`TransactionPool.cpp:23`).
///
/// Exposed because the ordering is the whole of `getPoolTransactionsForBlockTemplate`
/// and a test that cannot see it can only assert on the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxPriority {
    pub fee: u64,
    pub size: usize,
    pub amount: u64,
    /// `inputs.size() / outputs.size()` — **integer** division in the C++
    /// (both are `size_t`), assigned to a `double` afterwards; `u64::MAX`
    /// stands in for `numeric_limits<double>::max()` when there are no outputs.
    pub in_out_ratio: u64,
    pub receive_time: u64,
}

impl TxPriority {
    fn of(entry: &PoolEntry) -> Self {
        let inputs = entry.transaction.prefix.inputs.len() as u64;
        let outputs = entry.transaction.prefix.outputs.len() as u64;
        Self {
            fee: entry.fee,
            size: entry.size(),
            amount: entry.amount,
            in_out_ratio: inputs.checked_div(outputs).unwrap_or(u64::MAX),
            receive_time: entry.receive_time,
        }
    }

    /// `TransactionPriorityComparator::operator()`: is `self` preferred over
    /// `other`?
    pub fn prefers(&self, other: &Self) -> bool {
        // Fee per byte, as the 128-bit cross product the C++ builds with
        // `mul128`: `left.fee * right.size` against `right.fee * left.size`.
        let lhs = self.fee as u128 * other.size as u128;
        let rhs = other.fee as u128 * self.size as u128;
        if lhs > rhs {
            return true;
        }
        if rhs > lhs {
            return false;
        }
        if self.amount != other.amount {
            return self.amount > other.amount;
        }
        if self.in_out_ratio != other.in_out_ratio {
            return self.in_out_ratio > other.in_out_ratio;
        }
        if self.size != other.size {
            return self.size < other.size;
        }
        // Older first; equal on everything means equivalent, not "less than".
        self.receive_time < other.receive_time
    }
}

/// One entry of the pool's priority index: its [`TxPriority`] and its
/// insertion sequence number.
///
/// Ordered exactly as `transactionsByPriority()` orders the pool: by
/// [`TxPriority::prefers`], most preferred first, and — where neither is
/// preferred — by insertion order, which is what `std::stable_sort` over the
/// `std::list` falls back on. `prefers` is a strict weak order (a
/// lexicographic comparison whose first key compares the rationals
/// `fee / size` exactly, by 128-bit cross multiplication), so this is a total
/// order and the sorted sequence it gives is the one the stable sort gives.
#[derive(Clone, Copy, Debug)]
struct PriorityKey {
    priority: TxPriority,
    seq: u64,
}

impl Ord for PriorityKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.priority.prefers(&other.priority) {
            std::cmp::Ordering::Less
        } else if other.priority.prefers(&self.priority) {
            std::cmp::Ordering::Greater
        } else {
            self.seq.cmp(&other.seq)
        }
    }
}

impl PartialOrd for PriorityKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PriorityKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for PriorityKey {}

/// The values [`TxContext`] is built from at admission, which are everything
/// an `InvalidAtHeight` rule reads besides the transaction itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextKey {
    /// `getTopBlockIndex()`.
    height: u64,
    /// `Core::blockMedianSize`.
    median: u64,
    /// `getLastTimestamps(1)[0]`.
    timestamp: u64,
}

impl ContextKey {
    fn of<C: PoolChain + ?Sized>(chain: &C) -> Self {
        Self { height: chain.top_index(), median: chain.block_median_size(), timestamp: chain.top_block_timestamp() }
    }
}

/// How long a remembered rejection stands, besides the time limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheScope {
    /// [`RejectionCategory::Invalid`]: nothing but the time limit.
    Always,
    /// [`RejectionCategory::InvalidSignature`]: until a chain switch.
    UntilChainSwitch,
    /// [`RejectionCategory::InvalidAtHeight`]: while the admission context is
    /// the one it was judged in.
    Context(ContextKey),
}

/// One remembered rejection.
#[derive(Clone, Debug)]
struct Remembered {
    rule: TxRule,
    scope: CacheScope,
    /// The length of the blob that was judged. The hash is taken over the
    /// transaction's re-serialization, while the size, fee and fusion rules
    /// measure the blob as it arrived; requiring the same length means a
    /// differently encoded copy of a transaction can never get a refusal
    /// remembered against the hash an honest encoding of it shares.
    blob_len: usize,
    /// The pool clock when it was rejected.
    at: u64,
    /// Which insertion this is, so that a stale position in the FIFO does not
    /// evict a newer entry for the same hash.
    seq: u64,
}

/// A bounded FIFO of transaction hashes the validator refused, each with the
/// rule and how long the refusal stands. Not in the C++, which re-validates a
/// transaction every time a peer offers it.
#[derive(Debug, Default)]
struct RejectionCache {
    entries: HashMap<Hash, Remembered>,
    order: VecDeque<(Hash, u64)>,
    next_seq: u64,
}

impl RejectionCache {
    /// The remembered rule for `hash`, if the refusal still stands. An entry
    /// past its time or judged in another context is dropped and missed; a
    /// blob of another length is missed and leaves the entry alone.
    fn lookup(
        &mut self,
        hash: &Hash,
        blob_len: usize,
        now: u64,
        ttl: u64,
        context: impl FnOnce() -> ContextKey,
    ) -> Option<TxRule> {
        let entry = self.entries.get(hash)?;
        if entry.blob_len != blob_len {
            return None;
        }
        let stands = now.saturating_sub(entry.at) < ttl
            && match entry.scope {
                CacheScope::Always | CacheScope::UntilChainSwitch => true,
                CacheScope::Context(key) => key == context(),
            };
        if stands {
            Some(entry.rule.clone())
        } else {
            self.entries.remove(hash);
            None
        }
    }

    fn insert(&mut self, hash: Hash, blob_len: usize, rule: TxRule, scope: CacheScope, now: u64, capacity: usize) {
        if capacity == 0 {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(hash, Remembered { rule, scope, blob_len, at: now, seq });
        self.order.push_back((hash, seq));
        // Oldest first. A queue position whose entry has since been replaced
        // or dropped is skipped; the queue is also held to twice the capacity
        // so that such stale positions cannot pile up.
        while self.entries.len() > capacity || self.order.len() > 2 * capacity {
            let Some((old, old_seq)) = self.order.pop_front() else { break };
            if self.entries.get(&old).is_some_and(|e| e.seq == old_seq) {
                self.entries.remove(&old);
            }
        }
    }

    /// Drop everything past its time.
    fn expire(&mut self, now: u64, ttl: u64) {
        self.entries.retain(|_, e| now.saturating_sub(e.at) < ttl);
        let entries = &self.entries;
        self.order.retain(|(hash, seq)| entries.get(hash).is_some_and(|e| e.seq == *seq));
    }

    /// Drop every refusal that read the ring members: the chain they were
    /// read from is no longer the main chain.
    fn forget_ring_dependent(&mut self) {
        self.entries.retain(|_, e| e.scope != CacheScope::UntilChainSwitch);
    }
}

/// `TransactionPool` plus the admission path `Core` wraps around it.
pub struct TransactionPool {
    cfg: PoolConfig,
    /// Insertion order, which is `std::list<PendingTransactionInfo>` order and
    /// therefore what `std::stable_sort` falls back on.
    entries: BTreeMap<u64, PoolEntry>,
    /// `entries` in `transactionsByPriority()` order, kept as they come and
    /// go, so that neither the priority order nor the least profitable entry
    /// ever needs a sort or a scan of the whole pool.
    priority: BTreeSet<PriorityKey>,
    by_hash: HashMap<Hash, u64>,
    by_payment_id: HashMap<Hash, Vec<Hash>>,
    /// `poolState.spentKeyImages`, with the owner so a collision can name it.
    key_images: HashMap<Hash, Hash>,
    size_bytes: u64,
    /// How many entries pay no fee: `getFusionTransactionCount()`, counted as
    /// they come and go.
    fee_less: usize,
    next_seq: u64,
    evicted: Vec<Hash>,
    /// `recentlyDeletedTransactions`: hash → the time it was deleted.
    recently_deleted: HashMap<Hash, u64>,
    /// Transactions the validator refused for a reason that cannot change.
    rejected: RejectionCache,
    clock: Option<u64>,
}

impl TransactionPool {
    pub fn new(cfg: PoolConfig) -> Self {
        Self {
            cfg,
            entries: BTreeMap::new(),
            priority: BTreeSet::new(),
            by_hash: HashMap::new(),
            by_payment_id: HashMap::new(),
            key_images: HashMap::new(),
            size_bytes: 0,
            fee_less: 0,
            next_seq: 0,
            evicted: Vec::new(),
            recently_deleted: HashMap::new(),
            rejected: RejectionCache::default(),
            clock: None,
        }
    }

    pub fn config(&self) -> &PoolConfig {
        &self.cfg
    }

    /// Fix what `time(nullptr)` returns, for deterministic tests. `None`
    /// restores the system clock.
    pub fn set_clock(&mut self, now: Option<u64>) {
        self.clock = now;
    }

    /// The pool's idea of the current time.
    pub fn now(&self) -> u64 {
        self.clock.unwrap_or_else(|| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
        })
    }

    // -- reads ---------------------------------------------------------------

    /// `getTransactionCount()`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `m_poolSizeBytes`.
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// `checkIfTransactionPresent(hash)`.
    pub fn contains(&self, hash: &Hash) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// `tryGetTransactionRef(hash)`.
    pub fn get(&self, hash: &Hash) -> Option<&PoolEntry> {
        self.by_hash.get(hash).and_then(|seq| self.entries.get(seq))
    }

    /// `getFusionTransactionCount()` — which counts **fee-less** transactions,
    /// not `isFusionTransaction` ones (`TransactionPool.cpp:382`).
    pub fn fusion_transaction_count(&self) -> usize {
        self.fee_less
    }

    /// `getTransactionHashesByPaymentId(paymentId)`.
    pub fn hashes_by_payment_id(&self, payment_id: &Hash) -> Vec<Hash> {
        self.by_payment_id.get(payment_id).cloned().unwrap_or_default()
    }

    /// `getPoolTransactionValidationState().spentKeyImages`.
    pub fn spent_key_images(&self) -> impl Iterator<Item = &Hash> {
        self.key_images.keys()
    }

    /// `transactionsByPriority()`: a stable sort of the pool in insertion order
    /// by [`TxPriority::prefers`], most preferred first.
    ///
    /// Read off the priority index rather than sorted: the index holds the
    /// same order (see `PriorityKey`), kept up to date on every insert and
    /// removal.
    pub fn by_priority(&self) -> Vec<&PoolEntry> {
        self.priority.iter().filter_map(|key| self.entries.get(&key.seq)).collect()
    }

    /// `getTransactionHashes()`, in priority order.
    pub fn hashes(&self) -> Vec<Hash> {
        self.by_priority().into_iter().map(|e| e.hash).collect()
    }

    /// `getPoolTransactionsForBlockTemplate()`: `(fee paying, fee-less)`, each
    /// in priority order. `fillBlockTemplate` takes the first list first.
    pub fn for_block_template(&self) -> (Vec<&PoolEntry>, Vec<&PoolEntry>) {
        let mut regular = Vec::new();
        let mut fusion = Vec::new();
        for entry in self.by_priority() {
            if entry.fee != 0 {
                regular.push(entry);
            } else {
                fusion.push(entry);
            }
        }
        (regular, fusion)
    }

    /// The blobs of the requested hashes, skipping the ones the pool does not
    /// hold. What `NOTIFY_REQUEST_TX_POOL` / a relay answer needs.
    pub fn transactions_for_relay(&self, hashes: &[Hash]) -> Vec<Vec<u8>> {
        hashes.iter().filter_map(|h| self.get(h).map(|e| e.blob.clone())).collect()
    }

    /// `takeEvictedTransactions()`: the hashes shed since the last call, which
    /// the caller is expected to announce.
    pub fn take_evicted(&mut self) -> Vec<Hash> {
        std::mem::take(&mut self.evicted)
    }

    // -- admission -----------------------------------------------------------

    /// `Core::addTransactionToPool(BinaryArray)` (`Core.cpp:2144`) followed by
    /// `TransactionPool::pushTransaction` (`TransactionPool.cpp:151`).
    ///
    /// The full pool path: deserialize, the "already have it" checks, the
    /// rejection cache, the fusion cap, the key-image intersection with the
    /// pool and the size budget, then `ValidateTransaction` with
    /// `isPoolTransaction = true` at the **top block index** (so every "from
    /// height H" rule fires as it would in block `H + 1`), then the insert and
    /// the eviction.
    ///
    /// The C++ validates first and runs the intersection and the budget check
    /// inside `pushTransaction` afterwards (`TransactionPool.cpp:169`, `:180`).
    /// Both are cheap lookups that do not depend on the validator's answer —
    /// only on the transaction's key images, fee, size and output sum, which
    /// it carries — and the pool does not change while the validator runs, so
    /// running them first refuses exactly the transactions the C++ refuses.
    /// What changes is the cost: a thousand validly signed spends of one output
    /// used to cost a thousand ring verifications to refuse, and now cost a
    /// hash lookup each. A transaction that is invalid *and* conflicts is now
    /// refused as the conflict rather than as the rule.
    ///
    /// Nothing about validity is ever skipped: the pool always pays for the
    /// ring resolution, the signature check and the transaction proof of
    /// work, even inside the checkpoint zone (`ValidateTransaction.cpp:578`
    /// has the `m_isPoolTransaction` term, `:657` does not).
    pub fn add<C: PoolChain + ?Sized>(&mut self, blob: &[u8], chain: &C, source: PoolSource) -> PoolStatus {
        let Ok(transaction) = Transaction::from_bytes(blob) else {
            return PoolStatus::DeserializationFailed;
        };
        let Ok(hash) = transaction.hash() else {
            return PoolStatus::DeserializationFailed;
        };
        self.add_parsed(transaction, blob.to_vec(), hash, chain, source)
    }

    /// [`TransactionPool::add`] for a transaction that is already parsed.
    pub fn add_parsed<C: PoolChain + ?Sized>(
        &mut self,
        transaction: Transaction,
        blob: Vec<u8>,
        hash: Hash,
        chain: &C,
        source: PoolSource,
    ) -> PoolStatus {
        // `Core.cpp:2179`
        if self.contains(&hash) {
            return PoolStatus::AlreadyInPool;
        }
        // Not in the C++: a transaction the validator refused a moment ago,
        // for a reason re-validation could not change, is refused on its hash.
        let now = self.now();
        let blob_len = blob.len();
        let ttl = self.cfg.rejection_cache_ttl;
        if let Some(rule) = self.rejected.lookup(&hash, blob_len, now, ttl, || ContextKey::of(chain)) {
            return PoolStatus::CachedRejection(rule);
        }
        // `Core::isTransactionInChain(hash)`, one lookup in the chain's
        // transaction index (`Core.cpp:1895`). The C++ does not run this check
        // on this path at all — it lets `INPUT_KEYIMAGE_ALREADY_SPENT` reject a
        // re-submitted transaction — but the RPC needs the two apart.
        if chain.transaction_in_chain(&hash).unwrap_or(false) {
            return PoolStatus::AlreadyInBlockchain;
        }
        // `TransactionPoolCleaner.cpp:37`. The C++ guard is
        // `it->second >= timeout`, comparing a unix timestamp against 86,400,
        // so it is true for every entry the map still holds; the map itself is
        // the rule.
        if self.recently_deleted.contains_key(&hash) {
            return PoolStatus::RecentlyDeleted;
        }

        let fee = transaction_fee(&transaction);
        // `Core.cpp:2220`: the cap counts **fee-less** transactions.
        if fee == 0 && self.fusion_transaction_count() >= self.cfg.fusion_max_pool_count {
            return PoolStatus::FusionPoolFull;
        }

        // `hasIntersections(poolState, transactionState)`
        // (`TransactionPool.cpp:169`), ahead of the validator. The validator
        // state of a transaction that passes holds exactly its key inputs'
        // images, so this is the same intersection, taken in input order.
        for input in &transaction.prefix.inputs {
            if let Input::Key { key_image, .. } = input {
                if let Some(holder) = self.key_images.get(key_image) {
                    return PoolStatus::KeyImageInPool { key_image: *key_image, holder: *holder };
                }
            }
        }

        // `TransactionPool.cpp:180`, ahead of the validator: at the budget and
        // worse than everything held, so taking it in would only evict it
        // again. The priority reads the fee, the size, the output sum, the
        // input/output counts and the receive time, none of which the
        // validator decides. `is_fusion` is filled in once it has run.
        let size = blob.len() as u64;
        let mut entry = PoolEntry {
            hash,
            payment_id: payment_id_of(&transaction),
            amount: transaction.prefix.sum_outputs().unwrap_or(0),
            fee,
            is_fusion: false,
            receive_time: now,
            source,
            transaction,
            blob,
        };
        if self.size_bytes + size > self.cfg.max_size_bytes && self.is_least_profitable(&entry) {
            return PoolStatus::PoolFull;
        }

        let mut state = ValidatorState::new();
        let context = ContextKey::of(chain);
        let ctx = TxContext {
            // `Core.cpp:2238`: `getTopBlockIndex()`.
            block_height: context.height,
            block_median_size: context.median,
            // `Core.cpp:2229`: `getLastTimestamps(1)[0]`.
            block_timestamp: context.timestamp,
            is_pool_transaction: true,
            checkpoints: chain.checkpoint_table(),
        };
        // One transaction a peer chose: checked input by input, stopping at the
        // first failure, rather than gathered and settled as a block's batch.
        // Same verdict and rule either way (`validate_transaction_early_exit`).
        let validation = match validate_transaction_early_exit(&entry.transaction, &entry.blob, &mut state, chain, &ctx)
        {
            Ok(v) => v,
            Err(TxError::Rule(rule)) => {
                self.remember_rejection(hash, blob_len, &rule, context, now);
                return PoolStatus::Rejected(rule);
            }
            // A state read failed. The C++ `checkIfSpent` logs and accepts;
            // refusing is the only honest answer a port can give, and it is
            // reported as the rule the C++ would have hit had the read said
            // "spent". Never remembered: the next read may succeed.
            Err(TxError::Chain(_)) => {
                return PoolStatus::Rejected(TxRule::InputKeyImageAlreadySpent { key_image: [0; 32] })
            }
        };
        entry.is_fusion = validation.is_fusion;

        self.insert(entry, &state.spent_key_images);
        let evicted = self.evict_to_fit();
        self.evicted.extend_from_slice(&evicted);
        // `Core.cpp:2205`: pushTransaction answers with whether the hash
        // survived its own eviction pass.
        if self.contains(&hash) {
            PoolStatus::Added
        } else {
            PoolStatus::PoolFull
        }
    }

    /// Remember a validator rejection, if its category says re-validation
    /// could not change it (see [`RejectionCategory`]).
    fn remember_rejection(&mut self, hash: Hash, blob_len: usize, rule: &TxRule, context: ContextKey, now: u64) {
        let scope = match RejectionCategory::of_rule(rule) {
            RejectionCategory::Invalid => CacheScope::Always,
            RejectionCategory::InvalidSignature => CacheScope::UntilChainSwitch,
            RejectionCategory::InvalidAtHeight => CacheScope::Context(context),
            RejectionCategory::ChainState | RejectionCategory::Policy | RejectionCategory::Malformed => return,
        };
        self.rejected.insert(hash, blob_len, rule.clone(), scope, now, self.cfg.rejection_cache_capacity);
    }

    /// `isLeastProfitableLocked` (`TransactionPool.cpp:215`): true when nothing
    /// in the pool is worse than the candidate. An empty pool is never "least
    /// profitable".
    ///
    /// The C++ walks the whole pool. Asking only the least preferred entry is
    /// the same question: if the candidate is preferred over any entry `p`, it
    /// is preferred over the last one too, since `p` is preferred over or
    /// equivalent to it and `prefers` is a strict weak order.
    fn is_least_profitable(&self, candidate: &PoolEntry) -> bool {
        match self.priority.last() {
            None => false,
            Some(worst) => !TxPriority::of(candidate).prefers(&worst.priority),
        }
    }

    fn insert(&mut self, entry: PoolEntry, key_images: &HashSet<Hash>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.priority.insert(PriorityKey { priority: TxPriority::of(&entry), seq });
        if entry.fee == 0 {
            self.fee_less += 1;
        }
        self.size_bytes += entry.size() as u64;
        self.by_hash.insert(entry.hash, seq);
        if let Some(pid) = entry.payment_id {
            self.by_payment_id.entry(pid).or_default().push(entry.hash);
        }
        for image in key_images {
            self.key_images.insert(*image, entry.hash);
        }
        self.entries.insert(seq, entry);
    }

    /// `evictToFitLocked` (`TransactionPool.cpp:237`): once over the cap, shed
    /// the least preferred until the pool is at `evict_to_percent` of it.
    fn evict_to_fit(&mut self) -> Vec<Hash> {
        let mut evicted = Vec::new();
        if self.size_bytes <= self.cfg.max_size_bytes {
            return evicted;
        }
        // `MAX / 100 * PERCENT`, in that order, as the C++ writes it.
        let target = self.cfg.max_size_bytes / 100 * self.cfg.evict_to_percent;
        // The C++ sorts the whole pool and walks it from the tail. The tail of
        // the priority index is that walk, one step at a time: removing the
        // last entry makes the next one the last.
        while self.size_bytes > target {
            let Some(worst) = self.priority.last() else { break };
            let Some(hash) = self.entries.get(&worst.seq).map(|e| e.hash) else { break };
            self.remove(&hash);
            evicted.push(hash);
        }
        evicted
    }

    /// `removeTransaction(hash)` (`TransactionPool.cpp:332`).
    pub fn remove(&mut self, hash: &Hash) -> bool {
        let Some(seq) = self.by_hash.remove(hash) else { return false };
        let Some(entry) = self.entries.remove(&seq) else { return false };
        self.priority.remove(&PriorityKey { priority: TxPriority::of(&entry), seq });
        if entry.fee == 0 {
            self.fee_less -= 1;
        }
        for input in &entry.transaction.prefix.inputs {
            if let Input::Key { key_image, .. } = input {
                if self.key_images.get(key_image) == Some(hash) {
                    self.key_images.remove(key_image);
                }
            }
        }
        if let Some(pid) = entry.payment_id {
            if let Some(list) = self.by_payment_id.get_mut(&pid) {
                list.retain(|h| h != hash);
                if list.is_empty() {
                    self.by_payment_id.remove(&pid);
                }
            }
        }
        let size = entry.size() as u64;
        self.size_bytes = self.size_bytes.saturating_sub(size);
        true
    }

    /// `flush()`.
    pub fn flush(&mut self) {
        for hash in self.hashes() {
            self.remove(&hash);
        }
    }

    // -- the cleaner ---------------------------------------------------------

    /// `TransactionPoolCleanWrapper::clean(height)`
    /// (`TransactionPoolCleaner.cpp:118`), run every 60 seconds
    /// (`OUTDATED_TRANSACTION_POLLING_INTERVAL`, `Core.cpp:261`).
    ///
    /// Two rules: an age at or past the live time, and a ring size no longer
    /// inside the tier at `height`. Deleted hashes are remembered for the same
    /// live time and refused if offered again.
    ///
    /// The C++ loop has a bug worth naming rather than copying: after removing
    /// an aged transaction it does **not** `continue`, and immediately calls
    /// `getTransaction(hash)` on the hash it just erased — an assert in a debug
    /// build and a dangling iterator in a release one. Only the intent is
    /// reproduced here.
    pub fn clean(&mut self, height: u64) -> Vec<Hash> {
        let now = self.now();
        let mut deleted = Vec::new();
        for hash in self.hashes() {
            let Some(entry) = self.get(&hash) else { continue };
            let timeout = match entry.source {
                PoolSource::AlternativeBlock => self.cfg.alt_block_tx_livetime,
                _ => self.cfg.tx_livetime,
            };
            let age = now.saturating_sub(entry.receive_time);
            let stale = age >= timeout;
            let bad_mixin = validate_ring_sizes(&entry.transaction.prefix.ring_sizes(), height).is_err();
            if stale || bad_mixin {
                self.recently_deleted.insert(hash, now);
                self.remove(&hash);
                deleted.push(hash);
            }
        }
        self.clean_recently_deleted(now);
        self.rejected.expire(now, self.cfg.rejection_cache_ttl);
        deleted
    }

    /// `cleanRecentlyDeletedTransactions(currentTime)`.
    fn clean_recently_deleted(&mut self, now: u64) {
        let timeout = self.cfg.tx_livetime;
        self.recently_deleted.retain(|_, at| now.saturating_sub(*at) < timeout);
    }

    /// Forget that a hash was deleted, so it may be offered again. The C++ has
    /// no such call; a test that wants the transaction back needs it.
    pub fn forget_deleted(&mut self, hash: &Hash) -> bool {
        self.recently_deleted.remove(hash).is_some()
    }

    // -- the rejection cache -------------------------------------------------

    /// How many rejections are remembered (some may be past their time until
    /// the next [`TransactionPool::clean`]).
    pub fn rejection_cache_len(&self) -> usize {
        self.rejected.entries.len()
    }

    /// Forget a remembered rejection, so the transaction is validated again
    /// the next time it is offered.
    pub fn forget_rejection(&mut self, hash: &Hash) -> bool {
        self.rejected.entries.remove(hash).is_some()
    }

    // -- reacting to the chain ----------------------------------------------

    /// `Core::checkAndRemoveInvalidPoolTransactions(blockTransactionsState)`
    /// (`Core.cpp:1833`), plus the bookkeeping that lets the pool answer
    /// "already in the blockchain".
    ///
    /// `block_index` is the index the block took, `block_transaction_hashes`
    /// its transactions (coinbase excluded — a coinbase is never pooled) and
    /// `spent_key_images` the key images the block spent, which is exactly the
    /// `TransactionValidatorState` the C++ hands in.
    ///
    /// `block_index` is not read: the block's own transaction hashes are given
    /// here, and everything older is [`PoolChain::transaction_in_chain`]'s
    /// answer. It stays in the signature because callers have it and a future
    /// rule may want it.
    pub fn on_block_added<C: PoolChain + ?Sized>(
        &mut self,
        chain: &C,
        block_index: u32,
        block_transaction_hashes: &[Hash],
        spent_key_images: &HashSet<Hash>,
    ) -> Vec<Hash> {
        let _ = block_index;

        let max_transaction_size = chain.block_median_size() * 2 - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64;
        let top_block_index = chain.top_index();
        let mut to_remove = Vec::new();
        for hash in self.hashes() {
            let Some(entry) = self.get(&hash) else { continue };
            // `isTransactionInChain(poolTxHash)`. The block being applied is
            // named directly, so a chain view without a transaction index
            // still sheds what this block mined.
            let in_chain =
                block_transaction_hashes.contains(&hash) || chain.transaction_in_chain(&hash).unwrap_or(false);
            let bad_mixin = validate_ring_sizes(&entry.transaction.prefix.ring_sizes(), top_block_index).is_err();
            let too_big = entry.size() as u64 > max_transaction_size;
            let double_spend = entry.transaction.prefix.inputs.iter().any(|i| match i {
                Input::Key { key_image, .. } => spent_key_images.contains(key_image),
                Input::Base { .. } => false,
            });
            if in_chain || bad_mixin || too_big || double_spend {
                to_remove.push(hash);
            }
        }
        for hash in &to_remove {
            self.remove(hash);
        }
        to_remove
    }

    /// Drop every pooled transaction that spends a key image the main chain
    /// has already spent, and return their hashes in priority order.
    ///
    /// # The node must call this after every chain switch
    ///
    /// That is, whenever a block comes back
    /// [`AddStatus::AlternativeAndSwitched`] — whether or not the unwound
    /// blocks carried transactions — once the chain state holds the new
    /// branch. [`add_block_with_pool`] does.
    ///
    /// [`TransactionPool::on_block_added`] sweeps the pool against the key
    /// images of **one** block, and after a switch that is only the block that
    /// triggered it (`Core.cpp:1717` hands the C++ sweep the same one state).
    /// A pooled transaction whose key image was spent by an *earlier* block of
    /// the new branch survives that sweep, and nothing else would catch it:
    /// `fillBlockTemplate`'s revalidation reads no chain state
    /// (`Core.cpp:4333`), so every template would carry it and every block
    /// mined from one would be rejected, until the 24-hour age limit. The C++
    /// has the same gap. This asks the chain about every key image in the pool
    /// — one point read each — which is affordable because chain switches are
    /// rare.
    ///
    /// A key image is spent if the chain holds it at or below the top index
    /// (`checkIfSpent(keyImage, topIndex)`, as admission asks). A read that
    /// fails keeps the transaction: the template builder asks again and skips
    /// anything it cannot confirm.
    ///
    /// It also drops every remembered `InvalidSignature` rejection, since the
    /// ring members those signatures were checked against may have changed
    /// with the branch.
    pub fn remove_spent_in_chain<C: PoolChain + ?Sized>(&mut self, chain: &C) -> Vec<Hash> {
        self.rejected.forget_ring_dependent();
        let top = chain.top_index();
        let spent: Vec<Hash> = self
            .by_priority()
            .into_iter()
            .filter(|entry| spends_on_chain(chain, &entry.transaction, top).unwrap_or(false))
            .map(|entry| entry.hash)
            .collect();
        for hash in &spent {
            self.remove(hash);
        }
        spent
    }

    /// `Core::copyTransactionsToPool(alt)` (`Core.cpp:567`): every transaction
    /// of the blocks that just left the main chain is offered back, and the
    /// ones the new chain has invalidated simply fail admission.
    ///
    /// This is only ever called on a chain switch, so it also drops the
    /// remembered `InvalidSignature` rejections, as
    /// [`TransactionPool::remove_spent_in_chain`] does.
    pub fn copy_transactions_to_pool<C: PoolChain + ?Sized>(&mut self, chain: &C, blobs: &[Vec<u8>]) -> Vec<Hash> {
        self.rejected.forget_ring_dependent();
        let mut restored = Vec::new();
        for blob in blobs {
            let Ok(transaction) = Transaction::from_bytes(blob) else { continue };
            let Ok(hash) = transaction.hash() else { continue };
            // The unwind has already taken these out of the chain's transaction
            // index, so "already in the blockchain" is false for them unless the
            // new chain mined them too — in which case refusing is right. Only
            // the cleaner's memory has to be cleared by hand.
            self.forget_deleted(&hash);
            if self.add_parsed(transaction, blob.clone(), hash, chain, PoolSource::AlternativeBlock).accepted() {
                restored.push(hash);
            }
        }
        restored
    }
}

/// `getTransactionFee()` (`CachedTransaction.cpp:65`): input sum − output sum,
/// and 0 the moment a `BaseInput` appears.
fn transaction_fee(tx: &Transaction) -> u64 {
    let outputs: u64 = tx.prefix.outputs.iter().fold(0u64, |a, o| a.wrapping_add(o.amount));
    let mut inputs: u64 = 0;
    for input in &tx.prefix.inputs {
        match input {
            Input::Key { amount, .. } => inputs = inputs.wrapping_add(*amount),
            Input::Base { .. } => return 0,
        }
    }
    inputs.wrapping_sub(outputs)
}

/// Whether any key input of `tx` spends a key image the chain holds as spent
/// at or below `block_index` (`checkIfSpent`).
pub(crate) fn spends_on_chain<C: PoolChain + ?Sized>(
    chain: &C,
    tx: &Transaction,
    block_index: u64,
) -> wrkz_chain::Result<bool> {
    for input in &tx.prefix.inputs {
        if let Input::Key { key_image, .. } = input {
            if chain.key_image_spent(key_image, block_index)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `getPaymentIdFromTxExtra` (`TransactionPool.cpp:156`).
fn payment_id_of(tx: &Transaction) -> Option<Hash> {
    match wrkz_primitives::tx::parse_extra_wallet(&tx.prefix.extra).payment_id {
        Some(PaymentId::Long(id)) => Some(id),
        _ => None,
    }
}

/// What [`add_block_with_pool`] reports.
#[derive(Clone, Debug)]
pub struct ChainUpdate {
    /// What the chain state did with the block.
    pub outcome: AddOutcome,
    /// Pool transactions dropped by the post-block sweep.
    pub removed: Vec<Hash>,
    /// Transactions returned to the pool from blocks that left the main chain.
    pub restored: Vec<Hash>,
    /// For a chain switch, the index of the lowest block that left the main
    /// chain; the block below it is the common root.
    pub lowest_unwound: Option<u32>,
}

/// The pool half of `Core::addBlock` (`Core.cpp:1652-1737`): add the block,
/// then sweep the pool with the block's key images, and on a chain switch
/// sweep it against the whole new chain
/// ([`TransactionPool::remove_spent_in_chain`], not in the C++) and put the
/// unwound blocks' transactions back.
///
/// The blocks that left the main chain come from
/// [`wrkz_chain::ChainState::add_block_detailed`], which is the only thing that
/// knows them: the C++ still holds the old leaf at this point
/// (`Core.cpp:1712`) and walks it directly.
pub fn add_block_with_pool<S: KvStore>(
    pool: &mut TransactionPool,
    chain: &mut ChainState<S>,
    block_blob: &[u8],
    tx_blobs: &[Vec<u8>],
) -> wrkz_chain::Result<ChainUpdate> {
    let block = BlockTemplate::from_bytes(block_blob)
        .map_err(|_| wrkz_chain::ChainError::Rule(wrkz_chain::Rule::DeserializationFailed("block blob")))?;

    let report = chain.add_block_detailed(block_blob, tx_blobs)?;
    let outcome = report.outcome;
    let lowest_unwound = report.unwound.last().map(|left| left.index);

    let mut removed = Vec::new();
    let mut restored = Vec::new();
    if matches!(outcome.status, AddStatus::Main | AddStatus::AlternativeAndSwitched) {
        if outcome.status == AddStatus::AlternativeAndSwitched {
            // Every block of the new branch, not just this one, may spend what
            // the pool holds.
            removed.extend(pool.remove_spent_in_chain(&*chain));
            // Exactly what `copyTransactionsToPool` walks the old leaf for.
            let mut unwound = Vec::new();
            for left in &report.unwound {
                unwound.extend(left.transactions.iter().cloned());
            }
            restored = pool.copy_transactions_to_pool(chain, &unwound);
        }
        let mut spent = HashSet::new();
        for blob in tx_blobs {
            if let Ok(tx) = Transaction::from_bytes(blob) {
                for input in &tx.prefix.inputs {
                    if let Input::Key { key_image, .. } = input {
                        spent.insert(*key_image);
                    }
                }
            }
        }
        removed.extend(pool.on_block_added(chain, outcome.index, &block.transaction_hashes, &spent));
    }
    Ok(ChainUpdate { outcome, removed, restored, lowest_unwound })
}

/// What the P2P layer needs from a pool, with the chain it validates against
/// already bound in. `wrkz-node` builds one of these per call from the pool and
/// the chain it owns.
pub trait PoolRelay {
    /// `Core::addTransactionToPool` from `NOTIFY_NEW_TRANSACTIONS`.
    fn add_from_network(&mut self, blob: &[u8]) -> PoolStatus;
    /// The blobs to send for the hashes a peer asked for.
    fn transactions_for_relay(&self, hashes: &[Hash]) -> Vec<Vec<u8>>;
    /// `checkIfTransactionPresent`.
    fn has_transaction(&self, hash: &Hash) -> bool;
    /// `getPoolTransactionHashes()`, in priority order.
    fn pool_transaction_hashes(&self) -> Vec<Hash>;
}

/// A pool bound to the chain it validates against.
pub struct PoolWithChain<'a, C: PoolChain + ?Sized> {
    pub pool: &'a mut TransactionPool,
    pub chain: &'a C,
}

impl<'a, C: PoolChain + ?Sized> PoolWithChain<'a, C> {
    pub fn new(pool: &'a mut TransactionPool, chain: &'a C) -> Self {
        Self { pool, chain }
    }
}

impl<C: PoolChain + ?Sized> PoolRelay for PoolWithChain<'_, C> {
    fn add_from_network(&mut self, blob: &[u8]) -> PoolStatus {
        self.pool.add(blob, self.chain, PoolSource::Network)
    }

    fn transactions_for_relay(&self, hashes: &[Hash]) -> Vec<Vec<u8>> {
        self.pool.transactions_for_relay(hashes)
    }

    fn has_transaction(&self, hash: &Hash) -> bool {
        self.pool.contains(hash)
    }

    fn pool_transaction_hashes(&self) -> Vec<Hash> {
        self.pool.hashes()
    }
}

/// The context `fillBlockTemplate` revalidates candidates in, built from the
/// chain and the template's height.
pub(crate) fn template_revalidate_context<C: PoolChain + ?Sized>(chain: &C, height: u64) -> RevalidateContext {
    RevalidateContext {
        block_height: height,
        block_median_size: chain.block_median_size(),
        block_timestamp: chain.top_block_timestamp(),
    }
}

/// `Core::validateBlockTemplateTransaction` (`Core.cpp:4330`).
pub(crate) fn valid_for_block_template(entry: &PoolEntry, ctx: &RevalidateContext) -> bool {
    revalidate_after_height_change(&entry.transaction, &entry.blob, ctx).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priority(fee: u64, size: usize, amount: u64, inputs: u64, outputs: u64, t: u64) -> TxPriority {
        TxPriority { fee, size, amount, in_out_ratio: inputs.checked_div(outputs).unwrap_or(u64::MAX), receive_time: t }
    }

    #[test]
    fn fee_per_byte_decides_first() {
        let cheap = priority(10, 1000, 0, 1, 1, 0);
        let rich = priority(10, 100, 0, 1, 1, 0);
        assert!(rich.prefers(&cheap));
        assert!(!cheap.prefers(&rich));
        // Equal fee per byte falls through to the amount.
        let a = priority(20, 200, 5, 1, 1, 0);
        let b = priority(10, 100, 4, 1, 1, 0);
        assert!(a.prefers(&b));
        assert!(!b.prefers(&a));
    }

    #[test]
    fn ties_walk_the_whole_ladder() {
        // Same fee per byte and amount: the higher input/output ratio wins.
        let many_in = priority(10, 100, 5, 8, 2, 0);
        let few_in = priority(10, 100, 5, 2, 2, 0);
        assert!(many_in.prefers(&few_in));
        // Then the smaller transaction.
        let small = priority(10, 100, 5, 2, 2, 0);
        let big = priority(20, 200, 5, 4, 4, 0);
        assert!(small.prefers(&big));
        // Then the older one.
        let old = priority(10, 100, 5, 2, 2, 1);
        let new = priority(10, 100, 5, 2, 2, 2);
        assert!(old.prefers(&new));
        // Equal on everything is equivalent, not "less than" both ways.
        let x = priority(10, 100, 5, 2, 2, 7);
        assert!(!x.prefers(&x));
    }

    /// A pool entry with the priority fields given and nothing else real: a
    /// blob of `size` zero bytes, `inputs` key inputs with key images unique
    /// to `tag`, and `outputs` outputs.
    fn synthetic_entry(tag: u64, fee: u64, size: usize, amount: u64, inputs: u8, outputs: u8, t: u64) -> PoolEntry {
        let image = |i: u8| {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&tag.to_le_bytes());
            h[8] = i;
            h
        };
        let mut tx = Transaction::default();
        tx.prefix.inputs = (0..inputs).map(|i| Input::Key { amount: 1, key_offsets: vec![1], key_image: image(i) }).collect();
        tx.prefix.outputs = (0..outputs).map(|_| wrkz_primitives::tx::Output { amount: 1, key: [0; 32] }).collect();
        let mut hash = image(0xff);
        hash[31] = 0xAA;
        PoolEntry {
            hash,
            transaction: tx,
            blob: vec![0; size],
            fee,
            amount,
            is_fusion: false,
            receive_time: t,
            source: PoolSource::Rpc,
            payment_id: None,
        }
    }

    fn key_images_of(entry: &PoolEntry) -> HashSet<Hash> {
        entry
            .transaction
            .prefix
            .inputs
            .iter()
            .filter_map(|i| match i {
                Input::Key { key_image, .. } => Some(*key_image),
                Input::Base { .. } => None,
            })
            .collect()
    }

    /// What the pool did before the priority index: a stable sort of the
    /// entries in insertion order.
    fn naive_order(pool: &TransactionPool) -> Vec<Hash> {
        let mut ordered: Vec<&PoolEntry> = pool.entries.values().collect();
        ordered.sort_by(|a, b| {
            let (pa, pb) = (TxPriority::of(a), TxPriority::of(b));
            if pa.prefers(&pb) {
                std::cmp::Ordering::Less
            } else if pb.prefers(&pa) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
        ordered.into_iter().map(|e| e.hash).collect()
    }

    /// A small deterministic generator, so that the entries are full of ties
    /// on every rung of the comparator's ladder.
    struct Lcg(u64);
    impl Lcg {
        fn pick(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
        fn entry(&mut self, tag: u64) -> PoolEntry {
            let fee = [0u64, 10, 20, 30, 100][self.pick(5) as usize];
            let size = [100usize, 200, 300][self.pick(3) as usize];
            let amount = 1 + self.pick(2);
            let inputs = 1 + self.pick(3) as u8;
            let outputs = 1 + self.pick(2) as u8;
            let t = self.pick(2);
            synthetic_entry(tag, fee, size, amount, inputs, outputs, t)
        }
    }

    /// The priority index gives exactly what the full stable sort gave, the
    /// least-profitable test and the eviction walk pick exactly what the scans
    /// over the whole pool picked, and the fee-less counter matches a count.
    #[test]
    fn the_priority_index_matches_the_full_sort_it_replaced() {
        let mut rng = Lcg(7);
        let mut pool = TransactionPool::new(PoolConfig { max_size_bytes: u64::MAX, ..PoolConfig::default() });
        let check = |pool: &TransactionPool| {
            assert_eq!(pool.hashes(), naive_order(pool), "priority order");
            assert_eq!(pool.fusion_transaction_count(), pool.entries.values().filter(|e| e.fee == 0).count());
        };

        for tag in 0..400u64 {
            let entry = rng.entry(tag);
            let images = key_images_of(&entry);
            pool.insert(entry, &images);
            if tag % 25 == 0 {
                check(&pool);
            }
        }
        check(&pool);

        // `isLeastProfitableLocked` as the C++ walks it, against the index.
        for tag in 1_000..1_200u64 {
            let candidate = rng.entry(tag);
            let c = TxPriority::of(&candidate);
            let naive = !pool.entries.values().any(|pooled| c.prefers(&TxPriority::of(pooled)));
            assert_eq!(pool.is_least_profitable(&candidate), naive, "candidate {tag}");
        }

        // Removal keeps the index in step.
        let every_seventh: Vec<Hash> = pool.hashes().into_iter().step_by(7).collect();
        for hash in &every_seventh {
            assert!(pool.remove(hash));
        }
        check(&pool);

        // `evictToFitLocked` as the C++ walks it: the reversed sort, until the
        // pool is at the target.
        pool.cfg.max_size_bytes = pool.size_bytes() / 2;
        let target = pool.cfg.max_size_bytes / 100 * pool.cfg.evict_to_percent;
        let mut size = pool.size_bytes();
        let mut expected = Vec::new();
        for hash in naive_order(&pool).into_iter().rev() {
            if size <= target {
                break;
            }
            size -= pool.get(&hash).expect("held").size() as u64;
            expected.push(hash);
        }
        assert!(!expected.is_empty());
        assert_eq!(pool.evict_to_fit(), expected, "the same victims, in the same order");
        assert_eq!(pool.size_bytes(), size);
        check(&pool);
    }

    #[test]
    fn the_rejection_cache_is_bounded_and_honours_its_scopes() {
        let mut cache = RejectionCache::default();
        let here = ContextKey { height: 10, median: 100_000, timestamp: 1 };
        let there = ContextKey { height: 11, ..here };
        let h = |n: u8| [n; 32];
        const LEN: usize = 300;

        cache.insert(h(1), LEN, TxRule::EmptyInputs, CacheScope::Always, 0, 3);
        cache.insert(h(2), LEN, TxRule::InputInvalidSignatures { input: 0 }, CacheScope::UntilChainSwitch, 0, 3);
        cache.insert(h(3), LEN, TxRule::PowInvalid { difficulty: 1 }, CacheScope::Context(here), 0, 3);
        assert_eq!(cache.lookup(&h(1), LEN, 10, 100, || here), Some(TxRule::EmptyInputs));
        assert_eq!(cache.lookup(&h(3), LEN, 10, 100, || here), Some(TxRule::PowInvalid { difficulty: 1 }));
        // A blob of another length misses, and leaves the entry alone.
        assert_eq!(cache.lookup(&h(3), LEN + 1, 10, 100, || here), None);
        assert!(cache.lookup(&h(3), LEN, 10, 100, || here).is_some());
        // Another context misses, and drops the entry.
        assert_eq!(cache.lookup(&h(3), LEN, 10, 100, || there), None);
        assert_eq!(cache.lookup(&h(3), LEN, 10, 100, || here), None);

        // A chain switch drops only the ring-dependent entry.
        cache.forget_ring_dependent();
        assert_eq!(cache.lookup(&h(2), LEN, 10, 100, || here), None);
        assert_eq!(cache.lookup(&h(1), LEN, 10, 100, || here), Some(TxRule::EmptyInputs));

        // Past its time it misses.
        assert_eq!(cache.lookup(&h(1), LEN, 100, 100, || here), None);

        // The capacity is a hard bound, oldest out first, and re-inserting a
        // hash does not let its stale queue position evict the new entry.
        let mut cache = RejectionCache::default();
        for n in 0..5u8 {
            cache.insert(h(n), LEN, TxRule::EmptyInputs, CacheScope::Always, 0, 3);
        }
        assert_eq!(cache.entries.len(), 3);
        assert_eq!(cache.lookup(&h(0), LEN, 0, 100, || here), None);
        assert_eq!(cache.lookup(&h(1), LEN, 0, 100, || here), None);
        assert!(cache.lookup(&h(2), LEN, 0, 100, || here).is_some());
        cache.insert(h(2), LEN, TxRule::WrongAmount, CacheScope::Always, 0, 3);
        cache.insert(h(5), LEN, TxRule::EmptyInputs, CacheScope::Always, 0, 3);
        assert_eq!(cache.lookup(&h(2), LEN, 0, 100, || here), Some(TxRule::WrongAmount), "the newer entry survives");
        assert!(cache.order.len() <= 6);
        for n in 10..100u8 {
            cache.insert(h(n), LEN, TxRule::EmptyInputs, CacheScope::Always, 0, 3);
            assert!(cache.entries.len() <= 3 && cache.order.len() <= 6);
        }
        cache.expire(1_000, 100);
        assert!(cache.entries.is_empty() && cache.order.is_empty());

        // Capacity 0 is off.
        let mut off = RejectionCache::default();
        off.insert(h(1), LEN, TxRule::EmptyInputs, CacheScope::Always, 0, 0);
        assert!(off.entries.is_empty());
    }

    #[test]
    fn every_rule_has_a_category_and_only_invalid_ones_blame_the_peer() {
        use RejectionCategory::*;
        assert_eq!(RejectionCategory::of_rule(&TxRule::InputInvalidDomainKeyImages), Invalid);
        assert_eq!(RejectionCategory::of_rule(&TxRule::InputInvalidSignatures { input: 3 }), InvalidSignature);
        assert_eq!(RejectionCategory::of_rule(&TxRule::WrongFee { fee: 1, minimum: 10 }), InvalidAtHeight);
        assert_eq!(RejectionCategory::of_rule(&TxRule::PowInvalid { difficulty: 1 }), InvalidAtHeight);
        assert_eq!(RejectionCategory::of_rule(&TxRule::InputKeyImageAlreadySpent { key_image: [0; 32] }), ChainState);
        assert_eq!(
            RejectionCategory::of_rule(&TxRule::InputInvalidGlobalIndex { amount: 1, global_index: 1 }),
            ChainState
        );
        assert!(Malformed.is_peer_fault() && Invalid.is_peer_fault() && InvalidSignature.is_peer_fault());
        assert!(!InvalidAtHeight.is_peer_fault() && !ChainState.is_peer_fault() && !Policy.is_peer_fault());

        let cached = PoolStatus::CachedRejection(TxRule::EmptyInputs);
        assert!(cached.is_cached_rejection() && !cached.accepted());
        assert_eq!(cached.message(), PoolStatus::Rejected(TxRule::EmptyInputs).message(), "the RPC sees no change");
        assert_eq!(cached.rule(), Some(&TxRule::EmptyInputs));
        assert_eq!(cached.category(), Some(Invalid));
        assert_eq!(PoolStatus::Added.category(), None);
        assert_eq!(PoolStatus::DeserializationFailed.category(), Some(Malformed));
        assert_eq!(PoolStatus::PoolFull.category(), Some(Policy));
        assert_eq!(PoolStatus::AlreadyInBlockchain.category(), Some(ChainState));
    }

    #[test]
    fn the_ratio_is_integer_division_like_the_cpp() {
        // 3/2 and 2/2 both truncate to 1, so the ratio cannot separate them and
        // the size decides.
        let three_two = priority(10, 100, 5, 3, 2, 0);
        let two_two = priority(10, 100, 5, 2, 2, 0);
        assert!(!three_two.prefers(&two_two));
        assert!(!two_two.prefers(&three_two));
    }
}
