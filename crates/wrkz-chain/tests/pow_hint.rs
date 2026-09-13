// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A proof-of-work hash computed ahead of time ([`PowHint`]) changes where the
//! hash runs and nothing else: the same blocks are accepted with the same
//! outcomes, and a hint that belongs to another block is ignored.
//!
//! Mainnet blocks 1-5 with checkpoints off, so every one of them has its proof
//! of work checked (v1-v4, the slow CryptoNight variants).

use serde_json::Value;
use std::path::PathBuf;
use wrkz_chain::{ChainState, Checkpoints, Config, PowHint};
use wrkz_primitives::block::BlockTemplate;
use wrkz_storage::MemStore;

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

fn fresh_chain() -> ChainState<MemStore> {
    let mut chain = ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::none()).unwrap();
    chain.set_clock(Some(1_900_000_000));
    chain
}

/// Apply the five blocks, each with the hint `hint_for(i)` hands it.
fn apply(hint_for: impl Fn(usize) -> Option<PowHint>) -> Vec<String> {
    let mut chain = fresh_chain();
    blocks_1_to_5()
        .iter()
        .enumerate()
        .map(|(i, (block, txs))| {
            format!(
                "{:?}",
                chain.add_block_detailed_with_pow(block, txs, hint_for(i)).expect("a mainnet block").outcome
            )
        })
        .collect()
}

#[test]
fn a_hint_changes_nothing_but_where_the_hash_ran() {
    let blocks = blocks_1_to_5();
    let templates: Vec<BlockTemplate> = blocks.iter().map(|(b, _)| BlockTemplate::from_bytes(b).unwrap()).collect();
    let wanted: Vec<Option<&BlockTemplate>> = templates.iter().map(Some).collect();

    let hints = PowHint::compute_many(&wanted, 4);
    assert!(hints.iter().all(Option::is_some), "four threads, five blocks: every one hashed ahead");
    for (hint, template) in hints.iter().zip(&templates) {
        assert_eq!(hint.unwrap().block_hash(), &template.hash().unwrap());
    }
    // Fewer than two threads, or nothing wanted: no hints, the chain hashes.
    assert!(PowHint::compute_many(&wanted, 1).iter().all(Option::is_none));
    assert!(PowHint::compute_many(&[None, None], 8).iter().all(Option::is_none));

    let plain = apply(|_| None);
    assert_eq!(apply(|i| hints[i]), plain, "hinted and unhinted apply alike");
    // Every block handed the *next* block's hint: ignored, hashed afresh, and
    // the same outcome again.
    assert_eq!(apply(|i| hints[(i + 1) % hints.len()]), plain, "a foreign hint is ignored");
}
