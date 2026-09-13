// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The stateless transaction rules of spec/06 applied to the real transaction
//! in live block 4,213,650 (judged at H = 4,213,649): structure, extra size,
//! output count, fee ladder, mixin tier, transaction proof of work, unlock
//! time, key image subgroup. State-dependent rules (spent key images, ring
//! member lookup, ring signature verification) need stage 3 storage.

use serde_json::Value;
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::*;
use wrkz_primitives::fees;
use wrkz_primitives::mixins::validate_ring_sizes;
use wrkz_primitives::tx::{parse_extra, parse_extra_wallet, Input, Transaction};
use wrkz_primitives::varint;

fn tip_block_with_tx() -> (BlockTemplate, Vec<Transaction>) {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/vectors/mainnet_rawblocks_4213648_to_4213650_v7.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let item = &v["items"].as_array().unwrap()[2];
    let b = BlockTemplate::from_bytes(&hex::decode(item["block"].as_str().unwrap()).unwrap()).unwrap();
    let txs = item["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| Transaction::from_bytes(&hex::decode(t.as_str().unwrap()).unwrap()).unwrap())
        .collect();
    (b, txs)
}

#[test]
fn live_transaction_passes_stateless_rules() {
    let (b, txs) = tip_block_with_tx();
    assert_eq!(b.coinbase_height(), Some(4213650));
    assert_eq!(txs.len(), 1);
    let tx = &txs[0];
    let h = 4213649u64; // previous block index
    let size = tx.to_bytes().unwrap().len();

    // 2. inputs: all key inputs, non-empty offsets, no zero relative offset after the first, key image in subgroup
    assert!(!tx.prefix.inputs.is_empty());
    for input in &tx.prefix.inputs {
        let Input::Key { key_offsets, key_image, .. } = input else { panic!("base input in a normal tx") };
        assert!(!key_offsets.is_empty());
        assert!(key_offsets.iter().skip(1).all(|&o| o != 0));
        assert!(wrkz_pow::curve::key_image_in_prime_subgroup(key_image));
    }
    // 3. outputs: non-zero, <= MAX_OUTPUT_SIZE_NODE, keys decompress
    for o in &tx.prefix.outputs {
        assert!(o.amount != 0 && o.amount <= MAX_OUTPUT_SIZE_NODE);
        assert!(wrkz_pow::curve::check_key(&o.key));
    }
    // 4. fee: the header says reward 1000070, so the fee is 70 and the ladder must allow it
    let fee = tx.fee().unwrap();
    assert_eq!(fee, 70);
    assert_eq!(b.coinbase_output_total(), Some(FIXED_REWARD_V1 + fee));
    assert!(fees::is_valid_normal_fee(fee, size, h), "fee {fee} for {size} bytes");
    // the wallet estimates the fee on a size estimate, so it may overpay within [min, 2*min] (10, step 13)
    let min = fees::required_minimum_fee(size, h);
    assert!(min <= fee && fee <= 2 * min, "fee {fee} vs minimum {min}");
    // 5. extra < 1024
    assert!(tx.prefix.extra.len() < MAX_EXTRA_SIZE_V2);
    let extra = parse_extra(&tx.prefix.extra);
    assert!(extra.public_key.is_some());
    let wextra = parse_extra_wallet(&tx.prefix.extra);
    assert_eq!(wextra.public_key, extra.public_key);
    // 6. unlock time (block index form): >= H + 15
    assert!(tx.prefix.unlock_time < 500_000_000 && tx.prefix.unlock_time >= h + MINIMUM_UNLOCK_TIME_BLOCKS);
    // 7. output count
    assert!(tx.prefix.outputs.len() <= NORMAL_TX_MAX_OUTPUT_COUNT_V1);
    // 8. mixin tier V5 at H: min 1, max 1 -> every ring is exactly 2
    assert_eq!(validate_ring_sizes(&tx.prefix.ring_sizes(), h), Ok(()));
    assert!(tx.prefix.ring_sizes().iter().all(|&r| r == 2));
    // 9. transaction proof of work: fee 70 < 10000, so cn_upx(prefix) must meet the dynamic difficulty
    let inputs = tx.prefix.inputs.len() as u64;
    let outputs = tx.prefix.outputs.len() as u64;
    let diff = transaction_pow_difficulty(h, false, inputs, outputs).unwrap();
    assert_eq!(diff, 40_000 + (inputs + 4 * outputs) * 1000);
    let pow = wrkz_pow::cn_upx(&tx.prefix.to_bytes());
    assert!(wrkz_pow::check_hash(&pow, diff), "tx PoW too weak for difficulty {diff}");
    // the wallet puts the 8-byte nonce field last in extra (TransactionPoW.h:58)
    let nonce = wextra.pow_nonce.expect("wallet-created tx carries a PoW nonce");
    assert_eq!(&tx.prefix.extra[tx.prefix.extra.len() - 8..], &nonce);
    assert_eq!(tx.prefix.extra[tx.prefix.extra.len() - 9], 0x04);
    // signatures: one per ring member per input (from 543,000)
    assert_eq!(tx.signatures.len(), tx.prefix.inputs.len());
    for (input, sigs) in tx.prefix.inputs.iter().zip(&tx.signatures) {
        let Input::Key { key_offsets, .. } = input else { unreachable!() };
        assert_eq!(sigs.len(), key_offsets.len());
        for s in sigs {
            assert!(wrkz_pow::curve::sc_check(&s[..32].try_into().unwrap()));
            assert!(wrkz_pow::curve::sc_check(&s[32..].try_into().unwrap()));
        }
    }
    // block-level: cumulative size within the cap for this height
    let cumulative = b.base_transaction.to_bytes().unwrap().len() + size;
    assert!((cumulative as u64) <= max_block_cumulative_size(4213650));
    // and the extra's pubkey tag is where the reserved_offset rule expects it (RpcServer.cpp:1656)
    assert_eq!(b.base_transaction.prefix.extra[0], 0x01);
    let _ = varint::encode(0);
}
