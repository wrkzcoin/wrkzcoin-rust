// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! What happened to the chain and the pool, for whoever is listening: the ZMQ
//! publisher and the notify hooks. These are the C++'s `BlockchainMessage`s
//! (`src/cryptonotecore/MessageQueue.h`) and the `Core` calls that raise them
//! (`Core.cpp:2017-2029`, `:2244-2265`).
//!
//! Two parts of the daemon change the chain: the P2P engine applying the
//! blocks peers send, and [`crate::node::ChainNode`] applying the blocks
//! `submitblock` and the stratum server submit. Both hold the same [`Events`]
//! and publish through it once the change is made, so a listener sees one
//! stream whichever way a block arrived.
//!
//! Publishing never waits on a listener: [`EventListener::on_event`] queues
//! and returns, and does its work on its own thread. With nobody listening,
//! [`Events`] costs a length check, and the one event that needs extra work to
//! describe — a block's transaction hashes — is not built at all.

use std::sync::Arc;

use wrkz_chain::{AddOutcome, AddStatus};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::Hash;

/// Why transactions left the pool (`Messages::DeleteTransaction::Reason`), in
/// the spelling `txpool_del` uses (`ZmqPublisher.cpp:332-345`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolRemoval {
    /// Mined by the block just added.
    InBlock,
    /// Aged out of the pool.
    Outdated,
    /// No longer valid against the chain: spent elsewhere, or refused by a
    /// rule the new height brings.
    NotActual,
}

impl PoolRemoval {
    pub fn as_str(self) -> &'static str {
        match self {
            PoolRemoval::InBlock => "InBlock",
            PoolRemoval::Outdated => "Outdated",
            PoolRemoval::NotActual => "NotActual",
        }
    }
}

/// One thing that happened (`CryptoNote::BlockchainMessage`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainEvent {
    /// A block joined the main chain on top of the previous tip
    /// (`Messages::NewBlock`). `transaction_hashes` is the coinbase's hash,
    /// then the block's transactions in block order.
    BlockAdded { index: u32, hash: Hash, transaction_hashes: Vec<Hash> },
    /// A block was kept on an alternative chain, with no switch
    /// (`Messages::NewAlternativeBlock`).
    AlternativeBlockAdded { index: u32, hash: Hash },
    /// The main chain moved onto another branch (`Messages::ChainSwitch`).
    /// `hashes` is the new main chain from the common root to the new tip, the
    /// root first. The blocks the switch brought in raise no
    /// [`ChainEvent::BlockAdded`] of their own, as in the C++.
    ChainSwitched { common_root_index: u32, hashes: Vec<Hash> },
    /// A transaction entered the pool (`Messages::AddTransaction`).
    PoolAdded { hash: Hash },
    /// Transactions left the pool (`Messages::DeleteTransaction`).
    PoolRemoved { hashes: Vec<Hash>, reason: PoolRemoval },
}

/// Something that wants the events.
///
/// Called on the thread that changed the chain or the pool, after it has let
/// go of their locks. It must not block — the P2P engine is one of the
/// callers — so an implementation queues the event and returns.
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: &ChainEvent);
}

/// The listeners a chain or pool writer publishes to: fixed at start-up,
/// cheap to clone, and empty by default.
#[derive(Clone, Default)]
pub struct Events {
    listeners: Arc<Vec<Arc<dyn EventListener>>>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events").field("listeners", &self.listeners.len()).finish()
    }
}

impl Events {
    pub fn new(listeners: Vec<Arc<dyn EventListener>>) -> Self {
        Self { listeners: Arc::new(listeners) }
    }

    /// Whether anyone is listening, so a caller can skip describing an event
    /// nobody will read.
    pub fn is_listening(&self) -> bool {
        !self.listeners.is_empty()
    }

    pub fn publish(&self, event: &ChainEvent) {
        for listener in self.listeners.iter() {
            listener.on_event(event);
        }
    }

    /// `txpool_add`, one event per transaction as the C++ raises them.
    pub fn pool_added(&self, hashes: &[Hash]) {
        for hash in hashes {
            self.publish(&ChainEvent::PoolAdded { hash: *hash });
        }
    }

