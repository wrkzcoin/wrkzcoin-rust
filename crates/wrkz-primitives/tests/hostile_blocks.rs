// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Hostile and negative-path block inputs (spec/04-serialization.md,
//! spec/07-blocks-consensus.md "Proof of work check", "Parent block rules").
//!
//! Every case here either used to panic — a wire length added to a cursor —
//! or is a rule that the passing vectors in `mainnet_blocks.rs` cannot show,
//! because a block that breaks it is one no honest producer emits.

use serde_json::Value;
use std::path::PathBuf;
use wrkz_primitives::block::{BlockTemplate, BLOCK_MAJOR_VERSION_2};

fn vectors() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

/// The raw block blobs of one vector file, in file order.
fn blobs(file: &str) -> Vec<Vec<u8>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(vectors().join(file)).unwrap()).unwrap();
    v["items"].as_array().unwrap().iter().map(|i| hex::decode(i["block"].as_str().unwrap()).unwrap()).collect()
}

/// The smallest v7 block blob that reaches `parse_extra` through
/// `ParentBlock::read`, carrying a merge-mining tag whose length varint is
/// `u64::MAX`.
///
/// 90 bytes is the floor: 34 for the block header, 40 for the parent-block
/// header up to `numberOfTransactions`, 5 for an empty version-0 coinbase and
/// its `extra` length, and 11 for the tag byte plus the ten-byte varint. This
/// blob used to overflow `i + len` inside `parse_extra`, which panicked in a
/// debug build and wrapped to an in-range slice index in a release one.
fn merge_mining_length_bomb() -> Vec<u8> {
    let mut extra = vec![0x03u8];
    extra.extend(wrkz_primitives::varint::encode(u64::MAX));
    let mut blob: Vec<u8> = vec![0x07, 0x00];
    blob.extend([0u8; 32]); // previous block hash
    blob.extend([0x00u8, 0x00, 0x00]); // parent major, parent minor, timestamp
    blob.extend([0u8; 32]); // parent previous block hash
    blob.extend([0u8; 4]); // nonce
    blob.push(0x01); // numberOfTransactions, branch depth 0
    blob.extend([0x00u8, 0x00, 0x00, 0x00]); // coinbase: version, unlock, 0 in, 0 out
    blob.push(extra.len() as u8);
    blob.extend(&extra);
    blob
}

#[test]
fn a_merge_mining_length_of_u64_max_is_an_error_not_a_panic() {
    let blob = merge_mining_length_bomb();
    assert_eq!(blob.len(), 90);
    assert!(BlockTemplate::from_bytes(&blob).is_err());
    // and so is every prefix and every one-byte extension of it
    for n in 0..=blob.len() {
        assert!(BlockTemplate::from_bytes(&blob[..n]).is_err(), "prefix of {n} bytes");
    }
    for b in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
        let mut longer = blob.clone();
        longer.push(b);
        assert!(BlockTemplate::from_bytes(&longer).is_err());
    }
}

#[test]
fn hostile_counts_and_lengths_are_errors_not_allocations() {
    let max = wrkz_primitives::varint::encode(u64::MAX);
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for major in [0x01u8, 0x02, 0x07, 0x08, 0xff] {
        // a header followed by a u64::MAX count in every position that takes one
        for tail in [&[][..], &max[..]] {
            let mut b = vec![major, 0x00];
            b.extend([0u8; 32]);
            b.extend(tail);
            cases.push(b);
        }
    }
    // v1 block whose transaction-hash count is u64::MAX
    let mut b = vec![0x01u8, 0x00, 0x00];
    b.extend([0u8; 32]); // prev
    b.extend([0u8; 4]); // nonce
    b.extend([0x01u8, 0x00, 0x00, 0x00, 0x00]); // coinbase v1, unlock 0, 0 in, 0 out, empty extra
    b.extend(&max); // tx_hashes count
    cases.push(b);
    // parent block claiming 65535 transactions (16 branch hashes) with nothing after
    let mut b = vec![0x02u8, 0x00];
    b.extend([0u8; 32]);
    b.extend([0x00u8, 0x00, 0x00]);
    b.extend([0u8; 32]);
    b.extend([0u8; 4]);
    b.extend([0xffu8, 0xff, 0x03]); // varint 65535
    cases.push(b);
    for c in &cases {
        assert!(BlockTemplate::from_bytes(c).is_err(), "{} must not parse", hex::encode(c));
    }
}

