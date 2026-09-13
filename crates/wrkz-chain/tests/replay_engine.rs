// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The replay engine against a C++-layout database, on `MemStore`.
//!
//! `wrkz-replay` needs RocksDB and a real node's database, which only the Linux
//! host has. The engine underneath it does not: this builds the records the C++
//! node would have written for blocks 0–5 (spec/11 "Keys and values"), replays
//! them into our state and checks what the replay is there to check — the block
//! hash, the cumulative difficulty, the already-generated coins, the cumulative
//! block size, the transaction count and the timestamp — including what happens
//! when one of them disagrees, when a block is rejected, and when the run is
//! resumed.

use serde_json::Value;
use std::path::PathBuf;
use wrkz_chain::replay::{replay, replay_windows, ReplayOptions};
use wrkz_chain::Window;
use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_primitives::block::BlockTemplate;
use wrkz_storage::codec::{self, KeyPart};
use wrkz_storage::reader::ChainReader;
use wrkz_storage::records::{CachedBlockInfo, RawBlockRecord};
use wrkz_storage::{KvStore, MemStore};

/// `(difficulty, reward)` of blocks 1–5, from the live headers of spec/09.
const BLOCKS_1_TO_5: [(u64, u64); 5] =
    [(1, 11_563_301), (1, 11_563_298), (60, 11_563_295), (3660, 11_563_292), (24806, 11_563_290)];

fn raw_blocks() -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/mainnet_rawblocks_0_to_5.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                hex::decode(item["block"].as_str().unwrap()).unwrap(),
                item["transactions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| hex::decode(t.as_str().unwrap()).unwrap())
                    .collect::<Vec<Vec<u8>>>(),
            )
        })
        .collect()
}

/// The `4`, `6`, `5` and `8` records a C++ node holds for blocks 0–5.
fn cpp_database() -> MemStore {
    let mut store = MemStore::default();
    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    ops.push((codec::DB_VERSION_KEY.to_vec(), Some(b"4".to_vec())));

    let mut cumulative_difficulty = 1u64;
    let mut coins = 1_500_000_000_000u64;
    let mut transactions = 1u64;
    for (index, (blob, txs)) in raw_blocks().into_iter().enumerate() {
        let block = BlockTemplate::from_bytes(&blob).unwrap();
        let hash = block.hash().unwrap();
        if index > 0 {
            let (difficulty, reward) = BLOCKS_1_TO_5[index - 1];
            cumulative_difficulty += difficulty;
            // Every one of these blocks is far inside the penalty-free zone and
            // carries no transactions, so the emission grows by the reward.
            coins += reward;
            transactions += 1;
        }
        let coinbase_size = block.base_transaction.to_bytes().unwrap().len() as u32;
        let info = CachedBlockInfo {
            block_hash: hash,
            timestamp: block.timestamp,
            block_size: coinbase_size + txs.iter().map(|t| t.len() as u32).sum::<u32>(),
            cumulative_difficulty,
            already_generated_coins: coins,
            already_generated_transactions: transactions,
        };
        let i = index as u32;
        ops.push((codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(i)), Some(info.encode())));
        ops.push((codec::key(codec::BLOCK_HASH_TO_BLOCK_INDEX, KeyPart::Hash(hash)), Some(codec::value_u32("5", i))));
        ops.push((
            codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(i)),
            Some(RawBlockRecord { block: blob, transactions: txs }.encode()),
        ));
    }
    ops.push((
        codec::key(codec::BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(codec::LAST_BLOCK_INDEX_KEY)),
        Some(codec::value_u32("8", 5)),
    ));
    store.write_batch(ops).unwrap();
    store
}

fn our_state(store: MemStore) -> ChainState<MemStore> {
    // `store_raw_blocks` off is what the replay tool defaults to: the source
    // database already has the bodies.
    let cfg = Config { store_raw_blocks: false, ..Config::default() };
    let mut chain = ChainState::open_or_genesis(store, cfg, Checkpoints::mainnet()).unwrap();
    chain.set_clock(Some(1_800_000_000));
    chain
}

fn quiet() -> impl FnMut(&str) {
    |_: &str| {}
}

#[test]
fn the_engine_replays_a_cpp_database_and_agrees_with_its_records() {
    let source = ChainReader::new(cpp_database());
    let mut chain = our_state(MemStore::default());
    let mut lines = Vec::new();
    let report = replay(&source, &mut chain, &ReplayOptions { progress: 2, ..Default::default() }, &mut |l| {
        lines.push(l.to_string())
    })
    .expect("the replay agrees with the C++ records");
    assert_eq!(report.top, 5);
    assert_eq!(report.applied, 5);
    assert_eq!(report.source_top, 5);
    assert_eq!(
        hex::encode(chain.tip_info().unwrap().block_hash),
        "513e3cbe87ff9ca63ee30197ac358de63ac30216b68359231917c1e10169e1cb"
    );
    assert_eq!(chain.tip_info().unwrap().cumulative_difficulty, 1 + 1 + 1 + 60 + 3660 + 24806);
    // A progress line every two blocks plus the schema, the range and the summary.
    assert!(lines.iter().any(|l| l.contains("blocks/s")), "progress lines: {lines:?}");
    assert!(lines.iter().any(|l| l.contains("schema version 4")));
}

