// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `Config::simnet` is permanent for a database, in both directions: a
//! simnet's blocks carry no proof of work, so its state must never be opened
//! as mainnet's, and a mainnet chain must never be opened as a simnet.
//!
//! What a simnet accepts is tested where blocks are made, with whole nodes, in
//! `crates/wrkz-simnet/tests/simnet.rs`.

use wrkz_chain::{keys, ChainState, Checkpoints, Config};
use wrkz_storage::{KvStore, MemStore};

fn simnet() -> Config {
    Config { simnet: true, ..Config::default() }
}

fn fresh_store() -> MemStore {
    let mut store = MemStore::default();
    store
        .write_batch(vec![(keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec()))])
        .unwrap();
    store
}

#[test]
fn a_fresh_simnet_state_is_marked_and_stays_a_simnet() {
    let chain =
        ChainState::open_or_genesis(fresh_store(), simnet(), Checkpoints::none()).expect("a fresh simnet opens");
    assert!(chain.is_simnet());
    let store = chain.into_store();
    assert_eq!(store.get(&keys::meta(keys::META_NETWORK)).unwrap().as_deref(), Some(keys::NETWORK_SIMNET));

    let chain = ChainState::open(store, simnet(), Checkpoints::none()).expect("a simnet reopens as one");
    let store = chain.into_store();

    let e = ChainState::open(store, Config::default(), Checkpoints::mainnet()).err().expect("refused").to_string();
    assert!(e.contains("belongs to a simnet"), "{e}");
    assert!(e.contains("--simnet"), "the message names the flag: {e}");
}

#[test]
fn a_mainnet_chain_cannot_become_a_simnet() {
    // Genesis is enough: the state holds a mainnet chain.
    let chain = ChainState::open_or_genesis(fresh_store(), Config::default(), Checkpoints::none()).unwrap();
    assert!(!chain.is_simnet());
    let store = chain.into_store();
    assert_eq!(store.get(&keys::meta(keys::META_NETWORK)).unwrap(), None, "mainnet writes no record");

    let e = ChainState::open(store, simnet(), Checkpoints::none()).err().expect("refused").to_string();
    assert!(e.contains("holds a mainnet chain"), "{e}");
}

#[test]
fn an_unknown_network_record_is_refused_either_way() {
    for cfg in [Config::default(), simnet()] {
        let mut store = fresh_store();
        store.write_batch(vec![(keys::meta(keys::META_NETWORK), Some(b"testnet".to_vec()))]).unwrap();
        let e = ChainState::open(store, cfg, Checkpoints::none()).err().expect("refused").to_string();
        assert!(e.contains("does not know"), "{e}");
    }
}
