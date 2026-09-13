// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Every route, driven from a fake node: the documented shape of each answer,
//! the transport rules of spec/09, and one test per error path.
//!
//! These run through [`wrkz_rpc::server::dispatch`], which is the whole
//! middleware and the whole route table — the only thing missing is the socket,
//! which `transport.rs` covers.

mod fake;

use fake::{h, FakeNode};
use std::sync::Arc;
use wrkz_rpc::api::NodeApi;
use wrkz_rpc::http::{Request, Response};
use wrkz_rpc::json::{parse, Json, ParseLimits};
use wrkz_rpc::server::{dispatch, Context, RpcMode, ServerConfig};

fn ctx_with(api: Arc<dyn NodeApi>, config: ServerConfig) -> Context {
    Context::new(api, config)
}

fn ctx() -> Context {
    ctx_with(Arc::new(FakeNode::default()), ServerConfig::default())
}

fn request(method: &str, path: &str, body: &str) -> Request {
    Request {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: body.as_bytes().to_vec(),
    }
}

fn get(ctx: &Context, path: &str) -> Response {
    dispatch(ctx, &request("GET", path, ""), "9.9.9.9")
}

fn post(ctx: &Context, path: &str, body: &str) -> Response {
    dispatch(ctx, &request("POST", path, body), "9.9.9.9")
}

