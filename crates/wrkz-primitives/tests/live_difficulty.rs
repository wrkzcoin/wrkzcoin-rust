// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! LWMA-2 (`nextDifficultyV5`) against the live chain: from the 63 consecutive
//! headers in spec/vectors/mainnet_headers_4213588_to_4213650.json, the port
//! must predict the recorded difficulty of blocks 4,213,649 and 4,213,650
//! exactly (spec/07 "Difficulty", spec/12 stage 3 replay in miniature).

use serde_json::Value;
use wrkz_primitives::constants::{block_major_version_for_index, difficulty_blocks_count};
use wrkz_primitives::difficulty::next_difficulty;

struct Header {
    height: u64,
    timestamp: u64,
    difficulty: u64,
}

fn headers() -> Vec<Header> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/vectors/mainnet_headers_4213588_to_4213650.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|h| Header {
            height: h["height"].as_u64().unwrap(),
            timestamp: h["timestamp"].as_u64().unwrap(),
            difficulty: h["difficulty"].as_u64().unwrap(),
        })
        .collect()
}

#[test]
fn lwma_v5_predicts_live_tip_difficulties() {
    let hs = headers();
    assert_eq!(hs.first().unwrap().height, 4213588);
    assert_eq!(hs.last().unwrap().height, 4213650);
    // cumulative difficulty with an arbitrary base: only differences matter
    let mut cum = Vec::with_capacity(hs.len());
    let mut acc: u64 = 123_456_789_000;
    for h in &hs {
        acc += h.difficulty;
        cum.push(acc);
    }
    let mut predicted = 0;
    for target in [4213649u64, 4213650] {
        let parent = target - 1;
        let n = difficulty_blocks_count(block_major_version_for_index(target), parent);
        assert_eq!(n, 61);
        let end = hs.iter().position(|h| h.height == parent).unwrap() + 1;
        let start = end - n;
        let ts: Vec<u64> = hs[start..end].iter().map(|h| h.timestamp).collect();
        let cd: Vec<u64> = cum[start..end].to_vec();
        let got = next_difficulty(block_major_version_for_index(target), parent, &ts, &cd)
            .expect("live window has a difficulty");
        let want = hs.iter().find(|h| h.height == target).unwrap().difficulty;
        assert_eq!(got, want, "difficulty of block {target}");
        predicted += 1;
    }
    assert_eq!(predicted, 2);
}
