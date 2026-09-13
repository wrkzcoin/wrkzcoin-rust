// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Real-chain acceptance for stage 3.2 (spec/06 acceptance 1, spec/07
//! acceptance 1–2, spec/11 acceptance 4).
//!
//! - genesis applies and its record matches the block in
//!   `vectors/mainnet_rawblocks_0_to_5.json`;
//! - blocks 0–5 replay with the mainnet checkpoints on;
//! - the same with checkpoints disabled from 0, so the proof of work and the
//!   coinbase rules of every block actually run;
//! - every other mainnet vector block (302,401, 600,001, 1,000,001, 4,213,000,
//!   4,213,648–4,213,650) passes every stateless rule, and its reward agrees
//!   with the recorded coinbase output total.
//!
//! Everything here runs on `MemStore`, so it runs on any host.

use serde_json::Value;
use std::path::PathBuf;
use wrkz_chain::records::OutputRecord;
use wrkz_chain::validate::{validate_transaction, ChainAccess, TxContext, ValidatorState};
use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::*;
use wrkz_primitives::tx::Transaction;
use wrkz_primitives::Hash;
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

/// `(index, hash, difficulty, reward)` for the vector blocks that are not in
/// `mainnet_rawblocks_0_to_5.json`, from the live headers of spec/09 and
/// `mainnet_headers_4213588_to_4213650.json`.
fn other_headers() -> Vec<(u64, &'static str, u64, u64)> {
    let mut v: Vec<(u64, &'static str, u64, u64)> = vec![
        (302_401, "e9e99274c55fe07f96ed18c6292f44aa570dfa543114b759716b572324c0f765", 7_767_351, 10_758_997),
        (600_001, "331f2464aa1a4abb6505802643d1e6a259c4eee9cc0305c1eedd7618bfad755b", 57_719_958, 10_022_204),
        (1_000_001, "38b8983c2fe4953dfd1857232702b3ab83ae25139a864d6e1160b1739633f144", 110_104_052, 9_113_539),
        (4_213_000, "c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604", 24_880_685, 1_000_000),
    ];
    let live: Value = serde_json::from_str(
        &std::fs::read_to_string(vectors().join("mainnet_headers_4213588_to_4213650.json")).unwrap(),
    )
    .unwrap();
    // Leaked so the table can stay `&'static str`; three entries, once.
    for h in live.as_array().unwrap().iter().filter(|h| h["height"].as_u64().unwrap() >= 4_213_648) {
        v.push((
            h["height"].as_u64().unwrap(),
            Box::leak(h["hash"].as_str().unwrap().to_string().into_boxed_str()),
            h["difficulty"].as_u64().unwrap(),
            h["reward"].as_u64().unwrap(),
        ));
    }
    v
}

fn chain(checkpoints: Checkpoints) -> ChainState<MemStore> {
    let mut chain =
        ChainState::open_or_genesis(MemStore::default(), Config::default(), checkpoints).expect("genesis applies");
    // Block 5 of the chain is from 2018; the future time limit is judged
    // against the validating node's clock, so pin it to a value above every
    // vector timestamp and let the rule stay meaningful.
    chain.set_clock(Some(1_800_000_000));
    chain
}

// (a) ------------------------------------------------------------------------

#[test]
fn genesis_applies_and_matches_the_vector() {
    let chain = chain(Checkpoints::mainnet());
    let (blob, txs) = raw_blocks("mainnet_rawblocks_0_to_5.json").remove(0);
    assert!(txs.is_empty());
    let vector = BlockTemplate::from_bytes(&blob).unwrap();
    let info = *chain.tip_info().unwrap();

    assert_eq!(chain.tip_index(), Some(0));
    assert_eq!(info.block_hash, vector.hash().unwrap());
    assert_eq!(hex::encode(info.block_hash), "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce");
    assert_eq!(info.timestamp, vector.timestamp);
    assert_eq!(info.timestamp, 0);
    assert_eq!(info.cumulative_difficulty, 1, "genesis carries difficulty 1");
    assert_eq!(info.already_generated_coins, vector.coinbase_output_total().unwrap());
    assert_eq!(info.already_generated_coins, GENESIS_BLOCK_REWARD);
    assert_eq!(info.already_generated_transactions, 1);
    // `DatabaseBlockchainCache::addGenesisBlock` stores the *coinbase* size.
    assert_eq!(info.block_size as usize, vector.base_transaction.to_bytes().unwrap().len());
    assert_eq!(info.block_size, 157);
    // The state a later block reads: the three genesis outputs are resolvable
    // and their unlock time is the coinbase's.
    assert_eq!(chain.output_count_for_amount(500_000_000_000).unwrap(), 3);
    for i in 0..3 {
        let out = chain.key_output(500_000_000_000, i).unwrap().unwrap();
        assert_eq!(out.unlock_time, 40);
        assert_eq!(out.block_index, 0);
        assert_eq!(out.public_key, vector.base_transaction.prefix.outputs[i as usize].key);
    }
    assert_eq!(chain.block_transaction_hashes(0).unwrap(), vec![vector.base_transaction.hash().unwrap()]);
}

