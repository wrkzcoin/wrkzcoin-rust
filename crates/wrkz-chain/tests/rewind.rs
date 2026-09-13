// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `ChainState::rewind_to`, the chain side of `--rewind-to-height`.
//!
//! The claim under test is the strong one: a rewind to height X leaves the
//! store **record for record** what it was when X was the tip, and applying the
//! same blocks again leaves it record for record what it was before the rewind.
//! Nothing is excluded from the comparison — not the meta records, not the
//! payment-id index, not the per-block output records a short unwind history
//! drops — so a rewind that forgot any record, or put one back wrong, fails
//! here by name.
//!
//! The chain is the harness's real chain (see `chainbuild`): seeded at
//! 4,400,000, above the last checkpoint, so the proof of work, the ring
//! signatures and every state rule run on the blocks being rewound and re-applied.
//! A second test runs on the first five real mainnet blocks from genesis, whose
//! coinbases pay out in seven denominations under block major version 1.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
mod chainbuild;

use chainbuild::*;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use wrkz_chain::{keys, ChainState, Checkpoints, Config, Rule, MIN_PRUNE_DEPTH};
use wrkz_primitives::Hash;
use wrkz_storage::MemStore;

type Records = BTreeMap<Vec<u8>, Vec<u8>>;

/// Seeded spendable outputs: one per spend the growth pattern can build.
const SEEDED: u32 = 160;
/// A payment id reused on both sides of every rewind, so its entries have to
/// come off the end of one list across several blocks.
const ID: Hash = [0x3c; 32];

fn keeping_bodies(unwind_history: u32) -> Config {
    Config { store_raw_blocks: true, unwind_history, recent_window: 256, ..Config::default() }
}

fn records(chain: &ChainState<MemStore>) -> Records {
    chain.store().map.clone()
}

/// `W<tag>` and the rest of the key in hex, so a failure names the record.
fn key_name(key: &[u8]) -> String {
    match key {
        [b'W', tag, rest @ ..] => format!("W{}:{}", *tag as char, hex::encode(rest)),
        _ => hex::encode(key),
    }
}

/// The keys on which two stores disagree, each labelled.
fn differences(got: &Records, want: &Records) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in want {
        match got.get(k) {
            Some(g) if g == v => {}
            Some(_) => out.push(format!("changed {}", key_name(k))),
            None => out.push(format!("missing {}", key_name(k))),
        }
    }
    for k in got.keys().filter(|k| !want.contains_key(*k)) {
        out.push(format!("extra {}", key_name(k)));
    }
    out
}

fn assert_same(got: &Records, want: &Records, what: &str) {
    let diffs = differences(got, want);
    assert!(diffs.is_empty(), "{what}: {} records differ, first {:?}", diffs.len(), &diffs[..diffs.len().min(12)]);
}

/// Apply the blocks this chain built at `from..=to` to its state again.
fn reapply(chain: &mut Chain, from: u32, to: u32) {
    for index in from..=to {
        let b = chain.built.iter().find(|b| b.index == index).expect("built here");
        chain.state.add_block(&b.blob, &b.tx_blobs).unwrap_or_else(|e| panic!("re-applying {index}: {e}"));
    }
}

/// Grows a chain with a mix that touches every record an unwind has to take
/// back: spends of seeded outputs (key images, ring members, new outputs), some
/// carrying a reused payment id, spends of coinbase outputs the chain itself
/// created, and coinbases of two amounts, so two per-amount counters move.
#[derive(Default)]
struct Grower {
    next_seeded: usize,
    spent_coinbases: HashSet<u32>,
}

impl Grower {
    fn grow(&mut self, chain: &mut Chain, blocks: u32) {
        for _ in 0..blocks {
            let next = chain.tip() + 1;
            let n = next - chain.seed_tip;
            let mut txs = Vec::new();
            if n % 5 == 2 && self.next_seeded + 1 < chain.outputs.len() {
                let (real, decoy) = (chain.outputs[self.next_seeded], chain.outputs[self.next_seeded + 1]);
                self.next_seeded += 1;
                let tag = format!("seeded {n}");
                txs.push(if n.is_multiple_of(3) {
                    build_spend_with_payment_id(chain.tip(), &real, &decoy, &chain.payee, FEE, tag.as_bytes(), ID)
                } else {
                    chain.spend(&real, &decoy, tag.as_bytes())
                });
            }
            if n % 9 == 4 {
                // The newest empty block's coinbase that has unlocked and is
                // not spent yet. Its amount is the seeded amount, so a seeded
                // output can stand in the ring beside it.
                let unlocked = next.saturating_sub(COINBASE_LOCK + 5);
                let found = chain
                    .built
                    .iter()
                    .rev()
                    .filter(|b| b.index <= unlocked && b.coinbase_output.amount == AMOUNT)
                    .find(|b| !self.spent_coinbases.contains(&b.index))
                    .map(|b| (b.index, b.coinbase_output));
                if let Some((index, real)) = found {
                    self.spent_coinbases.insert(index);
                    let tag = format!("coinbase {n}");
                    txs.push(build_spend(chain.tip(), &real, &chain.outputs[0], &chain.payee, FEE, tag.as_bytes()));
                }
            }
            chain.push(&txs);
        }
    }
}

