// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A block's `(amount, global index)` pairs rebuilt from its body equal the
//! per-block record that was written for it — so a state imported with a short
//! unwind history, which dropped those records, still answers
//! `/get_global_indexes_for_range` exactly.

use serde_json::Value;
use std::path::PathBuf;
use wrkz_chain::{keys, ChainState, Checkpoints, Config};
use wrkz_storage::{KvStore, MemStore};

fn blocks_1_to_5() -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/mainnet_rawblocks_0_to_5.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|item| {
            let txs =
                item["transactions"].as_array().unwrap().iter().map(|t| hex::decode(t.as_str().unwrap()).unwrap());
            (hex::decode(item["block"].as_str().unwrap()).unwrap(), txs.collect())
        })
        .collect()
}

fn chain_to_5() -> ChainState<MemStore> {
    let mut chain =
        ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
    chain.set_clock(Some(1_900_000_000));
    for (block, txs) in blocks_1_to_5() {
        chain.add_block(&block, &txs).expect("a mainnet block");
    }
    chain
}

#[test]
fn dropped_output_lists_are_rebuilt_exactly() {
    let chain = chain_to_5();
    let stored: Vec<_> = (0..=5).map(|i| chain.block_output_refs(i).unwrap().expect("written")).collect();
    assert!(stored.iter().all(|refs| !refs.is_empty()), "every block here has a coinbase output");

    // What an import with a short unwind history leaves behind.
    let mut store = chain.into_store();
    for i in 0..=5 {
        store.delete(keys::block_outputs(i)).unwrap();
    }
    let chain = ChainState::open(store, Config::default(), Checkpoints::mainnet()).unwrap();
    for (i, want) in stored.iter().enumerate() {
        assert_eq!(chain.block_output_refs(i as u32).unwrap().as_ref(), Some(want), "block {i}");
    }
    assert_eq!(chain.block_output_refs(6).unwrap(), None, "above the tip");

    // Without the body there is nothing to rebuild from.
    let mut store = chain.into_store();
    store.delete(keys::raw_block(3)).unwrap();
    let chain = ChainState::open(store, Config::default(), Checkpoints::mainnet()).unwrap();
    assert_eq!(chain.block_output_refs(3).unwrap(), None);
    assert!(chain.block_output_refs(4).unwrap().is_some());
}
