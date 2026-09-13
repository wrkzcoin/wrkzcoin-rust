// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The `f_*` block-explorer JSON-RPC methods, over a synthetic chain.
//!
//! Three things are being checked, and they are different things:
//!
//! 1. the **gate** — every `f_*` method is refused on a `Standard` daemon with
//!    the C++'s 403 and its message;
//! 2. the **shapes** — every field `RpcServer.cpp` prints, with the value it
//!    computes, including the ones an earlier pass left out of
//!    `f_transaction_json` (mixin, size, the block details) and the payment-id
//!    lookup that had no index behind it;
//! 3. the **refusals** — a lite node answers for a height above its line and
//!    reports an error, never a wrong answer, below it.
//!
//! The chain is seeded record by record into a [`MemStore`], the way
//! `chain_backed.rs` seeds one: these are v1 blocks at heights no validator
//! would accept today, and nothing here is asking a validator anything. The
//! blobs are real, though — `f_block_json` and `f_transaction_json` parse them
//! back out of the store, so a wrong byte would surface as a wrong field.

use std::sync::Arc;
use wrkz_chain::records::{BlockInfo, OutputRecord};
use wrkz_chain::{keys, records, ChainState, Checkpoints, Config};
use wrkz_mempool::TransactionPool;
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::tx::{build_extra, build_nonce, Input, Output, PaymentId, Transaction, TransactionPrefix};
use wrkz_primitives::Hash;
use wrkz_rpc::http::{Request, Response};
use wrkz_rpc::json::{parse, Json, ParseLimits};
use wrkz_rpc::node::{serving_config, ChainNode};
use wrkz_rpc::server::{dispatch, Context, RpcMode, ServerConfig};
use wrkz_storage::{KvStore, MemStore};

/// The top block index of the synthetic chain. Above 30, because
/// `f_blocks_list_json` walks `height` down to `height - 30` and the C++ runs
/// its unsigned loop off the end below that.
const TOP: u32 = 40;
/// The block that carries the one non-coinbase transaction.
const TX_BLOCK: u32 = 35;
const COINBASE_AMOUNT: u64 = 1_000_000;
const TX_FEE: u64 = 1_000;
const PAYMENT_ID: Hash = [0x5a; 32];
const BASE_TIME: u64 = 1_500_000_000;

// ---------------------------------------------------------------------------
// the synthetic chain
// ---------------------------------------------------------------------------

fn key(seed: u8) -> Hash {
    wrkz_pow::cn_fast_hash(&[seed, 0x9e])
}

/// The coinbase of block `index`: one `BaseInput`, one output.
fn coinbase(index: u32) -> Transaction {
    Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: index as u64 + 40,
            inputs: vec![Input::Base { block_index: index as u64 }],
            outputs: vec![Output { amount: COINBASE_AMOUNT, key: key(index as u8) }],
            extra: build_extra(&key(0xC0 ^ index as u8), None, None).expect("extra"),
        },
        signatures: Vec::new(),
    }
}

/// The one real transaction: a ring of four over one key input, two outputs,
/// and a plaintext long payment id in the extra.
///
/// The ring size is what `mixin` reports — `Core.cpp:4895` takes the largest
/// `key_offsets.len()` over the key inputs, so this must read as 4 and not 3.
fn payment_transaction() -> Transaction {
    let nonce = build_nonce(Some(&PaymentId::Long(PAYMENT_ID)), None);
    Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: 0,
            inputs: vec![Input::Key { amount: COINBASE_AMOUNT, key_offsets: vec![7, 3, 11, 2], key_image: key(0xAA) }],
            outputs: vec![
                Output { amount: COINBASE_AMOUNT - TX_FEE - 500, key: key(0xB1) },
                Output { amount: 500, key: key(0xB2) },
            ],
            extra: build_extra(&key(0xB0), Some(&nonce), None).expect("extra"),
        },
        // One signature per ring member, so the blob is the size a real one is.
        signatures: vec![vec![[0u8; 64]; 4]],
    }
}

fn block_at(index: u32, previous: Hash, transactions: &[Transaction]) -> BlockTemplate {
    BlockTemplate {
        major_version: 1,
        minor_version: 0,
        timestamp: BASE_TIME + index as u64 * 60,
        previous_block_hash: previous,
        nonce: index,
        parent_block: None,
        base_transaction: coinbase(index),
        transaction_hashes: transactions.iter().map(|t| t.hash().expect("hashes")).collect(),
    }
}

/// The seeded chain, plus the hash of the payment transaction and of its block.
struct Seeded {
    chain: ChainState<MemStore>,
    tx_hash: Hash,
    tx_block_hash: Hash,
    coinbase_hash: Hash,
}

