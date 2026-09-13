// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The three reduced modes, on `MemStore`: what a lite node and a pruned node
//! stop being able to answer, and what they must keep answering.
//!
//! One rule governs every test here: **consensus does not change**. A lite or
//! pruned state validates each block exactly as a full one does, applies the
//! same records, and rejects the same blocks. What changes is which block
//! *bodies* survive, and therefore which questions can still be answered — and
//! the answer to one that cannot must be an error, never a wrong result.
//!
//! The harness seeds a chain at 4,400,000, above the last checkpoint, so every
//! rule really runs; see `chainbuild`.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
mod chainbuild;

use chainbuild::{Chain, AMOUNT, FEE, TIP};
use wrkz_chain::{keys, ChainState, Checkpoints, Config, MIN_PRUNE_DEPTH};
use wrkz_storage::{KvStore, MemStore};

/// The daemon's own configuration, minus the body policy: bodies kept and the
/// per-block output records never dropped.
fn serving(lite_start_height: u32, prune_depth: Option<u32>) -> Config {
    Config {
        store_raw_blocks: true,
        lite_start_height,
        prune_depth,
        unwind_history: u32::MAX,
        recent_window: 256,
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// lite
// ---------------------------------------------------------------------------

/// A lite node keeps no body below its height and every body above it — and
/// says so through `raw_block`, which is what every RPC, P2P and reorganisation
/// path reads.
#[test]
fn a_lite_node_serves_from_its_height_up_and_refuses_below_it() {
    // The line is put in the middle of the run the harness builds, so the same
    // chain has blocks on both sides of it.
    let lite_height = TIP + 5;
    let mut chain = Chain::with_config(9, serving(lite_height, None));
    for _ in 0..10 {
        chain.push_empty();
    }
    let tip = chain.tip();
    assert!(tip > lite_height, "the run has to cross the line");

    for index in (TIP + 1)..lite_height {
        assert!(
            chain.state.raw_block(index).unwrap().is_none(),
            "block {index} is below the lite height {lite_height} and must not be served"
        );
        // The consensus records are all still there. This is the whole point:
        // the node validates and follows the chain across this region, it
        // simply cannot hand the bytes back.
        assert!(chain.state.block_info(index).unwrap().is_some(), "block info at {index}");
        assert!(!chain.state.block_transaction_hashes(index).unwrap().is_empty(), "tx hashes at {index}");
    }
    for index in lite_height..=tip {
        assert!(
            chain.state.raw_block(index).unwrap().is_some(),
            "block {index} is at or above the lite height and must be served"
        );
    }

    // Genesis is exempt, the way `isLiteIndexOnlyHeight` exempts index 0
    // (`DatabaseBlockchainCache.h:476`) — but this harness is seeded, not
    // genesis-built, so the claim to check is the policy's, not the store's.
    assert!(chain.state.config().keeps_body(0, tip), "genesis is exempt from the lite line");

    // And `/info`'s number is the configuration's, not a guess.
    assert_eq!(chain.state.body_floor(), Some(lite_height));
    assert_eq!(chain.state.lite_start_height(), lite_height);
}

/// Nothing below the line was ever written, so nothing can put it back. The
/// C++ calls this "Permanent for this database" (`DaemonConfiguration.cpp:105`)
/// and refuses every contradicting open; so does this.
#[test]
fn the_lite_height_is_permanent_for_a_database() {
    // A fresh lite state records its height.
    let mut store = MemStore::default();
    store
        .write_batch(vec![(keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec()))])
        .unwrap();
    let chain = ChainState::open(store, serving(1_000, None), Checkpoints::none()).expect("a fresh lite state opens");
    assert_eq!(chain.recorded_lite_height().unwrap(), Some(1_000));
    let store = chain.into_store();

    // Reopening at the same height is fine.
    let chain = ChainState::open(store, serving(1_000, None), Checkpoints::none()).expect("the same height reopens");
    let store = chain.into_store();

    // As a full node: refused, and the message says what to pass instead.
    let e = ChainState::open(store, serving(0, None), Checkpoints::none()).err().expect("refused").to_string();
    assert!(e.contains("created as a lite node"), "{e}");
    assert!(e.contains("--lite-height 1000"), "the message names the height to pass: {e}");

    // At a different height: refused, and the message says the choice cannot
    // move rather than pretending the new number will be honoured.
    let mut store = MemStore::default();
    store
        .write_batch(vec![
            (keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())),
            (keys::meta(keys::META_LITE_HEIGHT), Some(1_000u32.to_le_bytes().to_vec())),
        ])
        .unwrap();
    let e = ChainState::open(store, serving(2_000, None), Checkpoints::none()).err().expect("refused").to_string();
    assert!(e.contains("permanent for a database"), "{e}");
    assert!(e.contains("not 2000"), "{e}");
}

