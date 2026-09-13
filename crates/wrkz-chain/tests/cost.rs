// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! What a replayed block costs, measured on the mainnet vectors.
//!
//! Not an assertion of speed — a laptop is not a benchmark rig and this makes
//! no claim about any machine but the one it runs on. It exists so an operator
//! can put a number on "how long will the 4.2 million blocks take", and so the
//! two modes can be compared: the linear pass with the checkpoint zone on,
//! where no proof of work and no signature is ever computed, against a windowed
//! pass where all of them are.
//!
//! Ignored by default because it is a measurement, not a test. Run it with
//!
//! ```text
//! cargo test --release -p wrkz-chain --test cost -- --ignored --nocapture
//! ```

use serde_json::Value;
use std::path::PathBuf;
use std::time::Instant;
use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::tx::Transaction;
use wrkz_storage::records::RawBlockRecord;
use wrkz_storage::MemStore;

fn vectors() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

fn raw_blocks(file: &str) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(vectors().join(file)).unwrap()).unwrap();
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
                    .collect(),
            )
        })
        .collect()
}

/// Median of `runs` timings of `f`, in microseconds. The median rather than the
/// mean: one scheduler hiccup should not become the published number.
fn micros(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        f();
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

/// Median of `runs` timings of `f`, with `setup` run before each one and *not*
/// timed. Building a `ChainState` applies genesis — a hex decode, a transaction
/// parse and three keccaks — which has nothing to do with the cost of a block.
fn micros_after_setup<T>(runs: usize, mut setup: impl FnMut() -> T, mut f: impl FnMut(&mut T)) -> f64 {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let mut state = setup();
        let t = Instant::now();
        f(&mut state);
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

fn row(what: &str, us: f64) {
    println!("{what:<52} {us:>10.1} us   {:>10.0}/s", 1e6 / us.max(1e-9));
}

#[test]
#[ignore = "a measurement, not a test; run with --ignored --nocapture"]
fn per_block_cost() {
    println!("\n== per-block cost on this machine ==\n");

    // ---- the pieces every block pays, in both modes ----------------------
    let tip = &raw_blocks("mainnet_rawblocks_4213648_to_4213650_v7.json")[2];
    let tip_block = BlockTemplate::from_bytes(&tip.0).unwrap();
    assert_eq!(tip_block.coinbase_height(), Some(4_213_650));
    assert_eq!(tip.1.len(), 1, "the tip vector block carries one real transaction");

    // The C++ `4` record is a varint per byte, so decoding it is not free.
    let record = RawBlockRecord { block: tip.0.clone(), transactions: tip.1.clone() }.encode();
    row(
        "decode the C++ raw block record (4,213,650)",
        micros(2000, || {
            std::hint::black_box(RawBlockRecord::decode(&record).unwrap());
        }),
    );
    row(
        "parse the block blob",
        micros(2000, || {
            std::hint::black_box(BlockTemplate::from_bytes(&tip.0).unwrap());
        }),
    );
    row(
        "block id (tree hash + 3 keccaks)",
        micros(2000, || {
            std::hint::black_box(tip_block.hash().unwrap());
        }),
    );
    // `check_key` decompresses an ed25519 point, once per output of every
    // transaction and of every coinbase. It is the largest per-block cost a
    // linear pass pays, and it scales with the coinbase's output count: one at
    // the tip, where the reward is a single digit, but seven for block 1.
    let key = tip_block.base_transaction.prefix.outputs[0].key;
    row(
        "check_key (one output key, point decompression)",
        micros(2000, || {
            std::hint::black_box(wrkz_pow::curve::check_key(&key));
        }),
    );
    let tx = Transaction::from_bytes(&tip.1[0]).unwrap();
    row(
        "parse + hash one real transaction",
        micros(2000, || {
            let t = Transaction::from_bytes(&tip.1[0]).unwrap();
            std::hint::black_box(t.hash().unwrap());
        }),
    );

    // ---- what only a run with checkpoints off pays ------------------------
    println!();
    let v7_input = tip_block.pow_input().unwrap();
    row(
        "block proof of work: cn_upx (v7, from 1,000,001)",
        micros(50, || {
            std::hint::black_box(wrkz_pow::cn_upx(&v7_input));
        }),
    );
    let v1 = &raw_blocks("mainnet_rawblocks_0_to_5.json")[1];
    let v1_input = BlockTemplate::from_bytes(&v1.0).unwrap().pow_input().unwrap();
    row(
        "block proof of work: cn_slow_hash_v0 (v1-v3, 0-3)",
        micros(20, || {
            std::hint::black_box(wrkz_pow::cn_slow_hash_v0(&v1_input));
        }),
    );
    let v4 = &raw_blocks("mainnet_rawblocks_0_to_5.json")[4];
    let v4_input = BlockTemplate::from_bytes(&v4.0).unwrap().pow_input().unwrap();
    row(
        "block proof of work: cn_lite_slow_hash_v1 (v4, 4-302,400)",
        micros(20, || {
            std::hint::black_box(wrkz_pow::cn_lite_slow_hash_v1(&v4_input));
        }),
    );
    let v5_input =
        BlockTemplate::from_bytes(&raw_blocks("mainnet_rawblocks_302401_v5.json")[0].0).unwrap().pow_input().unwrap();
    row(
        "block proof of work: cn_turtle_lite_slow_hash_v2 (v5)",
        micros(50, || {
            std::hint::black_box(wrkz_pow::cn_turtle_lite_slow_hash_v2(&v5_input));
        }),
    );
    let v6_input =
        BlockTemplate::from_bytes(&raw_blocks("mainnet_rawblocks_600001_v6.json")[0].0).unwrap().pow_input().unwrap();
    row(
        "block proof of work: chukwa_slow_hash (v6)",
        micros(50, || {
            std::hint::black_box(wrkz_pow::chukwa_slow_hash(&v6_input));
        }),
    );

    let prefix = tx.prefix.to_bytes();
    row(
        "transaction proof of work: cn_upx over the prefix",
        micros(50, || {
            std::hint::black_box(wrkz_pow::cn_upx(&prefix));
        }),
    );

    // Ring verification cost is a function of the ring size, not of the keys,
    // so a synthetic ring measures the real thing.
    for ring_size in [2usize, 4, 8] {
        let (sec, pk) = wrkz_pow::curve::generate_keys();
        let mut ring = vec![pk];
        for _ in 1..ring_size {
            ring.push(wrkz_pow::curve::generate_keys().1);
        }
        let image = wrkz_pow::curve::generate_key_image(&pk, &sec);
        let prefix_hash = [7u8; 32];
        let sigs = wrkz_pow::curve::generate_ring_signature(&prefix_hash, &image, &ring, &sec, 0).unwrap();
        assert!(wrkz_pow::curve::check_ring_signature(&prefix_hash, &image, &ring, &sigs));
        row(
            &format!("verify one ring signature, ring size {ring_size}"),
            micros(500, || {
                std::hint::black_box(wrkz_pow::curve::check_ring_signature(&prefix_hash, &image, &ring, &sigs));
            }),
        );
    }

    // ---- end to end: apply blocks 1-5, both ways --------------------------
    //
    // Built outside the timed closure: parsing the 4,198-entry checkpoint table
    // is a one-off a real run pays once, and folding it into a five-block
    // measurement would triple the number.
    println!();
    let blocks = raw_blocks("mainnet_rawblocks_0_to_5.json");
    let on = Checkpoints::mainnet();
    let off = {
        let mut c = Checkpoints::mainnet();
        c.disable_from(Some(0));
        c
    };
    for (label, cp) in [("checkpoints on (the linear pass)", &on), ("checkpoints off", &off)] {
        let per_block = micros_after_setup(
            20,
            || {
                let mut chain = ChainState::open_or_genesis(
                    MemStore::default(),
                    Config { store_raw_blocks: false, ..Config::default() },
                    cp.clone(),
                )
                .unwrap();
                chain.set_clock(Some(1_800_000_000));
                chain
            },
            |chain| {
                for (blob, txs) in blocks.iter().skip(1) {
                    chain.add_block(blob, txs).unwrap();
                }
            },
        ) / 5.0;
        row(&format!("apply blocks 1-5 (v1..v4), {label}"), per_block);
    }
    row(
        "build the checkpoint table (once per run)",
        micros(50, || {
            std::hint::black_box(Checkpoints::mainnet());
        }),
    );

    println!(
        "\nA linear pass keeps checkpoints on below 4,188,000, so it never runs a proof of work or a\n\
         ring signature there. Its cost per block is the record decode, the parse, the block id, one\n\
         check_key per output, one parse+hash per transaction and about twenty state writes. Blocks\n\
         1-5 carry five to seven coinbase outputs each, so the end-to-end figure above is an upper\n\
         bound: from 1,500,000 the reward is a single digit and every coinbase has one output.\n\
         A windowed pass adds, per block, one block proof of work of the version's algorithm, and\n\
         per transaction one transaction proof of work plus one ring signature per key input.\n"
    );
}