fn rpc(ctx: &Context, method: &str, params: &str) -> Response {
    post(ctx, "/json_rpc", &format!(r#"{{"jsonrpc":"2.0","id":"7","method":"{method}","params":{params}}}"#))
}

fn body(res: &Response) -> Json {
    parse(&res.body, ParseLimits::default()).unwrap_or_else(|e| panic!("body is not JSON: {e}"))
}

fn text(res: &Response) -> String {
    String::from_utf8(res.body.clone()).unwrap()
}

fn result(res: &Response) -> Json {
    body(res).get("result").cloned().unwrap_or_else(|| panic!("no result in {}", text(res)))
}

/// Every key the object must have, with the type it must have.
fn assert_shape(value: &Json, expected: &[(&str, &str)]) {
    for (key, kind) in expected {
        let got = value.get(key).unwrap_or_else(|| panic!("missing {key} in {}", value.to_string()));
        assert_eq!(got.type_name(), *kind, "{key} in {}", value.to_string());
    }
    if let Json::Object(members) = value {
        for (key, _) in members {
            assert!(expected.iter().any(|(k, _)| k == key), "unexpected member {key} in {}", value.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// plain endpoints
// ---------------------------------------------------------------------------

#[test]
fn info_has_every_documented_field_and_the_derived_ones() {
    let ctx = ctx();
    for path in ["/info", "/getinfo"] {
        let res = get(&ctx, path);
        assert_eq!(res.status, 200);
        assert_eq!(res.header("Content-Type"), Some("application/json"));
        let j = body(&res);
        assert_shape(
            &j,
            &[
                ("alt_blocks_count", "number"),
                ("compression", "string"),
                ("difficulty", "number"),
                ("grey_peerlist_size", "number"),
                ("hashrate", "number"),
                ("height", "number"),
                ("incoming_connections_count", "number"),
                ("last_known_block_index", "number"),
                ("last_seed_bootstrap", "number"),
                ("lite", "bool"),
                ("lite_start_height", "number"),
                ("major_version", "number"),
                ("minor_version", "number"),
                ("network_height", "number"),
                ("outgoing_connections_count", "number"),
                ("prune_capability_active", "bool"),
                ("prune_depth", "number"),
                ("pruned", "bool"),
                ("seed_nodes_count", "number"),
                ("start_time", "number"),
                ("status", "string"),
                ("supported_height", "number"),
                ("sync_active_peers", "number"),
                ("sync_avg_batch_size", "number"),
                ("sync_demoted_peers", "number"),
                ("sync_features", "array"),
                ("synced", "bool"),
                ("top_block_hash", "string"),
                ("tx_count", "number"),
                ("tx_pool_size", "number"),
                ("upgrade_heights", "array"),
                ("version", "string"),
                ("white_peerlist_size", "number"),
            ],
        );
        assert_eq!(j.get("status").unwrap().as_str(), Some("OK"));
        // `hashrate` is `difficulty / DIFFICULTY_TARGET` in integer arithmetic.
        assert_eq!(j.get("hashrate").unwrap().as_u64(), Some(52_006_338 / 60));
        assert_eq!(j.get("height").unwrap().as_u64(), Some(4_213_001), "a count, not an index");
        assert_eq!(j.get("last_known_block_index").unwrap().as_u64(), Some(4_213_000), "an index");
        assert_eq!(j.get("synced").unwrap().as_bool(), Some(true));
        assert_eq!(j.get("supported_height").unwrap().as_u64(), Some(4_500_000));
        let features: Vec<&str> =
            j.get("sync_features").unwrap().as_array().unwrap().iter().filter_map(Json::as_str).collect();
        assert_eq!(features, vec!["skipEmptyBlocks", "base64", "heightRange"]);
        assert_eq!(j.get("upgrade_heights").unwrap().as_array().unwrap()[0], Json::U64(1));
    }
}

/// The live sample of spec/09 is emitted in alphabetical key order because
/// `nlohmann::json` stores an object in a `std::map`. Ours must be too.
#[test]
fn info_keys_come_out_in_the_order_the_cpp_emits_them() {
    let res = get(&ctx(), "/info");
    let raw = text(&res);
    assert!(raw.starts_with(r#"{"alt_blocks_count":"#), "{raw}");
    // The parser keeps the order it read members in, which is the order they
    // were written in.
    let Json::Object(members) = body(&res) else { panic!("not an object") };
    let keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "keys are in ascending byte order");
    assert_eq!(keys.last(), Some(&"white_peerlist_size"));
}

#[test]
fn info_answers_busy_with_503_when_the_chain_moves_under_it() {
    let node = FakeNode { info_fails: true, ..Default::default() };
    let ctx = ctx_with(Arc::new(node), ServerConfig::default());
    let res = get(&ctx, "/info");
    assert_eq!(res.status, 503);
    assert_eq!(text(&res), r#"{"error":"Chain is reorganizing, please retry shortly","status":"BUSY"}"#);
}

#[test]
fn height_has_three_members_and_no_hash() {
    let ctx = ctx();
    for path in ["/height", "/getheight"] {
        let res = get(&ctx, path);
        assert_eq!(res.status, 200);
        let j = body(&res);
        assert_shape(&j, &[("height", "number"), ("network_height", "number"), ("status", "string")]);
        // xmrig reads the absence of `hash` as "this is a CryptoNote daemon".
        assert!(!j.has("hash"), "/getheight must not carry a hash member");
        assert_eq!(text(&res), r#"{"height":4213001,"network_height":4213001,"status":"OK"}"#);
    }
}

#[test]
fn peers_lists_both_colours() {
    let res = get(&ctx(), "/peers");
    assert_eq!(text(&res), r#"{"peers":["1.2.3.4:17855"],"peers_gray":["5.6.7.8:17855"],"status":"OK"}"#);
}

#[test]
fn sendrawtransaction_reports_the_hash_only_once_the_hex_parsed() {
    let ctx = ctx();
    // Not hex: `status` and `error`, and no `transactionHash`.
    let res = post(&ctx, "/sendrawtransaction", r#"{"tx_as_hex":"zz"}"#);
    assert_eq!(res.status, 200, "always HTTP 200");
    assert_eq!(text(&res), r#"{"error":"Failed to parse transaction from hex buffer","status":"Failed"}"#);

    // Accepted: hash, empty error, OK.
    let res = post(&ctx, "/sendrawtransaction", r#"{"tx_as_hex":"0102"}"#);
    let j = body(&res);
    assert_shape(&j, &[("error", "string"), ("status", "string"), ("transactionHash", "string")]);
    assert_eq!(j.get("status").unwrap().as_str(), Some("OK"));
    assert_eq!(j.get("error").unwrap().as_str(), Some(""));
    assert_eq!(j.get("transactionHash").unwrap().as_str(), Some(hex::encode(wrkz_pow::cn_fast_hash(&[1, 2])).as_str()));

    // Refused by the pool: the message the pool gave, and the hash.
    let node = FakeNode { send_result: Err("Transaction already exists in pool".into()), ..Default::default() };
    let ctx = ctx_with(Arc::new(node), ServerConfig::default());
    let j = body(&post(&ctx, "/sendrawtransaction", r#"{"tx_as_hex":"0102"}"#));
    assert_eq!(j.get("status").unwrap().as_str(), Some("Failed"));
    assert_eq!(j.get("error").unwrap().as_str(), Some("Transaction already exists in pool"));
}

#[test]
fn sendrawtransaction_needs_a_synced_node() {
    let node = FakeNode { synced: false, ..Default::default() };
    let ctx = ctx_with(Arc::new(node), ServerConfig::default());
    let res = post(&ctx, "/sendrawtransaction", r#"{"tx_as_hex":"0102"}"#);
    assert_eq!(res.status, 503);
    assert_eq!(
        text(&res),
        r#"{"error":"Daemon must be synced to process this RPC method call, please retry when synced","status":"Failed"}"#
    );
    // Nothing else is gated on it.
    assert_eq!(get(&ctx, "/height").status, 200);
    assert_eq!(post(&ctx, "/getrandom_outs", r#"{"amounts":[1],"outs_count":1}"#).status, 200);
}

#[test]
fn getrandom_outs_matches_the_captured_vector_shape() {
    let res = post(&ctx(), "/getrandom_outs", r#"{"amounts":[10000,50000],"outs_count":3}"#);
    let j = body(&res);
    assert_shape(&j, &[("outs", "array"), ("status", "string")]);
    let outs = j.get("outs").unwrap().as_array().unwrap();
    assert_eq!(outs.len(), 2, "one entry per requested amount, in order");
    assert_shape(&outs[0], &[("amount", "number"), ("outs", "array")]);
    assert_eq!(outs[0].get("amount").unwrap().as_u64(), Some(10_000));
    assert_shape(
        &outs[0].get("outs").unwrap().as_array().unwrap()[0],
        &[("global_amount_index", "number"), ("out_key", "string")],
    );
    // The same field names as `spec/vectors/mainnet_getrandom_outs.json`.
    let vector = parse(
        &std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/vectors/mainnet_getrandom_outs.json")).unwrap(),
        ParseLimits::default(),
    )
    .unwrap();
    assert!(wrkz_rpc::diff::Comparison::default().compare(&vector, &j).is_empty());
}

#[test]
fn getrandom_outs_reports_a_lookup_failure_as_400_with_the_error_code() {
    let node = FakeNode { random_outs: Err("Output is locked".into()), ..Default::default() };
    let ctx = ctx_with(Arc::new(node), ServerConfig::default());
    let res = post(&ctx, "/getrandom_outs", r#"{"amounts":[10000],"outs_count":3}"#);
    assert_eq!(res.status, 400);
    assert_eq!(text(&res), r#"{"errorCode":27,"errorMessage":"Output is locked"}"#);
}

#[test]
fn getrandom_outs_refuses_what_no_wallet_asks_for() {
    let ctx = ctx();
    let ask = |count: u64, amounts: usize| {
        let amounts = vec!["10000"; amounts].join(",");
        post(&ctx, "/getrandom_outs", &format!(r#"{{"amounts":[{amounts}],"outs_count":{count}}}"#))
    };
    // The most a wallet asks for a transaction it can send: ring size 8 for
    // every input a 124,400-byte transaction can hold. And the caps exactly.
    assert_eq!(ask(8, 690).status, 200);
    assert_eq!(ask(100, 1_000).status, 200);
    assert_eq!(ask(10, 10_000).status, 200);

    let res = ask(101, 1);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"outs_count exceeds the maximum of 100","status":"Failed"}"#.into())
    );
    // 65,537 used to wrap to one through the C++'s `uint16_t`; it is refused.
    assert_eq!(ask(65_537, 1).status, 400);
    let res = ask(1, 10_001);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"amounts has more than the maximum of 10000 entries","status":"Failed"}"#.into())
    );
    // Each within its own cap, but too many decoys in all.
    let res = ask(100, 1_001);
    assert_eq!(res.status, 400);
    assert!(text(&res).contains("exceeds the maximum of 100000 outputs per request"), "{}", text(&res));
    // A missing parameter is still the C++'s message, cap or no cap.
    let res = post(&ctx, "/getrandom_outs", r#"{"outs_count":1000}"#);
    assert_eq!(text(&res), r#"{"error":"Missing JSON parameter: 'amounts'","status":"Failed"}"#);
}

#[test]
fn getwalletsyncdata_has_the_wallettypes_shape() {
    let res = post(
        &ctx(),
        "/getwalletsyncdata",
        r#"{"blockHashCheckpoints":[],"startHeight":4213000,"startTimestamp":0,"blockCount":2,"skipCoinbaseTransactions":false}"#,
    );
    let j = body(&res);
    assert_shape(&j, &[("items", "array"), ("scannedToHeight", "number"), ("status", "string"), ("synced", "bool")]);
    let item = &j.get("items").unwrap().as_array().unwrap()[0];
    assert_shape(
        item,
        &[
            ("blockHash", "string"),
            ("blockHeight", "number"),
            ("blockTimestamp", "number"),
            ("coinbaseTX", "object"),
            ("transactions", "array"),
        ],
    );
    assert_shape(
        item.get("coinbaseTX").unwrap(),
        &[("hash", "string"), ("outputs", "array"), ("txPublicKey", "string"), ("unlockTime", "number")],
    );
    let tx = &item.get("transactions").unwrap().as_array().unwrap()[0];
    assert_shape(
        tx,
        &[
            ("hash", "string"),
            ("inputs", "array"),
            ("outputs", "array"),
            ("paymentID", "string"),
            ("txPublicKey", "string"),
            ("unlockTime", "number"),
        ],
    );
    assert_shape(
        &tx.get("inputs").unwrap().as_array().unwrap()[0],
        &[("amount", "number"), ("k_image", "string"), ("key_offsets", "array")],
    );
    assert_shape(&tx.get("outputs").unwrap().as_array().unwrap()[0], &[("amount", "number"), ("key", "string")]);
    assert_eq!(j.get("synced").unwrap().as_bool(), Some(false), "false while blocks are still coming");
    assert!(!j.has("topBlock"), "topBlock only appears once the caller is at the top");
}

#[test]
fn getwalletsyncdata_honours_skip_input_key_offsets_and_base64() {
    let ctx = ctx();
    let j =
        body(&post(&ctx, "/getwalletsyncdata", r#"{"startHeight":4213000,"blockCount":2,"skipInputKeyOffsets":true}"#));
    let tx = &j.get("items").unwrap().as_array().unwrap()[0].get("transactions").unwrap().as_array().unwrap()[0];
    assert!(!tx.get("inputs").unwrap().as_array().unwrap()[0].has("key_offsets"));

    let j = body(&post(&ctx, "/getwalletsyncdata", r#"{"startHeight":4213000,"blockCount":2,"encoding":"base64"}"#));
    let hash = j.get("items").unwrap().as_array().unwrap()[0].get("blockHash").unwrap().as_str().unwrap();
    assert_eq!(hash, wrkz_rpc::base64::encode(&h(0xc5)), "44 base64 characters, not 64 hex");

    // Anything else is a 400.
    let res = post(&ctx, "/getwalletsyncdata", r#"{"startHeight":1,"encoding":"rot13"}"#);
    assert_eq!(res.status, 400);
    assert_eq!(text(&res), r#"{"error":"encoding must be either 'hex' or 'base64'","status":"Failed"}"#);
}

#[test]
fn getwalletsyncdata_reports_the_top_block_when_the_caller_is_synced() {
    let node = FakeNode { sync_items: Vec::new(), ..Default::default() };
    let ctx = ctx_with(Arc::new(node), ServerConfig::default());
    let j = body(&post(&ctx, "/getwalletsyncdata", r#"{"startHeight":9999999,"blockCount":2}"#));
    assert_eq!(j.get("synced").unwrap().as_bool(), Some(true));
    assert_shape(j.get("topBlock").unwrap(), &[("hash", "string"), ("height", "number")]);
    assert!(!j.has("scannedToHeight"), "a zero scannedToHeight is left out entirely");
}

#[test]
fn getwalletsyncdata_refuses_a_bad_checkpoint_and_an_over_limit_block_count() {
    let ctx = ctx();
    let res = post(&ctx, "/getwalletsyncdata", r#"{"blockHashCheckpoints":["nothex"],"startHeight":1}"#);
    assert_eq!(res.status, 400);
    assert_eq!(text(&res), r#"{"error":"blockHashCheckpoints contains invalid hash","status":"Failed"}"#);

    let res = post(&ctx, "/getwalletsyncdata", r#"{"startHeight":1,"blockCount":100000}"#);
    assert_eq!(res.status, 400);
    assert_eq!(text(&res), r#"{"error":"blockCount exceeds rpc-max-block-count","status":"Failed"}"#);
}

#[test]
fn getrawblocks_returns_hex_bytes() {
    let j = body(&post(&ctx(), "/getrawblocks", r#"{"startHeight":4213000,"blockCount":1}"#));
    assert_shape(&j, &[("items", "array"), ("status", "string"), ("synced", "bool")]);
    let item = &j.get("items").unwrap().as_array().unwrap()[0];
    assert_shape(item, &[("block", "string"), ("transactions", "array")]);
    assert_eq!(item.get("block").unwrap().as_str(), Some("070000"));
    assert_eq!(item.get("transactions").unwrap().as_array().unwrap()[0].as_str(), Some("010203"));
    // `/getrawblocks` reads none of the wallet-sync-only flags.
    let res = post(&ctx(), "/getrawblocks", r#"{"startHeight":1,"encoding":"rot13"}"#);
    assert_eq!(res.status, 200, "encoding is not a /getrawblocks parameter");
}

#[test]
fn get_global_indexes_for_range_matches_the_captured_vector() {
    let res = post(&ctx(), "/get_global_indexes_for_range", r#"{"startHeight":4213000,"endHeight":4213001}"#);
    let j = body(&res);
    let vector = parse(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/vectors/mainnet_get_global_indexes_for_range.json"
        ))
        .unwrap(),
        ParseLimits::default(),
    )
    .unwrap();
    assert!(wrkz_rpc::diff::Comparison::default().compare(&vector, &j).is_empty());
    assert_eq!(
        j.get("indexes").unwrap().as_array().unwrap()[0].get("value").unwrap().as_array().unwrap()[0],
        Json::U64(3_808_773)
    );
}

#[test]
fn get_global_indexes_for_range_checks_its_bounds() {
    let ctx = ctx();
    let res = post(&ctx, "/get_global_indexes_for_range", r#"{"startHeight":10,"endHeight":1}"#);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"endHeight must be >= startHeight","status":"Failed"}"#.into())
    );

    let res = post(&ctx, "/get_global_indexes_for_range", r#"{"startHeight":0,"endHeight":900000}"#);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"Requested range exceeds rpc-max-global-index-range","status":"Failed"}"#.into())
    );

    let res = post(&ctx, "/get_global_indexes_for_range", "{}");
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"Missing JSON parameter: 'startHeight'","status":"Failed"}"#.into())
    );
}

#[test]
fn get_o_indexes_answers_by_transaction_hash() {
    let ctx = ctx();
    let res = post(&ctx, "/get_o_indexes", &format!(r#"{{"txid":"{}"}}"#, hex::encode(h(0xaf))));
    assert_eq!(text(&res), r#"{"o_indexes":[3808773],"status":"OK"}"#);

    // An unknown transaction is the C++'s own 500.
    let res = post(&ctx, "/get_o_indexes", &format!(r#"{{"txid":"{}"}}"#, hex::encode(h(0x01))));
    assert_eq!(res.status, 500);
    assert_eq!(text(&res), r#"{"error":"Internal error: Failed to getTransactionGlobalIndexes","status":"Failed"}"#);

    let res = post(&ctx, "/get_o_indexes", r#"{"txid":"nothex"}"#);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"txid specified is not a valid hex string!","status":"Failed"}"#.into())
    );
}

#[test]
fn get_transactions_status_sorts_every_hash_into_one_of_three_buckets() {
    let body_text = format!(
        r#"{{"transactionHashes":["{}","{}","{}"]}}"#,
        hex::encode(h(0xaf)),
        hex::encode(h(9)),
        hex::encode(h(0x01))
    );
    let res = post(&ctx(), "/get_transactions_status", &body_text);
    let j = body(&res);
    assert_shape(
        &j,
        &[
            ("status", "string"),
            ("transactionsInBlock", "array"),
            ("transactionsInPool", "array"),
            ("transactionsUnknown", "array"),
        ],
    );
    assert_eq!(j.get("transactionsInBlock").unwrap().as_array().unwrap().len(), 1);
    assert_eq!(j.get("transactionsInPool").unwrap().as_array().unwrap().len(), 1);
    assert_eq!(j.get("transactionsUnknown").unwrap().as_array().unwrap().len(), 1);

    let res = post(&ctx(), "/get_transactions_status", r#"{"transactionHashes":["nothex"]}"#);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"Transaction hash specified is not a valid hex string!","status":"Failed"}"#.into())
    );
}

#[test]
fn get_pool_changes_lite_carries_transaction_prefixes() {
    let ctx = ctx();
    let res = post(
        &ctx,
        "/get_pool_changes_lite",
        &format!(r#"{{"tailBlockId":"{}","knownTxsIds":[]}}"#, hex::encode(h(0x2c))),
    );
    let j = body(&res);
    assert_shape(
        &j,
        &[("addedTxs", "array"), ("deletedTxsIds", "array"), ("isTailBlockActual", "bool"), ("status", "string")],
    );
    assert_eq!(j.get("isTailBlockActual").unwrap().as_bool(), Some(true));
    let added = &j.get("addedTxs").unwrap().as_array().unwrap()[0];
    assert_shape(added, &[("transactionPrefixInfo.txHash", "string"), ("transactionPrefixInfo.txPrefix", "object")]);
    let prefix = added.get("transactionPrefixInfo.txPrefix").unwrap();
    assert_shape(
        prefix,
        &[("extra", "string"), ("unlock_time", "number"), ("version", "number"), ("vin", "array"), ("vout", "array")],
    );
    let vin = prefix.get("vin").unwrap().as_array().unwrap();
    assert_eq!(vin[0].get("type").unwrap().as_str(), Some("02"), "a key input");
    assert_shape(
        vin[0].get("value").unwrap(),
        &[("amount", "number"), ("k_image", "string"), ("key_offsets", "array")],
    );
    let vout = prefix.get("vout").unwrap().as_array().unwrap();
    assert_shape(&vout[0], &[("amount", "number"), ("target", "object")]);
    assert_eq!(vout[0].get("target").unwrap().get("type").unwrap().as_str(), Some("02"));

    let res = post(&ctx, "/get_pool_changes_lite", r#"{"tailBlockId":"nothex","knownTxsIds":[]}"#);
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"tailBlockId specified is not a valid hex string!","status":"Failed"}"#.into())
    );
}

#[test]
fn queryblockslite_carries_the_block_as_a_byte_array() {
    let res = post(&ctx(), "/queryblockslite", r#"{"blockIds":[],"timestamp":0}"#);
    let j = body(&res);
    assert_shape(
        &j,
        &[
            ("currentHeight", "number"),
            ("fullOffset", "number"),
            ("items", "array"),
            ("startHeight", "number"),
            ("status", "string"),
        ],
    );
    let item = &j.get("items").unwrap().as_array().unwrap()[0];
    assert_shape(
        item,
        &[
            ("blockShortInfo.block", "array"),
            ("blockShortInfo.blockId", "string"),
            ("blockShortInfo.txPrefixes", "array"),
        ],
    );
    // The block is a JSON array of byte values, as the C++ emits it.
    assert_eq!(
        item.get("blockShortInfo.block").unwrap().as_array().unwrap(),
        &[Json::U64(7), Json::U64(0), Json::U64(1)]
    );
    // A base input prints `"type":"ff"` with a bare `height`.
    let vin = item.get("blockShortInfo.txPrefixes").unwrap().as_array().unwrap()[0]
        .get("transactionPrefixInfo.txPrefix")
        .unwrap()
        .get("vin")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec();
    assert_eq!(vin[0].get("type").unwrap().as_str(), Some("ff"));
    assert_shape(vin[0].get("value").unwrap(), &[("height", "number")]);
    assert_eq!(vin[1].get("type").unwrap().as_str(), Some("02"));
}

#[test]
fn queryblocksdetailed_is_an_explorer_route_and_403s_by_default() {
    let res = post(&ctx(), "/queryblocksdetailed", r#"{"blockIds":[]}"#);
    assert_eq!(res.status, 403);
    assert!(text(&res).contains("--daemon-mode explorer"));
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

#[test]
fn json_rpc_echoes_the_id_and_wraps_the_result() {
    let res = rpc(&ctx(), "getblockcount", "{}");
    assert_eq!(res.status, 200);
    assert_eq!(text(&res), r#"{"id":"7","jsonrpc":"2.0","result":{"count":4213001,"status":"OK"}}"#);
    // A request with no id gets no id back.
    let res = post(&ctx(), "/json_rpc", r#"{"jsonrpc":"2.0","method":"getblockcount"}"#);
    assert_eq!(text(&res), r#"{"jsonrpc":"2.0","result":{"count":4213001,"status":"OK"}}"#);
}

#[test]
fn block_headers_have_the_thirteen_members_of_the_cpp() {
    let ctx = ctx();
    let expected: &[(&str, &str)] = &[
        ("block_size", "number"),
        ("depth", "number"),
        ("difficulty", "number"),
        ("hash", "string"),
        ("height", "number"),
        ("major_version", "number"),
        ("minor_version", "number"),
        ("nonce", "number"),
        ("num_txes", "number"),
        ("orphan_status", "bool"),
        ("prev_hash", "string"),
        ("reward", "number"),
        ("timestamp", "number"),
    ];
    for (method, params) in [
        ("getlastblockheader", "{}".to_string()),
        ("getblockheaderbyheight", r#"{"height":4213000}"#.to_string()),
        ("getblockheaderbyhash", format!(r#"{{"hash":"{}"}}"#, hex::encode(h(0xc5)))),
    ] {
        let res = rpc(&ctx, method, &params);
        let r = result(&res);
        assert_shape(&r, &[("block_header", "object"), ("status", "string")]);
        assert_shape(r.get("block_header").unwrap(), expected);
        assert_eq!(r.get("block_header").unwrap().get("height").unwrap().as_u64(), Some(4_213_000), "an index");
        assert_eq!(r.get("block_header").unwrap().get("num_txes").unwrap().as_u64(), Some(1), "the coinbase counts");
    }
    // `depth` is measured from the tip.
    let r = result(&rpc(&ctx, "getblockheaderbyheight", r#"{"height":4213000}"#));
    assert_eq!(r.get("block_header").unwrap().get("depth").unwrap().as_u64(), Some(0));
}

#[test]
fn block_header_lookups_report_the_cpp_error_codes() {
    let ctx = ctx();
    let cases: &[(&str, &str, i64, &str)] = &[
        (
            "getblockheaderbyheight",
            r#"{"height":9999999}"#,
            -2,
            "Requested block header for a height that is higher than the current blockchain height! Current height: 4213000",
        ),
        ("getblockheaderbyhash", r#"{"hash":"nothex"}"#, -1, "Block hash specified is not a valid hex!"),
        (
            "getblockheaderbyhash",
            r#"{"hash":"0000000000000000000000000000000000000000000000000000000000000001"}"#,
            -5,
            "Block hash specified does not exist!",
        ),
    ];
    for (method, params, code, message) in cases {
        let res = rpc(&ctx, method, params);
        assert_eq!(res.status, 200, "a JSON-RPC error is still HTTP 200");
        let e = body(&res).get("error").cloned().unwrap();
        assert_eq!(e.get("code").unwrap(), &Json::I64(*code), "{method}");
        assert_eq!(e.get("message").unwrap().as_str(), Some(*message), "{method}");
        assert!(!body(&res).has("result"));
    }
}

#[test]
fn getblocktemplate_places_the_reserved_offset_past_the_public_key() {
    let address = "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
    let res = rpc(&ctx(), "getblocktemplate", &format!(r#"{{"wallet_address":"{address}","reserve_size":8}}"#));
    let r = result(&res);
    assert_shape(
        &r,
        &[
            ("blocktemplate_blob", "string"),
            ("difficulty", "number"),
            ("height", "number"),
            ("reserved_offset", "number"),
            ("status", "string"),
        ],
    );
    assert_eq!(r.get("height").unwrap().as_u64(), Some(4_213_001), "a count");
    // The fake's blob is 8 filler bytes, the key, two tag bytes, then the
    // reserve: offset = 8 + 32 + 2.
    assert_eq!(r.get("reserved_offset").unwrap().as_u64(), Some(42));
    let blob = hex::decode(r.get("blocktemplate_blob").unwrap().as_str().unwrap()).unwrap();
    assert_eq!(&blob[42..50], &[0u8; 8], "the reserved bytes are zeroes at that offset");

    // `extra_nonce` wins over `reserve_size`.
    let r = result(&rpc(
        &ctx(),
        "getblocktemplate",
        &format!(r#"{{"wallet_address":"{address}","reserve_size":8,"extra_nonce":"aabb"}}"#),
    ));
    let blob = hex::decode(r.get("blocktemplate_blob").unwrap().as_str().unwrap()).unwrap();
    assert_eq!(&blob[42..44], &[0xaa, 0xbb]);
}

#[test]
fn getblocktemplate_reports_the_cpp_error_codes() {
    let ctx = ctx();
    let address = "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
    let err = |params: String| -> (i64, String) {
        let res = rpc(&ctx, "getblocktemplate", &params);
        let e = body(&res).get("error").cloned().unwrap();
        let code = match e.get("code").unwrap() {
            Json::I64(v) => *v,
            Json::U64(v) => *v as i64,
            other => panic!("code is {other:?}"),
        };
        (code, e.get("message").unwrap().as_str().unwrap().to_string())
    };
    assert_eq!(err(format!(r#"{{"wallet_address":"{address}","reserve_size":256}}"#)).0, -3);
    assert_eq!(
        err(format!(r#"{{"wallet_address":"{address}","extra_nonce":"zz"}}"#)),
        (-3, "Given extra nonce is not hex!".into())
    );
    assert_eq!(err(r#"{"wallet_address":"short","reserve_size":0}"#.into()).0, -4);

    let node = FakeNode { template_error: Some("difficulty is zero".into()), ..Default::default() };
    let ctx2 = ctx_with(Arc::new(node), ServerConfig::default());
    let res = rpc(&ctx2, "getblocktemplate", &format!(r#"{{"wallet_address":"{address}","reserve_size":0}}"#));
    let e = body(&res).get("error").cloned().unwrap();
    assert_eq!(e.get("code").unwrap(), &Json::I64(-5));
    assert_eq!(e.get("message").unwrap().as_str(), Some("Failed to create block template: difficulty is zero"));
}

#[test]
fn submitblock_takes_exactly_one_hex_blob() {
    let ctx = ctx();
    let res = rpc(&ctx, "submitblock", r#"["0102"]"#);
    assert_eq!(text(&res), r#"{"id":"7","jsonrpc":"2.0","result":{"status":"OK"}}"#);

    let e = |params: &str| -> (i64, String) {
        let res = rpc(&ctx, "submitblock", params);
        let e = body(&res).get("error").cloned().unwrap();
        let code = match e.get("code").unwrap() {
            Json::I64(v) => *v,
            other => panic!("code is {other:?}"),
        };
        (code, e.get("message").unwrap().as_str().unwrap().to_string())
    };
    assert_eq!(e("[]"), (-1, "You must submit one and only one block blob! (Found 0)".into()));
    assert_eq!(e(r#"["a","b"]"#), (-1, "You must submit one and only one block blob! (Found 2)".into()));
    assert_eq!(e(r#"["zz"]"#), (-6, "Submitted block blob is not hex!".into()));

    let node = FakeNode { submit: wrkz_rpc::api::SubmitOutcome::NotAccepted, ..Default::default() };
    let ctx2 = ctx_with(Arc::new(node), ServerConfig::default());
    let res = rpc(&ctx2, "submitblock", r#"["0102"]"#);
    let err = body(&res).get("error").cloned().unwrap();
    assert_eq!(err.get("code").unwrap(), &Json::I64(-7));
    assert_eq!(err.get("message").unwrap().as_str(), Some("Block not accepted"));
}

#[test]
fn explorer_methods_are_refused_in_standard_mode_and_answer_in_explorer_mode() {
    let ctx = ctx();
    for method in [
        "f_blocks_list_json",
        "f_block_json",
        "f_transaction_json",
        "f_on_transactions_pool_json",
        "f_transactions_by_payment_id_json",
    ] {
        let res = rpc(&ctx, method, "{}");
        assert_eq!(res.status, 403, "{method}");
        assert!(text(&res).contains("--daemon-mode explorer"));
    }

    let explorer =
        ctx_with(Arc::new(FakeNode::default()), ServerConfig { mode: RpcMode::Explorer, ..Default::default() });
    let r = result(&rpc(&explorer, "f_on_transactions_pool_json", "{}"));
    assert_shape(&r, &[("status", "string"), ("transactions", "array")]);
    assert_shape(
        &r.get("transactions").unwrap().as_array().unwrap()[0],
        &[("amount_out", "number"), ("fee", "number"), ("hash", "string"), ("size", "number")],
    );

    let r = result(&rpc(&explorer, "f_blocks_list_json", r#"{"height":4213000}"#));
    assert_shape(&r, &[("blocks", "array"), ("status", "string")]);
    assert_shape(
        &r.get("blocks").unwrap().as_array().unwrap()[0],
        &[
            ("cumul_size", "number"),
            ("difficulty", "number"),
            ("hash", "string"),
            ("height", "number"),
            ("timestamp", "number"),
            ("tx_count", "number"),
        ],
    );
    assert_eq!(r.get("blocks").unwrap().as_array().unwrap().len(), 31, "height down to height - 30");

    let r = result(&rpc(&explorer, "f_block_json", &format!(r#"{{"hash":"{}"}}"#, hex::encode(h(0xc5)))));
    let b = r.get("block").unwrap();
    assert_shape(
        b,
        &[
            ("alreadyGeneratedCoins", "string"),
            ("alreadyGeneratedTransactions", "number"),
            ("baseReward", "number"),
            ("blockSize", "number"),
            ("depth", "number"),
            ("difficulty", "number"),
            ("effectiveSizeMedian", "number"),
            ("hash", "string"),
            ("height", "number"),
            ("major_version", "number"),
            ("minor_version", "number"),
            ("nonce", "number"),
            ("orphan_status", "bool"),
            ("penalty", "number"),
            ("prev_hash", "string"),
            ("reward", "number"),
            ("sizeMedian", "number"),
            ("timestamp", "number"),
            ("totalFeeAmount", "number"),
            ("transactions", "array"),
            ("transactionsCumulativeSize", "number"),
        ],
    );
    assert_eq!(
        b.get("alreadyGeneratedCoins").unwrap().as_str(),
        Some("30000000000000"),
        "a string, as std::to_string emits it"
    );
    assert_eq!(b.get("effectiveSizeMedian").unwrap().as_u64(), Some(100_000));
}

/// The methods other CryptoNote daemons serve, and this one does not.
#[test]
fn methods_the_cpp_does_not_route_are_404_with_no_body() {
    let ctx = ctx();
    for method in [
        "on_getblockhash",
        "getblocksbyheights",
        "getblockdetailsbyheight",
        "getblock",
        "getblocks",
        "gettransaction",
        "gettransactionspool",
        "getcurrencyid",
        "",
    ] {
        let res = rpc(&ctx, method, "{}");
        assert_eq!(res.status, 404, "{method}");
        assert!(res.body.is_empty(), "{method}");
    }
}

#[test]
fn paths_the_cpp_does_not_route_are_404() {
    let ctx = ctx();
    for (method, path) in [
        ("GET", "/fee"),
        ("GET", "/getpeers"),
        ("POST", "/getblocks"),
        ("POST", "/gettransactions"),
        ("POST", "/queryblocks"),
        ("POST", "/get_pool_changes"),
        ("GET", "/"),
        ("GET", "/getwalletsyncdata"),
        ("POST", "/info"),
    ] {
        let res = dispatch(&ctx, &request(method, path, "{}"), "9.9.9.9");
        assert_eq!(res.status, 404, "{method} {path}");
    }
}

// ---------------------------------------------------------------------------
// transport rules (spec/09)
// ---------------------------------------------------------------------------

#[test]
fn a_body_that_is_not_json_is_a_400_with_the_cpp_message() {
    let ctx = ctx();
    let res = post(&ctx, "/json_rpc", "not json");
    assert_eq!(res.status, 400);
    let message = body(&res).get("error").unwrap().as_str().unwrap().to_string();
    assert!(message.starts_with("Warning: received body is not JSON encoded!"));
    assert!(message.ends_with("Body:\nnot jsonFailed to parse request body as JSON"));

    // A long body is echoed only as far as its first 256 bytes.
    let res = post(&ctx, "/json_rpc", &"x".repeat(4096));
    assert_eq!(res.status, 400);
    let message = body(&res).get("error").unwrap().as_str().unwrap().to_string();
    assert!(
        message.ends_with(&format!("Body:\n{}...[3840 more bytes]Failed to parse request body as JSON", "x".repeat(256))),
        "{message}"
    );

    // A route that needs a body sees the same message.
    let res = post(&ctx, "/get_transactions_status", "{");
    assert_eq!(res.status, 400);
    // A route that needs no body ignores it entirely.
    assert_eq!(dispatch(&ctx, &request("GET", "/height", "not json"), "9.9.9.9").status, 200);
}

#[test]
fn json_rpc_without_a_method_is_a_400() {
    let res = post(&ctx(), "/json_rpc", "{}");
    assert_eq!(
        (res.status, text(&res)),
        (400, r#"{"error":"Missing JSON parameter: 'method'","status":"Failed"}"#.into())
    );
}

#[test]
fn nesting_past_the_cap_is_refused_as_a_parse_failure() {
    let ctx = ctx();
    let deep = format!("{}{}", "[".repeat(500), "]".repeat(500));
    let res = post(&ctx, "/get_transactions_status", &deep);
    assert_eq!(res.status, 400, "a hostile body cannot recurse the parser");
}

#[test]
fn an_oversized_body_is_413() {
    let ctx = ctx_with(
        Arc::new(FakeNode::default()),
        ServerConfig {
            limits: wrkz_rpc::http::HttpLimits { max_body: 16, ..Default::default() },
            ..Default::default()
        },
    );
    let res = post(&ctx, "/get_transactions_status", &"x".repeat(64));
    assert_eq!(res.status, 413);
    assert_eq!(text(&res), r#"{"error":"RPC request body too large","status":"Failed"}"#);
}

#[test]
fn the_access_token_is_accepted_in_either_header_and_required_otherwise() {
    let config = ServerConfig { access_token: "s3cret".into(), ..Default::default() };
    let ctx = ctx_with(Arc::new(FakeNode::default()), config);

    let res = get(&ctx, "/height");
    assert_eq!(res.status, 401);
    assert_eq!(text(&res), r#"{"error":"Unauthorized RPC request","status":"Failed"}"#);

    let with = |name: &str, value: &str| {
        let mut req = request("GET", "/height", "");
        req.headers.push((name.into(), value.into()));
        dispatch(&ctx, &req, "9.9.9.9")
    };
    assert_eq!(with("X-API-Key", "s3cret").status, 200);
    assert_eq!(with("x-api-key", "s3cret").status, 200, "header names are case-insensitive");
    assert_eq!(with("Authorization", "Bearer s3cret").status, 200);
    assert_eq!(with("Authorization", "Bearer wrong").status, 401);
    assert_eq!(with("Authorization", "Basic s3cret").status, 401);
    assert_eq!(with("X-API-Key", "wrong").status, 401);
}

#[test]
fn json_rpc_checks_the_token_before_it_parses_the_body() {
    let config = ServerConfig { access_token: "s3cret".into(), ..Default::default() };
    let ctx = ctx_with(Arc::new(FakeNode::default()), config);
    // Without the token nothing is parsed: whatever the body, it is the 401.
    for body in ["not json", r#"{"method":"nosuch"}"#, r#"{"jsonrpc":"2.0","method":"getblockcount"}"#] {
        let res = post(&ctx, "/json_rpc", body);
        assert_eq!(
            (res.status, text(&res)),
            (401, r#"{"error":"Unauthorized RPC request","status":"Failed"}"#.into()),
            "{body}"
        );
    }
    // With it, the C++ order is unchanged.
    let with = |body: &str| {
        let mut req = request("POST", "/json_rpc", body);
        req.headers.push(("X-API-Key".into(), "s3cret".into()));
        dispatch(&ctx, &req, "9.9.9.9")
    };
    assert_eq!(with("not json").status, 400);
    assert_eq!(with("{}").status, 400);
    assert_eq!(with(r#"{"method":"nosuch"}"#).status, 404);
    assert_eq!(with(r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":{}}"#).status, 200);
}

#[test]
fn the_rate_limit_is_per_ip_and_exempts_loopback() {
    let config = ServerConfig { max_requests_per_minute: 3, ..Default::default() };
    let ctx = ctx_with(Arc::new(FakeNode::default()), config);
    for _ in 0..3 {
        assert_eq!(dispatch(&ctx, &request("GET", "/height", ""), "9.9.9.9").status, 200);
    }
    let res = dispatch(&ctx, &request("GET", "/height", ""), "9.9.9.9");
    assert_eq!(res.status, 429);
    assert_eq!(text(&res), r#"{"error":"Too many RPC requests, please retry later","status":"Failed"}"#);
    // Another address has its own budget.
    assert_eq!(dispatch(&ctx, &request("GET", "/height", ""), "8.8.8.8").status, 200);
    // Loopback is never limited.
    for _ in 0..20 {
        assert_eq!(dispatch(&ctx, &request("GET", "/height", ""), "127.0.0.1").status, 200);
        assert_eq!(dispatch(&ctx, &request("GET", "/height", ""), "::1").status, 200);
    }
}

#[test]
fn x_forwarded_for_is_read_only_when_the_proxy_is_trusted() {
    let make = |trust| {
        ctx_with(
            Arc::new(FakeNode::default()),
            ServerConfig { max_requests_per_minute: 1, trust_proxy: trust, ..Default::default() },
        )
    };
    let call = |ctx: &Context, forwarded: &str, peer: &str| {
        let mut req = request("GET", "/height", "");
        req.headers.push(("X-Forwarded-For".into(), forwarded.into()));
        dispatch(ctx, &req, peer).status
    };
    // Trusted: the limit follows the forwarded address, so the same peer with
    // two different clients behind it is not limited on the first repeat.
    let trusted = make(true);
    assert_eq!(call(&trusted, "1.1.1.1, 2.2.2.2", "10.0.0.1"), 200);
    assert_eq!(call(&trusted, "3.3.3.3", "10.0.0.1"), 200);
    assert_eq!(call(&trusted, "1.1.1.1", "10.0.0.1"), 429);
    // Untrusted: the header is ignored and the peer's own budget is spent.
    let untrusted = make(false);
    assert_eq!(call(&untrusted, "1.1.1.1", "10.0.0.1"), 200);
    assert_eq!(call(&untrusted, "3.3.3.3", "10.0.0.1"), 429);
}

#[test]
fn options_answers_cors_only_when_cors_is_enabled() {
    // Off: an empty `Allow`, no origin header — exactly what the seed node does.
    let ctx = ctx();
    let mut req = request("OPTIONS", "/info", "");
    let res = dispatch(&ctx, &req, "9.9.9.9");
    assert_eq!(res.status, 200);
    assert_eq!(res.header("Allow"), Some(""));
    assert!(res.header("Access-Control-Allow-Origin").is_none());
    assert!(res.body.is_empty());

    // On: the configured origin everywhere, and the method list on a preflight.
    let ctx = ctx_with(Arc::new(FakeNode::default()), ServerConfig { cors_header: "*".into(), ..Default::default() });
    let res = dispatch(&ctx, &req, "9.9.9.9");
    assert_eq!(res.header("Allow"), Some("OPTIONS, GET, POST"));
    assert_eq!(res.header("Access-Control-Allow-Origin"), Some("*"));
    assert_eq!(res.header("Access-Control-Allow-Headers"), Some("Origin, X-Requested-With, Content-Type, Accept"));

    req.headers.push(("Access-Control-Request-Method".into(), "POST".into()));
    let res = dispatch(&ctx, &req, "9.9.9.9");
    assert_eq!(res.header("Access-Control-Allow-Methods"), Some("OPTIONS, GET, POST"));
    assert!(res.header("Allow").is_none());

    // Ordinary responses carry the origin too, failures included.
    assert_eq!(get(&ctx, "/height").header("Access-Control-Allow-Origin"), Some("*"));
    let ctx = ctx_with(
        Arc::new(FakeNode::default()),
        ServerConfig { cors_header: "*".into(), access_token: "x".into(), ..Default::default() },
    );
    let res = get(&ctx, "/height");
    assert_eq!(res.status, 401);
    assert_eq!(res.header("Access-Control-Allow-Origin"), Some("*"));
}

/// The three status strings this surface uses, and nothing else.
#[test]
fn only_ok_failed_and_busy_appear_as_status_strings() {
    let ctx = ctx();
    assert_eq!(body(&get(&ctx, "/height")).get("status").unwrap().as_str(), Some("OK"));
    assert_eq!(
        body(&post(&ctx, "/get_global_indexes_for_range", "{}")).get("status").unwrap().as_str(),
        Some("Failed")
    );
    let busy = ctx_with(Arc::new(FakeNode { info_fails: true, ..Default::default() }), ServerConfig::default());
    assert_eq!(body(&get(&busy, "/info")).get("status").unwrap().as_str(), Some("BUSY"));
}