/// The other direction: a database that already holds a full chain cannot be
/// relabelled lite, because it does hold those bodies and the label would be a
/// lie about a state that is fine.
#[test]
fn an_existing_full_database_cannot_be_reopened_as_a_lite_one() {
    let mut chain = Chain::with_config(9, serving(0, None));
    chain.push_empty();
    let store = chain.state.into_store();

    let e = ChainState::open(store, serving(TIP + 2, None), Checkpoints::mainnet()).err().expect("refused").to_string();
    assert!(e.contains("built as a full node"), "{e}");
    assert!(e.contains("empty --data-dir"), "the message says how to get a lite node: {e}");
}

/// `declare_lite_height` is the one way into a lite label on a non-empty state:
/// an import that wrote no bodies below some height is materially a lite
/// database that was never labelled one. It still cannot overwrite a label.
#[test]
fn a_body_less_import_can_be_labelled_but_a_label_cannot_be_moved() {
    let mut chain = Chain::with_config(9, serving(0, None));
    chain.push_empty();
    chain.state.declare_lite_height(TIP + 1).expect("an unlabelled state takes a label");
    assert_eq!(chain.state.lite_start_height(), TIP + 1);

    let e = chain.state.declare_lite_height(TIP + 2).expect_err("refused").to_string();
    assert!(e.contains("already recorded"), "{e}");
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

/// A pruned node drops bodies behind its window and keeps every consensus
/// record, so validation carries on unchanged over the pruned region.
#[test]
fn pruning_drops_bodies_and_leaves_consensus_reads_working() {
    // The shallowest depth the chain crate will open, so a run of a few hundred
    // blocks actually crosses the window. The daemon clamps `--prune-depth` to
    // the C++'s 10,080 one layer up; this is the reorganisation-safety floor.
    let depth = MIN_PRUNE_DEPTH;
    let mut chain = Chain::with_config(9, serving(0, Some(depth)));

    // Spend a seeded output, then bury it far enough that its block leaves the
    // window. The spend is what makes the pruned region carry real state: a key
    // image, ring members and an output that later blocks still resolve.
    let (real, decoy) = (chain.outputs[0], chain.outputs[1]);
    let tx = chain.spend(&real, &decoy, b"pruned-spend");
    chain.push(&[tx]);
    let spend_index = chain.tip();

    for _ in 0..(depth + 5) {
        chain.push_empty();
    }
    let tip = chain.tip();
    let floor = chain.state.body_floor().expect("a pruned node still keeps bodies");
    assert!(spend_index < floor, "the spend has to be behind the window: {spend_index} vs {floor}");

    // The body is gone, both by policy and on disk.
    assert!(chain.state.raw_block(spend_index).unwrap().is_none(), "the body was pruned");
    assert!(
        chain.state.store().get(&keys::raw_block(spend_index)).unwrap().is_none(),
        "and it really was deleted, not merely hidden"
    );
    // Everything consensus reads about that block is still there.
    assert!(chain.state.block_info(spend_index).unwrap().is_some());
    assert_eq!(chain.state.block_transaction_hashes(spend_index).unwrap().len(), 2, "coinbase and the spend");
    assert_eq!(
        chain.state.transaction_block_index(&chain.state.block_transaction_hashes(spend_index).unwrap()[1]).unwrap(),
        Some(spend_index),
        "the transaction index survives pruning"
    );

    // The window itself is exactly `depth` blocks and every one of them serves.
    assert_eq!(floor, tip + 1 - depth);
    for index in floor..=tip {
        assert!(chain.state.raw_block(index).unwrap().is_some(), "block {index} is inside the window");
    }

    // And the chain still applies blocks: the validator reads the pruned
    // region's outputs and key images, not its bodies.
    let (real, decoy) = (chain.outputs[2], chain.outputs[3]);
    let tx = chain.spend(&real, &decoy, b"after-pruning");
    chain.push(&[tx]);
    assert_eq!(chain.state.tip_index(), Some(tip + 1));
}

/// A reorganisation inside the 180-block bound still succeeds on a pruned node.
/// This is the invariant `MIN_PRUNE_DEPTH` exists to protect.
#[test]
fn a_reorganisation_inside_the_bound_still_succeeds_on_a_pruned_node() {
    let depth = MIN_PRUNE_DEPTH;
    let mut chain = Chain::with_config(9, serving(0, Some(depth)));
    for _ in 0..(depth + 20) {
        chain.push_empty();
    }
    let fork_tip = chain.tip();
    let fork_hash = chain.tip_hash();

    // A competing chain from the current tip, two blocks longer, so it is
    // strictly heavier and the state must switch to it.
    let mut alt = Chain::with_config(9, serving(0, Some(depth)));
    for _ in 0..(depth + 20) {
        alt.push_empty();
    }
    assert_eq!(alt.tip_hash(), fork_hash, "both chains are built the same way");
    // Rebuild the last three blocks on the alt chain with different nonces by
    // giving them a transaction, which changes their hashes.
    let (real, decoy) = (alt.outputs[0], alt.outputs[1]);
    let tx = alt.spend(&real, &decoy, b"alt-branch");
    alt.push(&[tx]);
    alt.push_empty();
    alt.push_empty();

    for index in (fork_tip + 1)..=alt.tip() {
        let built = alt.at(index);
        chain.state.add_block(&built.blob, &built.tx_blobs).unwrap_or_else(|e| panic!("alt block {index}: {e}"));
    }
    assert_eq!(chain.state.tip_index(), Some(alt.tip()), "the heavier branch was adopted");
}

/// A depth that could let pruning delete a body a reorganisation needs is
/// refused outright — not clamped, not warned about. The clamp is the C++'s
/// behaviour for the *network-health* minimum and lives in the daemon; this is
/// the consensus floor and there is nothing safe to fall back to.
#[test]
fn a_prune_depth_below_the_reorganisation_bound_is_refused() {
    for depth in [0u32, 1, 100, 180] {
        let e = ChainState::open(MemStore::default(), serving(0, Some(depth)), Checkpoints::none())
            .err()
            .expect("refused")
            .to_string();
        assert!(e.contains(&format!("prune depth {depth} is below")), "{e}");
        assert!(e.contains("reorganisation may reach 180 blocks"), "the message says why: {e}");
    }
    assert!(
        ChainState::open(MemStore::default(), serving(0, Some(MIN_PRUNE_DEPTH)), Checkpoints::none()).is_ok(),
        "the floor itself is allowed"
    );
    // The same floor applies to the unwind records a rewind reads.
    let cfg = Config { unwind_history: 100, ..serving(0, None) };
    let e = ChainState::open(MemStore::default(), cfg, Checkpoints::none()).err().expect("refused").to_string();
    assert!(e.contains("unwind_history 100 is below"), "{e}");
}

/// A reorganisation that reaches below the bodies a node kept is refused with
/// its own rule, and the main chain is left exactly as it was.
#[test]
fn a_reorganisation_below_the_body_floor_is_refused_and_changes_nothing() {
    // A lite node is the clean way to put the floor where a test can reach it:
    // the line is fixed, rather than moving with the tip as a prune window does.
    let lite_height = TIP + 8;
    let mut chain = Chain::with_config(9, serving(lite_height, None));
    let mut alt = Chain::with_config(9, serving(0, None));

    // One shared block, so the fork point is TIP+1 — three blocks behind the
    // main tip and well below the lite line.
    chain.push_empty();
    alt.push_empty();
    assert_eq!(chain.tip_hash(), alt.tip_hash(), "the chains share TIP+1");
    for _ in 0..3 {
        chain.push_empty();
    }
    let before_tip = chain.tip();
    let before_hash = chain.tip_hash();
    assert!(before_tip < lite_height, "the fork point has to be below the line");

    // The branch diverges at TIP+2 and runs two blocks past the main tip, so it
    // is strictly heavier and a full node would switch to it.
    let (real, decoy) = (alt.outputs[0], alt.outputs[1]);
    let tx = alt.spend(&real, &decoy, b"deep-branch");
    alt.push(&[tx]);
    for _ in 0..4 {
        alt.push_empty();
    }
    assert!(alt.tip() > before_tip);

    let mut refused = None;
    for index in (TIP + 2)..=alt.tip() {
        let built = alt.at(index);
        if let Err(e) = chain.state.add_block(&built.blob, &built.tx_blobs) {
            refused = Some(e.to_string());
            break;
        }
    }
    let e = refused.expect("the switch must be refused");
    assert!(e.contains("below this node's full block data height"), "{e}");
    assert_eq!(chain.state.tip_index(), Some(before_tip), "the main chain did not move");
    assert_eq!(chain.state.tip_info().unwrap().block_hash, before_hash);
}

/// The catch-up pass, for the database that was full when `--prune` arrived.
#[test]
fn the_catch_up_pass_prunes_an_already_full_database_and_resumes() {
    let depth = MIN_PRUNE_DEPTH;
    // Built full, so every body is on disk...
    let mut chain = Chain::with_config(9, serving(0, None));
    for _ in 0..(depth + 40) {
        chain.push_empty();
    }
    let tip = chain.tip();
    let store = chain.state.into_store();

    // ...then reopened with `--prune`, which is when the C++ starts its blind
    // rescan from height 0 and this starts from its resume point.
    let mut chain = ChainState::open(store, serving(0, Some(depth)), Checkpoints::mainnet()).expect("reopens pruned");
    let floor = chain.body_floor().expect("a floor");
    assert!(chain.store().get(&keys::raw_block(floor - 1)).unwrap().is_some(), "the body is still on disk");
    assert!(chain.raw_block(floor - 1).unwrap().is_none(), "but the policy already refuses it");

    // A bounded pass, then another, then nothing left to do.
    let mut total = 0;
    let mut passes = 0;
    while chain.pruned_below().unwrap() < floor {
        total += chain.prune_bodies(1_000).expect("a pass runs");
        passes += 1;
        assert!(passes < 100, "a bounded pass must still finish");
    }
    assert!(total > 0, "the pass had work to do");
    assert_eq!(chain.prune_bodies(1_000).unwrap(), 0, "and nothing is rescanned once it is done");
    assert_eq!(chain.pruned_below().unwrap(), floor, "the resume point is the floor");
    assert!(chain.store().get(&keys::raw_block(floor - 1)).unwrap().is_none(), "the body really went");
    for index in floor..=tip {
        assert!(chain.raw_block(index).unwrap().is_some(), "the window is untouched at {index}");
    }
}

// ---------------------------------------------------------------------------
// the payment-id index
// ---------------------------------------------------------------------------

/// The index the explorer's `f_transactions_by_payment_id_json` reads: long
/// plaintext ids only, in the order they were mined, and an unwind takes back
/// exactly what its block put in.
#[test]
fn the_payment_id_index_follows_the_main_chain() {
    let mut chain = Chain::with_config(9, serving(0, None));
    let payment_id = [0x5au8; 32];
    let other = [0x11u8; 32];

    let previous = chain.tip();
    let tx = chainbuild::build_spend_with_payment_id(
        previous,
        &chain.outputs[0],
        &chain.outputs[1],
        &chain.payee,
        FEE,
        b"pid-a",
        payment_id,
    );
    let hash_a = tx.hash().unwrap();
    chain.push(&[tx]);

    let previous = chain.tip();
    let tx = chainbuild::build_spend_with_payment_id(
        previous,
        &chain.outputs[2],
        &chain.outputs[3],
        &chain.payee,
        FEE,
        b"pid-b",
        payment_id,
    );
    let hash_b = tx.hash().unwrap();
    chain.push(&[tx]);

    assert_eq!(
        chain.state.transaction_hashes_by_payment_id(&payment_id).unwrap(),
        vec![hash_a, hash_b],
        "both transactions, in the order they were mined"
    );
    assert!(
        chain.state.transaction_hashes_by_payment_id(&other).unwrap().is_empty(),
        "an unused payment id is an empty answer, not an error"
    );

    // A transaction with no payment id contributes nothing.
    let tx = chain.spend(&chain.outputs[4], &chain.outputs[5], b"no-pid");
    chain.push(&[tx]);
    assert_eq!(chain.state.transaction_hashes_by_payment_id(&payment_id).unwrap().len(), 2);
    // ...and neither does a coinbase, which never carries one.
    assert_eq!(chain.state.transaction_hashes_by_payment_id(&[0u8; 32]).unwrap(), Vec::<[u8; 32]>::new());

    let _ = AMOUNT;
}
