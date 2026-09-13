// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `verify_state`: an honest state verifies record for record, a tampered one
//! is caught at the block that wrote the record, a state that dropped its
//! per-block output lists is still fully compared, and a run resumes.

use serde_json::Value;
use std::path::PathBuf;
use wrkz_chain::replay::ReplayOptions;
use wrkz_chain::verify::verify_state;
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

fn fresh() -> ChainState<MemStore> {
    let mut chain =
        ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
    chain.set_clock(Some(1_900_000_000));
    chain
}

fn given() -> ChainState<MemStore> {
    let mut chain = fresh();
    for (block, txs) in blocks_1_to_5() {
        chain.add_block(&block, &txs).expect("a mainnet block");
    }
    chain
}

fn reopen(store: MemStore) -> ChainState<MemStore> {
    ChainState::open(store, Config::default(), Checkpoints::mainnet()).unwrap()
}

fn quiet() -> impl FnMut(&str) {
    |_| {}
}

#[test]
fn an_honest_state_verifies_record_for_record() {
    let source = given();
    let mut target = fresh();
    let report = verify_state(&source, &mut target, &ReplayOptions::default(), &mut quiet()).unwrap();
    assert_eq!((report.top, report.applied, report.source_top, report.stopped), (5, 5, 5, false));
    assert_eq!(target.tip_info(), source.tip_info());
}

#[test]
fn a_tampered_output_record_is_caught_at_its_block() {
    let source = given();
    let (amount, global_index) = source.block_output_refs(3).unwrap().unwrap()[0];
    let mut store = source.into_store();
    let key = keys::output(amount, global_index);
    let mut record = store.get(&key).unwrap().unwrap();
    record[0] ^= 1;
    store.put(key, record).unwrap();
    let err = verify_state(&reopen(store), &mut fresh(), &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(err.starts_with("block 3:"), "{err}");
    assert!(err.contains(&format!("amount {amount} at global index {global_index}")), "{err}");
}

#[test]
fn a_tampered_block_info_is_caught() {
    let source = given();
    let mut store = source.into_store();
    let mut info = store.get(&keys::block_info(2)).unwrap().unwrap();
    let last = info.len() - 1;
    info[last] ^= 1;
    store.put(keys::block_info(2), info).unwrap();
    let err = verify_state(&reopen(store), &mut fresh(), &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(err.starts_with("block 2: the block info differs"), "{err}");
}

#[test]
fn dropped_output_lists_are_still_compared_in_full() {
    let source = given();
    let mut store = source.into_store();
    for i in 0..=5 {
        store.delete(keys::block_outputs(i)).unwrap();
    }
    let report = verify_state(&reopen(store), &mut fresh(), &ReplayOptions::default(), &mut quiet()).unwrap();
    assert_eq!(report.top, 5);
}

/// The given state dropped its lists — as every older import did — and one of
/// its output records was altered: still caught, at the block that created it.
#[test]
fn a_tampered_output_record_is_caught_where_the_list_was_dropped() {
    let source = given();
    let (amount, global_index) = source.block_output_refs(4).unwrap().unwrap()[0];
    let mut store = source.into_store();
    for i in 0..=5 {
        store.delete(keys::block_outputs(i)).unwrap();
    }
    let key = keys::output(amount, global_index);
    let mut record = store.get(&key).unwrap().unwrap();
    let last = record.len() - 1;
    record[last] ^= 1;
    store.put(key, record).unwrap();
    let err = verify_state(&reopen(store), &mut fresh(), &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(err.starts_with("block 4:"), "{err}");
    assert!(err.contains(&format!("amount {amount} at global index {global_index}")), "{err}");
}

#[test]
fn a_missing_output_record_is_caught() {
    let source = given();
    let (amount, global_index) = source.block_output_refs(2).unwrap().unwrap()[0];
    let mut store = source.into_store();
    store.delete(keys::block_outputs(2)).unwrap();
    store.delete(keys::output(amount, global_index)).unwrap();
    let err = verify_state(&reopen(store), &mut fresh(), &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(err.starts_with("block 2:"), "{err}");
    assert!(err.contains("the given state holds nothing"), "{err}");
}

/// Where the given state kept its per-block list, the list itself must agree.
#[test]
fn a_per_block_output_list_that_disagrees_is_caught() {
    let source = given();
    let mut refs = source.block_output_refs(3).unwrap().unwrap();
    refs.reverse();
    refs.push((1, 999));
    let mut store = source.into_store();
    store.put(keys::block_outputs(3), wrkz_chain::records::encode_output_refs(&refs)).unwrap();
    let err = verify_state(&reopen(store), &mut fresh(), &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(err.starts_with("block 3: the outputs it created differ"), "{err}");
}

#[test]
fn a_run_resumes_where_the_last_stopped() {
    let source = given();
    let mut target = fresh();
    let first = verify_state(&source, &mut target, &ReplayOptions { to: Some(3), ..Default::default() }, &mut quiet());
    assert_eq!(first.unwrap().top, 3);
    let second = verify_state(&source, &mut target, &ReplayOptions::default(), &mut quiet()).unwrap();
    assert_eq!((second.top, second.applied), (5, 2));
}
