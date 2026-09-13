// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The transaction pool the P2P layer talks to.
//!
//! Stage 3.4 owns pool policy (`06-transactions.md` admission,
//! `fillBlockTemplate`). Stage 3.3 only needs the three things the protocol
//! handler asks of `m_core`:
//!
//! - `addTransactionToPool` on `NOTIFY_NEW_TRANSACTIONS`
//!   (`CryptoNoteProtocolHandler.cpp:770`),
//! - `getPoolTransactionHashes` for `NOTIFY_REQUEST_TX_POOL`
//!   (`:1645`),
//! - `getTransactions` to answer `NOTIFY_MISSING_TXS` and to fill in a lite
//!   block's known transactions (`:1210`, `:1556`).
//!
//! [`TxPool`] is that interface. [`BoundedTxSet`] is the stage-3.3
//! implementation: stateless checks only, a bounded set for relay, oldest
//! evicted. Wiring a real pool in stage 3.4 is a matter of implementing the
//! trait and handing it to [`crate::Node`].

use std::collections::HashMap;

use wrkz_primitives::tx::Transaction;
use wrkz_primitives::Hash;

/// What became of a transaction a peer relayed, as far as the engine cares:
/// whether to relay it on, and whether the peer that sent it is at fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxVerdict {
    /// In the pool now; relay it.
    Accepted,
    /// Not taken, for a reason that is no fault of the sender: we already hold
    /// it, the pool is full, it conflicts with our chain or pool, or it misses
    /// a fee, mixin or unlock rule that depends on the height.
    NotAccepted,
    /// Not taken because it breaks a rule no honest relay passes on: it does
    /// not parse, or its signatures or proof of work are wrong. The text is
    /// for the log.
    Invalid(String),
}

/// What the P2P layer needs from a transaction pool.
///
/// Every method takes the *serialized* transaction, because that is what the
/// wire carries and what the size and fee rules measure; re-serializing a
/// parsed transaction can differ byte for byte and would change its hash.
pub trait TxPool {
    /// `Core::addTransactionToPool`. Returns whether the transaction was
    /// accepted; only accepted ones are relayed on
    /// (`handle_notify_new_transactions`).
    ///
    /// A transaction we already hold counts as accepted-but-not-new: return
    /// `false` so it is not relayed a second time, which is what
    /// `getPoolChanges` achieves in the C++.
    fn add_transaction(&mut self, blob: &[u8]) -> bool;

    /// [`TxPool::add_transaction`] for a transaction a peer relayed, saying
    /// whether a refusal is the sender's fault, which the engine scores. The
    /// default cannot tell and never blames the sender.
    fn add_relayed_transaction(&mut self, blob: &[u8]) -> TxVerdict {
        if self.add_transaction(blob) {
            TxVerdict::Accepted
        } else {
            TxVerdict::NotAccepted
        }
    }

    /// `Core::getPoolTransactionHashes`.
    fn transaction_hashes(&self) -> Vec<Hash>;

    /// `Core::getTransaction`: the blob for a hash, from the pool only. The
    /// node asks the chain separately for confirmed transactions.
    fn transaction(&self, hash: &Hash) -> Option<Vec<u8>>;

    /// The hashes of `wanted` this pool does not hold.
    fn missing(&self, wanted: &[Hash]) -> Vec<Hash> {
        wanted.iter().filter(|h| self.transaction(h).is_none()).copied().collect()
    }

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `Core::addBlock`'s pool bookkeeping, once a block is on the main chain:
    /// drop what it mined, what it double-spends and what its height
    /// invalidates (`TransactionPool::on_block_added`).
    ///
    /// The default does nothing, which is right for a relay-only set: it holds
    /// transactions for forwarding and never mines them.
    fn on_block_added(
        &mut self,
        _block_index: u32,
        _block_transaction_hashes: &[Hash],
        _spent_key_images: &std::collections::HashSet<Hash>,
    ) {
    }

    /// A chain switch: drop every pooled transaction the new main chain has
    /// spent, whichever of its blocks spent it. The C++ checks the pool against
    /// the switching block only (`Core.cpp:1717`), which leaves a double spend
    /// mined in an earlier block of the branch in the pool — and in every block
    /// template — until it expires. Called before
    /// [`TxPool::on_blocks_unwound`]. The default does nothing.
    fn on_chain_switched(&mut self) {}

    /// `Core::copyTransactionsToPool`: the transactions of blocks that just
    /// left the main chain in a reorganisation, offered back.
    fn on_blocks_unwound(&mut self, _transactions: &[Vec<u8>]) {}

    /// `TransactionPoolCleanWrapper::clean` at a new height: age out what has
    /// been waiting too long.
    fn clean(&mut self, _height: u64) {}
}

/// A bounded set of relayable transactions with the stateless checks only.
///
/// What it does **not** do, and what 3.4 must add: key-image conflict
/// detection against the chain and the rest of the pool, ring member
/// resolution and signature verification, the fee and mixin ladders at the
/// current height, unlock-time rules, and eviction by age and fee rather than
/// by insertion order. Until then a transaction here has been parsed and
/// shape-checked and nothing more, which is enough to relay it without
/// amplifying garbage and not enough to mine it.
pub struct BoundedTxSet {
    by_hash: HashMap<Hash, Vec<u8>>,
    /// Insertion order, for eviction.
    order: std::collections::VecDeque<Hash>,
    max_transactions: usize,
    max_blob_bytes: usize,
    bytes: usize,
}