#[test]
fn the_replay_resumes_from_the_applied_height() {
    let source = ChainReader::new(cpp_database());
    let mut store = MemStore::default();

    // First run: stop at 2.
    {
        let mut chain = our_state(std::mem::take(&mut store));
        let report = replay(&source, &mut chain, &ReplayOptions { to: Some(2), ..Default::default() }, &mut quiet())
            .expect("first half");
        assert_eq!((report.top, report.applied), (2, 2));
        store = chain.into_store();
    }
    // Second run on the same state: continues at 3, not at 1.
    {
        let mut chain = our_state(std::mem::take(&mut store));
        assert_eq!(chain.tip_index(), Some(2), "the applied height survived the reopen");
        let report = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).expect("second half");
        assert_eq!((report.top, report.applied), (5, 3));
        store = chain.into_store();
    }
    // A third run has nothing to do and says so rather than failing.
    {
        let mut chain = our_state(store);
        let report = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap();
        assert_eq!((report.top, report.applied), (5, 0));
    }
}

#[test]
fn a_from_above_the_applied_height_is_refused() {
    let source = ChainReader::new(cpp_database());
    let mut chain = our_state(MemStore::default());
    let e =
        replay(&source, &mut chain, &ReplayOptions { from: Some(3), ..Default::default() }, &mut quiet()).unwrap_err();
    assert!(e.contains("cannot skip blocks"), "{e}");
}

/// The point of the cross-check: if our arithmetic and the C++ record ever
/// disagree, the replay must say which block and which value.
#[test]
fn a_disagreeing_cpp_record_stops_the_replay_and_names_the_block() {
    for (field, mangle) in [
        ("cumulative difficulty", 0usize),
        ("already-generated coins", 1),
        ("block hash", 2),
        ("cumulative block size", 3),
        ("transaction count", 4),
        ("timestamp", 5),
    ] {
        let mut store = cpp_database();
        let key = codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(3));
        let mut info = CachedBlockInfo::decode(&store.get(&key).unwrap().unwrap()).unwrap();
        match mangle {
            0 => info.cumulative_difficulty += 1,
            1 => info.already_generated_coins += 1,
            2 => info.block_hash[0] ^= 1,
            3 => info.block_size += 1,
            4 => info.already_generated_transactions += 1,
            _ => info.timestamp += 1,
        }
        store.put(key, info.encode()).unwrap();

        let source = ChainReader::new(store);
        let mut chain = our_state(MemStore::default());
        let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
        assert!(e.starts_with("block 3"), "{field}: {e}");
        assert!(e.contains(field), "expected the message to name the {field}: {e}");
        // Everything below the failure is applied and the run is resumable.
        assert_eq!(chain.tip_index(), Some(3), "{field}");
    }
}

/// A rejected block names the index, the hash and the rule.
#[test]
fn a_rejected_block_names_the_index_the_hash_and_the_rule() {
    let mut store = cpp_database();
    // Rewrite block 3's raw record with block 4's body: its `prev_id` no longer
    // points at block 2, so it is an orphan at that index.
    let blocks = raw_blocks();
    store
        .put(
            codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(3)),
            RawBlockRecord { block: blocks[4].0.clone(), transactions: vec![] }.encode(),
        )
        .unwrap();

    let source = ChainReader::new(store);
    let mut chain = our_state(MemStore::default());
    let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(e.starts_with("block 3 ("), "{e}");
    assert!(e.contains("bc9ecbdcde0fc6ca467025af49ba239e49148702af9503bce8627714f6974a31"), "{e}");
    assert!(e.contains("REJECTED_AS_ORPHANED"), "{e}");
    assert_eq!(chain.tip_index(), Some(2));
}

/// The genesis record is compared too, which is what proves our
/// `apply_genesis` against `DatabaseBlockchainCache::addGenesisBlock` — nothing
/// in the block loop would ever look at index 0.
#[test]
fn a_disagreeing_genesis_record_stops_the_replay_before_the_first_block() {
    let mut store = cpp_database();
    let key = codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(0));
    let mut info = CachedBlockInfo::decode(&store.get(&key).unwrap().unwrap()).unwrap();
    // The C++ stores the *coinbase* size (157) for genesis, from an aggregate
    // initializer whose field order is the declaration order.
    assert_eq!(info.block_size, 157);
    info.block_size = 197;
    store.put(key, info.encode()).unwrap();

    let source = ChainReader::new(store);
    let mut chain = our_state(MemStore::default());
    let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(e.starts_with("block 0"), "{e}");
    assert!(e.contains("cumulative block size"), "{e}");
    assert_eq!(chain.tip_index(), Some(0), "nothing was applied");
}