    /// `txpool_del`. Nothing is published for an empty list.
    pub fn pool_removed(&self, hashes: Vec<Hash>, reason: PoolRemoval) {
        if !hashes.is_empty() {
            self.publish(&ChainEvent::PoolRemoved { hashes, reason });
        }
    }

    /// Everything adding one block did, in the order of [`block_events`].
    /// `hash_at` reads the main chain after the block was added.
    pub fn block_applied(&self, applied: &AppliedBlock<'_>, hash_at: impl Fn(u32) -> Option<Hash>) {
        if self.is_listening() {
            for event in block_events(applied, hash_at) {
                self.publish(&event);
            }
        }
    }
}

/// What adding one block changed, as its caller knows it.
#[derive(Clone, Copy, Debug)]
pub struct AppliedBlock<'a> {
    pub outcome: &'a AddOutcome,
    /// The block as it was added. Only its coinbase and its list of
    /// transaction hashes are read.
    pub block_blob: &'a [u8],
    /// For a switch, the index of the lowest block that left the main chain —
    /// the last entry of `AddReport::unwound`. The common root is the block
    /// below it.
    pub lowest_unwound: Option<u32>,
    /// Pool transactions the block's bookkeeping dropped.
    pub pool_removed: &'a [Hash],
    /// Transactions of unwound blocks that went back into the pool.
    pub pool_restored: &'a [Hash],
}