/// Grow to X, snapshot, grow `depth` more, rewind to X, compare; re-apply,
/// compare with the snapshot taken before the rewind.
fn rewind_and_reapply(chain: &mut Chain, below: u32, depth: u32) {
    let mut grower = Grower::default();
    grower.grow(chain, below);
    let x = chain.tip();
    let at_x = records(&chain.state);
    let (median_at_x, info_at_x) = (chain.state.block_median_size(), *chain.state.tip_info().unwrap());

    grower.grow(chain, depth);
    let y = chain.tip();
    let at_y = records(&chain.state);

    assert_eq!(chain.state.rewind_to(x).unwrap(), depth, "every block above {x} goes");
    assert_eq!(chain.state.tip_index(), Some(x));
    assert_eq!(chain.state.tip_info(), Some(&info_at_x), "the in-memory window follows the store");
    assert_eq!(chain.state.block_median_size(), median_at_x);
    let rewound = records(&chain.state);
    assert_same(&rewound, &at_x, &format!("after rewinding {depth} blocks to {x}"));

    // What a restarted daemon would open.
    let reopened = ChainState::open(MemStore { map: rewound }, chain.state.config().clone(), Checkpoints::mainnet())
        .expect("the rewound state opens");
    assert_eq!(reopened.tip_index(), Some(x));
    assert_eq!(reopened.tip_info(), Some(&info_at_x));
    assert_eq!(reopened.block_median_size(), median_at_x);

    reapply(chain, x + 1, y);
    assert_same(&records(&chain.state), &at_y, &format!("after re-applying {x}+1..={y}"));
}

/// Deeper than the unwind history, so the removed blocks' output records were
/// dropped and have to be rebuilt from their bodies, and the kept blocks' were
/// dropped too and have to come back.
#[test]
fn a_rewind_past_the_unwind_history_restores_the_store_record_for_record() {
    let mut chain = Chain::with_config(SEEDED, keeping_bodies(512));
    rewind_and_reapply(&mut chain, 40, 600);
    assert!(
        chain.state.store().map.keys().any(|k| k.starts_with(b"WO")),
        "the per-block output records exist, so the comparison above covered them"
    );
}

/// Within the unwind history every record is still there.
#[test]
fn a_short_rewind_restores_the_store_record_for_record() {
    let mut chain = Chain::with_config(SEEDED, keeping_bodies(512));
    rewind_and_reapply(&mut chain, 60, 40);
}

/// The daemon's configuration: nothing is ever dropped.
#[test]
fn a_rewind_on_the_daemon_configuration_restores_the_store_record_for_record() {
    let mut chain = Chain::with_config(SEEDED, keeping_bodies(u32::MAX));
    rewind_and_reapply(&mut chain, 50, 120);
}

/// A rewind of one block, and one to exactly the tip, which is nothing.
#[test]
fn rewinding_to_the_tip_or_above_changes_nothing_and_one_block_is_one_block() {
    let mut chain = Chain::with_config(SEEDED, keeping_bodies(u32::MAX));
    Grower::default().grow(&mut chain, 12);
    let tip = chain.tip();
    let before = records(&chain.state);
    assert_eq!(chain.state.rewind_to(tip).unwrap(), 0);
    assert_eq!(chain.state.rewind_to(tip + 100).unwrap(), 0);
    assert_same(&records(&chain.state), &before, "a rewind to the tip or above");

    assert_eq!(chain.state.rewind_to(tip - 1).unwrap(), 1);
    assert_eq!(chain.state.tip_index(), Some(tip - 1));
    assert_eq!(
        chain.state.tip_info().unwrap().block_hash,
        chain.state.block_info(tip - 1).unwrap().unwrap().block_hash
    );
    assert!(chain.state.block_info(tip).unwrap().is_none());
}

/// Alternative chains are in memory and are forgotten, as across a restart.
#[test]
fn a_rewind_forgets_the_alternative_chains() {
    let mut chain = Chain::with_config(SEEDED, keeping_bodies(u32::MAX));
    Grower::default().grow(&mut chain, 8);
    let tip = chain.tip();
    let parent = chain.state.block_info(tip - 1).unwrap().unwrap().block_hash;
    let (sibling, _) = chain.branch_block(parent, tip, b"a sibling of the tip");
    chain.state.add_block(&sibling, &[]).expect("an equal-weight sibling is kept as an alternative");
    assert_eq!(chain.state.alternative_block_count(), 1);
    chain.state.rewind_to(tip - 3).unwrap();
    assert_eq!(chain.state.alternative_block_count(), 0);
}

