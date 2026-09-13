// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The RPC over a real [`ChainState`] and [`TransactionPool`]:
//!
//! - the captured `/getwalletsyncdata` vector reproduced from the captured raw
//!   block, byte for byte;
//! - `getblockheaderbyheight` against the conformance row of spec/09;
//! - the global-index endpoints against `mainnet_get_global_indexes_for_range.json`;
//! - `getblocktemplate` → `submitblock` → the chain grows, over a chain built
//!   from genesis by this crate's own handlers.
//!
//! The chain is seeded the way `crates/wrkz-chain/tests/synthetic.rs` seeds
//! one: the records [`ChainState`] itself writes, put straight into a
//! [`MemStore`], because a real chain of 4.2 million blocks cannot be built in
//! a test and no smaller height carries the captured data.

use std::sync::Arc;
use wrkz_chain::records::BlockInfo;
use wrkz_chain::{keys, records, ChainState, Checkpoints};
use wrkz_mempool::TransactionPool;
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::Hash;
use wrkz_rpc::http::{Request, Response};
use wrkz_rpc::json::{parse, Json, ParseLimits};
use wrkz_rpc::node::{serving_config, ChainNode};
use wrkz_rpc::server::{dispatch, Context, ServerConfig};
use wrkz_storage::{KvStore, MemStore};

// ---------------------------------------------------------------------------
// the captured block
// ---------------------------------------------------------------------------

const VECTOR_INDEX: u32 = 4_213_000;
/// spec/09's conformance row for 4,213,000.
const VECTOR_HASH: &str = "c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604";
const VECTOR_PREV: &str = "3d3c595b6f7dd2a02c12b1b601c3e563894083de8cfd5ce7bad052bc907cd678";
const VECTOR_TIMESTAMP: u64 = 1_788_894_799;
const VECTOR_DIFFICULTY: u64 = 24_880_685;
const VECTOR_NONCE: u64 = 7822;
const VECTOR_REWARD: u64 = 1_000_000;
const VECTOR_BLOCK_SIZE: u64 = 211;
/// The global index the coinbase's one output got (`mainnet_get_global_indexes_for_range.json`).
const VECTOR_GLOBAL_INDEX: u32 = 3_808_773;
/// The running transaction count `/info` is expected to report as `tx_count`.
const VECTOR_TX_COUNT: u64 = 3_510_834;

fn vectors() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

