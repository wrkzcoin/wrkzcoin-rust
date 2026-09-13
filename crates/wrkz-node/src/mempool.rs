// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! [`SharedMempool`]: the real `wrkz-mempool` pool behind the [`TxPool`] hook
//! the P2P engine talks to, shared with the RPC server.
//!
//! Stage 3.3 shipped [`crate::pool::BoundedTxSet`], a relay-only stand-in with
//! stateless checks. This replaces it with `wrkz_mempool::TransactionPool`,
//! which runs the admission path of `Core::addTransactionToPool` against the
//! chain — and, crucially, is the *same* pool the RPC server serves
//! `/sendrawtransaction`, `/get_transactions_status`, `getblocktemplate` and
//! `f_on_transactions_pool_json` from. A daemon with two pools would relay
//! transactions it would not mine and mine transactions it never relayed.
//!
//! # Locking
//!
//! Two locks, always taken in this order: the chain (read) and then the pool.
//! Every path in this file and in `wrkz-rpc`'s `ChainNode` follows it, so the
//! two cannot deadlock against each other. The chain guard is a *read* guard
//! everywhere here: admission validates against the chain and writes only to
//! the pool.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use wrkz_chain::{ChainState, TxRule};
use wrkz_mempool::{PoolSource, PoolStatus, TransactionPool};
use wrkz_primitives::tx::{Input, Transaction};
use wrkz_primitives::Hash;
use wrkz_rpc::events::{Events, PoolRemoval};
use wrkz_storage::KvStore;

use crate::node::SharedChain;
use crate::pool::{TxPool, TxVerdict};

/// Whether a pool refusal is the relaying peer's fault ([`TxVerdict`]).
///
/// Only rules a relay checks from the transaction itself before passing it on:
/// it parses, its inputs and outputs are well-formed, its signatures verify,
/// its proof of work is sufficient. Everything that depends on the height or
/// on which chain and pool the relay holds — a key image already spent, a
/// global index, the fee and mixin ladders, unlock times, the pool being full
/// — can differ honestly between two nodes and is not held against anyone.
/// The ring members a signature is checked against are resolved from our
/// chain, so a peer on the losing side of a reorganisation could relay a
/// transaction whose signatures fail here; that is rare, and one
/// [`crate::peers::Offence::InvalidTransaction`] is a fifth of a ban.
fn verdict_of(status: &PoolStatus) -> TxVerdict {
    match status {
        PoolStatus::Added => TxVerdict::Accepted,
        PoolStatus::DeserializationFailed => TxVerdict::Invalid(status.message()),
        PoolStatus::Rejected(rule) if tx_rule_is_intrinsic(rule) => TxVerdict::Invalid(rule.to_string()),
        // The same refusal, answered from the rejection cache: relaying a
        // transaction already proven invalid is no more honest the second time.
        PoolStatus::CachedRejection(rule) if tx_rule_is_intrinsic(rule) => TxVerdict::Invalid(rule.to_string()),
        _ => TxVerdict::NotAccepted,
    }
}

/// The transaction rules checked from the transaction alone (see
/// [`verdict_of`]). Shared with the engine's judgement of an invalid block.
pub(crate) fn tx_rule_is_intrinsic(rule: &TxRule) -> bool {
    matches!(
        rule,
        TxRule::EmptyInputs
            | TxRule::InputUnknownType
            | TxRule::InputIdenticalKeyImages
            | TxRule::InputEmptyOutputUsage
            | TxRule::InputInvalidDomainKeyImages
            | TxRule::InputIdenticalOutputIndexes
            | TxRule::InputsAmountOverflow
            | TxRule::OutputInvalidKey
            | TxRule::OutputsAmountOverflow
            | TxRule::PowInvalid { .. }
            | TxRule::InputInvalidSignaturesCount { .. }
            | TxRule::InputInvalidSignatures { .. }
    )
}

/// The pool, shared between the P2P engine and the RPC server.
pub type SharedPool = Arc<Mutex<TransactionPool>>;

