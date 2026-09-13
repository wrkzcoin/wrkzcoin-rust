// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The difficulty algorithm switch at parent index 100,000, against the live
//! chain (spec/07 "Difficulty").
//!
//! This is the boundary a windowed replay of the real database caught: the port
//! predicted 14,866,321 for block 100,001 where the chain records 14,767,992.
//! Two things have to be right for this test to pass, and both look wrong until
//! the C++ is read carefully:
//!
//! 1. the algorithm for parent 100,000 is `nextDifficultyV4`, not
//!    `nextDifficultyV3`, because `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` is defined
//!    as `LWMA_2_DIFFICULTY_BLOCK_INDEX` and the V4 arm shadows the V3 arm;
//! 2. `nextDifficultyV4` has **no upper clamp** on a solvetime, because its
//!    `clamp(-6 * T, ST, 6 * T)` passes the arguments in the wrong order for
//!    `clamp(n, lower, upper)`. Block 100,001's window contains an interval of
//!    414 seconds, above `6 * T = 360`, so a real clamp changes the result.

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
        .join("../../spec/vectors/mainnet_headers_99939_to_100001.json");
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
fn lwma_predicts_the_first_block_after_the_switch() {
    let hs = headers();
    assert_eq!(hs.first().unwrap().height, 99939);
    assert_eq!(hs.last().unwrap().height, 100_001);

    // Cumulative difficulty with an arbitrary base: only differences matter.
    let mut cum = Vec::with_capacity(hs.len());
    let mut acc: u64 = 987_654_321_000;
    for h in &hs {
        acc += h.difficulty;
        cum.push(acc);
    }

    let target = 100_001u64;
    let parent = target - 1;
    let version = block_major_version_for_index(target);
    let n = difficulty_blocks_count(version, parent);
    assert_eq!(n, 61, "the V3 window applies from parent index 100,000");

    let end = hs.iter().position(|h| h.height == parent).unwrap() + 1;
    let start = end - n;
    let ts: Vec<u64> = hs[start..end].iter().map(|h| h.timestamp).collect();
    let cd: Vec<u64> = cum[start..end].to_vec();

    // The window really does contain a solvetime above 6 * T, which is what
    // makes this block able to tell a clamped implementation from the C++.
    let over: Vec<i64> = ts.windows(2).map(|w| w[1] as i64 - w[0] as i64).filter(|d| *d > 6 * 60).collect();
    assert_eq!(over, vec![414], "block 100,001's window has one 414 second interval");

    let got = next_difficulty(version, parent, &ts, &cd).expect("live window has a difficulty");
    let want = hs.iter().find(|h| h.height == target).unwrap().difficulty;
    assert_eq!(got, want, "difficulty of block {target}");
    assert_eq!(want, 14_767_992);
}