// (b) and (c) ----------------------------------------------------------------

/// The recorded per-block values of blocks 1–5, from the live headers in
/// spec/09: `(difficulty, reward)`.
const BLOCKS_1_TO_5: [(u64, u64); 5] =
    [(1, 11_563_301), (1, 11_563_298), (60, 11_563_295), (3660, 11_563_292), (24806, 11_563_290)];

fn replay_0_to_5(checkpoints: Checkpoints) {
    let mut chain = chain(checkpoints);
    let blocks = raw_blocks("mainnet_rawblocks_0_to_5.json");
    let mut expected_coins = GENESIS_BLOCK_REWARD;
    let mut expected_cumulative = 1u64;
    for (i, (blob, txs)) in blocks.iter().enumerate().skip(1) {
        let outcome = chain.add_block(blob, txs).unwrap_or_else(|e| panic!("block {i}: {e}"));
        let (difficulty, reward) = BLOCKS_1_TO_5[i - 1];
        assert_eq!(outcome.index, i as u32);
        assert_eq!(outcome.difficulty, difficulty, "block {i} difficulty");
        expected_cumulative += difficulty;
        expected_coins += reward;
        assert_eq!(outcome.cumulative_difficulty, expected_cumulative, "block {i} cumulative difficulty");
        assert_eq!(outcome.already_generated_coins, expected_coins, "block {i} emission");
        // The reward the rule computed is the coinbase total the block carries.
        let block = BlockTemplate::from_bytes(blob).unwrap();
        assert_eq!(block.coinbase_output_total().unwrap(), reward, "block {i} reward");
        assert_eq!(outcome.hash, block.hash().unwrap());
        assert_eq!(chain.block_index_by_hash(&outcome.hash).unwrap(), Some(i as u32));
    }
    assert_eq!(chain.tip_index(), Some(5));
    assert_eq!(
        hex::encode(chain.tip_info().unwrap().block_hash),
        "513e3cbe87ff9ca63ee30197ac358de63ac30216b68359231917c1e10169e1cb"
    );
}

#[test]
fn blocks_0_to_5_replay_with_checkpoints_on() {
    replay_0_to_5(Checkpoints::mainnet());
}

#[test]
fn blocks_0_to_5_replay_with_checkpoints_disabled_from_zero() {
    // Nothing is checkpointed any more, so every block's proof of work is
    // computed and checked against the difficulty this state derived, and the
    // coinbase and transaction rules run in full.
    let mut cp = Checkpoints::mainnet();
    cp.disable_from(Some(0));
    assert!(!cp.is_in_checkpoint_zone(0));
    replay_0_to_5(cp);
}

// (d) ------------------------------------------------------------------------

/// A chain view with no outputs at all. Used with a checkpoint that covers the
/// block, which is what makes the expensive input checks skip — see
/// [`stateless_transaction_rules`].
struct NoState;

impl ChainAccess for NoState {
    fn key_image_spent(&self, _: &Hash, _: u64) -> wrkz_chain::Result<bool> {
        Ok(false)
    }
    fn key_output(&self, _: u64, _: u64) -> wrkz_chain::Result<Option<OutputRecord>> {
        Ok(None)
    }
    fn top_block_timestamp(&self) -> u64 {
        1_800_000_000
    }
    fn now(&self) -> u64 {
        1_800_000_000
    }
}

/// Every transaction rule that does not need chain state, for the transactions
/// of one block.
///
/// The trick is `is_pool_transaction: true` with a checkpoint above the block:
/// `validateTransactionInputsExpensive` skips inside the checkpoint zone for
/// pool and block transactions alike (`ValidateTransaction.cpp:657`), while
/// `validateTransactionPoW` skips inside the zone **only** for block
/// transactions (line 578). So this combination runs rules 1–9 — size, inputs,
/// outputs, fee, extra, unlock time, output count, mixin and the transaction
/// proof of work — and nothing that would need the output table.
fn stateless_transaction_rules(block: &BlockTemplate, tx_blobs: &[Vec<u8>], index: u64) {
    let mut cp = Checkpoints::none();
    cp.add(index as u32 + 1000, [0u8; 32]);
    let ctx = TxContext {
        block_height: index - 1,
        // The smallest legal median at this height: the strictest size limit.
        block_median_size: full_reward_zone(block.major_version) as u64,
        block_timestamp: block.timestamp,
        is_pool_transaction: true,
        checkpoints: &cp,
    };
    let mut state = ValidatorState::new();
    for (i, blob) in tx_blobs.iter().enumerate() {
        let tx = Transaction::from_bytes(blob).unwrap_or_else(|e| panic!("block {index} tx {i}: {e}"));
        assert_eq!(tx.to_bytes().unwrap(), *blob, "block {index} tx {i} re-serializes");
        assert_eq!(tx.hash().unwrap(), block.transaction_hashes[i], "block {index} tx {i} hash");
        validate_transaction(&tx, blob, &mut state, &NoState, &ctx)
            .unwrap_or_else(|e| panic!("block {index} tx {i}: {e}"));
    }
}