/// The [`TxPool`] the engine sees, over a shared [`TransactionPool`].
///
/// Cloning gives another handle to the same pool and the same chain, which is
/// how the daemon hands one to the engine and keeps one for the RPC.
pub struct SharedMempool<S: KvStore> {
    pool: SharedPool,
    chain: SharedChain<S>,
    /// Where a transaction offered to [`TxPool::add_transaction`] came from.
    /// The engine only ever calls it for `NOTIFY_NEW_TRANSACTIONS`.
    source: PoolSource,
    /// Where what enters and leaves the pool through the engine is published.
    events: Events,
}

impl<S: KvStore> Clone for SharedMempool<S> {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            chain: Arc::clone(&self.chain),
            source: self.source,
            events: self.events.clone(),
        }
    }
}

impl<S: KvStore> SharedMempool<S> {
    pub fn new(pool: SharedPool, chain: SharedChain<S>) -> Self {
        Self { pool, chain, source: PoolSource::Network, events: Events::default() }
    }

    /// Publish the transactions the engine's traffic adds to and removes from
    /// the pool to `events`' listeners ([`wrkz_rpc::events`]).
    pub fn with_events(mut self, events: Events) -> Self {
        self.events = events;
        self
    }

    pub fn handle(&self) -> SharedPool {
        Arc::clone(&self.pool)
    }

    /// `txpool_add` for a transaction the pool just admitted.
    fn publish_added(&self, blob: &[u8]) {
        if self.events.is_listening() {
            if let Some(hash) = Transaction::from_bytes(blob).ok().and_then(|tx| tx.hash().ok()) {
                self.events.pool_added(&[hash]);
            }
        }
    }

    fn locked(&self) -> MutexGuard<'_, TransactionPool> {
        self.pool.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn chain(&self) -> std::sync::RwLockReadGuard<'_, ChainState<S>> {
        self.chain.read().unwrap_or_else(|p| p.into_inner())
    }
}

impl<S: KvStore + Send + Sync> TxPool for SharedMempool<S> {
    /// `Core::addTransactionToPool`. Only a transaction that was actually
    /// accepted is relayed on, which is what the `bool` means to the engine.
    fn add_transaction(&mut self, blob: &[u8]) -> bool {
        let chain = self.chain();
        let accepted = self.locked().add(blob, &*chain, self.source).accepted();
        drop(chain);
        if accepted {
            self.publish_added(blob);
        }
        accepted
    }

    /// The same admission, with the pool's reason kept so the engine can
    /// score a peer that relays something no honest relay would.
    fn add_relayed_transaction(&mut self, blob: &[u8]) -> TxVerdict {
        let chain = self.chain();
        let status = self.locked().add(blob, &*chain, self.source);
        drop(chain);
        if status.accepted() {
            self.publish_added(blob);
        }
        verdict_of(&status)
    }

    fn transaction_hashes(&self) -> Vec<Hash> {
        self.locked().hashes()
    }

    fn transaction(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.locked().get(hash).map(|e| e.blob.clone())
    }

    fn len(&self) -> usize {
        self.locked().len()
    }

    /// `Core::addBlock`'s pool bookkeeping: drop what the block mined, what it
    /// double-spends, and what its height invalidates
    /// (`TransactionPool::on_block_added`).
    fn on_block_added(&mut self, index: u32, transaction_hashes: &[Hash], spent_key_images: &HashSet<Hash>) {
        let chain = self.chain();
        let dropped = self.locked().on_block_added(&*chain, index, transaction_hashes, spent_key_images);
        drop(chain);
        if !dropped.is_empty() {
            crate::log_debug!("pool: {} transactions dropped after block {index}", dropped.len());
            if self.events.is_listening() {
                // What the block mined, then whatever else it made invalid, as
                // `wrkz_rpc::events::block_events` splits them.
                let (mined, invalid): (Vec<Hash>, Vec<Hash>) =
                    dropped.into_iter().partition(|hash| transaction_hashes.contains(hash));
                self.events.pool_removed(mined, PoolRemoval::InBlock);
                self.events.pool_removed(invalid, PoolRemoval::NotActual);
            }
        }
    }

