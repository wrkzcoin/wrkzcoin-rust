// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `#[ignore]`: the live comparison against the C++ seed node.
//!
//! Run with a network:
//!
//! ```text
//! cargo test -p wrkz-rpc -- --ignored --nocapture
//! WRKZ_REFERENCE=http://127.0.0.1:17856 cargo test -p wrkz-rpc -- --ignored
//! ```
//!
//! # What is compared, exactly
//!
//! This test starts **our** server over a synthetic chain (a fake node holding
//! one captured block, so every route produces a body) on a loopback port, puts
//! the request set of [`wrkz_rpc::probe`] to both it and the live daemon, and
//! diffs the two JSON documents in [`wrkz_rpc::diff::Mode::Shape`]:
//!
//! - **every key** the reference returns must be present on ours, and vice
//!   versa, at the same path;
//! - **every value's type** must match (`number`, `string`, `bool`, `array`,
//!   `object`, `null`);
//! - **array elements** are compared element-wise over the overlap, so a
//!   1,000-block response and a 1-block one still compare their element shape;
//!   an array that is empty on exactly one side is *reported* but does not fail
//!   the test: whether a given mainnet block holds transactions, and whether the
//!   live pool holds anything this minute, is chain content, not contract. The
//!   element shape of exactly those arrays is pinned offline instead, against
//!   `RpcServer.cpp` and against the captured vectors
//!   (`tests/chain_backed.rs`, `tests/endpoints.rs`);
//! - the **HTTP status** must match, which is what pins the 400/403/404/503
//!   paths;
//! - **values are not compared.** The two nodes hold different chains at
//!   different heights with different peers and different clocks, so no value
//!   could agree. `wrkz-rpc-diff --values` does compare them, for two nodes on
//!   the same chain.
//!
//! What this therefore proves, and its limits: it proves the *contract* — field
//! names, nesting, types, status codes — for every route on both sides. It does
//! not prove any value, and it cannot: only spec/09's acceptance 2 (the port
//! replaying the same database) can, and the captured vectors in
//! `tests/chain_backed.rs` do it for the ranges we have bytes for.

mod fake;

use fake::FakeNode;
use std::sync::Arc;
use std::time::Duration;
use wrkz_rpc::diff::Comparison;
use wrkz_rpc::http::client::parse_base;
use wrkz_rpc::probe;
use wrkz_rpc::server::{self, ServerConfig};

const DEFAULT_REFERENCE: &str = "http://node-fin.wrkz.work:17856";

#[test]
#[ignore = "needs the network and the live seed node"]
fn our_shapes_match_the_live_daemon() {
    let reference_url = std::env::var("WRKZ_REFERENCE").unwrap_or_else(|_| DEFAULT_REFERENCE.into());
    let reference = parse_base(&reference_url).expect("a http:// reference URL");

    // Our server, over the fake node, on a loopback port. Loopback is exempt
    // from the rate limit, so the probe set cannot trip it.
    let server =
        server::start(Arc::new(FakeNode::default()), ServerConfig { bind: "127.0.0.1:0".into(), ..Default::default() })
            .expect("our server binds");
    let ours = parse_base(&format!("http://{}", server.local_addr())).unwrap();

    let probes = probe::probes();
    let results = probe::run(&reference, &ours, &probes, &Comparison::default(), Duration::from_secs(30));

    let mut failed = Vec::new();
    for r in &results {
        // An array that is empty on one side is a content difference: mainnet
        // block 4,213,000 holds only its coinbase, and the live pool may hold
        // nothing at all. Report it, do not fail on it.
        let (content, shape): (Vec<_>, Vec<_>) =
            r.differences.iter().partition(|d| matches!(d, wrkz_rpc::diff::Difference::EmptyOnOneSide { .. }));
        let clean = r.error.is_none() && shape.is_empty() && r.reference_status == r.ours_status;
        if clean && content.is_empty() {
            println!("ok      {:<32} [{}]", r.name, r.ours_status);
            continue;
        }
        println!(
            "{}  {:<32} [{} vs {}]",
            if clean { "ok*   " } else { "DIFF  " },
            r.name,
            r.reference_status,
            r.ours_status
        );
        if let Some(e) = &r.error {
            println!("          error: {e}");
        }
        for d in content {
            println!("          (content) {d}");
        }
        for d in &shape {
            println!("          {d}");
        }
        if !clean {
            failed.push(r.name);
        }
    }

    // A probe the reference could not answer at all (the network is down, or
    // the node is behind a proxy) is reported but is not a shape failure: the
    // runner says so by leaving both statuses at zero.
    let unreachable: Vec<&str> = results
        .iter()
        .filter(|r| r.error.as_deref().is_some_and(|e| e.starts_with("reference:")))
        .map(|r| r.name)
        .collect();
    assert!(unreachable.is_empty(), "the reference daemon did not answer: {unreachable:?}");

    assert!(failed.is_empty(), "shapes differ from the live daemon for: {failed:?}");
}