impl BoundedTxSet {
    /// `max_transactions` entries, each at most `max_blob_bytes`.
    pub fn new(max_transactions: usize, max_blob_bytes: usize) -> Self {
        Self {
            by_hash: HashMap::new(),
            order: std::collections::VecDeque::new(),
            max_transactions,
            max_blob_bytes,
            bytes: 0,
        }
    }

    /// The bytes currently held, so the caller can log the cost of relay.
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    /// The stateless part of `ValidateTransaction`: it must parse, consume the
    /// whole blob, not be a coinbase, and have at least one input and one
    /// output. Everything past that needs chain state and belongs to 3.4.
    fn stateless_check(blob: &[u8]) -> Option<Hash> {
        let tx = Transaction::from_bytes(blob).ok()?;
        if tx.prefix.is_coinbase() || tx.prefix.inputs.is_empty() || tx.prefix.outputs.is_empty() {
            return None;
        }
        // `sum_inputs`/`sum_outputs` return None on overflow, which
        // `validate_inputs`/`validate_outputs` reject as
        // INPUTS_AMOUNT_OVERFLOW / OUTPUTS_AMOUNT_OVERFLOW.
        tx.prefix.sum_inputs()?;
        tx.prefix.sum_outputs()?;
        tx.hash().ok()
    }

    fn evict_one(&mut self) {
        if let Some(h) = self.order.pop_front() {
            if let Some(blob) = self.by_hash.remove(&h) {
                self.bytes -= blob.len();
            }
        }
    }
}

impl TxPool for BoundedTxSet {
    fn add_transaction(&mut self, blob: &[u8]) -> bool {
        if blob.len() > self.max_blob_bytes {
            return false;
        }
        let Some(hash) = Self::stateless_check(blob) else { return false };
        if self.by_hash.contains_key(&hash) {
            return false;
        }
        while self.by_hash.len() >= self.max_transactions {
            self.evict_one();
        }
        self.bytes += blob.len();
        self.by_hash.insert(hash, blob.to_vec());
        self.order.push_back(hash);
        true
    }

    fn transaction_hashes(&self) -> Vec<Hash> {
        self.order.iter().copied().collect()
    }

    fn transaction(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.by_hash.get(hash).cloned()
    }

    fn len(&self) -> usize {
        self.by_hash.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real mainnet transaction, from the block 4,213,650 vector.
    fn sample_tx() -> Vec<u8> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../spec/vectors/mainnet_rawblocks_4213648_to_4213650_v7.json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for item in v["items"].as_array().unwrap() {
            if let Some(t) = item["transactions"].as_array().unwrap().first() {
                return hex::decode(t.as_str().unwrap()).unwrap();
            }
        }
        panic!("no transaction in the vector");
    }

    #[test]
    fn accepts_a_real_transaction_once() {
        let blob = sample_tx();
        let hash = Transaction::from_bytes(&blob).unwrap().hash().unwrap();
        let mut pool = BoundedTxSet::new(4, 1 << 20);
        assert!(pool.add_transaction(&blob));
        assert_eq!(pool.transaction_hashes(), vec![hash]);
        assert_eq!(pool.transaction(&hash), Some(blob.clone()));
        assert!(pool.missing(&[hash]).is_empty());
        assert_eq!(pool.missing(&[[9u8; 32]]), vec![[9u8; 32]]);
        // a duplicate is not relayed again
        assert!(!pool.add_transaction(&blob));
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.byte_len(), blob.len());
    }

    #[test]
    fn rejects_garbage_and_oversized_blobs() {
        let mut pool = BoundedTxSet::new(4, 16);
        assert!(!pool.add_transaction(b"not a transaction"), "over the blob limit");
        let mut pool = BoundedTxSet::new(4, 1 << 20);
        assert!(!pool.add_transaction(b""));
        assert!(!pool.add_transaction(b"not a transaction"));
        // a coinbase is never a pool transaction
        let genesis = wrkz_primitives::block::genesis_block();
        assert!(!pool.add_transaction(&genesis.base_transaction.to_bytes().unwrap()));
        assert!(pool.is_empty());
    }

    /// The set is bounded: the oldest entry leaves when a new one arrives.
    #[test]
    fn evicts_the_oldest() {
        let blob = sample_tx();
        let mut pool = BoundedTxSet::new(1, 1 << 20);
        assert!(pool.add_transaction(&blob));
        // a second, different transaction: flip a byte of the extra so the
        // hash changes but the shape stays valid
        let mut other = blob.clone();
        let last = other.len() - 1;
        other[last] ^= 0xff;
        if pool.add_transaction(&other) {
            assert_eq!(pool.len(), 1);
            assert!(pool.transaction(&Transaction::from_bytes(&blob).unwrap().hash().unwrap()).is_none());
        }
    }
}