/// The events one added block raises, in the order the C++ raises them
/// (`Core::addBlock`, `Core.cpp:1690-1777`, then `:2017-2029`): what left the
/// pool, what came back to it, then the block or the switch.
///
/// Two differences from the C++, both so a subscriber can use what it is told:
///
/// - the `InBlock` removal names the transactions this block mined. The C++'s
///   list is always empty — it is reserved and never filled (`Core.cpp:62-67`).
///   Anything else the bookkeeping dropped is `NotActual`.
/// - an empty removal is not raised at all; the C++ raises one per block.
pub fn block_events(applied: &AppliedBlock<'_>, hash_at: impl Fn(u32) -> Option<Hash>) -> Vec<ChainEvent> {
    let outcome = applied.outcome;
    if outcome.status == AddStatus::Alternative {
        return vec![ChainEvent::AlternativeBlockAdded { index: outcome.index, hash: outcome.hash }];
    }
    let block = BlockTemplate::from_bytes(applied.block_blob).ok();
    let mined: &[Hash] = block.as_ref().map_or(&[], |b| b.transaction_hashes.as_slice());

    let mut events = Vec::new();
    let (in_block, not_actual): (Vec<Hash>, Vec<Hash>) =
        applied.pool_removed.iter().copied().partition(|hash| mined.contains(hash));
    for (hashes, reason) in [(in_block, PoolRemoval::InBlock), (not_actual, PoolRemoval::NotActual)] {
        if !hashes.is_empty() {
            events.push(ChainEvent::PoolRemoved { hashes, reason });
        }
    }
    events.extend(applied.pool_restored.iter().map(|hash| ChainEvent::PoolAdded { hash: *hash }));

    match outcome.status {
        AddStatus::Main => {
            if let Some(block) = &block {
                let mut transaction_hashes = Vec::with_capacity(block.transaction_hashes.len() + 1);
                if let Ok(coinbase) = block.base_transaction.hash() {
                    transaction_hashes.push(coinbase);
                }
                transaction_hashes.extend_from_slice(&block.transaction_hashes);
                events.push(ChainEvent::BlockAdded { index: outcome.index, hash: outcome.hash, transaction_hashes });
            }
        }
        AddStatus::AlternativeAndSwitched => {
            if let Some(lowest) = applied.lowest_unwound {
                let root = lowest.saturating_sub(1);
                let hashes: Option<Vec<Hash>> = (root..=outcome.index).map(&hash_at).collect();
                if let Some(hashes) = hashes {
                    events.push(ChainEvent::ChainSwitched { common_root_index: root, hashes });
                }
            }
        }
        AddStatus::Alternative => {}
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wrkz_primitives::tx::{Input, Transaction, TransactionPrefix};

    fn block(transaction_hashes: Vec<Hash>) -> (Vec<u8>, Hash) {
        let block = BlockTemplate {
            major_version: 1,
            minor_version: 0,
            timestamp: 1_800_000_000,
            previous_block_hash: [1; 32],
            nonce: 0,
            parent_block: None,
            base_transaction: Transaction {
                prefix: TransactionPrefix {
                    version: 1,
                    unlock_time: 50,
                    inputs: vec![Input::Base { block_index: 10 }],
                    outputs: Vec::new(),
                    extra: Vec::new(),
                },
                signatures: Vec::new(),
            },
            transaction_hashes,
        };
        let coinbase = block.base_transaction.hash().unwrap();
        (block.to_bytes().unwrap(), coinbase)
    }

    fn outcome(index: u32, status: AddStatus) -> AddOutcome {
        AddOutcome {
            index,
            hash: [index as u8; 32],
            cumulative_difficulty: 0,
            already_generated_coins: 0,
            difficulty: 1,
            status,
        }
    }

    #[test]
    fn a_main_chain_block_reports_the_pool_first_and_its_coinbase_first() {
        let (blob, coinbase) = block(vec![[0xa1; 32], [0xa2; 32]]);
        let added = outcome(10, AddStatus::Main);
        let applied = AppliedBlock {
            outcome: &added,
            block_blob: &blob,
            lowest_unwound: None,
            pool_removed: &[[0xa2; 32], [0xee; 32]],
            pool_restored: &[],
        };
        assert_eq!(
            block_events(&applied, |_| None),
            vec![
                ChainEvent::PoolRemoved { hashes: vec![[0xa2; 32]], reason: PoolRemoval::InBlock },
                ChainEvent::PoolRemoved { hashes: vec![[0xee; 32]], reason: PoolRemoval::NotActual },
                ChainEvent::BlockAdded {
                    index: 10,
                    hash: [10; 32],
                    transaction_hashes: vec![coinbase, [0xa1; 32], [0xa2; 32]],
                },
            ]
        );
    }

    #[test]
    fn an_alternative_block_is_only_that() {
        let (blob, _) = block(Vec::new());
        let kept = outcome(7, AddStatus::Alternative);
        let applied = AppliedBlock {
            outcome: &kept,
            block_blob: &blob,
            lowest_unwound: None,
            pool_removed: &[],
            pool_restored: &[],
        };
        assert_eq!(
            block_events(&applied, |_| None),
            vec![ChainEvent::AlternativeBlockAdded { index: 7, hash: [7; 32] }]
        );
    }

    #[test]
    fn a_switch_names_the_new_chain_from_the_common_root_and_no_block() {
        let (blob, _) = block(Vec::new());
        let switched = outcome(12, AddStatus::AlternativeAndSwitched);
        // Blocks 10 and 11 left the main chain; 9 is the common root.
        let applied = AppliedBlock {
            outcome: &switched,
            block_blob: &blob,
            lowest_unwound: Some(10),
            pool_removed: &[],
            pool_restored: &[[0xbb; 32]],
        };
        let events = block_events(&applied, |i| Some([i as u8 + 100; 32]));
        assert_eq!(
            events,
            vec![
                ChainEvent::PoolAdded { hash: [0xbb; 32] },
                ChainEvent::ChainSwitched {
                    common_root_index: 9,
                    hashes: vec![[109; 32], [110; 32], [111; 32], [112; 32]],
                },
            ]
        );
    }

    struct Recorder(Mutex<Vec<ChainEvent>>);

    impl EventListener for Recorder {
        fn on_event(&self, event: &ChainEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    #[test]
    fn every_listener_hears_every_event_and_empty_removals_are_not_sent() {
        let first = Arc::new(Recorder(Mutex::new(Vec::new())));
        let second = Arc::new(Recorder(Mutex::new(Vec::new())));
        let events = Events::new(vec![first.clone(), second.clone()]);
        assert!(events.is_listening());
        events.pool_removed(Vec::new(), PoolRemoval::Outdated);
        events.pool_added(&[[1; 32], [2; 32]]);
        let want = vec![ChainEvent::PoolAdded { hash: [1; 32] }, ChainEvent::PoolAdded { hash: [2; 32] }];
        assert_eq!(*first.0.lock().unwrap(), want);
        assert_eq!(*second.0.lock().unwrap(), want);
        assert!(!Events::default().is_listening());
    }
}