#[test]
fn every_other_mainnet_vector_block_passes_the_stateless_rules() {
    let files = [
        "mainnet_rawblocks_302401_v5.json",
        "mainnet_rawblocks_600001_v6.json",
        "mainnet_rawblocks_1000001_v7.json",
        "mainnet_rawblocks_4213000_v7.json",
        "mainnet_rawblocks_4213648_to_4213650_v7.json",
    ];
    let headers = other_headers();
    let mut checked = 0;
    for file in files {
        for (blob, txs) in raw_blocks(file) {
            let block = BlockTemplate::from_bytes(&blob).unwrap();
            let index = block.coinbase_height().unwrap();
            let (_, hash, difficulty, reward) =
                *headers.iter().find(|h| h.0 == index).unwrap_or_else(|| panic!("no header for {index}"));

            // Block identity and the version rule.
            assert_eq!(hex::encode(block.hash().unwrap()), hash, "block {index} hash");
            assert_eq!(block.major_version, block_major_version_for_index(index), "block {index} version");

            // The parent-block rules of `Core::validateBlock` (`Core.cpp:2714`).
            assert!(block.validate_parent_block().unwrap(), "block {index} parent block");

            // Coinbase structure, exactly as `validateBlock` checks it.
            assert_eq!(block.coinbase_height(), Some(index), "block {index} coinbase base input");
            assert_eq!(block.base_transaction.prefix.unlock_time, index + 40, "block {index} coinbase unlock");
            if index >= TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT {
                assert!(block.base_transaction.signatures.is_empty(), "block {index} coinbase signatures");
            }
            for out in &block.base_transaction.prefix.outputs {
                assert_ne!(out.amount, 0, "block {index} coinbase zero output");
                assert!(wrkz_pow::curve::check_key(&out.key), "block {index} coinbase output key");
            }

            // Proof of work at the recorded difficulty, merge-mining commitment
            // included (`Currency::checkProofOfWork`).
            assert!(block.check_proof_of_work(difficulty).unwrap(), "block {index} proof of work");

            // The block's transaction list.
            assert_eq!(txs.len(), block.transaction_hashes.len(), "block {index} transaction count");
            stateless_transaction_rules(&block, &txs, index);

            // The reward. The coinbase total must be what the header records.
            let coinbase_total = block.coinbase_output_total().unwrap();
            assert_eq!(coinbase_total, reward, "block {index} coinbase total");

            // And the rule must produce it. `alreadyGeneratedCoins` is chain
            // state and no vector records it, so:
            //  - from FIXED_REWARD_V1_HEIGHT the base reward is flat and the
            //    emission does not enter the rule at all, which makes this an
            //    unconditional check;
            //  - below it, the emission is recovered from the recorded reward
            //    and the block's fees, and the rule is required to reproduce
            //    the reward from that value. That is a round trip, not an
            //    independent check of the emission: only a full replay from
            //    genesis (`wrkz-replay`) proves the emission itself.
            let fee: u64 = txs
                .iter()
                .map(|b| Transaction::from_bytes(b).unwrap())
                .map(|t| t.fee().expect("outputs do not exceed inputs"))
                .sum();
            let cumulative_size = block.base_transaction.to_bytes().unwrap().len() as u64
                + txs.iter().map(|t| t.len() as u64).sum::<u64>();
            // Every vector block is far below the penalty zone, so the median
            // is its floor and no penalty applies.
            let median = full_reward_zone(block.major_version) as u64;
            assert!(cumulative_size <= median, "block {index} is inside the penalty-free zone");
            let already_generated_coins = if index >= FIXED_REWARD_V1_HEIGHT {
                assert_eq!(reward, FIXED_REWARD_V1 + fee, "block {index} flat reward plus fees");
                0
            } else {
                // reward = ((MONEY_SUPPLY - agc) >> 22) + fee, so any agc in
                // the 2^22-wide band that floors to `base` reproduces it; take
                // the lowest.
                let base = reward - fee;
                MONEY_SUPPLY - ((base + 1) << EMISSION_SPEED_FACTOR) + 1
            };
            let computed = wrkz_chain::reward::get_block_reward(
                block.major_version,
                median,
                cumulative_size,
                already_generated_coins,
                fee,
                index,
            )
            .expect("inside the size ceiling");
            assert_eq!(computed.reward, reward, "block {index} reward rule");
            checked += 1;
        }
    }
    assert_eq!(checked, 7, "302401, 600001, 1000001, 4213000 and 4213648-4213650");
}