/// A state that keeps no bodies and a short unwind history — what `wrkz-replay`
/// writes by default — cannot find what a block beyond that history created.
/// That is refused before anything is written, and within the history the
/// same state rewinds.
#[test]
fn a_rewind_past_what_the_state_can_undo_is_refused_and_changes_nothing() {
    let mut chain = Chain::with_config(SEEDED, Config { store_raw_blocks: false, ..keeping_bodies(512) });
    Grower::default().grow(&mut chain, 600);
    let tip = chain.tip();
    let before = records(&chain.state);

    let e = chain.state.rewind_to(tip - 590).expect_err("the records are gone and there is no body");
    // Top down, the first block whose output record was dropped.
    assert_eq!(e.rule(), Some(&Rule::ReorganisationUnavailable { at_index: tip - 512 }), "{e}");
    assert_eq!(chain.state.tip_index(), Some(tip));
    assert_same(&records(&chain.state), &before, "a refused rewind");

    assert_eq!(chain.state.rewind_to(tip - 100).unwrap(), 100, "within the history it works");
}

/// A lite node never goes back below its lite height, as the C++ refuses
/// (`DatabaseBlockchainCache.cpp:960`).
#[test]
fn a_lite_node_refuses_to_rewind_below_its_lite_height() {
    let lite = TIP + 10;
    let cfg = Config { lite_start_height: lite, ..keeping_bodies(u32::MAX) };
    let mut chain = Chain::with_config(SEEDED, cfg);
    Grower::default().grow(&mut chain, 30);
    let before = records(&chain.state);

    // Removing block `lite - 1` is removing an index-only block.
    let e = chain.state.rewind_to(lite - 2).expect_err("below the line");
    assert_eq!(e.rule(), Some(&Rule::ReorganisationBelowBodyFloor { at_index: lite - 1, floor: lite }), "{e}");
    assert_same(&records(&chain.state), &before, "a refused rewind");

    // Down to the line itself only removes blocks that have bodies.
    assert_eq!(chain.state.rewind_to(lite - 1).unwrap(), TIP + 30 - (lite - 1));
}

/// A pruned node rewinds from its records even where the bodies are gone.
/// What does not come back is those bodies; re-applying the blocks prunes the
/// same ones again and ends exactly where it was.
#[test]
fn a_pruned_node_rewinds_past_its_window_from_its_records() {
    let depth = MIN_PRUNE_DEPTH;
    let cfg = Config { prune_depth: Some(depth), ..keeping_bodies(u32::MAX) };
    let mut chain = Chain::with_config(SEEDED, cfg);
    let mut grower = Grower::default();
    grower.grow(&mut chain, 20);
    let x = chain.tip();
    let at_x = records(&chain.state);
    grower.grow(&mut chain, depth + 30);
    let y = chain.tip();
    let at_y = records(&chain.state);

    assert_eq!(chain.state.rewind_to(x).unwrap(), depth + 30);
    let diffs = differences(&records(&chain.state), &at_x);
    let raw_bodies: Vec<&String> = diffs.iter().filter(|d| d.starts_with("missing Wr:")).collect();
    assert_eq!(
        raw_bodies.len(),
        diffs.len(),
        "only bodies the prune deleted may differ: {:?}",
        &diffs[..diffs.len().min(12)]
    );
    assert_eq!(raw_bodies.len(), 20, "the bodies of the 20 kept blocks were pruned on the way up");

    reapply(&mut chain, x + 1, y);
    assert_same(&records(&chain.state), &at_y, "after re-applying");
}

// ---------------------------------------------------------------------------
// real blocks from genesis
// ---------------------------------------------------------------------------

fn mainnet_blocks_1_to_5() -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
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

/// Mainnet blocks 1 to 5 on top of the genesis this state constructs: rewound
/// to 2 and to genesis, each record for record the state that height had.
#[test]
fn real_blocks_from_genesis_rewind_record_for_record() {
    let mut chain =
        ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
    chain.set_clock(Some(1_900_000_000));
    let mut snapshots = vec![records(&chain)];
    let blocks = mainnet_blocks_1_to_5();
    for (block, txs) in &blocks {
        chain.add_block(block, txs).expect("a mainnet block");
        snapshots.push(records(&chain));
    }
    assert_eq!(chain.tip_index(), Some(5));

    assert_eq!(chain.rewind_to(2).unwrap(), 3);
    assert_same(&records(&chain), &snapshots[2], "rewound to block 2");
    for (block, txs) in &blocks[2..] {
        chain.add_block(block, txs).expect("re-applied");
    }
    assert_same(&records(&chain), &snapshots[5], "re-applied to block 5");

    assert_eq!(chain.rewind_to(0).unwrap(), 5);
    assert_same(&records(&chain), &snapshots[0], "rewound to genesis");
    assert!(chain.store().map.contains_key(&keys::raw_block(0)), "genesis stays");
}
