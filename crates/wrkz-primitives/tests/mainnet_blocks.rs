// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Real-chain acceptance (spec/02 §2, spec/04 §1-2, spec/07 §1-2):
//! every raw block in spec/vectors/mainnet_rawblocks_*.json must
//!   - parse and re-serialize byte-identically and reject trailing bytes,
//!   - reproduce the block id from the live headers,
//!   - satisfy its recorded difficulty with the proof-of-work function of its version,
//!   - satisfy the merge-mining commitment (v2+),
//!
//! and every transaction blob must parse, re-serialize and hash to the coinbase / tx_hashes.
//!
//! The 4,213,648–4,213,650 file was pulled from the live tip on 2026-09-09.

use serde_json::Value;
use std::path::PathBuf;
use wrkz_primitives::block::{genesis_block_hash, BlockTemplate};
use wrkz_primitives::constants::block_major_version_for_index;
use wrkz_primitives::tx::Transaction;
use wrkz_primitives::Error;

fn vectors() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

/// (index, major, hash, difficulty, nonce, timestamp, reward, num_txes).
type Header = (u64, u8, String, u64, u32, u64, u64, u64);

/// The [`Header`] of every vector block, from spec/09-rpc-and-wallet-sync.md
/// plus the live headers file.
fn headers() -> Vec<Header> {
    let mut v = vec![
        (0, 1, "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce", 1, 70, 0, 1500000000000, 1),
        (
            1,
            1,
            "93bb1fd850d9e904ca810cdb57935b6df45cd75fc3a86358a421e126c1ae7b51",
            1,
            271363011,
            1529831318,
            11563301,
            1,
        ),
        (
            2,
            2,
            "4fc480b6507b6df08a92496f3af83dd16b5b44ea1ba76792bd4e6381696c29c3",
            1,
            798020427,
            1529831318,
            11563298,
            1,
        ),
        (
            3,
            3,
            "e2c36c96876cec05e1e9b0f488eef4a0e1487ba38a2f52a3054123bab9bff5de",
            60,
            2550627478,
            1529831318,
            11563295,
            1,
        ),
        (
            4,
            4,
            "bc9ecbdcde0fc6ca467025af49ba239e49148702af9503bce8627714f6974a31",
            3660,
            1935153120,
            1529831327,
            11563292,
            1,
        ),
        (
            5,
            4,
            "513e3cbe87ff9ca63ee30197ac358de63ac30216b68359231917c1e10169e1cb",
            24806,
            2928845235,
            1529831456,
            11563290,
            1,
        ),
        (
            302401,
            5,
            "e9e99274c55fe07f96ed18c6292f44aa570dfa543114b759716b572324c0f765",
            7767351,
            13994,
            1548325067,
            10758997,
            2,
        ),
        (
            600001,
            6,
            "331f2464aa1a4abb6505802643d1e6a259c4eee9cc0305c1eedd7618bfad755b",
            57719958,
            2469645125,
            1566359399,
            10022204,
            1,
        ),
        (
            1000001,
            7,
            "38b8983c2fe4953dfd1857232702b3ab83ae25139a864d6e1160b1739633f144",
            110104052,
            32173218,
            1590601653,
            9113539,
            1,
        ),
        (
            4213000,
            7,
            "c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604",
            24880685,
            7822,
            1788894799,
            1000000,
            1,
        ),
    ]
    .into_iter()
    .map(|(i, m, h, d, n, t, r, x)| (i, m, h.to_string(), d, n, t, r, x))
    .collect::<Vec<_>>();
    let live: Value = serde_json::from_str(
        &std::fs::read_to_string(vectors().join("mainnet_headers_4213588_to_4213650.json")).unwrap(),
    )
    .unwrap();
    for h in live.as_array().unwrap().iter().filter(|h| h["height"].as_u64().unwrap() >= 4213648) {
        v.push((
            h["height"].as_u64().unwrap(),
            h["major_version"].as_u64().unwrap() as u8,
            h["hash"].as_str().unwrap().to_string(),
            h["difficulty"].as_u64().unwrap(),
            h["nonce"].as_u64().unwrap() as u32,
            h["timestamp"].as_u64().unwrap(),
            h["reward"].as_u64().unwrap(),
            h["num_txes"].as_u64().unwrap(),
        ));
    }
    v
}