fn read_vector(name: &str) -> Json {
    let raw = std::fs::read(vectors().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    parse(&raw, ParseLimits { max_bytes: 64 * 1024 * 1024, max_depth: 64 }).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The block blob of 4,213,000, from `mainnet_rawblocks_4213000_v7.json`.
fn captured_block() -> (BlockTemplate, Vec<u8>) {
    let v = read_vector("mainnet_rawblocks_4213000_v7.json");
    let hex_blob = v.get("items").unwrap().as_array().unwrap()[0].get("block").unwrap().as_str().unwrap();
    let blob = hex::decode(hex_blob).expect("the vector's block is hex");
    let block = BlockTemplate::from_bytes(&blob).expect("the captured block parses");
    (block, blob)
}

/// A [`ChainState`] whose tip is the captured block 4,213,000, with enough
/// history behind it for the reward and difficulty windows.
fn seeded_chain() -> ChainState<MemStore> {
    let (block, blob) = captured_block();
    assert_eq!(hex::encode(block.hash().unwrap()), VECTOR_HASH, "the captured block hashes to its published id");

    let coinbase_hash = block.base_transaction.hash().expect("the coinbase hashes");
    let coinbase_size = block.base_transaction.to_bytes().unwrap().len() as u32;
    let tip_hash: Hash = block.hash().unwrap();

    const HISTORY: u32 = 300;
    let base = VECTOR_INDEX - HISTORY;
    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut cumulative = 10_000_000_000_000u64;
    for i in base..VECTOR_INDEX {
        let info = BlockInfo {
            block_hash: wrkz_pow::cn_fast_hash(&i.to_le_bytes()),
            timestamp: VECTOR_TIMESTAMP - (VECTOR_INDEX - i) as u64 * 60,
            block_size: coinbase_size,
            cumulative_difficulty: cumulative,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: VECTOR_TX_COUNT + i as u64,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(i), Some(records::encode_hashes(&[info.block_hash]))));
        cumulative += VECTOR_DIFFICULTY;
    }
    // The captured tip. `block_size` is the *stored cumulative* size, which for
    // an empty block is the coinbase's own size — the header handler adds the
    // block blob and subtracts the coinbase again, reaching 211.
    let tip = BlockInfo {
        block_hash: tip_hash,
        timestamp: VECTOR_TIMESTAMP,
        block_size: coinbase_size,
        cumulative_difficulty: cumulative,
        already_generated_coins: 30_000_000_000_000 + VECTOR_REWARD,
        already_generated_transactions: VECTOR_TX_COUNT + VECTOR_INDEX as u64 + 1,
    };
    ops.push((keys::block_info(VECTOR_INDEX), Some(tip.encode())));
    ops.push((keys::hash_to_index(&tip_hash), Some(VECTOR_INDEX.to_le_bytes().to_vec())));
    ops.push((keys::block_tx_hashes(VECTOR_INDEX), Some(records::encode_hashes(&[coinbase_hash]))));
    ops.push((keys::transaction_index(&coinbase_hash), Some(VECTOR_INDEX.to_le_bytes().to_vec())));
    ops.push((keys::raw_block(VECTOR_INDEX), Some(records::encode_raw_block(&blob, &[]))));
    // The one output the coinbase created, and the record the global-index
    // endpoints read.
    ops.push((
        keys::block_outputs(VECTOR_INDEX),
        Some(records::encode_output_refs(&[(VECTOR_REWARD, VECTOR_GLOBAL_INDEX)])),
    ));
    ops.push((keys::output_count(VECTOR_REWARD), Some((VECTOR_GLOBAL_INDEX + 1).to_le_bytes().to_vec())));

    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(VECTOR_INDEX.to_le_bytes().to_vec())));

    let mut store = MemStore::default();
    store.write_batch(ops).unwrap();
    let mut chain = ChainState::open(store, serving_config(), Checkpoints::mainnet()).expect("the seeded state opens");
    chain.set_clock(Some(VECTOR_TIMESTAMP + 60));
    assert_eq!(chain.tip_index(), Some(VECTOR_INDEX));
    chain
}

fn ctx_over(chain: ChainState<MemStore>) -> Context {
    let node = ChainNode::standalone(chain, TransactionPool::new(Default::default()));
    Context::new(Arc::new(node), ServerConfig::default())
}

fn request(method: &str, path: &str, body: &str) -> Request {
    Request {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    }
}

fn call(ctx: &Context, method: &str, path: &str, body: &str) -> Response {
    dispatch(ctx, &request(method, path, body), "127.0.0.1")
}

fn json(res: &Response) -> Json {
    parse(&res.body, ParseLimits { max_bytes: 64 * 1024 * 1024, max_depth: 64 })
        .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", String::from_utf8_lossy(&res.body)))
}

// ---------------------------------------------------------------------------
// the captured vectors
// ---------------------------------------------------------------------------