    /// A chain switch: sweep the whole pool against the new main chain
    /// (`TransactionPool::remove_spent_in_chain`), not just against the block
    /// that caused it.
    fn on_chain_switched(&mut self) {
        let chain = self.chain();
        let dropped = self.locked().remove_spent_in_chain(&*chain);
        drop(chain);
        if !dropped.is_empty() {
            crate::log_info!("pool: {} transactions spent on the new chain were dropped", dropped.len());
            self.events.pool_removed(dropped, PoolRemoval::NotActual);
        }
    }

    /// `Core::copyTransactionsToPool`: the transactions of blocks that just
    /// left the main chain are offered back.
    fn on_blocks_unwound(&mut self, transactions: &[Vec<u8>]) {
        if transactions.is_empty() {
            return;
        }
        let chain = self.chain();
        let restored = self.locked().copy_transactions_to_pool(&*chain, transactions);
        drop(chain);
        crate::log_info!("pool: {} of {} unwound transactions came back", restored.len(), transactions.len());
        self.events.pool_added(&restored);
    }

    /// `TransactionPoolCleanWrapper::clean` at a new height.
    fn clean(&mut self, height: u64) {
        let dropped = self.locked().clean(height);
        if !dropped.is_empty() {
            crate::log_debug!("pool: {} transactions expired at height {height}", dropped.len());
            self.events.pool_removed(dropped, PoolRemoval::Outdated);
        }
    }
}

/// The key images a block's transactions spend, which is the
/// `TransactionValidatorState` `on_block_added` wants.
pub fn spent_key_images(tx_blobs: &[Vec<u8>]) -> HashSet<Hash> {
    let mut images = HashSet::new();
    for blob in tx_blobs {
        let Ok(tx) = wrkz_primitives::tx::Transaction::from_bytes(blob) else { continue };
        for input in &tx.prefix.inputs {
            if let Input::Key { key_image, .. } = input {
                images.insert(*key_image);
            }
        }
    }
    images
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;
    use wrkz_chain::{Checkpoints, Config};
    use wrkz_storage::MemStore;

    fn shared() -> SharedMempool<MemStore> {
        let chain =
            ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
        SharedMempool::new(Arc::new(Mutex::new(TransactionPool::new(Default::default()))), Arc::new(RwLock::new(chain)))
    }

    #[test]
    fn garbage_is_not_accepted_and_not_relayed() {
        let mut pool = shared();
        assert!(!pool.add_transaction(&[0x00]));
        assert!(!pool.add_transaction(&[]));
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
        assert!(pool.transaction(&[0; 32]).is_none());
        assert!(pool.transaction_hashes().is_empty());
    }

    /// A relayed blob that does not parse is the sender's fault; one the pool
    /// merely declines is not.
    #[test]
    fn a_relayed_blob_that_does_not_parse_is_the_senders_fault() {
        let mut pool = shared();
        assert!(matches!(pool.add_relayed_transaction(&[0x00]), TxVerdict::Invalid(_)));
        assert_eq!(verdict_of(&PoolStatus::AlreadyInPool), TxVerdict::NotAccepted);
        assert_eq!(verdict_of(&PoolStatus::PoolFull), TxVerdict::NotAccepted);
        assert_eq!(verdict_of(&PoolStatus::Rejected(TxRule::WrongFee { fee: 1, minimum: 2 })), TxVerdict::NotAccepted);
        assert!(matches!(
            verdict_of(&PoolStatus::Rejected(TxRule::InputInvalidSignatures { input: 0 })),
            TxVerdict::Invalid(_)
        ));
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn two_handles_see_one_pool() {
        let pool = shared();
        let other = pool.clone();
        assert!(Arc::ptr_eq(&pool.handle(), &other.handle()));
    }

    #[test]
    fn key_images_are_collected_from_every_transaction() {
        assert!(spent_key_images(&[vec![0x00]]).is_empty(), "an unparseable blob contributes nothing");
        assert!(spent_key_images(&[]).is_empty());
    }
}