/// A database with no schema version is not a WrkzCoin database.
#[test]
fn a_foreign_database_is_refused() {
    let mut store = cpp_database();
    store.delete(codec::DB_VERSION_KEY.to_vec()).unwrap();
    let source = ChainReader::new(store);
    let mut chain = our_state(MemStore::default());
    let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(e.contains("db_scheme_version"), "{e}");
}

// ---------------------------------------------------------------------------
// windowed replay
// ---------------------------------------------------------------------------

/// A windowed run validates only its windows, and takes everything below them
/// from the source database.
///
/// The proof that only the window is validated: block 2's raw record is
/// replaced with garbage. A linear run trips over it; a windowed run over
/// `4..=5` never reads it, because block 3's state comes from the source's `6`
/// records and not from replaying 1, 2 and 3.
#[test]
fn a_windowed_run_validates_only_its_windows_and_seeds_the_rest() {
    // A source whose block 2 body is garbage.
    let broken = || {
        let mut store = cpp_database();
        store
            .put(
                codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(2)),
                RawBlockRecord { block: vec![0xff; 8], transactions: vec![] }.encode(),
            )
            .unwrap();
        store
    };

    // Linear: stops at block 2.
    {
        let source = ChainReader::new(broken());
        let mut chain = our_state(MemStore::default());
        let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
        assert!(e.starts_with("block 2"), "{e}");
    }

    // Windowed over 4..=5: block 2 is never read.
    let source = ChainReader::new(broken());
    let mut chain = our_state(MemStore::default());
    let mut lines = Vec::new();
    let report =
        replay_windows(&source, &mut chain, &[Window { start: 4, end: 5 }], &ReplayOptions::default(), &mut |l| {
            lines.push(l.to_string())
        })
        .expect("the window does not touch block 2");

    assert_eq!(report.windows.len(), 1);
    assert_eq!(report.blocks(), 2);
    assert_eq!(report.transactions(), 0, "blocks 4 and 5 carry no transactions");
    assert_eq!(report.rings(), 0);
    assert_eq!(report.source_top, 5);
    assert_eq!(chain.tip_index(), Some(5));

    // Block 3 is in our state because it was *seeded*, and its record is the
    // source's, byte for byte.
    let seeded = chain.block_info(3).unwrap().unwrap();
    let theirs = source.block_info(3).unwrap().unwrap();
    assert_eq!(seeded.block_hash, theirs.block_hash);
    assert_eq!(seeded.cumulative_difficulty, theirs.cumulative_difficulty);
    assert_eq!(seeded.already_generated_coins, theirs.already_generated_coins);
    // And the difficulty of block 4 was derived from that seeded window, not
    // taken from anywhere: 3660, the value the live header records.
    assert!(lines.iter().any(|l| l.contains("window 4..=5")), "{lines:?}");
    let tip = chain.tip_info().unwrap();
    assert_eq!(tip.cumulative_difficulty, source.block_info(5).unwrap().unwrap().cumulative_difficulty);

    // The garbage record is still garbage: the window really did skip it.
    assert_eq!(
        source.store.get(&codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(2))).unwrap().unwrap(),
        RawBlockRecord { block: vec![0xff; 8], transactions: vec![] }.encode()
    );
}

/// A window whose blocks are wrong still fails, and names the block: the state
/// rules run inside a window, they are not skipped along with the seeding.
#[test]
fn a_windowed_run_still_validates_the_blocks_inside_the_window() {
    let mut store = cpp_database();
    // One atomic unit too much in block 5's coinbase. The reward rule computes
    // the same 11,563,290 it always did, from the emission our state seeded out
    // of the source, and rejects the block.
    let blocks = raw_blocks();
    let mut block = BlockTemplate::from_bytes(&blocks[5].0).unwrap();
    block.base_transaction.prefix.outputs[0].amount += 1;
    store
        .put(
            codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(5)),
            RawBlockRecord { block: block.to_bytes().unwrap(), transactions: vec![] }.encode(),
        )
        .unwrap();
    let source = ChainReader::new(store);
    let mut chain = our_state(MemStore::default());
    let e =
        replay_windows(&source, &mut chain, &[Window { start: 4, end: 5 }], &ReplayOptions::default(), &mut quiet())
            .unwrap_err();
    assert!(e.starts_with("block 5 ("), "{e}");
    assert!(e.contains("BLOCK_REWARD_MISMATCH"), "{e}");
    assert!(e.contains("expected 11563290"), "the rule computed the real reward from the seeded emission: {e}");
    // Block 4 was applied first, so the window really did run.
    assert_eq!(chain.tip_index(), Some(4));
}

