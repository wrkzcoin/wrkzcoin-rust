// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The server on a real socket, driven by the wallet's own client
//! (`wrkz_wallet::txpow::TxPowServer`) — the path Rust Pluton Wallet takes.

use std::sync::Arc;
use std::time::Duration;

use wrkz_primitives::constants::TX_POW_NONCE_SIZE;
use wrkz_primitives::tx::{Input, Output, TransactionPrefix, TX_EXTRA_TRANSACTION_POW_NONCE};
use wrkz_txpow_server::api::{Api, ApiConfig};
use wrkz_txpow_server::log::{Level, Logger};
use wrkz_txpow_server::serve::{self, ServeConfig};
use wrkz_txpow_server::service::{Limits, PowService};
use wrkz_wallet::http::UreqTransport;
use wrkz_wallet::txpow::{TxPowError, TxPowServer};

/// Low enough that two threads find a nonce in well under a second.
const DIFFICULTY: u64 = 300;

struct Server {
    running: serve::Running,
    service: Arc<PowService>,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.service.stop();
        self.running.stop();
    }
}

fn start(api_key: &str) -> Server {
    let logger = Arc::new(Logger::new(Level::Disabled));
    let limits = Limits { threads: 2, fixed_difficulty: Some(DIFFICULTY), ..Limits::default() };
    let service = PowService::start(limits, Arc::clone(&logger));
    let config = ApiConfig {
        api_key: api_key.into(),
        cors_header: "*".into(),
        max_wait_ms: 30_000,
        max_difficulty: 1_000_000,
        ..ApiConfig::default()
    };
    let api = Arc::new(Api::new(config, Arc::clone(&service), Arc::clone(&logger)));
    let serve_config = ServeConfig { bind_ip: "127.0.0.1".into(), bind_port: 0, ..ServeConfig::default() };
    let running = serve::start(api, serve_config, logger).expect("bind");
    let url = format!("http://{}", running.local_addr());
    Server { running, service, url }
}

fn prefix(fee: u64) -> Vec<u8> {
    let mut extra = vec![0x01];
    extra.extend_from_slice(&[0x22; 32]);
    extra.push(TX_EXTRA_TRANSACTION_POW_NONCE);
    extra.extend_from_slice(&[0; TX_POW_NONCE_SIZE]);
    TransactionPrefix {
        version: 1,
        unlock_time: 0,
        inputs: vec![Input::Key { amount: 100_000, key_offsets: (1..=8).collect(), key_image: [5; 32] }],
        outputs: vec![Output { amount: 100_000 - fee, key: [6; 32] }],
        extra,
    }
    .to_bytes()
}

fn client(url: &str, key: Option<&str>) -> TxPowServer<UreqTransport> {
    TxPowServer::new(url, key, UreqTransport::for_pow_server()).unwrap().with_timeout(Duration::from_secs(60))
}

#[test]
fn the_wallet_client_gets_a_nonce_that_checks_out() {
    let server = start("s3cret");
    let bytes = prefix(1000);
    let nonce = client(&server.url, Some("s3cret")).solve(&bytes, DIFFICULTY, 4_400_000).expect("solved");

    let mut solved = bytes;
    let at = solved.len() - TX_POW_NONCE_SIZE;
    solved[at..].copy_from_slice(&nonce);
    assert!(wrkz_pow::check_hash(&wrkz_pow::cn_upx(&solved), DIFFICULTY));

    let probe = client(&server.url, None).probe();
    assert!(probe.ok, "{probe:?}");
    assert_eq!((probe.threads, probe.capacity), (2, 64));
}

#[test]
fn a_wrong_key_and_a_bad_prefix_are_refused_with_their_reason() {
    let server = start("s3cret");
    match client(&server.url, Some("wrong")).solve(&prefix(1000), DIFFICULTY, 4_400_000) {
        Err(TxPowError::Refused(why)) => assert!(why.contains("401") && why.contains("X-API-KEY"), "{why}"),
        other => panic!("{other:?}"),
    }
    match client(&server.url, Some("s3cret")).solve(&prefix(0), DIFFICULTY, 4_400_000) {
        Err(TxPowError::Refused(why)) => assert!(why.contains("zero-fee"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_other_routes_answer_as_documented() {
    let server = start("");
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();

    let preflight = agent.request("OPTIONS", &format!("{}/pow", server.url)).call().unwrap();
    assert_eq!(preflight.status(), 204);
    assert_eq!(preflight.header("Access-Control-Allow-Origin"), Some("*"));

    let unknown = agent.get(&format!("{}/pow/{}", server.url, "0".repeat(32))).call();
    match unknown {
        Err(ureq::Error::Status(404, r)) => assert!(r.into_string().unwrap().contains("unknown or expired job")),
        other => panic!("{:?}", other.map(|r| r.status())),
    }

    let _ = client(&server.url, None).solve(&prefix(1000), DIFFICULTY, 4_400_000).expect("solved");
    let stats: serde_json::Value =
        serde_json::from_str(&agent.get(&format!("{}/stats", server.url)).call().unwrap().into_string().unwrap())
            .unwrap();
    assert_eq!(stats["jobs"]["completed"], 1);
    assert_eq!(stats["limits"]["max_difficulty"], 1_000_000);
    assert!(stats["version"].as_str().unwrap().starts_with("wrkz-txpow-server "));

    let root: serde_json::Value =
        serde_json::from_str(&agent.get(&format!("{}/", server.url)).call().unwrap().into_string().unwrap()).unwrap();
    assert_eq!(root["name"], "wrkz-txpow-server");
}