#[test]
fn every_prefix_of_every_mainnet_block_is_an_error() {
    for file in [
        "mainnet_rawblocks_0_to_5.json",
        "mainnet_rawblocks_600001_v6.json",
        "mainnet_rawblocks_4213648_to_4213650_v7.json",
    ] {
        for blob in blobs(file) {
            assert!(BlockTemplate::from_bytes(&blob).is_ok());
            for n in 0..blob.len() {
                assert!(BlockTemplate::from_bytes(&blob[..n]).is_err(), "{file}: prefix of {n} bytes parsed");
            }
        }
    }
}

#[test]
fn a_block_that_does_not_match_the_merge_mining_commitment_fails_the_pow_check() {
    // Block 4,213,650, difficulty from the live header in
    // spec/vectors/mainnet_headers_4213588_to_4213650.json.
    let blob = blobs("mainnet_rawblocks_4213648_to_4213650_v7.json").pop().unwrap();
    let good = BlockTemplate::from_bytes(&blob).unwrap();
    let headers: Value = serde_json::from_str(
        &std::fs::read_to_string(vectors().join("mainnet_headers_4213588_to_4213650.json")).unwrap(),
    )
    .unwrap();
    let index = good.coinbase_height().unwrap();
    let difficulty = headers
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["height"].as_u64() == Some(index))
        .and_then(|h| h["difficulty"].as_u64())
        .expect("live header for this block");
    assert!(good.check_proof_of_work(difficulty).unwrap(), "the real block passes");

    // `previousBlockHash` is in the block's own hashing blob, which the
    // merge-mining tag commits to, but not in the parent-block blob the proof
    // of work hashes. Changing it therefore leaves the PoW hash alone and
    // breaks only the commitment — exactly the case that stops one parent
    // block from being replayed under a different auxiliary header.
    let mut forged = good.clone();
    forged.previous_block_hash[0] ^= 0x01;
    assert_eq!(forged.pow_hash().unwrap(), good.pow_hash().unwrap(), "the PoW input is unchanged");
    assert_ne!(forged.auxiliary_header_hash().unwrap(), good.auxiliary_header_hash().unwrap());
    assert!(!forged.check_proof_of_work(difficulty).unwrap(), "the merge-mining root no longer matches");

    // The same for a changed transaction list, which is also inside the
    // auxiliary header hash only.
    let mut forged = good.clone();
    forged.transaction_hashes.push([0x5a; 32]);
    assert!(!forged.check_proof_of_work(difficulty).unwrap());

    // And a block that misses the difficulty target fails first, whatever the
    // commitment says.
    assert!(!good.check_proof_of_work(difficulty.saturating_mul(1 << 20)).unwrap());
}

#[test]
fn a_v2_block_may_not_carry_a_parent_major_version_above_one() {
    // `Core::validateBlock` (`Core.cpp:2714`) applies this only to v2 blocks.
    // Parent major 0 is what the daemon's own templates carry (`Core.cpp:2365`
    // assigns `BLOCK_MINOR_VERSION_0` over the line above it), so 0 and 1 pass
    // and 2 does not.
    let blob = blobs("mainnet_rawblocks_0_to_5.json").into_iter().nth(2).unwrap();
    let block = BlockTemplate::from_bytes(&blob).unwrap();
    assert_eq!(block.major_version, BLOCK_MAJOR_VERSION_2);
    assert_eq!(block.parent_block.as_ref().unwrap().major_version, 0, "block 2 is a daemon template");
    assert!(block.validate_parent_block().unwrap());

    for parent_major in [0u8, 1] {
        let mut b = block.clone();
        b.parent_block.as_mut().unwrap().major_version = parent_major;
        assert!(b.validate_parent_block().unwrap(), "parent major {parent_major} in a v2 block");
    }
    for parent_major in [2u8, 3, 12, 255] {
        let mut b = block.clone();
        b.parent_block.as_mut().unwrap().major_version = parent_major;
        assert!(!b.validate_parent_block().unwrap(), "parent major {parent_major} in a v2 block");
    }

    // Above v2 the rule does not apply: block 600,001 really does carry a
    // foreign parent at major 12.
    let v6 = BlockTemplate::from_bytes(&blobs("mainnet_rawblocks_600001_v6.json")[0]).unwrap();
    assert_eq!(v6.parent_block.as_ref().unwrap().major_version, 12);
    assert!(v6.validate_parent_block().unwrap());
    let mut b = v6.clone();
    b.parent_block.as_mut().unwrap().major_version = 255;
    assert!(b.validate_parent_block().unwrap(), "the parent major rule is v2-only");
}