/// Checkpoints are off inside a window, so the proof of work of every block in
/// it is computed and checked — which a linear run with checkpoints on skips.
#[test]
fn a_window_verifies_proof_of_work_that_the_checkpoint_zone_would_skip() {
    // Blocks 0-5 are all inside the mainnet checkpoint zone (it ends at
    // 4,188,000), so a linear run never hashes them.
    assert!(Checkpoints::mainnet().is_in_checkpoint_zone(5));

    let mut store = cpp_database();
    // Break block 4's nonce. The block id changes with it, so the C++ `6` and
    // `5` records no longer match either; what matters is that the run stops.
    let blocks = raw_blocks();
    let mut block = BlockTemplate::from_bytes(&blocks[4].0).unwrap();
    block.nonce ^= 1;
    store
        .put(
            codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(4)),
            RawBlockRecord { block: block.to_bytes().unwrap(), transactions: vec![] }.encode(),
        )
        .unwrap();

    let source = ChainReader::new(store);
    let mut chain = our_state(MemStore::default());
    let e =
        replay_windows(&source, &mut chain, &[Window { start: 4, end: 5 }], &ReplayOptions::default(), &mut quiet())
            .unwrap_err();
    assert!(e.starts_with("block 4 ("), "{e}");
    assert!(e.contains("PROOF_OF_WORK_TOO_WEAK"), "a window must actually check the work: {e}");
}

/// The window list the `--windows forks` mode uses, clamped to a short chain.
#[test]
fn the_fork_windows_of_a_short_chain_are_replayable() {
    let source = ChainReader::new(cpp_database());
    let mut chain = our_state(MemStore::default());
    // Windows of 2 around the rule changes at 1, 2 and 3 merge into 1..=5 on a
    // five-block chain.
    let windows = wrkz_chain::windows::fork_windows(2, 5);
    assert_eq!(windows, vec![Window { start: 1, end: 5 }]);
    let report =
        replay_windows(&source, &mut chain, &windows, &ReplayOptions::default(), &mut quiet()).expect("replays");
    assert_eq!(report.blocks(), 5);
    assert_eq!(chain.tip_index(), Some(5));
}

/// A linear state and a windowed state must not be mixed: a windowed run seeds
/// block infos it never validated.
#[test]
fn the_two_modes_refuse_to_share_a_state_directory() {
    let source = ChainReader::new(cpp_database());

    // Linear first, then windowed on the same store.
    let mut store = MemStore::default();
    {
        let mut chain = our_state(std::mem::take(&mut store));
        replay(&source, &mut chain, &ReplayOptions { to: Some(2), ..Default::default() }, &mut quiet()).unwrap();
        assert_eq!(chain.tag().unwrap().as_deref(), Some("linear"));
        store = chain.into_store();
    }
    {
        let mut chain = our_state(std::mem::take(&mut store));
        let e = replay_windows(
            &source,
            &mut chain,
            &[Window { start: 4, end: 5 }],
            &ReplayOptions::default(),
            &mut quiet(),
        )
        .unwrap_err();
        assert!(e.contains("`linear` replay"), "{e}");
        assert!(e.contains("different directory"), "{e}");
        store = chain.into_store();
    }
    // And the linear run can still continue on its own directory.
    {
        let mut chain = our_state(store);
        let report = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap();
        assert_eq!(report.top, 5);
    }

    // Windowed first, then linear.
    let mut store = MemStore::default();
    {
        let mut chain = our_state(std::mem::take(&mut store));
        replay_windows(&source, &mut chain, &[Window { start: 4, end: 5 }], &ReplayOptions::default(), &mut quiet())
            .unwrap();
        assert_eq!(chain.tag().unwrap().as_deref(), Some("windows"));
        store = chain.into_store();
    }
    {
        let mut chain = our_state(store);
        let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
        assert!(e.contains("`windows` replay"), "{e}");
    }
}

/// A state that holds a chain but no mode tag — one written before mode tags
/// existed — is refused rather than reinterpreted.
#[test]
fn an_untagged_state_that_holds_a_chain_is_refused() {
    let source = ChainReader::new(cpp_database());
    let mut store = MemStore::default();
    {
        let mut chain = our_state(std::mem::take(&mut store));
        replay(&source, &mut chain, &ReplayOptions { to: Some(3), ..Default::default() }, &mut quiet()).unwrap();
        store = chain.into_store();
    }
    // Strip the tag, as an older build would have left it.
    store.delete(wrkz_chain::keys::meta(wrkz_chain::keys::META_TAG)).unwrap();
    let mut chain = our_state(store);
    let e = replay(&source, &mut chain, &ReplayOptions::default(), &mut quiet()).unwrap_err();
    assert!(e.contains("no mode tag"), "{e}");
}