fn seed(cfg: Config) -> Seeded {
    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut previous = [0u8; 32];
    let mut cumulative = 0u64;
    let mut coins = 0u64;
    let mut txs_so_far = 0u64;
    let mut global_index = 0u32;
    let mut tx_hash = [0u8; 32];
    let mut tx_block_hash = [0u8; 32];
    let mut coinbase_hash = [0u8; 32];

    for index in 0..=TOP {
        let transactions: Vec<Transaction> = if index == TX_BLOCK { vec![payment_transaction()] } else { Vec::new() };
        let block = block_at(index, previous, &transactions);
        let blob = block.to_bytes().expect("the block serialises");
        let hash = block.hash().expect("the block hashes");
        let tx_blobs: Vec<Vec<u8>> = transactions.iter().map(|t| t.to_bytes().expect("serialises")).collect();

        let cb_hash = block.base_transaction.hash().expect("the coinbase hashes");
        let mut hashes = vec![cb_hash];
        hashes.extend(block.transaction_hashes.iter().copied());

        // The stored cumulative size, which is what `blockSize` and
        // `transactionsCumulativeSize` are computed from.
        let coinbase_size = block.base_transaction.to_bytes().unwrap().len() as u32;
        let block_size = coinbase_size + tx_blobs.iter().map(|b| b.len() as u32).sum::<u32>();

        cumulative += 1_000;
        coins += COINBASE_AMOUNT;
        txs_so_far += hashes.len() as u64;
        let info = BlockInfo {
            block_hash: hash,
            timestamp: block.timestamp,
            block_size,
            cumulative_difficulty: cumulative,
            already_generated_coins: coins,
            already_generated_transactions: txs_so_far,
        };
        ops.push((keys::block_info(index), Some(info.encode())));
        ops.push((keys::hash_to_index(&hash), Some(index.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(index), Some(records::encode_hashes(&hashes))));
        for h in &hashes {
            ops.push((keys::transaction_index(h), Some(index.to_le_bytes().to_vec())));
        }
        ops.push((keys::raw_block(index), Some(records::encode_raw_block(&blob, &tx_blobs))));

        // The coinbase output's own record — which `/queryblocksdetailed` reads
        // to name the transaction a ring member came from — and the per-block
        // record, so the global-index endpoints and the unwind record are
        // consistent with the blocks.
        let record = OutputRecord {
            public_key: key(index as u8),
            unlock_time: index as u64 + 40,
            transaction_hash: cb_hash,
            output_index: 0,
            block_index: index,
        };
        ops.push((keys::output(COINBASE_AMOUNT, global_index), Some(record.encode())));
        let mut refs = vec![(COINBASE_AMOUNT, global_index)];
        global_index += 1;
        if index == TX_BLOCK {
            tx_hash = block.transaction_hashes[0];
            tx_block_hash = hash;
            coinbase_hash = cb_hash;
            for out in &transactions[0].prefix.outputs {
                refs.push((out.amount, 0));
            }
            ops.push((keys::payment_id(&PAYMENT_ID), Some(records::encode_hashes(&[tx_hash]))));
            ops.push((keys::block_payment_ids(index), Some(records::encode_payment_id_refs(&[(PAYMENT_ID, tx_hash)]))));
        }
        ops.push((keys::block_outputs(index), Some(records::encode_output_refs(&refs))));

        previous = hash;
    }
    ops.push((keys::output_count(COINBASE_AMOUNT), Some(global_index.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(TOP.to_le_bytes().to_vec())));
    if cfg.lite_start_height != 0 {
        ops.push((keys::meta(keys::META_LITE_HEIGHT), Some(cfg.lite_start_height.to_le_bytes().to_vec())));
    }

    let mut store = MemStore::default();
    store.write_batch(ops).unwrap();
    let mut chain = ChainState::open(store, cfg, Checkpoints::none()).expect("the seeded state opens");
    chain.set_clock(Some(BASE_TIME + TOP as u64 * 60 + 60));
    assert_eq!(chain.tip_index(), Some(TOP));
    Seeded { chain, tx_hash, tx_block_hash, coinbase_hash }
}

fn full() -> Config {
    serving_config()
}

fn lite(from: u32) -> Config {
    Config { lite_start_height: from, ..serving_config() }
}

fn ctx_over(chain: ChainState<MemStore>, mode: RpcMode) -> Context {
    let node = ChainNode::standalone(chain, TransactionPool::new(Default::default()));
    Context::new(Arc::new(node), ServerConfig { mode, ..ServerConfig::default() })
}

fn rpc(ctx: &Context, method: &str, params: &str) -> Response {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#);
    dispatch(
        ctx,
        &Request {
            method: "POST".into(),
            path: "/json_rpc".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
            body: body.into_bytes(),
        },
        "127.0.0.1",
    )
}

fn json(res: &Response) -> Json {
    parse(&res.body, ParseLimits { max_bytes: 16 * 1024 * 1024, max_depth: 64 })
        .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", String::from_utf8_lossy(&res.body)))
}

fn result_of(res: &Response) -> Json {
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let body = json(res);
    body.get("result").unwrap_or_else(|| panic!("no result: {body:?}")).clone()
}

/// A plain (non-JSON-RPC) POST route.
fn post(ctx: &Context, path: &str, body: &str) -> Response {
    dispatch(
        ctx,
        &Request {
            method: "POST".into(),
            path: path.into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        },
        "127.0.0.1",
    )
}

fn ok_body(res: &Response) -> Json {
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    json(res)
}

fn error_of(res: &Response) -> (i64, String) {
    assert_eq!(res.status, 200, "a JSON-RPC error is still an HTTP 200");
    let e = json(res).get("error").expect("an error object").clone();
    let code = match e.get("code").expect("a code") {
        Json::U64(n) => *n as i64,
        Json::I64(n) => *n,
        other => panic!("code is not an integer: {other:?}"),
    };
    (code, e.get("message").unwrap().as_str().unwrap().to_string())
}

// ---------------------------------------------------------------------------
// the gate
// ---------------------------------------------------------------------------

/// Every `f_*` method is refused on a `Standard` daemon, with the 403 and the
/// message `RpcServer.cpp:589-601` produces — not a 404, which is what an
/// unknown method gets, and not an empty result.
#[test]
fn the_explorer_methods_are_gated_by_the_flag() {
    let seeded = seed(full());
    let hash = hex::encode(seeded.tx_block_hash);
    let calls: [(&str, String); 5] = [
        ("f_blocks_list_json", r#"{"height":40}"#.into()),
        ("f_block_json", format!(r#"{{"hash":"{hash}"}}"#)),
        ("f_transaction_json", format!(r#"{{"hash":"{}"}}"#, hex::encode(seeded.tx_hash))),
        ("f_on_transactions_pool_json", "{}".into()),
        ("f_transactions_by_payment_id_json", format!(r#"{{"paymentId":"{}"}}"#, hex::encode(PAYMENT_ID))),
    ];

    let standard = ctx_over(seed(full()).chain, RpcMode::Standard);
    for (method, params) in &calls {
        let res = rpc(&standard, method, params);
        assert_eq!(res.status, 403, "{method} must be refused on a standard daemon");
        let body = String::from_utf8_lossy(&res.body).to_string();
        assert!(body.contains("do not have permission"), "{method}: {body}");
        assert!(body.contains("explorer"), "{method}: the message names the flag: {body}");
    }

    let explorer = ctx_over(seed(full()).chain, RpcMode::Explorer);
    for (method, params) in &calls {
        let res = rpc(&explorer, method, params);
        assert_eq!(res.status, 200, "{method} must be served in explorer mode");
        assert!(json(&res).get("result").is_some(), "{method} answered with an error: {:?}", json(&res));
    }
}

// ---------------------------------------------------------------------------
// the shapes
// ---------------------------------------------------------------------------

#[test]
fn f_blocks_list_json_returns_thirty_one_rows_newest_first() {
    let seeded = seed(full());
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);
    let r = result_of(&rpc(&ctx, "f_blocks_list_json", &format!(r#"{{"height":{TOP}}}"#)));
    assert_eq!(r.get("status").unwrap().as_str(), Some("OK"));
    let blocks = r.get("blocks").unwrap().as_array().unwrap();
    // `height` down to `height - 30`, inclusive: 31 rows, not 30.
    assert_eq!(blocks.len(), 31);
    assert_eq!(blocks[0].get("height").unwrap().as_u64(), Some(TOP as u64), "newest first");
    assert_eq!(blocks[30].get("height").unwrap().as_u64(), Some(TOP as u64 - 30));
    for row in blocks {
        for field in ["cumul_size", "difficulty", "hash", "height", "timestamp", "tx_count"] {
            assert!(row.get(field).is_some(), "row is missing {field}: {row:?}");
        }
    }
    // The block that carries a transaction counts it, plus the coinbase.
    let row = blocks.iter().find(|b| b.get("height").unwrap().as_u64() == Some(TX_BLOCK as u64)).unwrap();
    assert_eq!(row.get("tx_count").unwrap().as_u64(), Some(2));

    // Above the tip: the C++'s `-2`, with the current *index* in the message.
    let (code, message) = error_of(&rpc(&ctx, "f_blocks_list_json", &format!(r#"{{"height":{}}}"#, TOP + 1)));
    assert_eq!(code, -2);
    assert!(message.contains(&format!("Current height: {TOP}")), "{message}");
}

#[test]
fn f_block_json_takes_a_hash_or_a_one_based_height() {
    let seeded = seed(full());
    let expected_hash = hex::encode(seeded.tx_block_hash);
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);

    let by_hash = result_of(&rpc(&ctx, "f_block_json", &format!(r#"{{"hash":"{expected_hash}"}}"#)));
    let block = by_hash.get("block").unwrap();
    assert_eq!(block.get("hash").unwrap().as_str(), Some(expected_hash.as_str()));
    assert_eq!(block.get("height").unwrap().as_u64(), Some(TX_BLOCK as u64));
    assert_eq!(block.get("depth").unwrap().as_u64(), Some((TOP - TX_BLOCK) as u64));
    assert_eq!(block.get("major_version").unwrap().as_u64(), Some(1));
    assert_eq!(block.get("orphan_status").unwrap().as_bool(), Some(false));
    assert_eq!(block.get("reward").unwrap().as_u64(), Some(COINBASE_AMOUNT));
    // A string, not a number: `std::to_string` at `RpcServer.cpp:2145`.
    assert!(block.get("alreadyGeneratedCoins").unwrap().as_str().is_some());
    // Coinbase first, then the block's transactions.
    let txs = block.get("transactions").unwrap().as_array().unwrap();
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[0].get("fee").unwrap().as_u64(), Some(0), "the coinbase pays no fee");
    assert_eq!(txs[1].get("hash").unwrap().as_str(), Some(hex::encode(seeded.tx_hash).as_str()));
    assert_eq!(txs[1].get("fee").unwrap().as_u64(), Some(TX_FEE));
    assert_eq!(block.get("totalFeeAmount").unwrap().as_u64(), Some(TX_FEE), "the coinbase is not counted");

    // The numeric form is a **height count**, so it resolves `height - 1` —
    // a long-standing off-by-one an explorer built against it depends on.
    let by_height = result_of(&rpc(&ctx, "f_block_json", &format!(r#"{{"hash":"{}"}}"#, TX_BLOCK + 1)));
    assert_eq!(by_height.get("block").unwrap().get("hash").unwrap().as_str(), Some(expected_hash.as_str()));

    let (code, message) = error_of(&rpc(&ctx, "f_block_json", r#"{"hash":"not hex at all"}"#));
    assert_eq!(code, -1);
    assert!(message.contains("not valid"), "{message}");
}

/// The method an earlier pass left unfinished: every field
/// `RpcServer.cpp:2171-2309` prints, with the value it computes.
#[test]
fn f_transaction_json_reports_the_block_the_transaction_and_the_details() {
    let seeded = seed(full());
    let tx_hash = hex::encode(seeded.tx_hash);
    let block_hash = hex::encode(seeded.tx_block_hash);
    let expected_blob = payment_transaction().to_bytes().unwrap();
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);

    let r = result_of(&rpc(&ctx, "f_transaction_json", &format!(r#"{{"hash":"{tx_hash}"}}"#)));
    assert_eq!(r.get("status").unwrap().as_str(), Some("OK"));

    // `result.block` — the block short shape, for the block that holds it.
    let block = r.get("block").unwrap();
    assert_eq!(block.get("hash").unwrap().as_str(), Some(block_hash.as_str()));
    assert_eq!(block.get("height").unwrap().as_u64(), Some(TX_BLOCK as u64));
    assert_eq!(block.get("tx_count").unwrap().as_u64(), Some(2));
    assert_eq!(block.get("timestamp").unwrap().as_u64(), Some(BASE_TIME + TX_BLOCK as u64 * 60));

    // `result.tx` — the transaction as it was serialised.
    let tx = r.get("tx").unwrap();
    assert_eq!(tx.get("version").unwrap().as_u64(), Some(1));
    assert_eq!(tx.get("unlock_time").unwrap().as_u64(), Some(0));
    // The hex of the extra **bytes**. The C++ hexes the vector's control block
    // here and emits heap pointers; see the handler's comment.
    let extra_hex = tx.get("extra").unwrap().as_str().unwrap();
    assert_eq!(extra_hex, hex::encode(&payment_transaction().prefix.extra));
    assert_eq!(tx.get("publicKey").unwrap().as_str(), Some(hex::encode(key(0xB0)).as_str()));
    // The nonce field's bytes, sub-tag included, which is what carries the id.
    let nonce = build_nonce(Some(&PaymentId::Long(PAYMENT_ID)), None);
    assert_eq!(tx.get("nonce").unwrap().as_str(), Some(hex::encode(&nonce).as_str()));

    let vin = tx.get("vin").unwrap().as_array().unwrap();
    assert_eq!(vin.len(), 1);
    assert_eq!(vin[0].get("type").unwrap().as_str(), Some("02"), "a key input's tag is the string 02");
    let value = vin[0].get("value").unwrap();
    assert_eq!(value.get("amount").unwrap().as_u64(), Some(COINBASE_AMOUNT));
    assert_eq!(value.get("k_image").unwrap().as_str(), Some(hex::encode(key(0xAA)).as_str()));
    let offsets: Vec<u64> =
        value.get("key_offsets").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    assert_eq!(offsets, vec![7, 3, 11, 2], "the **relative** offsets, as the transaction carries them");

    let vout = tx.get("vout").unwrap().as_array().unwrap();
    assert_eq!(vout.len(), 2);
    assert_eq!(vout[0].get("amount").unwrap().as_u64(), Some(COINBASE_AMOUNT - TX_FEE - 500));
    assert_eq!(vout[0].get("target").unwrap().get("type").unwrap().as_str(), Some("02"));
    assert_eq!(
        vout[0].get("target").unwrap().get("data").unwrap().get("key").unwrap().as_str(),
        Some(hex::encode(key(0xB1)).as_str())
    );

    // `result.txDetails` — the derived values, which is where mixin and size
    // were missing.
    let d = r.get("txDetails").unwrap();
    assert_eq!(d.get("hash").unwrap().as_str(), Some(tx_hash.as_str()));
    assert_eq!(d.get("fee").unwrap().as_u64(), Some(TX_FEE));
    assert_eq!(d.get("amount_out").unwrap().as_u64(), Some(COINBASE_AMOUNT - TX_FEE));
    // `Core.cpp:4895`: the ring **size**, not the ring size minus one.
    assert_eq!(d.get("mixin").unwrap().as_u64(), Some(4));
    assert_eq!(d.get("size").unwrap().as_u64(), Some(expected_blob.len() as u64));
    assert_eq!(d.get("paymentId").unwrap().as_str(), Some(hex::encode(PAYMENT_ID).as_str()));
    assert_eq!(d.get("paymentIdEncrypted").unwrap().as_bool(), Some(false));
}

#[test]
fn f_transaction_json_reports_a_coinbase_and_refuses_what_it_does_not_hold() {
    let seeded = seed(full());
    let coinbase_hash = hex::encode(seeded.coinbase_hash);
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);

    let r = result_of(&rpc(&ctx, "f_transaction_json", &format!(r#"{{"hash":"{coinbase_hash}"}}"#)));
    let d = r.get("txDetails").unwrap();
    // A coinbase has no key input, so `mixin` is 0 and the fee is 0 — the C++
    // `getTransactionFee` returns 0 the moment it sees a `BaseInput`.
    assert_eq!(d.get("mixin").unwrap().as_u64(), Some(0));
    assert_eq!(d.get("fee").unwrap().as_u64(), Some(0));
    assert_eq!(d.get("amount_out").unwrap().as_u64(), Some(COINBASE_AMOUNT));
    assert_eq!(d.get("paymentId").unwrap().as_str(), Some(""), "a coinbase never carries one");
    let vin = r.get("tx").unwrap().get("vin").unwrap().as_array().unwrap();
    assert_eq!(vin[0].get("type").unwrap().as_str(), Some("ff"), "a coinbase input's tag is the string ff");
    assert_eq!(vin[0].get("value").unwrap().get("height").unwrap().as_u64(), Some(TX_BLOCK as u64));

    // A hash nothing holds. Both messages say "Block hash" for a transaction
    // hash; that is the C++'s wording (`RpcServer.cpp:2183`, `:2200`).
    let (code, message) = error_of(&rpc(&ctx, "f_transaction_json", &format!(r#"{{"hash":"{}"}}"#, "ab".repeat(32))));
    assert_eq!(code, -1);
    assert_eq!(message, "Block hash specified does not exist!");
    let (code, message) = error_of(&rpc(&ctx, "f_transaction_json", r#"{"hash":"zz"}"#));
    assert_eq!(code, -1);
    assert_eq!(message, "Block hash specified is not a valid hex!");
}

#[test]
fn the_payment_id_lookup_finds_the_transaction_and_refuses_what_cannot_be_looked_up() {
    let seeded = seed(full());
    let tx_hash = hex::encode(seeded.tx_hash);
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);

    let r = result_of(&rpc(
        &ctx,
        "f_transactions_by_payment_id_json",
        &format!(r#"{{"paymentId":"{}"}}"#, hex::encode(PAYMENT_ID)),
    ));
    assert_eq!(r.get("status").unwrap().as_str(), Some("OK"));
    let hashes = r.get("transactionHashes").unwrap().as_array().unwrap();
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0].as_str(), Some(tx_hash.as_str()));
    assert_eq!(r.get("totalCount").unwrap().as_u64(), Some(1));
    assert_eq!(r.get("truncated").unwrap().as_bool(), Some(false));

    // A payment id nothing used is an empty answer, not an error: the C++
    // `getTransactionHashesByPaymentId` returns `{}` for a missing count entry.
    let r = result_of(&rpc(
        &ctx,
        "f_transactions_by_payment_id_json",
        &format!(r#"{{"paymentId":"{}"}}"#, "11".repeat(32)),
    ));
    assert!(r.get("transactionHashes").unwrap().as_array().unwrap().is_empty());
    assert_eq!(r.get("totalCount").unwrap().as_u64(), Some(0));

    // A short id is encrypted to its receiver, so there is nothing stable to
    // index. Saying so beats an empty list that reads as "never used".
    let (code, message) =
        error_of(&rpc(&ctx, "f_transactions_by_payment_id_json", r#"{"paymentId":"0123456789abcdef"}"#));
    assert_eq!(code, -1);
    assert!(message.starts_with("Short payment IDs are encrypted"), "{message}");

    let (code, message) = error_of(&rpc(&ctx, "f_transactions_by_payment_id_json", r#"{"paymentId":"nope"}"#));
    assert_eq!(code, -1);
    assert_eq!(message, "Payment ID specified is not 64 valid hex characters!");
}

/// The answer is bounded by `--rpc-max-block-count`, and the caller is told it
/// was cut rather than being handed a short list that looks complete.
#[test]
fn a_reused_payment_id_is_truncated_and_says_so() {
    let seeded = seed(full());
    let mut store = seeded.chain.into_store();
    // Ten transactions under one payment id. Only the index record matters
    // here: the handler reads it and never opens the transactions.
    let many: Vec<Hash> = (0..10u8).map(|i| wrkz_pow::cn_fast_hash(&[i, 0x77])).collect();
    store.write_batch(vec![(keys::payment_id(&PAYMENT_ID), Some(records::encode_hashes(&many)))]).unwrap();
    let chain = ChainState::open(store, full(), Checkpoints::none()).unwrap();

    let node = ChainNode::standalone(chain, TransactionPool::new(Default::default()));
    let ctx = Context::new(
        Arc::new(node),
        ServerConfig { mode: RpcMode::Explorer, max_block_count: 4, ..ServerConfig::default() },
    );
    let r = result_of(&rpc(
        &ctx,
        "f_transactions_by_payment_id_json",
        &format!(r#"{{"paymentId":"{}"}}"#, hex::encode(PAYMENT_ID)),
    ));
    assert_eq!(r.get("transactionHashes").unwrap().as_array().unwrap().len(), 4, "cut to the cap");
    assert_eq!(r.get("totalCount").unwrap().as_u64(), Some(10), "the count is the one before the cut");
    assert_eq!(r.get("truncated").unwrap().as_bool(), Some(true));
}

// ---------------------------------------------------------------------------
// /queryblocksdetailed
// ---------------------------------------------------------------------------

fn u64_of(j: &Json, key: &str) -> u64 {
    j.get(key).unwrap_or_else(|| panic!("no {key}")).as_u64().unwrap_or_else(|| panic!("{key} is not a number"))
}

fn str_of<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key).unwrap_or_else(|| panic!("no {key}")).as_str().unwrap_or_else(|| panic!("{key} is not a string"))
}

/// Every block and transaction field `RpcServer.cpp:2784-2926` prints, with the
/// value `Core::getBlockDetails` and `Core::getTransactionDetails` compute.
#[test]
fn queryblocksdetailed_prints_every_block_and_transaction_field() {
    let seeded = seed(full());
    let block_30 = seeded.chain.block_info(30).unwrap().unwrap().block_hash;
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);
    let body = format!(r#"{{"blockIds":["{}"],"timestamp":0,"blockCount":10}}"#, hex::encode(block_30));
    let r = ok_body(&post(&ctx, "/queryblocksdetailed", &body));
    assert_eq!(str_of(&r, "status"), "OK");
    // The caller holds block 30. The whole chain is inside one UTC day, so the
    // timestamp bound is 0 and the offset is the start itself.
    assert_eq!(u64_of(&r, "startHeight"), 30);
    assert_eq!(u64_of(&r, "currentHeight"), TOP as u64);
    assert_eq!(u64_of(&r, "fullOffset"), 30);
    let blocks = r.get("blocks").unwrap().as_array().unwrap();
    assert_eq!(blocks.len(), 10, "min(blockCount, top - fullOffset + 1)");
    assert_eq!(str_of(&blocks[0], "hash"), hex::encode(block_30));

    // The block with the payment transaction.
    let b = &blocks[(TX_BLOCK - 30) as usize];
    assert_eq!(str_of(b, "hash"), hex::encode(seeded.tx_block_hash));
    assert_eq!(u64_of(b, "index"), TX_BLOCK as u64);
    assert_eq!(u64_of(b, "major_version"), 1);
    assert_eq!(u64_of(b, "minor_version"), 0);
    assert_eq!(u64_of(b, "timestamp"), BASE_TIME + TX_BLOCK as u64 * 60);
    assert_eq!(u64_of(b, "nonce"), TX_BLOCK as u64);
    assert_eq!(u64_of(b, "difficulty"), 1_000, "the block's own difficulty, not the cumulative");
    assert_eq!(u64_of(b, "reward"), COINBASE_AMOUNT);
    assert_eq!(
        str_of(b, "alreadyGeneratedCoins"),
        (36 * COINBASE_AMOUNT).to_string(),
        "a string, as the C++ prints it"
    );
    assert_eq!(u64_of(b, "alreadyGeneratedTransactions"), 37, "35 coinbases below, then a coinbase and a transfer");
    assert_eq!(u64_of(b, "totalFeeAmount"), TX_FEE);
    assert_eq!(str_of(b, "prevBlockHash"), str_of(&blocks[(TX_BLOCK - 31) as usize], "hash"));

    // Every coinbase here serialises to the same length, which is therefore
    // the median, and the stored size is that plus the transfer.
    let coinbase_len = coinbase(0).to_bytes().unwrap().len() as u64;
    let transfer_len = payment_transaction().to_bytes().unwrap().len() as u64;
    assert_eq!(u64_of(b, "transactionsCumulativeSize"), coinbase_len + transfer_len);
    assert_eq!(u64_of(b, "sizeMedian"), coinbase_len);
    let base =
        wrkz_chain::reward::get_block_reward(1, coinbase_len, 0, TX_BLOCK as u64 * COINBASE_AMOUNT, 0, TX_BLOCK as u64)
            .unwrap()
            .reward;
    assert_eq!(u64_of(b, "baseReward"), base);
    // `Core.cpp:4752`: the block blob plus the transactions, the coinbase once.
    let previous: Hash = hex::decode(str_of(b, "prevBlockHash")).unwrap().try_into().unwrap();
    let blob_len = block_at(TX_BLOCK, previous, &[payment_transaction()]).to_bytes().unwrap().len() as u64;
    assert_eq!(u64_of(b, "blockSize"), blob_len + coinbase_len + transfer_len - coinbase_len);

    let txs = b.get("transactions").unwrap().as_array().unwrap();
    assert_eq!(txs.len(), 2, "the coinbase, then the transfer");

    let cb = &txs[0];
    assert_eq!(str_of(cb, "hash"), hex::encode(seeded.coinbase_hash));
    assert_eq!(str_of(cb, "blockHash"), hex::encode(seeded.tx_block_hash));
    assert_eq!(u64_of(cb, "blockIndex"), TX_BLOCK as u64);
    assert_eq!(cb.get("inBlockchain").unwrap().as_bool(), Some(true));
    assert_eq!(u64_of(cb, "fee"), 0);
    assert_eq!(u64_of(cb, "totalInputsAmount"), 0, "a base input counts nothing");
    assert_eq!(u64_of(cb, "totalOutputsAmount"), COINBASE_AMOUNT);
    assert_eq!(u64_of(cb, "mixin"), 0);
    assert_eq!(u64_of(cb, "unlockTime"), TX_BLOCK as u64 + 40);
    assert_eq!(u64_of(cb, "timestamp"), BASE_TIME + TX_BLOCK as u64 * 60);
    assert_eq!(u64_of(cb, "size"), coinbase_len);
    assert_eq!(u64_of(cb, "signaturesSize"), 0, "a lone base input deserialises with no signature vector");
    assert!(cb.get("signatures").unwrap().as_array().unwrap().is_empty());
    assert_eq!(str_of(cb, "paymentId"), "00".repeat(32));
    let extra = cb.get("extra").unwrap();
    assert_eq!(str_of(extra, "publicKey"), hex::encode(key(0xC0 ^ TX_BLOCK as u8)));
    assert!(extra.get("nonce").unwrap().as_array().unwrap().is_empty());
    assert_eq!(str_of(extra, "raw"), hex::encode(&coinbase(TX_BLOCK).prefix.extra));
    let input = &cb.get("inputs").unwrap().as_array().unwrap()[0];
    assert_eq!(str_of(input, "type"), "ff");
    assert_eq!(u64_of(input.get("data").unwrap(), "amount"), COINBASE_AMOUNT);
    assert_eq!(u64_of(input.get("data").unwrap().get("input").unwrap(), "height"), TX_BLOCK as u64);
    let output = &cb.get("outputs").unwrap().as_array().unwrap()[0];
    assert_eq!(u64_of(output, "globalIndex"), TX_BLOCK as u64, "one coinbase output per block before it");
    let inner = output.get("output").unwrap();
    assert_eq!(u64_of(inner, "amount"), COINBASE_AMOUNT);
    assert_eq!(str_of(inner.get("target").unwrap(), "type"), "02");
    assert_eq!(str_of(inner.get("target").unwrap().get("data").unwrap(), "key"), hex::encode(key(TX_BLOCK as u8)));

    let tx = &txs[1];
    assert_eq!(str_of(tx, "hash"), hex::encode(seeded.tx_hash));
    assert_eq!(u64_of(tx, "fee"), TX_FEE);
    assert_eq!(u64_of(tx, "totalInputsAmount"), COINBASE_AMOUNT);
    assert_eq!(u64_of(tx, "totalOutputsAmount"), COINBASE_AMOUNT - TX_FEE);
    assert_eq!(u64_of(tx, "mixin"), 4, "the ring size");
    assert_eq!(u64_of(tx, "size"), transfer_len);
    assert_eq!(str_of(tx, "paymentId"), hex::encode(PAYMENT_ID));
    let extra = tx.get("extra").unwrap();
    assert_eq!(str_of(extra, "publicKey"), hex::encode(key(0xB0)));
    let nonce: Vec<u64> = extra.get("nonce").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    let expected: Vec<u64> =
        build_nonce(Some(&PaymentId::Long(PAYMENT_ID)), None).iter().map(|b| u64::from(*b)).collect();
    assert_eq!(nonce, expected, "the nonce field's bytes as numbers, sub-tag included");
    assert_eq!(u64_of(tx, "signaturesSize"), 1, "one vector per input");
    let sigs = tx.get("signatures").unwrap().as_array().unwrap();
    assert_eq!(sigs.len(), 4, "one entry per ring member");
    assert_eq!(u64_of(&sigs[3], "first"), 0, "the input the signature belongs to");
    assert_eq!(str_of(&sigs[3], "second"), "00".repeat(64));

    let input = &tx.get("inputs").unwrap().as_array().unwrap()[0];
    assert_eq!(str_of(input, "type"), "02");
    let data = input.get("data").unwrap();
    assert_eq!(u64_of(data, "mixin"), 4);
    let inner = data.get("input").unwrap();
    assert_eq!(u64_of(inner, "amount"), COINBASE_AMOUNT);
    assert_eq!(str_of(inner, "k_image"), hex::encode(key(0xAA)));
    let offsets: Vec<u64> =
        inner.get("key_offsets").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    assert_eq!(offsets, vec![7, 3, 11, 2], "relative, as carried");
    // Absolute 7, 10, 21, 23: the last ring member is global index 23, the
    // coinbase output of block 23.
    let reference = data.get("output").unwrap();
    assert_eq!(str_of(reference, "transactionHash"), hex::encode(coinbase(23).hash().unwrap()));
    assert_eq!(u64_of(reference, "number"), 0);
    let outputs = tx.get("outputs").unwrap().as_array().unwrap();
    assert_eq!(outputs.len(), 2);
    assert_eq!(u64_of(outputs[0].get("output").unwrap(), "amount"), COINBASE_AMOUNT - TX_FEE - 500);
    assert_eq!(u64_of(&outputs[1], "globalIndex"), 0, "as the seeded per-block record says");
}

/// Below `fullOffset` a block is its hash and zeros (`Core::pushBlockHashes`),
/// and `blockCount` is the C++'s: 1 is raised to 2, and a value that does not
/// fit the `uint32_t` it is handed wraps — here to 0, the 10,000 default.
#[test]
fn queryblocksdetailed_sends_hashes_below_the_offset_and_clamps_the_count() {
    // A lite node from 20: a sync below its line is answered with hashes up to
    // the line, as for `queryblockslite`.
    let seeded = seed(lite(20));
    let hashes: Vec<Hash> = (0..=TOP).map(|i| seeded.chain.block_info(i).unwrap().unwrap().block_hash).collect();
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);

    let r = ok_body(&post(&ctx, "/queryblocksdetailed", r#"{"blockIds":[],"timestamp":0,"blockCount":1}"#));
    assert_eq!(u64_of(&r, "startHeight"), 0);
    assert_eq!(u64_of(&r, "fullOffset"), 20);
    let blocks = r.get("blocks").unwrap().as_array().unwrap();
    assert_eq!(blocks.len(), 2, "blockCount 1 is raised to 2, and 2 hashes do not reach the offset");
    for (i, b) in blocks.iter().enumerate() {
        assert_eq!(str_of(b, "hash"), hex::encode(hashes[i]));
        assert_eq!(u64_of(b, "index"), 0, "a hash-only entry is otherwise the struct's zeros");
        assert_eq!(u64_of(b, "reward"), 0);
        assert_eq!(str_of(b, "alreadyGeneratedCoins"), "0");
        assert_eq!(str_of(b, "prevBlockHash"), "00".repeat(32));
        assert!(b.get("transactions").unwrap().as_array().unwrap().is_empty());
    }

    let r = ok_body(&post(&ctx, "/queryblocksdetailed", r#"{"blockIds":[],"blockCount":4294967296}"#));
    let blocks = r.get("blocks").unwrap().as_array().unwrap();
    assert_eq!(blocks.len(), 20 + 21, "the 20 hashes below the line, then every block from it to the top");
    assert!(blocks[19].get("transactions").unwrap().as_array().unwrap().is_empty());
    assert_eq!(u64_of(&blocks[20], "index"), 20);
    assert_eq!(blocks[20].get("transactions").unwrap().as_array().unwrap().len(), 1);
    assert_eq!(u64_of(&blocks[40], "index"), TOP as u64);

    let res = post(&ctx, "/queryblocksdetailed", r#"{"blockIds":["zz"]}"#);
    assert_eq!(res.status, 400);
    assert_eq!(str_of(&json(&res), "error"), "Block hash specified is not a valid hex string!");
}

// ---------------------------------------------------------------------------
// the refusals
// ---------------------------------------------------------------------------

/// A lite node answers for the region it kept and reports an error below it.
/// The error must name the mode: "no such block" would be a lie about a block
/// this node has the index of and follows the chain through.
#[test]
fn a_lite_node_answers_above_its_line_and_reports_an_error_below_it() {
    let line = TX_BLOCK;
    let seeded = seed(lite(line));
    // The blocks below the line were seeded with bodies — this is a state that
    // was *converted*, which is the harshest case: the bytes are on disk and
    // the policy still has to refuse them, or two nodes with the same flag
    // would answer differently.
    let above = hex::encode(seeded.chain.block_info(line).unwrap().unwrap().block_hash);
    let below = hex::encode(seeded.chain.block_info(line - 1).unwrap().unwrap().block_hash);
    let ctx = ctx_over(seeded.chain, RpcMode::Explorer);

    let r = result_of(&rpc(&ctx, "f_block_json", &format!(r#"{{"hash":"{above}"}}"#)));
    assert_eq!(r.get("block").unwrap().get("height").unwrap().as_u64(), Some(line as u64));

    let res = rpc(&ctx, "f_block_json", &format!(r#"{{"hash":"{below}"}}"#));
    assert_eq!(res.status, 500, "the C++ answers a body it cannot read with a 500");
    let body = String::from_utf8_lossy(&res.body).to_string();
    assert!(body.contains("lite node"), "the error names the mode: {body}");
    assert!(body.contains(&line.to_string()), "and the height its data starts at: {body}");
    assert!(!body.contains("wasn't found"), "it must not read as 'no such block': {body}");

    // `f_blocks_list_json` walks 31 blocks and so crosses the line from any
    // height near it: it must fail rather than return a short or wrong list.
    let res = rpc(&ctx, "f_blocks_list_json", &format!(r#"{{"height":{TOP}}}"#));
    assert_eq!(res.status, 500);
    assert!(String::from_utf8_lossy(&res.body).contains("lite node"));
}

/// `/info`'s three mode fields are the node's own configuration, so `lite`,
/// `lite_start_height`, `pruned` and `prune_depth` describe the database that
/// is actually running rather than a default.
#[test]
fn info_reports_the_mode_the_node_is_actually_in() {
    let get_info = |cfg: Config| {
        let ctx = ctx_over(seed(cfg).chain, RpcMode::Standard);
        let res = dispatch(
            &ctx,
            &Request {
                method: "GET".into(),
                path: "/info".into(),
                query: String::new(),
                version: "HTTP/1.1".into(),
                headers: Vec::new(),
                body: Vec::new(),
            },
            "127.0.0.1",
        );
        assert_eq!(res.status, 200);
        json(&res)
    };

    let i = get_info(full());
    assert_eq!(i.get("lite").unwrap().as_bool(), Some(false));
    assert_eq!(i.get("lite_start_height").unwrap().as_u64(), Some(0));
    assert_eq!(i.get("pruned").unwrap().as_bool(), Some(false));

    let i = get_info(lite(TX_BLOCK));
    assert_eq!(i.get("lite").unwrap().as_bool(), Some(true));
    assert_eq!(i.get("lite_start_height").unwrap().as_u64(), Some(TX_BLOCK as u64));

    let depth = wrkz_chain::MIN_PRUNE_DEPTH;
    let i = get_info(Config { prune_depth: Some(depth), ..serving_config() });
    assert_eq!(i.get("pruned").unwrap().as_bool(), Some(true));
    assert_eq!(i.get("prune_depth").unwrap().as_u64(), Some(depth as u64));
    // A window deeper than the chain leaves the floor at 0: nothing to drop.
    assert_eq!(i.get("lite_start_height").unwrap().as_u64(), Some(0));
}