/// `spec/vectors/mainnet_getwalletsyncdata_4213000.json`, reproduced.
///
/// The captured response covers 4,213,000 **and** 4,213,001, and `vectors/`
/// holds the raw block of the first only — there is no way to put 4,213,001
/// into a chain state without its bytes, because its `blockHash` is a hash of
/// them. So this seeds the block we have, asks for the same window, and
/// requires `items[0]` to be **byte-identical** to the captured `items[0]`:
/// every field name, every value, and the key order.
#[test]
fn getwalletsyncdata_reproduces_the_captured_block_exactly() {
    let ctx = ctx_over(seeded_chain());
    let res = call(
        &ctx,
        "POST",
        "/getwalletsyncdata",
        &format!(
            r#"{{"blockHashCheckpoints":[],"startHeight":{VECTOR_INDEX},"startTimestamp":0,"blockCount":2,"skipCoinbaseTransactions":false}}"#
        ),
    );
    assert_eq!(res.status, 200);
    let ours = json(&res);
    let captured = read_vector("mainnet_getwalletsyncdata_4213000.json");

    let ours_items = ours.get("items").unwrap().as_array().unwrap();
    let captured_items = captured.get("items").unwrap().as_array().unwrap();
    assert_eq!(ours_items.len(), 1, "only 4,213,000 is in this state");
    assert_eq!(captured_items.len(), 2);
    assert_eq!(
        ours_items[0].to_string(),
        captured_items[0].to_string(),
        "items[0] must be byte-identical to the captured response"
    );

    // And the envelope around it.
    assert_eq!(ours.get("status").unwrap().as_str(), Some("OK"));
    assert_eq!(ours.get("synced").unwrap().as_bool(), Some(false));
    assert_eq!(ours.get("scannedToHeight").unwrap().as_u64(), Some(VECTOR_INDEX as u64));
    assert!(!ours.has("topBlock"), "topBlock only appears once the caller is at the top");
    // The captured envelope says 4,213,001 because that daemon had the next
    // block; the fields are otherwise the same set.
    assert_eq!(captured.get("scannedToHeight").unwrap().as_u64(), Some(VECTOR_INDEX as u64 + 1));
    assert!(wrkz_rpc::diff::Comparison::default().compare(&captured, &ours).is_empty(), "same shape");
}

/// The same request with `skipCoinbaseTransactions`, which is what a wallet
/// that has already scanned the coinbase sends.
#[test]
fn getwalletsyncdata_drops_the_coinbase_when_asked() {
    let ctx = ctx_over(seeded_chain());
    let res = call(
        &ctx,
        "POST",
        "/getwalletsyncdata",
        &format!(r#"{{"startHeight":{VECTOR_INDEX},"blockCount":2,"skipCoinbaseTransactions":true}}"#),
    );
    let j = json(&res);
    let item = &j.get("items").unwrap().as_array().unwrap()[0];
    assert!(!item.has("coinbaseTX"));
    assert_eq!(item.get("transactions").unwrap().as_array().unwrap().len(), 0);

    // With `skipEmptyBlocks` as well, an empty block is scanned past rather
    // than sent — but the response still says how far it looked, or the wallet
    // would read the gap as "synced".
    let res = call(
        &ctx,
        "POST",
        "/getwalletsyncdata",
        &format!(
            r#"{{"startHeight":{VECTOR_INDEX},"blockCount":2,"skipCoinbaseTransactions":true,"skipEmptyBlocks":true}}"#
        ),
    );
    let j = json(&res);
    assert_eq!(j.get("scannedToHeight").unwrap().as_u64(), Some(VECTOR_INDEX as u64));
    let items = j.get("items").unwrap().as_array().unwrap();
    assert_eq!(items.len(), 1, "the last scanned height is always reported");
    assert_eq!(items[0].get("blockHeight").unwrap().as_u64(), Some(VECTOR_INDEX as u64));
}

/// spec/09's conformance row for index 4,213,000.
#[test]
fn getblockheaderbyheight_matches_the_conformance_row() {
    let ctx = ctx_over(seeded_chain());
    let res = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"getblockheaderbyheight","params":{{"height":{VECTOR_INDEX}}}}}"#
        ),
    );
    let h = json(&res).get("result").unwrap().get("block_header").unwrap().clone();
    assert_eq!(h.get("height").unwrap().as_u64(), Some(VECTOR_INDEX as u64));
    assert_eq!(h.get("hash").unwrap().as_str(), Some(VECTOR_HASH));
    assert_eq!(h.get("prev_hash").unwrap().as_str(), Some(VECTOR_PREV));
    assert_eq!(h.get("major_version").unwrap().as_u64(), Some(7));
    assert_eq!(h.get("minor_version").unwrap().as_u64(), Some(0));
    assert_eq!(h.get("nonce").unwrap().as_u64(), Some(VECTOR_NONCE));
    assert_eq!(h.get("timestamp").unwrap().as_u64(), Some(VECTOR_TIMESTAMP));
    assert_eq!(h.get("difficulty").unwrap().as_u64(), Some(VECTOR_DIFFICULTY));
    assert_eq!(h.get("reward").unwrap().as_u64(), Some(VECTOR_REWARD));
    assert_eq!(h.get("block_size").unwrap().as_u64(), Some(VECTOR_BLOCK_SIZE));
    assert_eq!(h.get("num_txes").unwrap().as_u64(), Some(1), "an empty block still counts its coinbase");
    assert_eq!(h.get("depth").unwrap().as_u64(), Some(0));
    assert_eq!(h.get("orphan_status").unwrap().as_bool(), Some(false));

    // The same header by hash.
    let by_hash = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"getblockheaderbyhash","params":{{"hash":"{VECTOR_HASH}"}}}}"#),
    );
    assert_eq!(json(&by_hash).get("result").unwrap().get("block_header").unwrap(), &h);
    // And as the last block header.
    let last = call(&ctx, "POST", "/json_rpc", r#"{"jsonrpc":"2.0","id":1,"method":"getlastblockheader","params":{}}"#);
    assert_eq!(json(&last).get("result").unwrap().get("block_header").unwrap(), &h);
}