/// The transport rules of spec/09 as the live daemon actually implements them,
/// checked directly so a change on that side is noticed.
#[test]
#[ignore = "needs the network and the live seed node"]
fn the_live_daemon_still_behaves_the_way_this_crate_assumes() {
    use wrkz_rpc::http::client;
    use wrkz_rpc::json::{parse, ParseLimits};

    let reference_url = std::env::var("WRKZ_REFERENCE").unwrap_or_else(|_| DEFAULT_REFERENCE.into());
    let base = parse_base(&reference_url).unwrap();
    let call = |method: &str, path: &str, body: Option<&str>| {
        client::request(&base, method, path, body, Duration::from_secs(30), 32 * 1024 * 1024).expect("reachable")
    };

    // `/getheight` carries no `hash`: xmrig reads its absence as "CryptoNote".
    let (status, body) = call("GET", "/getheight", None);
    assert_eq!(status, 200);
    let j = parse(&body, ParseLimits::default()).unwrap();
    assert!(!j.has("hash"));

    // `/info` keys come out in ascending byte order, which is what this crate
    // reproduces by sorting.
    let (_, body) = call("GET", "/info", None);
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(text.starts_with(r#"{"alt_blocks_count":"#), "{text}");
    let wrkz_rpc::Json::Object(members) = parse(&body, ParseLimits::default()).unwrap() else { panic!() };
    let keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);

    // An unrouted path and an unrouted JSON-RPC method are both 404.
    for path in ["/fee", "/getpeers", "/getblocks", "/queryblocks", "/get_pool_changes"] {
        assert_eq!(call("GET", path, None).0, 404, "{path}");
    }
    let (status, _) =
        call("POST", "/json_rpc", Some(r#"{"jsonrpc":"2.0","id":1,"method":"on_getblockhash","params":[1]}"#));
    assert_eq!(status, 404, "on_getblockhash is not routed");

    // A JSON-RPC error is HTTP 200 with the daemon's own small code set.
    let (status, body) = call(
        "POST",
        "/json_rpc",
        Some(r#"{"jsonrpc":"2.0","id":1,"method":"getblockheaderbyheight","params":{"height":99999999999}}"#),
    );
    assert_eq!(status, 200);
    let j = parse(&body, ParseLimits::default()).unwrap();
    assert_eq!(j.get("error").unwrap().get("code").unwrap(), &wrkz_rpc::Json::I64(-2));

    // The 400 paths spec/09 names.
    let (status, body) = call("POST", "/json_rpc", Some("not json"));
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("Failed to parse request body as JSON"));
    let (status, body) = call("POST", "/json_rpc", Some("{}"));
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("Missing JSON parameter: 'method'"));
    let (status, body) = call("POST", "/get_global_indexes_for_range", Some(r#"{"startHeight":10,"endHeight":1}"#));
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("endHeight must be >= startHeight"));

    // `/sendrawtransaction` is HTTP 200 either way.
    let (status, body) = call("POST", "/sendrawtransaction", Some(r#"{"tx_as_hex":"zz"}"#));
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("Failed to parse transaction from hex buffer"));

    // Explorer routes are off on a standard daemon.
    let (status, body) = call(
        "POST",
        "/json_rpc",
        Some(r#"{"jsonrpc":"2.0","id":1,"method":"f_block_json","params":{"hash":"4213000"}}"#),
    );
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("--daemon-mode explorer"));
}