/// Every raw block from every vector file, keyed by the block index found in its coinbase.
fn raw_blocks() -> Vec<(u64, Vec<u8>, Vec<Vec<u8>>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(vectors()).unwrap() {
        let p = entry.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("mainnet_rawblocks_") {
            continue;
        }
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        for item in v["items"].as_array().unwrap() {
            let block = hex::decode(item["block"].as_str().unwrap()).unwrap();
            let txs: Vec<Vec<u8>> = item["transactions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| hex::decode(t.as_str().unwrap()).unwrap())
                .collect();
            let parsed = BlockTemplate::from_bytes(&block).unwrap_or_else(|e| panic!("{name}: {e}"));
            out.push((parsed.coinbase_height().unwrap(), block, txs));
        }
    }
    out.sort_by_key(|(i, _, _)| *i);
    out
}

#[test]
fn every_mainnet_block_round_trips_and_hashes() {
    let headers = headers();
    let blocks = raw_blocks();
    assert!(blocks.len() >= 13, "expected 13 vector blocks, found {}", blocks.len());
    let mut checked = 0;
    for (index, blob, txs) in &blocks {
        let b = BlockTemplate::from_bytes(blob).unwrap();
        // 1. round trip and trailing-byte rejection
        assert_eq!(b.to_bytes().unwrap(), *blob, "block {index} re-serializes");
        let mut longer = blob.clone();
        longer.push(0);
        assert!(matches!(BlockTemplate::from_bytes(&longer), Err(Error::TrailingBytes(1))), "block {index} trailing");
        // 2. version rule and structure
        assert_eq!(b.major_version, block_major_version_for_index(*index), "block {index} version");
        assert_eq!(b.base_transaction.prefix.unlock_time, index + 40, "block {index} coinbase unlock");
        if b.major_version >= 2 {
            let pb = b.parent_block.as_ref().unwrap();
            // Two producer shapes exist on chain: the daemon template (parent major 0,
            // coinbase version 0 with no inputs) and pool software (parent major 1,
            // coinbase version 1 with one BaseInput); block 600,001 has a foreign
            // parent (major 12, 5 transactions, a v2 coinbase, branch depth 2).
            assert!(pb.major_version <= 1 || *index == 600_001, "block {index} parent major {}", pb.major_version);
            assert!(pb.transaction_count >= 1);
            assert_eq!(pb.base_transaction_branch.len(), wrkz_pow::tree_depth(pb.transaction_count as usize));
            assert_eq!(pb.blockchain_branch.len() as u64, pb.merge_mining_tag().unwrap().depth);
            assert!(b.validate_parent_block().unwrap(), "block {index} parent block rules");
        }
        // 3. transactions: parse, round trip, hash into tx_hashes
        assert_eq!(txs.len(), b.transaction_hashes.len(), "block {index} tx count");
        for (t, want) in txs.iter().zip(&b.transaction_hashes) {
            let tx = Transaction::from_bytes(t).unwrap();
            assert_eq!(tx.to_bytes().unwrap(), *t);
            assert_eq!(tx.hash().unwrap(), *want, "block {index} tx hash");
        }
        // 4. block id, proof of work, merge-mining commitment against the live header
        let Some(h) = headers.iter().find(|h| h.0 == *index) else { continue };
        let (_, major, hash, difficulty, nonce, timestamp, reward, num_txes) = h;
        assert_eq!(b.major_version, *major);
        assert_eq!(hex::encode(b.hash().unwrap()), *hash, "block {index} id");
        assert_eq!(b.nonce, *nonce, "block {index} nonce");
        assert_eq!(b.timestamp, *timestamp, "block {index} timestamp");
        assert_eq!(b.transaction_hashes.len() as u64 + 1, *num_txes);
        assert!(
            wrkz_pow::check_hash(&b.pow_hash().unwrap(), *difficulty),
            "block {index} PoW vs difficulty {difficulty}"
        );
        assert!(
            !wrkz_pow::check_hash(&b.pow_hash().unwrap(), difficulty.saturating_mul(1 << 20)),
            "block {index}: PoW should fail a much higher target"
        );
        assert!(b.check_proof_of_work(*difficulty).unwrap(), "block {index} full checkProofOfWork");
        // 5. reward == coinbase outputs (no size penalty on any of these blocks)
        assert_eq!(b.coinbase_output_total(), Some(*reward), "block {index} reward");
        checked += 1;
    }
    assert_eq!(checked, headers.len(), "every header had a raw block");
}

#[test]
fn genesis_is_first_vector_block() {
    let blocks = raw_blocks();
    let (i, blob, _) = &blocks[0];
    assert_eq!(*i, 0);
    let g = wrkz_primitives::block::genesis_block();
    assert_eq!(g.to_bytes().unwrap(), *blob, "generateGenesisBlock matches the served block 0");
    assert_eq!(hex::encode(genesis_block_hash()), "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce");
}

#[test]
fn synthetic_blocks_txt_vectors() {
    // spec/vectors/blocks.txt: for each "### synthetic vN" block, parse the block blob and
    // reproduce the header hashing blob, aux hash, parent blobs, block id and pow hash.
    let text = std::fs::read_to_string(vectors().join("blocks.txt")).unwrap();
    let mut cur: Option<BlockTemplate> = None;
    let mut seen = 0;
    for line in text.lines() {
        if let Some((k, v)) = line.rsplit_once(" = ") {
            let v = v.trim();
            let k = k.split(" = ").next().unwrap().trim(); // "aux block header hash = keccak(...)" -> first label
            match k {
                "block blob (BlockTemplate serialization)" => {
                    let blob = hex::decode(v).unwrap();
                    let b = BlockTemplate::from_bytes(&blob).unwrap();
                    assert_eq!(b.to_bytes().unwrap(), blob);
                    cur = Some(b);
                }
                "coinbase tx hash" => {
                    assert_eq!(hex::encode(cur.as_ref().unwrap().base_transaction.hash().unwrap()), v)
                }
                "tx tree hash" => assert_eq!(hex::encode(cur.as_ref().unwrap().transaction_tree_hash().unwrap()), v),
                "header hashing blob (header||treehash||varint(txcount))" => {
                    assert_eq!(hex::encode(cur.as_ref().unwrap().header_hashing_blob().unwrap()), v)
                }
                "aux block header hash" => {
                    assert_eq!(hex::encode(cur.as_ref().unwrap().auxiliary_header_hash().unwrap()), v)
                }
                "parent block hashing blob full (for block id)" => {
                    assert_eq!(hex::encode(cur.as_ref().unwrap().parent_hashing_blob(false).unwrap()), v)
                }
                "parent block hashing blob header-only (PoW input)" => {
                    assert_eq!(hex::encode(cur.as_ref().unwrap().parent_hashing_blob(true).unwrap()), v)
                }
                "block id" => assert_eq!(hex::encode(cur.as_ref().unwrap().hash().unwrap()), v),
                "pow hash" => {
                    let b = cur.as_ref().unwrap();
                    assert_eq!(hex::encode(b.pow_hash().unwrap()), v);
                    assert!(b.check_proof_of_work(1).unwrap());
                    seen += 1;
                }
                _ => {}
            }
        }
    }
    assert_eq!(seen, 8, "genesis + 7 synthetic versions");
}