/// `spec/vectors/mainnet_get_global_indexes_for_range.json`, reproduced.
#[test]
fn the_global_index_endpoints_reproduce_the_captured_vector() {
    let ctx = ctx_over(seeded_chain());
    let res = call(
        &ctx,
        "POST",
        "/get_global_indexes_for_range",
        &format!(r#"{{"startHeight":{VECTOR_INDEX},"endHeight":{}}}"#, VECTOR_INDEX + 1),
    );
    let ours = json(&res);
    let captured = read_vector("mainnet_get_global_indexes_for_range.json");
    assert_eq!(ours.to_string(), captured.to_string(), "byte-identical to the captured response");

    // The deprecated per-transaction form answers the same index.
    let coinbase_hash = captured.get("indexes").unwrap().as_array().unwrap()[0].get("key").unwrap().as_str().unwrap();
    let res = call(&ctx, "POST", "/get_o_indexes", &format!(r#"{{"txid":"{coinbase_hash}"}}"#));
    assert_eq!(
        String::from_utf8(res.body).unwrap(),
        format!(r#"{{"o_indexes":[{VECTOR_GLOBAL_INDEX}],"status":"OK"}}"#)
    );
}

/// `/getrawblocks` gives back exactly the bytes the vector was captured from.
#[test]
fn getrawblocks_returns_the_captured_bytes() {
    let ctx = ctx_over(seeded_chain());
    let res = call(
        &ctx,
        "POST",
        "/getrawblocks",
        &format!(r#"{{"startHeight":{VECTOR_INDEX},"blockCount":1,"skipCoinbaseTransactions":false}}"#),
    );
    let ours = json(&res);
    let captured = read_vector("mainnet_rawblocks_4213000_v7.json");
    assert_eq!(
        ours.get("items").unwrap().as_array().unwrap()[0].to_string(),
        captured.get("items").unwrap().as_array().unwrap()[0].to_string()
    );
    assert_eq!(ours.get("status").unwrap().as_str(), Some("OK"));
    assert_eq!(ours.get("synced").unwrap().as_bool(), Some(false));
}

/// `/info` and `/height` over the seeded chain: counts, not indexes.
#[test]
fn info_and_height_report_counts_over_a_real_chain() {
    let ctx = ctx_over(seeded_chain());
    let j = json(&call(&ctx, "GET", "/info", ""));
    assert_eq!(j.get("height").unwrap().as_u64(), Some(VECTOR_INDEX as u64 + 1));
    assert_eq!(j.get("top_block_hash").unwrap().as_str(), Some(VECTOR_HASH));
    assert_eq!(j.get("tx_count").unwrap().as_u64(), Some(VECTOR_TX_COUNT), "coinbases excluded");
    assert_eq!(j.get("major_version").unwrap().as_u64(), Some(7));
    assert_eq!(j.get("tx_pool_size").unwrap().as_u64(), Some(0));
    assert_eq!(j.get("alt_blocks_count").unwrap().as_u64(), Some(0));
    // The next block's difficulty, from the seeded window.
    assert!(j.get("difficulty").unwrap().as_u64().unwrap() > 0);

    let j = json(&call(&ctx, "GET", "/height", ""));
    assert_eq!(j.get("height").unwrap().as_u64(), Some(VECTOR_INDEX as u64 + 1));
    let j = json(&call(&ctx, "POST", "/json_rpc", r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount"}"#));
    assert_eq!(j.get("result").unwrap().get("count").unwrap().as_u64(), Some(VECTOR_INDEX as u64 + 1));
}

/// `/get_transactions_status` over the transaction index the chain state keeps.
#[test]
fn get_transactions_status_finds_a_mined_transaction() {
    let ctx = ctx_over(seeded_chain());
    let captured = read_vector("mainnet_get_global_indexes_for_range.json");
    let coinbase = captured.get("indexes").unwrap().as_array().unwrap()[0].get("key").unwrap().as_str().unwrap();
    let unknown = "0000000000000000000000000000000000000000000000000000000000000001";
    let res = call(
        &ctx,
        "POST",
        "/get_transactions_status",
        &format!(r#"{{"transactionHashes":["{coinbase}","{unknown}"]}}"#),
    );
    let j = json(&res);
    assert_eq!(j.get("transactionsInBlock").unwrap().as_array().unwrap()[0].as_str(), Some(coinbase));
    assert_eq!(j.get("transactionsInPool").unwrap().as_array().unwrap().len(), 0);
    assert_eq!(j.get("transactionsUnknown").unwrap().as_array().unwrap()[0].as_str(), Some(unknown));
}

// ---------------------------------------------------------------------------
// mining: template -> submit -> the chain grows
// ---------------------------------------------------------------------------

/// A chain from genesis, so a block can actually be mined into it: block 1 is
/// major version 1 at difficulty 1, which any proof of work satisfies.
fn genesis_ctx() -> Context {
    let chain =
        ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).expect("genesis");
    let node = ChainNode::standalone(chain, TransactionPool::new(Default::default()));
    Context::new(Arc::new(node), ServerConfig::default())
}

const MINER_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

#[test]
fn a_block_mined_through_getblocktemplate_and_submitblock_reaches_the_chain() {
    let ctx = genesis_ctx();
    assert_eq!(ctx.api.top_index(), 0, "only genesis");

    let res = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"getblocktemplate","params":{{"wallet_address":"{MINER_ADDRESS}","reserve_size":8}}}}"#
        ),
    );
    let template = json(&res).get("result").cloned().expect("a template");
    assert_eq!(template.get("status").unwrap().as_str(), Some("OK"));
    assert_eq!(template.get("height").unwrap().as_u64(), Some(1), "a count: the block being built");
    let difficulty = template.get("difficulty").unwrap().as_u64().unwrap();
    let offset = template.get("reserved_offset").unwrap().as_u64().unwrap() as usize;
    let blob = hex::decode(template.get("blocktemplate_blob").unwrap().as_str().unwrap()).unwrap();
    assert_eq!(&blob[offset..offset + 8], &[0u8; 8], "the reserved bytes are where the offset says");

    // The template parses, and its proof of work already clears difficulty 1.
    let parsed = BlockTemplate::from_bytes(&blob).expect("the template parses");
    assert_eq!(parsed.major_version, 1, "block 1 is major version 1");
    assert!(parsed.check_proof_of_work(difficulty).unwrap(), "difficulty 1 is met by any hash");

    let res = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(r#"{{"jsonrpc":"2.0","id":2,"method":"submitblock","params":["{}"]}}"#, hex::encode(&blob)),
    );
    assert_eq!(String::from_utf8(res.body).unwrap(), r#"{"id":2,"jsonrpc":"2.0","result":{"status":"OK"}}"#);
    assert_eq!(ctx.api.top_index(), 1, "the block reached the chain");

    // And it is served back through the ordinary read paths.
    let j = json(&call(&ctx, "GET", "/height", ""));
    assert_eq!(j.get("height").unwrap().as_u64(), Some(2));
    let h = json(&call(&ctx, "POST", "/json_rpc", r#"{"jsonrpc":"2.0","id":3,"method":"getlastblockheader"}"#));
    let header = h.get("result").unwrap().get("block_header").unwrap();
    assert_eq!(header.get("height").unwrap().as_u64(), Some(1));
    assert_eq!(header.get("hash").unwrap().as_str(), Some(hex::encode(parsed.hash().unwrap()).as_str()));
    assert_eq!(header.get("num_txes").unwrap().as_u64(), Some(1));
    assert_eq!(header.get("reward").unwrap().as_u64(), parsed.coinbase_output_total());

    // Submitting the same block again is `ALREADY_EXISTS`, which the C++ counts
    // as the `BLOCK_ADDED` condition and answers OK for.
    let res = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(r#"{{"jsonrpc":"2.0","id":4,"method":"submitblock","params":["{}"]}}"#, hex::encode(&blob)),
    );
    assert_eq!(json(&res).get("result").unwrap().get("status").unwrap().as_str(), Some("OK"));
    assert_eq!(ctx.api.top_index(), 1);
}

/// `RpcServer.cpp:1778-1789`: a block `submitblock` adds to the main chain is
/// announced to peers as a **block**. It used to be queued with the
/// transactions `/sendrawtransaction` accepted, which the daemon loop sends on
/// as `NOTIFY_NEW_TRANSACTIONS` — every peer refused it, and the network heard
/// of the block only at its next timed sync.
#[test]
fn a_mined_block_is_queued_for_announcement_as_a_block() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let chain =
        ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).expect("genesis");
    let woken = Arc::new(AtomicUsize::new(0));
    let node = {
        let woken = Arc::clone(&woken);
        Arc::new(ChainNode::standalone(chain, TransactionPool::new(Default::default())).with_mined_block_hook(
            Box::new(move || {
                woken.fetch_add(1, Ordering::SeqCst);
            }),
        ))
    };
    let ctx = Context::new(Arc::clone(&node) as Arc<dyn wrkz_rpc::NodeApi>, ServerConfig::default());

    let res = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"getblocktemplate","params":{{"wallet_address":"{MINER_ADDRESS}","reserve_size":8}}}}"#
        ),
    );
    let template = json(&res).get("result").cloned().expect("a template");
    let blob = hex::decode(template.get("blocktemplate_blob").unwrap().as_str().unwrap()).unwrap();
    let submit = format!(r#"{{"jsonrpc":"2.0","id":2,"method":"submitblock","params":["{}"]}}"#, hex::encode(&blob));

    let res = call(&ctx, "POST", "/json_rpc", &submit);
    assert_eq!(json(&res).get("result").unwrap().get("status").unwrap().as_str(), Some("OK"));
    let mined = node.take_block_relay_queue();
    assert_eq!(mined.len(), 1, "one block to announce");
    assert_eq!(mined[0].block, blob);
    assert!(mined[0].transactions.is_empty(), "the template carried only its coinbase");
    assert!(node.take_relay_queue().is_empty(), "a block is never relayed as a transaction");
    assert_eq!(woken.load(Ordering::SeqCst), 1, "the daemon loop is woken to announce it");

    // `ALREADY_EXISTS` answers OK, as the C++ does, but announces nothing.
    let res = call(&ctx, "POST", "/json_rpc", &submit);
    assert_eq!(json(&res).get("result").unwrap().get("status").unwrap().as_str(), Some("OK"));
    assert!(node.take_block_relay_queue().is_empty());
    assert_eq!(woken.load(Ordering::SeqCst), 1);
}

#[test]
fn a_block_that_is_not_a_block_is_refused_with_minus_seven() {
    let ctx = genesis_ctx();
    let res = call(&ctx, "POST", "/json_rpc", r#"{"jsonrpc":"2.0","id":1,"method":"submitblock","params":["00"]}"#);
    let e = json(&res).get("error").cloned().unwrap();
    assert_eq!(e.get("code").unwrap(), &Json::I64(-7));
    assert_eq!(e.get("message").unwrap().as_str(), Some("Block not accepted"));
    assert_eq!(ctx.api.top_index(), 0);
}

#[test]
fn sendrawtransaction_puts_a_transaction_through_the_real_pool() {
    let ctx = genesis_ctx();
    // Garbage that is valid hex but not a transaction.
    let res = call(&ctx, "POST", "/sendrawtransaction", r#"{"tx_as_hex":"00"}"#);
    let j = json(&res);
    assert_eq!(res.status, 200);
    assert_eq!(j.get("status").unwrap().as_str(), Some("Failed"));
    assert_eq!(j.get("error").unwrap().as_str(), Some("Could not deserialize transaction"));
    assert!(j.has("transactionHash"), "the hash is reported once the hex parsed");
    // Nothing reached the pool, so nothing is queued for relay.
    let j = json(&call(&ctx, "GET", "/info", ""));
    assert_eq!(j.get("tx_pool_size").unwrap().as_u64(), Some(0));
}

/// The explorer routes over a real chain, in explorer mode.
#[test]
fn f_block_json_describes_the_captured_block() {
    let node = ChainNode::standalone(seeded_chain(), TransactionPool::new(Default::default()));
    let ctx =
        Context::new(Arc::new(node), ServerConfig { mode: wrkz_rpc::server::RpcMode::Explorer, ..Default::default() });
    let res = call(
        &ctx,
        "POST",
        "/json_rpc",
        &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"f_block_json","params":{{"hash":"{VECTOR_HASH}"}}}}"#),
    );
    let b = json(&res).get("result").unwrap().get("block").unwrap().clone();
    assert_eq!(b.get("hash").unwrap().as_str(), Some(VECTOR_HASH));
    assert_eq!(b.get("height").unwrap().as_u64(), Some(VECTOR_INDEX as u64));
    assert_eq!(b.get("blockSize").unwrap().as_u64(), Some(VECTOR_BLOCK_SIZE));
    assert_eq!(b.get("reward").unwrap().as_u64(), Some(VECTOR_REWARD));
    assert_eq!(b.get("totalFeeAmount").unwrap().as_u64(), Some(0));
    assert_eq!(b.get("transactions").unwrap().as_array().unwrap().len(), 1, "the coinbase");
    // `alreadyGeneratedCoins` is a string, as `std::to_string` emits it.
    assert_eq!(b.get("alreadyGeneratedCoins").unwrap().type_name(), "string");
    // The effective median is at least the granted full reward zone of v7.
    assert_eq!(b.get("effectiveSizeMedian").unwrap().as_u64(), Some(100_000));
}
