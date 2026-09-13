// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet-api` driven through its dispatcher, with no socket and no
//! daemon: every route, its status code, its JSON shape and its error paths.
//!
//! The wallet is one of the three fixtures in `tests/fixtures`, which were
//! written by the real C++ `wrkz-wallet-api` (see that directory's README), so
//! what these tests open is a wallet the C++ produced. The daemon is
//! [`CannedDaemon`], which answers `/info` with a fixed height and nothing
//! else, so the whole surface runs offline and deterministically.
//!
//! The one `#[ignore]`d test at the bottom drives a live
//! `http://127.0.0.1:17856` daemon through a real listener.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use wrkz_rpc::http::Request;
use wrkz_rpc::json::{Json, ParseLimits};

use wrkz_wallet::api::{dispatch, real_daemon_factory, serve, ApiConfig, ApiState, DaemonFactory, WalletDaemon};
use wrkz_wallet::daemon::{
    DaemonError, GlobalIndexes, Info, RandomOuts, SendResult, SyncRequest, TransactionsStatus, WalletSyncData,
};
use wrkz_wallet::sync::SyncDaemon;
use wrkz_wallet::transfer::TransferDaemon;

////////////////////////
/* CONSTANTS          */
////////////////////////

const PASSWORD: &str = "password";
const API_KEY: &str = "test-api-key";

/// The fixture wallet's primary address (`tests/fixtures/README.md`).
const ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
/// Subwallet index 1 of `spec05-subwallets.wallet`.
const SUBWALLET_ADDRESS: &str =
    "WrkzQdNHHbjcLRmmnmCVGHEew2YuXAZwgLULczrHRdrRbB7D23QWHA63mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkjzSFJF";
/// Subwallet index 2 of `spec05-subwallets.wallet`.
const SUBWALLET_ADDRESS_2: &str =
    "WrkzVMsEUdkjnkdnZnBChhCkwwLZ3CgoWZUodFDEc9yRSPL77sKUkRt3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkp2zfWK";
const SPEND_SECRET: &str = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
const VIEW_SECRET: &str = "779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700";
const SPEND_PUBLIC: &str = "857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3";
const VIEW_PUBLIC: &str = "0489cb98c7108372eaff2cdeddc5e76166b017a847537bf8499d61465395e942";
const MNEMONIC: &str = "eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell different \
                        dating intended richly slower inundate ridges slug inundate ridges slug were rotate rudely \
                        viewpoint";
const HASH: &str = "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f";
const NETWORK_HEIGHT: u64 = 4_213_000;

/// `Errors.h` codes the tests assert on.
const NOT_ENOUGH_BALANCE: u64 = 11;
const ADDRESS_NOT_IN_WALLET: u64 = 10;
const WRONG_PASSWORD: u64 = 5;
const ILLEGAL_VIEW_WALLET_OPERATION: u64 = 39;
const CANNOT_DELETE_PRIMARY_ADDRESS: u64 = 42;
const TX_PRIVATE_KEY_NOT_FOUND: u64 = 43;
const PREPARED_TRANSACTION_NOT_FOUND: u64 = 58;
const FILENAME_NON_EXISTENT: u64 = 1;
const WALLET_FILE_ALREADY_EXISTS: u64 = 8;

/// An address that parses but belongs to no fixture: a throwaway wallet's, made
/// once so every test names the same one.
fn foreign() -> &'static str {
    static ADDRESS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ADDRESS.get_or_init(|| {
        wrkz_wallet::file::Wallet::create_new(0).expect("keys").primary_address().expect("address").to_string()
    })
}

////////////////////////
/* CANNED DAEMON      */
////////////////////////

/// A daemon that answers `/info` and nothing else, so every route that only
/// needs a height works and every route that needs the chain fails the way it
/// would against an empty one.
struct CannedDaemon {
    network_height: u64,
    peers: u64,
    difficulty: u64,
}

impl Default for CannedDaemon {
    fn default() -> Self {
        CannedDaemon { network_height: NETWORK_HEIGHT, peers: 8, difficulty: 60_000 }
    }
}

impl SyncDaemon for CannedDaemon {
    fn wallet_sync_data(&self, _req: &SyncRequest) -> Result<WalletSyncData, DaemonError> {
        Ok(WalletSyncData {
            items: Vec::new(),
            scanned_to_height: None,
            synced: true,
            top_block: None,
            status: "OK".into(),
        })
    }

    fn global_indexes_for_range(&self, _start: u64, _end: u64) -> Result<GlobalIndexes, DaemonError> {
        Ok(GlobalIndexes { indexes: Vec::new(), status: "OK".into() })
    }

    fn transactions_status(&self, hashes: &[String]) -> Result<TransactionsStatus, DaemonError> {
        Ok(TransactionsStatus {
            transactions_in_pool: Vec::new(),
            transactions_in_block: Vec::new(),
            transactions_unknown: hashes.to_vec(),
            status: "OK".into(),
        })
    }

    fn info(&self) -> Result<Info, DaemonError> {
        Ok(Info {
            height: self.network_height + 1,
            network_height: self.network_height + 1,
            difficulty: self.difficulty,
            incoming_connections_count: self.peers / 2,
            outgoing_connections_count: self.peers - self.peers / 2,
            lite_start_height: 0,
            sync_features: vec!["skipEmptyBlocks".into()],
            compression: Some("none".into()),
            synced: true,
            top_block_hash: None,
            supported_height: None,
            upgrade_heights: Vec::new(),
            version: Some("0.4.8".into()),
            status: "OK".into(),
        })
    }
}

impl TransferDaemon for CannedDaemon {
    fn random_outs(&self, _amounts: &[u64], _outs_count: u64) -> Result<RandomOuts, DaemonError> {
        Ok(RandomOuts { outs: Vec::new(), status: "OK".into() })
    }

    fn send_raw_transaction(&self, _tx_hex: &str) -> Result<SendResult, DaemonError> {
        Ok(SendResult { status: "OK".into(), error: None })
    }
}

fn canned_factory() -> DaemonFactory {
    Box::new(|_host, _port, _ssl| Ok(Box::new(CannedDaemon::default()) as Box<dyn WalletDaemon>))
}

/// A factory that always fails, for the "daemon unreachable" path.
fn broken_factory() -> DaemonFactory {
    Box::new(|_host, _port, _ssl| Err("connection refused".to_string()))
}

////////////////////////
/* HARNESS            */
////////////////////////

/// A temporary directory that removes itself.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("wrkz-wallet-api-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    fn join(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }

    /// Copy a fixture in and return the path as the API would be given it.
    fn with_fixture(&self, fixture: &str, name: &str) -> String {
        let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(fixture);
        let dst = self.0.join(name);
        std::fs::copy(&src, &dst).expect("copy fixture");
        dst.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One response, decomposed the way the tests want it.
struct Answer {
    status: u16,
    body: String,
    json: Option<Json>,
}

impl Answer {
    fn json(&self) -> &Json {
        self.json.as_ref().unwrap_or_else(|| panic!("expected a JSON body, got {:?}", self.body))
    }

    fn str_at(&self, key: &str) -> &str {
        self.json().get(key).and_then(Json::as_str).unwrap_or_else(|| panic!("no string {key} in {}", self.body))
    }

    fn u64_at(&self, key: &str) -> u64 {
        self.json().get(key).and_then(Json::as_u64).unwrap_or_else(|| panic!("no number {key} in {}", self.body))
    }

    fn bool_at(&self, key: &str) -> bool {
        self.json().get(key).and_then(Json::as_bool).unwrap_or_else(|| panic!("no bool {key} in {}", self.body))
    }

    /// The `errorCode` of an error body, asserting the status is 400.
    fn error_code(&self) -> u64 {
        assert_eq!(self.status, 400, "expected a 400 error body, got {} {}", self.status, self.body);
        assert!(!self.str_at("errorMessage").is_empty(), "an error body must carry a message");
        self.u64_at("errorCode")
    }

    fn keys(&self) -> Vec<String> {
        match self.json() {
            Json::Object(items) => {
                let mut keys: Vec<String> = items.iter().map(|(k, _)| k.clone()).collect();
                keys.sort();
                keys
            }
            other => panic!("not an object: {other:?}"),
        }
    }
}

struct Api {
    state: Arc<ApiState>,
    dir: TempDir,
}

impl Api {
    fn new(tag: &str) -> Api {
        Api::with_factory(tag, canned_factory())
    }

    fn with_factory(tag: &str, factory: DaemonFactory) -> Api {
        let config = ApiConfig { rpc_password: API_KEY.to_string(), ..Default::default() };
        Api { state: Arc::new(ApiState::new(config, factory)), dir: TempDir::new(tag) }
    }

    fn with_cors(tag: &str, origin: &str) -> Api {
        let config =
            ApiConfig { rpc_password: API_KEY.to_string(), cors_header: origin.to_string(), ..Default::default() };
        Api { state: Arc::new(ApiState::new(config, canned_factory())), dir: TempDir::new(tag) }
    }

    /// One request with the correct `X-API-KEY`.
    fn call(&self, method: &str, path: &str, body: &str) -> Answer {
        self.call_with_key(method, path, body, Some(API_KEY))
    }

    fn call_with_key(&self, method: &str, path: &str, body: &str, key: Option<&str>) -> Answer {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(key) = key {
            headers.push(("X-API-KEY".to_string(), key.to_string()));
        }

        let request = Request {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            version: "HTTP/1.1".to_string(),
            headers,
            body: body.as_bytes().to_vec(),
        };

        let response = dispatch(&self.state, &request);
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let json = wrkz_rpc::json::parse(&response.body, ParseLimits::default()).ok();
        Answer { status: response.status, body, json }
    }

    /// Open a fixture wallet and assert it worked.
    fn open_fixture(&self, fixture: &str) -> String {
        let path = self.dir.with_fixture(fixture, fixture);
        let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
        let answer = self.call("POST", "/wallet/open", &body);
        assert_eq!(answer.status, 200, "open failed: {}", answer.body);
        assert!(answer.body.is_empty(), "openWallet sets no content");
        path
    }
}

/// A JSON string literal for a path, which on Windows is full of backslashes.
fn quote(s: &str) -> String {
    Json::Str(s.to_string()).to_string()
}

////////////////////////
/* MIDDLEWARE         */
////////////////////////

#[test]
fn a_missing_or_wrong_api_key_is_401_with_no_body() {
    let api = Api::new("auth");

    let answer = api.call_with_key("GET", "/status", "", None);
    assert_eq!(answer.status, 401);
    assert!(answer.body.is_empty());

    let answer = api.call_with_key("GET", "/status", "", Some("wrong"));
    assert_eq!(answer.status, 401);
    assert!(answer.body.is_empty());
}

#[test]
fn an_unrouted_path_is_404_before_authentication() {
    let api = Api::new("404");
    // No key at all, so this proves the 404 comes first.
    for (method, path) in
        [("GET", "/nope"), ("POST", "/save"), ("GET", "/transactions/notanumber"), ("PATCH", "/status")]
    {
        let answer = api.call_with_key(method, path, "", None);
        assert_eq!(answer.status, 404, "{method} {path}");
        assert!(answer.body.is_empty());
    }
}

#[test]
fn a_route_needing_an_open_wallet_is_403_when_none_is() {
    let api = Api::new("closed");
    for (method, path) in [
        ("GET", "/status"),
        ("GET", "/balance"),
        ("GET", "/addresses"),
        ("PUT", "/save"),
        ("DELETE", "/wallet"),
        ("POST", "/addresses/create"),
    ] {
        let answer = api.call(method, path, "");
        assert_eq!(answer.status, 403, "{method} {path} -> {}", answer.body);
        assert!(answer.body.is_empty());
    }
}

#[test]
fn opening_a_wallet_twice_is_403() {
    let api = Api::new("reopen");
    let path = api.open_fixture("spec05-seed.wallet");
    let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    let answer = api.call("POST", "/wallet/open", &body);
    assert_eq!(answer.status, 403);
    assert!(answer.body.is_empty());
}

#[test]
fn a_missing_request_parameter_is_400_with_no_body() {
    let api = Api::new("badjson");

    // `filename` is required; the C++ throws a json::exception, which the
    // middleware answers with a bare 400.
    let answer = api.call("POST", "/wallet/open", r#"{"password":"x"}"#);
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty(), "a missing parameter carries no error body");

    // The wrong type is the same exception.
    let answer = api.call("POST", "/wallet/open", r#"{"filename":5,"password":"x"}"#);
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty());

    // No body at all, likewise.
    let answer = api.call("POST", "/wallet/open", "");
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty());
}

#[test]
fn the_view_wallet_ban_is_400_with_the_errors_h_code() {
    let api = Api::new("viewban");
    api.open_fixture("spec05-view.wallet");

    for (method, path) in [
        ("POST", "/addresses/create"),
        ("GET", &format!("/keys/{ADDRESS}")),
        ("GET", &format!("/keys/mnemonic/{ADDRESS}")),
        ("GET", &format!("/transactions/privatekey/{HASH}")),
        ("POST", "/transactions/send/basic"),
        ("POST", "/transactions/send/sweep"),
    ] {
        let answer = api.call(method, path, "{}");
        assert_eq!(answer.error_code(), ILLEGAL_VIEW_WALLET_OPERATION, "{method} {path}");
    }

    // A view wallet may still read its balance and its addresses.
    assert_eq!(api.call("GET", "/balance", "").status, 200);
    assert_eq!(api.call("GET", "/addresses", "").status, 200);
}

#[test]
fn cors_headers_appear_only_when_enable_cors_set_one() {
    let plain = Api::new("nocors");
    let request = Request {
        method: "OPTIONS".into(),
        path: "/status".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: vec![("Access-Control-Request-Method".into(), "GET".into())],
        body: Vec::new(),
    };
    let response = dispatch(&plain.state, &request);
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Access-Control-Allow-Methods"), Some(""));
    assert_eq!(response.header("Access-Control-Allow-Origin"), None);

    let cors = Api::with_cors("cors", "*");
    let response = dispatch(&cors.state, &request);
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Access-Control-Allow-Methods"), Some("OPTIONS, GET, POST, PUT, DELETE"));
    assert_eq!(response.header("Access-Control-Allow-Origin"), Some("*"));
    assert_eq!(
        response.header("Access-Control-Allow-Headers"),
        Some("Origin, X-Requested-With, Content-Type, Accept, X-API-KEY")
    );

    // And on an ordinary answer too, including a rejected one.
    let answer = cors.call_with_key("GET", "/status", "", None);
    assert_eq!(answer.status, 401);
    let request = Request {
        method: "GET".into(),
        path: "/status".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: vec![("X-API-KEY".into(), API_KEY.into())],
        body: Vec::new(),
    };
    assert_eq!(dispatch(&cors.state, &request).header("Access-Control-Allow-Origin"), Some("*"));
}

////////////////////////
/* WALLET LIFECYCLE   */
////////////////////////

#[test]
fn open_reports_the_errors_h_code_for_every_failure() {
    let api = Api::new("openerr");

    let missing = api.dir.join("no-such.wallet");
    let answer = api.call("POST", "/wallet/open", &format!(r#"{{"filename":{},"password":"x"}}"#, quote(&missing)));
    assert_eq!(answer.error_code(), FILENAME_NON_EXISTENT);

    let path = api.dir.with_fixture("spec05-seed.wallet", "spec05-seed.wallet");
    let answer = api.call("POST", "/wallet/open", &format!(r#"{{"filename":{},"password":"wrong"}}"#, quote(&path)));
    assert_eq!(answer.error_code(), WRONG_PASSWORD);
}

#[test]
fn create_import_and_close_walk_the_whole_lifecycle() {
    let api = Api::new("lifecycle");

    // create
    let created = api.dir.join("created.wallet");
    let body = format!(r#"{{"filename":{},"password":"pw"}}"#, quote(&created));
    assert_eq!(api.call("POST", "/wallet/create", &body).status, 200);
    assert!(std::path::Path::new(&created).exists(), "createWallet writes the file");

    // A second create over the same file is WALLET_FILE_ALREADY_EXISTS, once
    // the first is closed.
    assert_eq!(api.call("DELETE", "/wallet", "").status, 200);
    assert_eq!(api.call("POST", "/wallet/create", &body).error_code(), WALLET_FILE_ALREADY_EXISTS);

    // import/seed
    let seeded = api.dir.join("seeded.wallet");
    let body = format!(
        r#"{{"filename":{},"password":"pw","mnemonicSeed":"{MNEMONIC}","scanHeight":4213000}}"#,
        quote(&seeded)
    );
    assert_eq!(api.call("POST", "/wallet/import/seed", &body).status, 200);
    assert_eq!(api.call("GET", "/addresses/primary", "").str_at("address"), ADDRESS);
    assert_eq!(api.call("DELETE", "/wallet", "").status, 200);

    // import/key
    let keyed = api.dir.join("keyed.wallet");
    let body = format!(
        r#"{{"filename":{},"password":"pw","privateSpendKey":"{SPEND_SECRET}","privateViewKey":"{VIEW_SECRET}","scanHeight":4213000}}"#,
        quote(&keyed)
    );
    assert_eq!(api.call("POST", "/wallet/import/key", &body).status, 200);
    assert_eq!(api.call("GET", "/addresses/primary", "").str_at("address"), ADDRESS);
    assert!(!api.call("GET", "/status", "").bool_at("isViewWallet"));
    assert_eq!(api.call("DELETE", "/wallet", "").status, 200);

    // import/view
    let viewed = api.dir.join("viewed.wallet");
    let body = format!(
        r#"{{"filename":{},"password":"pw","address":"{ADDRESS}","privateViewKey":"{VIEW_SECRET}","scanHeight":4213000}}"#,
        quote(&viewed)
    );
    assert_eq!(api.call("POST", "/wallet/import/view", &body).status, 200);
    assert!(api.call("GET", "/status", "").bool_at("isViewWallet"));
    assert_eq!(api.call("DELETE", "/wallet", "").status, 200);

    // and now nothing is open again
    assert_eq!(api.call("GET", "/status", "").status, 403);
}

#[test]
fn a_bad_seed_or_key_is_the_right_errors_h_code() {
    let api = Api::new("badimport");
    let path = api.dir.join("x.wallet");

    let body = format!(r#"{{"filename":{},"password":"pw","mnemonicSeed":"not a real seed"}}"#, quote(&path));
    assert_eq!(api.call("POST", "/wallet/import/seed", &body).error_code(), 7); // INVALID_MNEMONIC

    // A key of the wrong length never parses, so it is the bare-400 path.
    let body = format!(
        r#"{{"filename":{},"password":"pw","privateSpendKey":"aa","privateViewKey":"{VIEW_SECRET}"}}"#,
        quote(&path)
    );
    let answer = api.call("POST", "/wallet/import/key", &body);
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty());
}

#[test]
fn a_daemon_that_cannot_be_reached_is_daemon_offline() {
    let api = Api::with_factory("nodaemon", broken_factory());
    let path = api.dir.with_fixture("spec05-seed.wallet", "spec05-seed.wallet");
    let answer =
        api.call("POST", "/wallet/open", &format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path)));
    assert_eq!(answer.error_code(), 30); // DAEMON_OFFLINE
}

////////////////////////
/* ADDRESSES          */
////////////////////////

#[test]
fn the_address_routes_have_the_cpp_shapes() {
    let foreign_address = foreign();
    let api = Api::new("addresses");
    api.open_fixture("spec05-subwallets.wallet");

    let answer = api.call("GET", "/addresses", "");
    assert_eq!(answer.status, 200);
    let addresses = answer.json().get("addresses").and_then(Json::as_array).expect("addresses array");
    assert_eq!(addresses.len(), 3);
    // Insertion order: the primary, then deterministic indexes 1 and 2.
    assert_eq!(addresses[0].as_str(), Some(ADDRESS));
    assert_eq!(addresses[1].as_str(), Some(SUBWALLET_ADDRESS));
    assert_eq!(addresses[2].as_str(), Some(SUBWALLET_ADDRESS_2));

    let answer = api.call("GET", "/addresses/primary", "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["address"]);
    assert_eq!(answer.str_at("address"), ADDRESS);

    // create
    let answer = api.call("POST", "/addresses/create", "{}");
    assert_eq!(answer.status, 201);
    assert_eq!(answer.keys(), vec!["address", "privateSpendKey", "publicSpendKey", "walletIndex"]);
    assert_eq!(answer.u64_at("walletIndex"), 3);
    assert_eq!(answer.str_at("privateSpendKey").len(), 64);
    let created = answer.str_at("address").to_string();

    // delete what we just made, and refuse to delete the primary
    assert_eq!(api.call("DELETE", &format!("/addresses/{created}"), "").status, 200);
    assert_eq!(api.call("DELETE", &format!("/addresses/{ADDRESS}"), "").error_code(), CANNOT_DELETE_PRIMARY_ADDRESS);
    assert_eq!(api.call("DELETE", &format!("/addresses/{foreign_address}"), "").error_code(), ADDRESS_NOT_IN_WALLET);

    // import by key, then by index
    let answer = api.call(
        "POST",
        "/addresses/import",
        r#"{"privateSpendKey":"2c7d88e6b43bb83f7215ecc744e73589d8d1a841e7ab8f26672c5490c1aa2b0a","scanHeight":1}"#,
    );
    // Index 1 is already in this fixture.
    assert_eq!(answer.error_code(), 38); // SUBWALLET_ALREADY_EXISTS

    let answer = api.call("POST", "/addresses/import/deterministic", r#"{"walletIndex":9,"scanHeight":1}"#);
    assert_eq!(answer.status, 201);
    assert_eq!(answer.keys(), vec!["address"]);
}

#[test]
fn a_view_address_can_only_be_imported_into_a_view_wallet() {
    let api = Api::new("viewaddr");
    api.open_fixture("spec05-view.wallet");

    let body = format!(r#"{{"publicSpendKey":"{SPEND_PUBLIC}","scanHeight":1}}"#);
    // Already the primary key of this wallet.
    assert_eq!(api.call("POST", "/addresses/import/view", &body).error_code(), 38);

    let other = "2c1c4f98aed340fd311ab7d1fe51a1c2e879fde0eb74695e3d10b33d62cc5086";
    let answer = api.call("POST", "/addresses/import/view", &format!(r#"{{"publicSpendKey":"{other}"}}"#));
    assert_eq!(answer.status, 201);
    assert_eq!(answer.keys(), vec!["address"]);

    // The same call on a spend wallet is ILLEGAL_NON_VIEW_WALLET_OPERATION.
    let spend = Api::new("viewaddr2");
    spend.open_fixture("spec05-seed.wallet");
    let answer = spend.call("POST", "/addresses/import/view", &format!(r#"{{"publicSpendKey":"{other}"}}"#));
    assert_eq!(answer.error_code(), 40);
}

#[test]
fn validate_address_reports_both_kinds() {
    let api = Api::new("validate");

    // `DoesntMatter`: no wallet needs to be open.
    let answer = api.call("POST", "/addresses/validate", &format!(r#"{{"address":"{ADDRESS}"}}"#));
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["actualAddress", "isIntegrated", "paymentID", "publicSpendKey", "publicViewKey"]);
    assert!(!answer.bool_at("isIntegrated"));
    assert_eq!(answer.str_at("paymentID"), "");
    assert_eq!(answer.str_at("actualAddress"), ADDRESS);
    assert_eq!(answer.str_at("publicSpendKey"), SPEND_PUBLIC);
    assert_eq!(answer.str_at("publicViewKey"), VIEW_PUBLIC);

    // An integrated address, built by the route that makes them.
    api.open_fixture("spec05-seed.wallet");
    let integrated =
        api.call("GET", &format!("/addresses/{ADDRESS}/0102030405060708"), "").str_at("integratedAddress").to_string();
    let answer = api.call("POST", "/addresses/validate", &format!(r#"{{"address":"{integrated}"}}"#));
    assert!(answer.bool_at("isIntegrated"));
    assert_eq!(answer.str_at("paymentID"), "0102030405060708");
    assert_eq!(answer.str_at("actualAddress"), ADDRESS);

    // A bad one carries an Errors.h code.
    let answer = api.call("POST", "/addresses/validate", r#"{"address":"TRTLnope"}"#);
    assert!((12..=15).contains(&answer.error_code()), "an address error is 12..15, got {}", answer.error_code());
}

////////////////////////
/* KEYS               */
////////////////////////

#[test]
fn the_key_routes_return_the_fixture_keys() {
    let foreign_address = foreign();
    let api = Api::new("keys");
    api.open_fixture("spec05-subwallets.wallet");

    let answer = api.call("GET", "/keys", "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["privateViewKey"]);
    assert_eq!(answer.str_at("privateViewKey"), VIEW_SECRET);

    let answer = api.call("GET", &format!("/keys/{ADDRESS}"), "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["privateSpendKey", "publicSpendKey", "walletIndex"]);
    assert_eq!(answer.str_at("privateSpendKey"), SPEND_SECRET);
    assert_eq!(answer.str_at("publicSpendKey"), SPEND_PUBLIC);
    assert_eq!(answer.u64_at("walletIndex"), 0);

    let answer = api.call("GET", &format!("/keys/mnemonic/{ADDRESS}"), "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["mnemonicSeed"]);
    assert_eq!(answer.str_at("mnemonicSeed"), MNEMONIC);

    // A subwallet's view key is not derived from its spend key.
    let answer = api.call("GET", &format!("/keys/mnemonic/{SUBWALLET_ADDRESS}"), "");
    assert_eq!(answer.error_code(), 41); // KEYS_NOT_DETERMINISTIC

    // An address we do not hold.
    assert_eq!(api.call("GET", &format!("/keys/{foreign_address}"), "").error_code(), ADDRESS_NOT_IN_WALLET);
}

////////////////////////
/* STATUS AND NODE    */
////////////////////////

#[test]
fn status_has_every_field_the_cpp_prints() {
    let api = Api::new("status");
    api.open_fixture("spec05-seed.wallet");

    let answer = api.call("GET", "/status", "");
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.keys(),
        vec![
            "daemonLiteStartHeight",
            "hashrate",
            "isDaemonSynced",
            "isOutOfSync",
            "isSyncStalledByLiteNode",
            "isViewWallet",
            "isWalletSynced",
            "localDaemonBlockCount",
            "networkBlockCount",
            "peerCount",
            "subWalletCount",
            "syncGapCoveredTo",
            "syncGapDaemonServesFrom",
            "walletBlockCount",
        ]
    );
    assert_eq!(answer.u64_at("networkBlockCount"), NETWORK_HEIGHT);
    assert_eq!(answer.u64_at("localDaemonBlockCount"), NETWORK_HEIGHT);
    assert_eq!(answer.u64_at("peerCount"), 8);
    // `Nigel::hashrate` is the difficulty over the block time.
    assert_eq!(answer.u64_at("hashrate"), 1_000);
    assert_eq!(answer.u64_at("subWalletCount"), 1);
    assert!(answer.bool_at("isDaemonSynced"));
    assert!(answer.bool_at("isWalletSynced") == (answer.u64_at("walletBlockCount") + 10 >= NETWORK_HEIGHT));
    assert!(answer.bool_at("isOutOfSync"));
    assert!(!answer.bool_at("isViewWallet"));
    assert!(!answer.bool_at("isSyncStalledByLiteNode"));
    assert_eq!(answer.u64_at("daemonLiteStartHeight"), 0);
}

#[test]
fn the_node_routes_report_and_swap_the_daemon() {
    let api = Api::new("node");
    api.open_fixture("spec05-seed.wallet");

    let answer = api.call("GET", "/node", "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["daemonHost", "daemonPort", "daemonSSL", "nodeAddress", "nodeFee"]);
    assert_eq!(answer.str_at("daemonHost"), "127.0.0.1");
    assert_eq!(answer.u64_at("daemonPort"), 17856);
    assert!(!answer.bool_at("daemonSSL"));
    // No WrkzCoin daemon serves the `/fee` route `Nigel` reads these from.
    assert_eq!(answer.u64_at("nodeFee"), 0);
    assert_eq!(answer.str_at("nodeAddress"), "");

    let answer = api.call("PUT", "/node", r#"{"daemonHost":"node.example","daemonPort":1234}"#);
    assert_eq!(answer.status, 200);
    assert!(answer.body.is_empty());

    let answer = api.call("GET", "/node", "");
    assert_eq!(answer.str_at("daemonHost"), "node.example");
    assert_eq!(answer.u64_at("daemonPort"), 1234);

    // `daemonHost` is required.
    let answer = api.call("PUT", "/node", "{}");
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty());

    let answer = api.call("PUT", "/sync/refresh", "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["daemonHost", "daemonPort", "daemonSSL", "message", "status"]);
    assert_eq!(answer.str_at("status"), "OK");
    assert_eq!(answer.str_at("message"), "Wallet sync refresh triggered");
    assert_eq!(answer.str_at("daemonHost"), "node.example");
}

#[test]
fn save_reset_and_export_do_what_they_say() {
    let api = Api::new("maint");
    let path = api.open_fixture("spec05-seed.wallet");

    let answer = api.call("PUT", "/save", "");
    assert_eq!(answer.status, 200);
    assert!(answer.body.is_empty());

    let answer = api.call("PUT", "/reset", r#"{"scanHeight":1000}"#);
    assert_eq!(answer.status, 200);
    assert!(answer.body.is_empty());
    // `BlockDownloader::getHeight` is `SynchronizationStatus::getHeight`, which
    // a reset empties — so the C++ reports zero here too, not the scan height
    // (`BlockDownloader.cpp:119`).
    assert_eq!(api.call("GET", "/status", "").u64_at("walletBlockCount"), 0);

    let exported = api.dir.join("exported.json");
    let answer = api.call("POST", "/export/json", &format!(r#"{{"filename":{}}}"#, quote(&exported)));
    assert_eq!(answer.status, 200);
    let text = std::fs::read_to_string(&exported).expect("export written");
    assert!(text.contains("\"walletFileFormatVersion\""), "the export is the wallet JSON");
    assert!(text.contains(SPEND_SECRET), "the export is the plaintext container");

    // A path that cannot be written is INVALID_WALLET_FILENAME.
    let answer =
        api.call("POST", "/export/json", &format!(r#"{{"filename":{}}}"#, quote(&api.dir.join("no/such/dir/x.json"))));
    assert_eq!(answer.error_code(), 2);

    // The wallet file is still openable after all that.
    assert_eq!(api.call("DELETE", "/wallet", "").status, 200);
    let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    assert_eq!(api.call("POST", "/wallet/open", &body).status, 200);
}

////////////////////////
/* BALANCES           */
////////////////////////

#[test]
fn the_balance_routes_have_the_cpp_shapes() {
    let foreign_address = foreign();
    let api = Api::new("balance");
    api.open_fixture("spec05-subwallets.wallet");

    let answer = api.call("GET", "/balance", "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["locked", "unlocked"]);
    assert_eq!(answer.u64_at("unlocked"), 0);
    assert_eq!(answer.u64_at("locked"), 0);

    let answer = api.call("GET", &format!("/balance/{ADDRESS}"), "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["locked", "unlocked"]);

    assert_eq!(api.call("GET", &format!("/balance/{foreign_address}"), "").error_code(), ADDRESS_NOT_IN_WALLET);

    // `/balances` is a bare array, one object per address.
    let answer = api.call("GET", "/balances", "");
    assert_eq!(answer.status, 200);
    let entries = answer.json().as_array().expect("a top level array");
    assert_eq!(entries.len(), 3);
    for entry in entries {
        assert!(entry.has("address") && entry.has("unlocked") && entry.has("locked"));
    }
    assert_eq!(entries[0].get("address").and_then(Json::as_str), Some(ADDRESS));
}

////////////////////////
/* TRANSACTIONS       */
////////////////////////

#[test]
fn every_transaction_listing_answers_with_a_transactions_array() {
    let api = Api::new("txs");
    api.open_fixture("spec05-seed.wallet");

    for path in [
        "/transactions".to_string(),
        "/transactions/unconfirmed".to_string(),
        format!("/transactions/unconfirmed/{ADDRESS}"),
        "/transactions/0".to_string(),
        "/transactions/0/1000".to_string(),
        format!("/transactions/address/{ADDRESS}/0"),
        format!("/transactions/address/{ADDRESS}/0/1000"),
        format!("/transactions/paymentid/{HASH}"),
        "/transactions/paymentid".to_string(),
    ] {
        let answer = api.call("GET", &path, "");
        assert_eq!(answer.status, 200, "{path} -> {}", answer.body);
        assert_eq!(answer.keys(), vec!["transactions"], "{path}");
        assert_eq!(answer.json().get("transactions").and_then(Json::as_array).map(<[Json]>::len), Some(0), "{path}");
    }
}

#[test]
fn a_height_range_the_wrong_way_round_is_400_with_no_body() {
    let api = Api::new("range");
    api.open_fixture("spec05-seed.wallet");

    for path in [
        "/transactions/1000/1000".to_string(),
        "/transactions/2000/1000".to_string(),
        format!("/transactions/address/{ADDRESS}/2000/1000"),
    ] {
        let answer = api.call("GET", &path, "");
        assert_eq!(answer.status, 400, "{path}");
        assert!(answer.body.is_empty(), "{path}");
    }

    // A height above u64::MAX is the same 400: the regex matched, `stoull`
    // threw.
    let answer = api.call("GET", "/transactions/99999999999999999999999999", "");
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty());
}

#[test]
fn an_unknown_transaction_hash_is_404_and_a_missing_private_key_is_an_error() {
    let api = Api::new("txlookup");
    api.open_fixture("spec05-seed.wallet");

    let answer = api.call("GET", &format!("/transactions/hash/{HASH}"), "");
    assert_eq!(answer.status, 404);
    assert!(answer.body.is_empty());

    let answer = api.call("GET", &format!("/transactions/privatekey/{HASH}"), "");
    assert_eq!(answer.error_code(), TX_PRIVATE_KEY_NOT_FOUND);
}

#[test]
fn a_transactions_transfers_carry_an_address_not_a_public_key() {
    // The wallet fixtures hold no transactions, so this walks the same JSON
    // builder over a wallet with one recorded by hand.
    let api = Api::new("transfers");
    let path = api.dir.join("with-tx.wallet");

    {
        use wrkz_wallet::file::{Hex32, SecretKey, Transaction, Transfer, Wallet};
        let mut wallet = Wallet::import_from_keys(
            &SecretKey::from_hex(SPEND_SECRET).unwrap(),
            &SecretKey::from_hex(VIEW_SECRET).unwrap(),
            4_213_000,
        )
        .unwrap();
        let spend_key = wallet.primary_sub_wallet().unwrap().public_spend_key;
        wallet.sub_wallets.transactions.push(Transaction {
            block_height: 4_213_001,
            fee: 1000,
            hash: Hex32::from_hex(HASH).unwrap(),
            is_coinbase_transaction: false,
            payment_id: "0102030405060708".into(),
            timestamp: 1_700_000_000,
            transfers: vec![Transfer { amount: 123_456, public_key: spend_key }],
            unlock_time: 0,
        });
        wallet.save(&path, PASSWORD).unwrap();
    }

    let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    assert_eq!(api.call("POST", "/wallet/open", &body).status, 200);

    let answer = api.call("GET", "/transactions", "");
    let txs = answer.json().get("transactions").and_then(Json::as_array).expect("array");
    assert_eq!(txs.len(), 1);
    let tx = &txs[0];

    let mut keys: Vec<&str> = match tx {
        Json::Object(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
        other => panic!("not an object: {other:?}"),
    };
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "blockHeight",
            "fee",
            "hash",
            "isCoinbaseTransaction",
            "paymentID",
            "timestamp",
            "transfers",
            "unlockTime"
        ]
    );

    let transfers = tx.get("transfers").and_then(Json::as_array).expect("transfers");
    assert_eq!(transfers.len(), 1);
    // `publicKeysToAddresses` swapped the key for the address.
    assert_eq!(transfers[0].get("address").and_then(Json::as_str), Some(ADDRESS));
    assert!(!transfers[0].has("publicKey"), "publicKey is erased");
    assert_eq!(transfers[0].get("amount").and_then(Json::as_u64), Some(123_456));

    // The single-transaction route wraps it in `transaction`.
    let answer = api.call("GET", &format!("/transactions/hash/{HASH}"), "");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.keys(), vec!["transaction"]);

    // …and the payment-id routes find it.
    let answer = api.call("GET", "/transactions/paymentid", "");
    assert_eq!(answer.json().get("transactions").and_then(Json::as_array).map(<[Json]>::len), Some(1));

    // The height range includes it, and one above it does not.
    let answer = api.call("GET", "/transactions/4213001/4213002", "");
    assert_eq!(answer.json().get("transactions").and_then(Json::as_array).map(<[Json]>::len), Some(1));
    let answer = api.call("GET", "/transactions/4213002/4213003", "");
    assert_eq!(answer.json().get("transactions").and_then(Json::as_array).map(<[Json]>::len), Some(0));
}

////////////////////////
/* SENDING            */
////////////////////////

#[test]
fn a_send_from_an_empty_wallet_is_not_enough_balance() {
    let foreign_address = foreign();
    let api = Api::new("send");
    api.open_fixture("spec05-seed.wallet");

    let body = format!(r#"{{"destination":"{foreign_address}","amount":1000}}"#);
    for path in ["/transactions/prepare/basic", "/transactions/send/basic"] {
        assert_eq!(api.call("POST", path, &body).error_code(), NOT_ENOUGH_BALANCE, "{path}");
    }

    let body = format!(r#"{{"destinations":[{{"address":"{foreign_address}","amount":1000}}]}}"#);
    for path in ["/transactions/prepare/advanced", "/transactions/send/advanced"] {
        assert_eq!(api.call("POST", path, &body).error_code(), NOT_ENOUGH_BALANCE, "{path}");
    }
}

/// A wallet holding one large unlocked input, so a send gets past
/// `validateAmount` and the checks after it can be reached at all.
fn funded_wallet(dir: &TempDir, name: &str) -> String {
    use wrkz_wallet::file::{Hex32, SecretKey, TransactionInput, Wallet};

    let path = dir.join(name);
    let mut wallet = Wallet::import_from_keys(
        &SecretKey::from_hex(SPEND_SECRET).unwrap(),
        &SecretKey::from_hex(VIEW_SECRET).unwrap(),
        4_213_000,
    )
    .unwrap();

    wallet.sub_wallets.sub_wallet[0].unspent_inputs.push(TransactionInput {
        amount: 100_000_000,
        block_height: 4_200_000,
        global_output_index: Some(1),
        key: Hex32([1u8; 32]),
        key_image: Hex32([2u8; 32]),
        parent_transaction_hash: Hex32::from_hex(HASH).unwrap(),
        private_ephemeral: None,
        spend_height: 0,
        transaction_index: 0,
        transaction_public_key: Hex32([3u8; 32]),
        unlock_time: 0,
    });

    wallet.save(&path, PASSWORD).unwrap();
    path
}

#[test]
fn the_send_routes_validate_before_they_build() {
    let foreign_address = foreign();
    let api = Api::new("sendvalidate");
    let path = funded_wallet(&api.dir, "funded.wallet");
    let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    assert_eq!(api.call("POST", "/wallet/open", &body).status, 200);
    assert_eq!(api.call("GET", "/balance", "").u64_at("unlocked"), 100_000_000);

    // `validateTransaction` (`ValidateParameters.cpp:60`) runs its checks in a
    // fixed order, and each of these is the first one to fail.

    // validateDestinations: a zero amount before a bad address.
    let body = format!(r#"{{"destination":"{foreign_address}","amount":0}}"#);
    assert_eq!(api.call("POST", "/transactions/send/basic", &body).error_code(), 19); // AMOUNT_IS_ZERO

    // validateDestinations: an address that is not one.
    let answer = api.call("POST", "/transactions/send/basic", r#"{"destination":"nope","amount":1000}"#);
    assert!((12..=15).contains(&answer.error_code()));

    // validateOurAddresses, on `sourceAddresses`.
    let body = format!(
        r#"{{"destinations":[{{"address":"{foreign_address}","amount":1000}}],"sourceAddresses":["{foreign_address}"]}}"#
    );
    assert_eq!(api.call("POST", "/transactions/send/advanced", &body).error_code(), ADDRESS_NOT_IN_WALLET);

    // validateAmount, which is why the empty-wallet test above never reaches
    // the checks below.
    let body = format!(r#"{{"destination":"{foreign_address}","amount":99999999999}}"#);
    assert_eq!(api.call("POST", "/transactions/send/basic", &body).error_code(), NOT_ENOUGH_BALANCE);

    // validateMixin.
    let body = format!(r#"{{"destinations":[{{"address":"{foreign_address}","amount":1000}}],"mixin":99}}"#);
    assert_eq!(api.call("POST", "/transactions/send/advanced", &body).error_code(), 22); // MIXIN_TOO_BIG

    // validatePaymentID.
    let body = format!(r#"{{"destination":"{foreign_address}","amount":1000,"paymentID":"zzzzzzzzzzzzzzzz"}}"#);
    assert_eq!(api.call("POST", "/transactions/send/basic", &body).error_code(), 24); // PAYMENT_ID_INVALID

    let body = format!(r#"{{"destination":"{foreign_address}","amount":1000,"paymentID":"abcd"}}"#);
    assert_eq!(api.call("POST", "/transactions/send/basic", &body).error_code(), 23); // PAYMENT_ID_WRONG_LENGTH

    // validateOurAddresses, on `changeAddress`.
    let body = format!(
        r#"{{"destinations":[{{"address":"{foreign_address}","amount":1000}}],"changeAddress":"{foreign_address}"}}"#
    );
    assert_eq!(api.call("POST", "/transactions/send/advanced", &body).error_code(), ADDRESS_NOT_IN_WALLET);

    // validateUnlockTime: a height below the minimum delay.
    let body = format!(r#"{{"destinations":[{{"address":"{foreign_address}","amount":1000}}],"unlockTime":4213001}}"#);
    assert_eq!(api.call("POST", "/transactions/send/advanced", &body).error_code(), 60); // UNLOCK_TIME_TOO_SMALL

    // The handler's own checks, which run before any of that.
    let answer = api.call("POST", "/transactions/send/advanced", r#"{"destinations":"nope"}"#);
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty(), "a mistyped parameter carries no error body");

    let body = format!(r#"{{"destinations":[{{"address":"{foreign_address}","amount":1000}}],"extra":"zz"}}"#);
    assert_eq!(api.call("POST", "/transactions/send/advanced", &body).error_code(), 53);
    // INVALID_EXTRA_DATA
}

#[test]
fn a_send_that_gets_past_validation_reports_the_daemons_refusal() {
    // The canned daemon returns no decoys, which is what an empty chain does,
    // so this is the last error before the ring would be built.
    let foreign_address = foreign();
    let api = Api::new("sendbuild");
    let path = funded_wallet(&api.dir, "funded.wallet");
    let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    assert_eq!(api.call("POST", "/wallet/open", &body).status, 200);

    let body = format!(r#"{{"destination":"{foreign_address}","amount":1000}}"#);
    let answer = api.call("POST", "/transactions/prepare/basic", &body);
    // 27 CANT_GET_FAKE_OUTPUTS or 28 NOT_ENOUGH_FAKE_OUTPUTS, depending on
    // which side of the ring build gave up first.
    assert!(
        matches!(answer.error_code(), 27 | 28),
        "expected a fake-output error, got {} {}",
        answer.error_code(),
        answer.body
    );
}

#[test]
fn a_prepared_transaction_can_be_deleted_and_an_unknown_one_cannot_be_sent() {
    let api = Api::new("prepared");
    api.open_fixture("spec05-seed.wallet");

    // Nothing has ever been prepared.
    let answer = api.call("DELETE", &format!("/transactions/prepared/{HASH}"), "");
    assert_eq!(answer.status, 404);
    assert!(answer.body.is_empty());

    let answer = api.call("POST", "/transactions/send/prepared", &format!(r#"{{"transactionHash":"{HASH}"}}"#));
    assert_eq!(answer.error_code(), PREPARED_TRANSACTION_NOT_FOUND);

    // A hash that is not 64 hex is not routed at all.
    assert_eq!(api.call("DELETE", "/transactions/prepared/abc", "").status, 404);
}

#[test]
fn a_sweep_of_an_empty_wallet_answers_200_with_a_failed_batch() {
    let foreign_address = foreign();
    let api = Api::new("sweep");
    api.open_fixture("spec05-seed.wallet");

    for path in ["/transactions/send/sweep", "/transactions/send/sweep/all"] {
        let answer = api.call("POST", path, &format!(r#"{{"destination":"{foreign_address}"}}"#));
        assert_eq!(answer.status, 200, "{path} -> {}", answer.body);
        assert_eq!(answer.keys(), vec!["transactions"], "{path}");

        let batches = answer.json().get("transactions").and_then(Json::as_array).expect("array");
        assert_eq!(batches.len(), 1, "{path}");
        assert_eq!(batches[0].get("success").and_then(Json::as_bool), Some(false));
        assert_eq!(batches[0].get("errorCode").and_then(Json::as_u64), Some(NOT_ENOUGH_BALANCE));
        assert!(batches[0].has("errorMessage"));
    }

    // A destination that is not an address is one failed batch, not a 400 —
    // `sweepToAddress` returns the error inside the array.
    let answer = api.call("POST", "/transactions/send/sweep/all", r#"{"destination":"nope"}"#);
    assert_eq!(answer.status, 200);
    let batches = answer.json().get("transactions").and_then(Json::as_array).expect("array");
    assert_eq!(batches[0].get("success").and_then(Json::as_bool), Some(false));

    // `destination` itself is required.
    let answer = api.call("POST", "/transactions/send/sweep", "{}");
    assert_eq!(answer.status, 400);
    assert!(answer.body.is_empty());
}

////////////////////////
/* BODY FORMATTING    */
////////////////////////

#[test]
fn every_body_is_nlohmann_dump_four_with_a_trailing_newline() {
    let foreign_address = foreign();
    let api = Api::new("format");
    api.open_fixture("spec05-seed.wallet");

    let answer = api.call("GET", "/balance", "");
    assert_eq!(answer.body, "{\n    \"locked\": 0,\n    \"unlocked\": 0\n}\n");

    // An empty array stays on one line, as `nlohmann` prints it.
    let answer = api.call("GET", "/transactions", "");
    assert_eq!(answer.body, "{\n    \"transactions\": []\n}\n");

    // And an error body has the same shape.
    let answer = api.call("GET", &format!("/balance/{foreign_address}"), "");
    assert!(answer.body.starts_with("{\n    \"errorCode\": 10,\n    \"errorMessage\": \""));
    assert!(answer.body.ends_with("\"\n}\n"));
}

#[test]
fn every_json_response_says_it_is_json() {
    let api = Api::new("ctype");
    api.open_fixture("spec05-seed.wallet");

    let request = Request {
        method: "GET".into(),
        path: "/balance".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: vec![("X-API-KEY".into(), API_KEY.into())],
        body: Vec::new(),
    };
    let response = dispatch(&api.state, &request);
    assert_eq!(response.header("Content-Type"), Some("application/json"));
}

////////////////////////
/* THE LISTENER       */
////////////////////////

#[test]
fn the_listener_serves_the_dispatcher_over_a_real_socket() {
    let dir = TempDir::new("listener");
    let path = {
        let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("spec05-seed.wallet");
        let dst = dir.0.join("spec05-seed.wallet");
        std::fs::copy(&src, &dst).unwrap();
        dst.to_string_lossy().into_owned()
    };

    let config = ApiConfig { rpc_password: API_KEY.into(), ..Default::default() };
    let state = Arc::new(ApiState::new(config, canned_factory()));
    let mut server =
        serve::start(Arc::clone(&state), serve::ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() })
            .expect("bind");
    let addr = server.local_addr();

    let base = format!("http://{addr}");
    let key = [("X-API-KEY", API_KEY)];

    // Unauthenticated first.
    let (status, _) = http_call(&base, "GET", "/status", "", &[]);
    assert_eq!(status, 401);

    // Then open a wallet and read its address back.
    let body = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    let (status, _) = http_call(&base, "POST", "/wallet/open", &body, &key);
    assert_eq!(status, 200);

    let (status, body) = http_call(&base, "GET", "/addresses/primary", "", &key);
    assert_eq!(status, 200);
    assert!(body.contains(ADDRESS), "{body}");

    let (status, _) = http_call(&base, "DELETE", "/wallet", "", &key);
    assert_eq!(status, 200);

    server.stop();
}

#[test]
fn the_ipv6_listener_serves_the_same_routes_or_says_why_it_cannot() {
    let config = ApiConfig { rpc_password: API_KEY.into(), ..Default::default() };
    let state = Arc::new(ApiState::new(config, canned_factory()));
    let serve_config =
        serve::ServeConfig { bind: "127.0.0.1:0".into(), bind_ipv6: "[::1]:0".into(), ..Default::default() };
    let mut server = serve::start(Arc::clone(&state), serve_config).expect("the IPv4 listener binds");

    match server.local_addr6() {
        Some(addr6) => {
            assert!(addr6.is_ipv6());
            let base = format!("http://{addr6}");
            // The same middleware: no key is 401, a key with no wallet is 403.
            assert_eq!(http_call(&base, "GET", "/status", "", &[]).0, 401);
            assert_eq!(http_call(&base, "GET", "/status", "", &[("X-API-KEY", API_KEY)]).0, 403);
        }
        // A host with no IPv6 loopback: the C++ warns and serves on; so does this.
        None => assert!(server.ipv6_error().is_some(), "no IPv6 listener and no reason why"),
    }
    assert_eq!(http_call(&format!("http://{}", server.local_addr()), "GET", "/status", "", &[]).0, 401);
    server.stop();

    // An IPv4 address given for the IPv6 listener is refused, not bound twice.
    let serve_config =
        serve::ServeConfig { bind: "127.0.0.1:0".into(), bind_ipv6: "127.0.0.1:0".into(), ..Default::default() };
    let server = serve::start(Arc::clone(&state), serve_config).expect("IPv4 still binds");
    assert!(server.local_addr6().is_none());
    assert!(server.ipv6_error().is_some_and(|e| e.contains("not an IPv6 address")), "{:?}", server.ipv6_error());
}

#[test]
fn a_large_answer_is_gzipped_only_for_a_client_that_asks() {
    use std::io::{Read, Write};

    let api = Api::new("gzip");
    api.open_fixture("spec05-seed.wallet");
    // Enough addresses that the pretty-printed list passes the kilobyte floor.
    for _ in 0..16 {
        assert_eq!(api.call("POST", "/addresses/create", "").status, 201);
    }
    let mut server =
        serve::start(Arc::clone(&api.state), serve::ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() })
            .expect("bind");

    let fetch = |accept: &str| -> (String, Vec<u8>) {
        let mut stream = std::net::TcpStream::connect(server.local_addr()).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let request =
            format!("GET /addresses HTTP/1.1\r\nHost: x\r\nX-API-KEY: {API_KEY}\r\n{accept}Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a head");
        (String::from_utf8_lossy(&raw[..split]).into_owned(), raw[split + 4..].to_vec())
    };

    let (head, body) = fetch("Accept-Encoding: gzip, deflate\r\n");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("Content-Encoding: gzip"), "{head}");
    let mut plain = String::new();
    flate2::read::GzDecoder::new(&body[..]).read_to_string(&mut plain).expect("a gzip body");
    let json = wrkz_rpc::json::parse(plain.as_bytes(), ParseLimits::default()).expect("JSON inside");
    assert_eq!(json.get("addresses").and_then(Json::as_array).map(<[Json]>::len), Some(17));

    // No `Accept-Encoding`, or one that refuses gzip: identity, as before.
    for accept in ["", "Accept-Encoding: gzip;q=0\r\n"] {
        let (head, body) = fetch(accept);
        assert!(!head.contains("Content-Encoding"), "{head}");
        assert!(wrkz_rpc::json::parse(&body, ParseLimits::default()).is_ok());
    }

    server.stop();
}

#[cfg(unix)]
#[test]
fn the_ipc_socket_serves_the_same_routes_and_still_wants_the_key() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let dir = TempDir::new("ipc");
    let socket = dir.join("api.sock");
    let wallet = dir.with_fixture("spec05-seed.wallet", "spec05-seed.wallet");

    let config = ApiConfig { rpc_password: API_KEY.into(), ..Default::default() };
    let state = Arc::new(ApiState::new(config, canned_factory()));
    let serve_config = serve::ServeConfig {
        bind: "127.0.0.1:0".into(),
        ipc: Some(wrkz_wallet::listen::IpcConfig { path: socket.clone(), mode: 0o600, group: String::new() }),
        ..Default::default()
    };
    let mut server = serve::start(Arc::clone(&state), serve_config).expect("bind");
    assert_eq!(server.ipc_path(), Some(socket.as_str()), "{:?}", server.ipc_error());

    let meta = std::fs::metadata(&socket).expect("the socket file exists");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600, "owner only, as asked");

    let call = |method: &str, path: &str, body: &str, headers: &[(&str, &str)]| -> (u16, String) {
        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let body = if body.is_empty() { None } else { Some(body.as_bytes()) };
        let (status, out) =
            wrkz_wallet::ipc::exchange(stream, method, path, "localhost", body, headers, 1 << 20).expect("exchange");
        (status, String::from_utf8_lossy(&out).into_owned())
    };

    // The socket's mode decides who may connect; the key still decides who may
    // do anything (`ParseArguments.cpp:104`).
    assert_eq!(call("GET", "/status", "", &[]).0, 401);
    assert_eq!(call("GET", "/status", "", &[("X-API-KEY", "wrong")]).0, 401);

    let key = [("X-API-KEY", API_KEY)];
    let open = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&wallet));
    assert_eq!(call("POST", "/wallet/open", &open, &key).0, 200);
    let (status, body) = call("GET", "/addresses/primary", "", &key);
    assert_eq!(status, 200);
    assert!(body.contains(ADDRESS), "{body}");

    server.stop();
    assert!(!std::path::Path::new(&socket).exists(), "the socket file is removed on the way out");
}

/// One HTTP round trip over a real socket, through the same
/// [`wrkz_wallet::ipc::exchange`] the IPC daemon client uses — which is also
/// what makes that transport exercised on a platform with no Unix sockets.
fn http_call(base: &str, method: &str, path: &str, body: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let addr = base.trim_start_matches("http://").to_string();
    let stream = std::net::TcpStream::connect(&addr).expect("connect");
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10))).unwrap();

    let body = if body.is_empty() { None } else { Some(body.as_bytes()) };
    let (status, out) =
        wrkz_wallet::ipc::exchange(stream, method, path, &addr, body, headers, 32 * 1024 * 1024).expect("exchange");
    (status, String::from_utf8_lossy(&out).into_owned())
}

////////////////////////
/* LIVE               */
////////////////////////

/// Against a daemon on `http://127.0.0.1:17856`, with a throwaway wallet.
///
/// ```sh
/// cargo test -p wrkz-wallet --test api -- --ignored live
/// ```
#[test]
#[ignore = "needs a daemon on 127.0.0.1:17856"]
fn live_the_api_syncs_and_reports_against_a_real_daemon() {
    let dir = TempDir::new("live");
    let path = dir.join("live.wallet");

    let config = ApiConfig { rpc_password: API_KEY.into(), ..Default::default() };
    let state = Arc::new(ApiState::new(config, real_daemon_factory()));
    let mut server =
        serve::start(Arc::clone(&state), serve::ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() })
            .expect("bind");
    let base = format!("http://{}", server.local_addr());
    let key = [("X-API-KEY", API_KEY)];

    let body = format!(
        r#"{{"filename":{},"password":"pw","mnemonicSeed":"{MNEMONIC}","scanHeight":4213000,"daemonHost":"127.0.0.1","daemonPort":17856}}"#,
        quote(&path)
    );
    let (status, body) = http_call(&base, "POST", "/wallet/import/seed", &body, &key);
    assert_eq!(status, 200, "{body}");

    let (status, body) = http_call(&base, "GET", "/status", "", &key);
    assert_eq!(status, 200);
    let json = wrkz_rpc::json::parse(body.as_bytes(), ParseLimits::default()).expect("json");
    assert!(json.get("networkBlockCount").and_then(Json::as_u64).unwrap_or(0) > 4_000_000, "{body}");
    assert!(json.get("peerCount").and_then(Json::as_u64).is_some());

    let (status, body) = http_call(&base, "GET", "/node", "", &key);
    assert_eq!(status, 200, "{body}");

    let (status, _) = http_call(&base, "DELETE", "/wallet", "", &key);
    assert_eq!(status, 200);
    server.stop();
}

////////////////////////
/* LOCKING            */
////////////////////////

/// A door a daemon call waits at, so a test can hold a round trip open for as
/// long as it likes and see what the rest of the API does meanwhile.
#[derive(Default)]
struct Gate {
    /// Calls that have reached the door, and whether it is open.
    state: std::sync::Mutex<(u32, bool)>,
    changed: std::sync::Condvar,
}

impl Gate {
    /// Wait here until the test opens the door.
    fn pass(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Whether `n` calls have reached the door within ten seconds.
    fn reached_by(&self, n: u32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut state = self.state.lock().unwrap();
        while state.0 < n {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
        true
    }

    fn callers(&self) -> u32 {
        self.state.lock().unwrap().0
    }

    fn open(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

/// [`CannedDaemon`] with a [`Gate`] in front of `/getwalletsyncdata`, of
/// `/getrandom_outs`, or both.
struct GatedDaemon {
    canned: CannedDaemon,
    sync: Option<Arc<Gate>>,
    outs: Option<Arc<Gate>>,
}

impl SyncDaemon for GatedDaemon {
    fn wallet_sync_data(&self, req: &SyncRequest) -> Result<WalletSyncData, DaemonError> {
        if let Some(gate) = &self.sync {
            gate.pass();
        }
        self.canned.wallet_sync_data(req)
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> Result<GlobalIndexes, DaemonError> {
        self.canned.global_indexes_for_range(start, end)
    }

    fn transactions_status(&self, hashes: &[String]) -> Result<TransactionsStatus, DaemonError> {
        self.canned.transactions_status(hashes)
    }

    fn info(&self) -> Result<Info, DaemonError> {
        self.canned.info()
    }
}

impl TransferDaemon for GatedDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> Result<RandomOuts, DaemonError> {
        if let Some(gate) = &self.outs {
            gate.pass();
        }
        self.canned.random_outs(amounts, outs_count)
    }

    fn send_raw_transaction(&self, tx_hex: &str) -> Result<SendResult, DaemonError> {
        self.canned.send_raw_transaction(tx_hex)
    }
}

fn gated_factory(sync: Option<Arc<Gate>>, outs: Option<Arc<Gate>>) -> DaemonFactory {
    Box::new(move |_host, _port, _ssl| {
        let daemon = GatedDaemon { canned: CannedDaemon::default(), sync: sync.clone(), outs: outs.clone() };
        Ok(Box::new(daemon) as Box<dyn WalletDaemon>)
    })
}

/// One request through the dispatcher, from whichever thread calls it.
fn dispatch_on(state: &ApiState, method: &str, path: &str, body: &str) -> (u16, String) {
    let request = Request {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        version: "HTTP/1.1".to_string(),
        headers: vec![("X-API-KEY".to_string(), API_KEY.to_string())],
        body: body.as_bytes().to_vec(),
    };
    let response = dispatch(state, &request);
    (response.status, String::from_utf8_lossy(&response.body).into_owned())
}

/// `f` on a thread of its own, or `None` if it has not answered within `limit`.
fn within<T: Send + 'static>(limit: std::time::Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(f());
    });
    receiver.recv_timeout(limit).ok()
}

/// The routes an integration polls, which must never queue behind sync or a
/// send.
const READ_ONLY: [&str; 7] = ["/status", "/balance", "/balances", "/addresses", "/keys", "/node", "/transactions"];

#[test]
fn reads_answer_while_the_sync_thread_waits_on_the_daemon() {
    let gate = Arc::new(Gate::default());
    let api = Api::with_factory("syncwait", gated_factory(Some(Arc::clone(&gate)), None));
    api.open_fixture("spec05-seed.wallet");
    let mut server =
        serve::start(Arc::clone(&api.state), serve::ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() })
            .expect("bind");

    // The sync thread is now inside its round trip, holding the wallet it is
    // syncing — the C++ holds no dispatcher lock across one at all.
    assert!(gate.reached_by(1), "the sync thread never reached the daemon");

    for path in READ_ONLY {
        let state = Arc::clone(&api.state);
        let (status, body) = within(std::time::Duration::from_secs(5), move || dispatch_on(&state, "GET", path, ""))
            .unwrap_or_else(|| panic!("GET {path} waited for the sync thread's daemon round trip"));
        assert_eq!(status, 200, "GET {path}: {body}");
    }
    // Over the socket as well, not only through the dispatcher.
    let base = format!("http://{}", server.local_addr());
    let answered = within(std::time::Duration::from_secs(5), move || {
        http_call(&base, "GET", "/status", "", &[("X-API-KEY", API_KEY)])
    });
    assert_eq!(answered.map(|(status, _)| status), Some(200));

    // A change waits for the step, so a step is never applied half before it
    // and half after; once the daemon answers, the change goes through and the
    // readers see it straight away.
    let state = Arc::clone(&api.state);
    let create = std::thread::spawn(move || dispatch_on(&state, "POST", "/addresses/create", ""));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!create.is_finished(), "a write went ahead of the sync step holding the wallet");
    gate.open();
    assert_eq!(create.join().unwrap().0, 201);
    let addresses = api.call("GET", "/addresses", "");
    assert_eq!(addresses.json().get("addresses").and_then(Json::as_array).map(<[Json]>::len), Some(2));

    server.stop();
}

#[test]
fn reads_answer_and_a_second_send_waits_while_a_send_waits_on_the_daemon() {
    let gate = Arc::new(Gate::default());
    let api = Api::with_factory("sendwait", gated_factory(None, Some(Arc::clone(&gate))));
    let path = funded_wallet(&api.dir, "funded.wallet");
    let open = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&path));
    assert_eq!(api.call("POST", "/wallet/open", &open).status, 200);

    let send = format!(r#"{{"destination":"{}","amount":1000}}"#, foreign());
    let first = {
        let (state, send) = (Arc::clone(&api.state), send.clone());
        std::thread::spawn(move || dispatch_on(&state, "POST", "/transactions/send/basic", &send))
    };
    assert!(gate.reached_by(1), "the send never asked the daemon for decoys");

    // The send is between building its inputs and relaying, which in the C++
    // happens under a shared lock: nothing a reader needs is held.
    for path in READ_ONLY {
        let state = Arc::clone(&api.state);
        let (status, body) = within(std::time::Duration::from_secs(5), move || dispatch_on(&state, "GET", path, ""))
            .unwrap_or_else(|| panic!("GET {path} waited for a send's daemon round trip"));
        assert_eq!(status, 200, "GET {path}: {body}");
    }
    assert_eq!(api.call("GET", "/balance", "").u64_at("unlocked"), 100_000_000, "nothing is spent until it is relayed");

    // A second send must not pick its inputs while the first may still spend
    // the same ones: it waits, as `m_transactionMutex` makes it wait.
    let second = {
        let (state, send) = (Arc::clone(&api.state), send.clone());
        std::thread::spawn(move || dispatch_on(&state, "POST", "/transactions/send/basic", &send))
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(gate.callers(), 1, "a second send reached the daemon while the first was building");
    assert!(!second.is_finished());

    gate.open();
    for (name, handle) in [("first", first), ("second", second)] {
        let (status, body) = handle.join().unwrap();
        // The canned daemon has no decoys: 27 or 28, depending on which side
        // of the ring build gave up first.
        assert_eq!(status, 400, "{name}: {body}");
        assert!(body.contains("\"errorCode\": 27") || body.contains("\"errorCode\": 28"), "{name}: {body}");
    }
    assert!(gate.callers() >= 2, "the second send ran once the first was done");
}

////////////////////////
/* TX NOTIFY          */
////////////////////////

/// Answers the first `/getwalletsyncdata` with one block holding a transaction
/// that pays the fixture wallet, and says it is synced after that.
struct PayingDaemon {
    canned: CannedDaemon,
    block: wrkz_wallet::daemon::SyncBlock,
    served: std::sync::atomic::AtomicBool,
}

const PAYMENT_HASH: &str = "5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed";

fn paying_block() -> wrkz_wallet::daemon::SyncBlock {
    use wrkz_pow::curve;
    use wrkz_wallet::daemon::{SyncBlock, SyncInput, SyncOutput, SyncTransaction};

    let key = |hex_text: &str| -> [u8; 32] { hex::decode(hex_text).unwrap().try_into().unwrap() };
    let (tx_secret, tx_public) = curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(b"tx-notify test"));
    let derivation = curve::generate_key_derivation(&key(VIEW_PUBLIC), &tx_secret).expect("a point");
    let output = curve::derive_public_key(&derivation, 0, &key(SPEND_PUBLIC)).expect("a point");

    SyncBlock {
        block_hash: "b10c".repeat(16),
        block_height: NETWORK_HEIGHT,
        block_timestamp: 1_788_894_799,
        coinbase_tx: None,
        transactions: vec![SyncTransaction {
            hash: PAYMENT_HASH.to_string(),
            outputs: vec![SyncOutput { amount: 9000, key: hex::encode(output), global_index: None }],
            tx_public_key: hex::encode(tx_public),
            unlock_time: 0,
            payment_id: String::new(),
            inputs: vec![SyncInput { amount: 10_000, k_image: "11".repeat(32), key_offsets: Vec::new() }],
        }],
    }
}

impl SyncDaemon for PayingDaemon {
    fn wallet_sync_data(&self, _req: &SyncRequest) -> Result<WalletSyncData, DaemonError> {
        if !self.served.swap(true, Ordering::SeqCst) {
            return Ok(WalletSyncData {
                items: vec![self.block.clone()],
                scanned_to_height: Some(NETWORK_HEIGHT),
                synced: false,
                top_block: None,
                status: "OK".into(),
            });
        }
        Ok(WalletSyncData {
            items: Vec::new(),
            scanned_to_height: Some(NETWORK_HEIGHT),
            synced: true,
            top_block: Some(wrkz_wallet::daemon::TopBlock {
                hash: self.block.block_hash.clone(),
                height: NETWORK_HEIGHT,
            }),
            status: "OK".into(),
        })
    }

    fn global_indexes_for_range(&self, _start: u64, _end: u64) -> Result<GlobalIndexes, DaemonError> {
        Ok(GlobalIndexes {
            indexes: vec![wrkz_wallet::daemon::GlobalIndexEntry { key: PAYMENT_HASH.to_string(), value: vec![7] }],
            status: "OK".into(),
        })
    }

    fn transactions_status(&self, hashes: &[String]) -> Result<TransactionsStatus, DaemonError> {
        self.canned.transactions_status(hashes)
    }

    fn info(&self) -> Result<Info, DaemonError> {
        self.canned.info()
    }
}

impl TransferDaemon for PayingDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> Result<RandomOuts, DaemonError> {
        self.canned.random_outs(amounts, outs_count)
    }

    fn send_raw_transaction(&self, tx_hex: &str) -> Result<SendResult, DaemonError> {
        self.canned.send_raw_transaction(tx_hex)
    }
}

/// A command with no shell of its own that writes two placeholders to `out`.
#[cfg(unix)]
fn recorder(out: &str) -> String {
    format!("sh -c 'printf \"%%s|%%s\" \"$1\" \"$2\" > {out}' notify %s %a")
}

#[cfg(windows)]
fn recorder(out: &str) -> String {
    format!("cmd /C echo %s^|%a> {out}")
}

#[cfg(any(unix, windows))]
#[test]
fn tx_notify_runs_for_a_transaction_sync_finds() {
    let dir = TempDir::new("txnotify");
    let out = dir.join("notified.txt");
    let wallet = dir.with_fixture("spec05-seed.wallet", "spec05-seed.wallet");

    let factory: DaemonFactory = Box::new(|_host, _port, _ssl| {
        let daemon =
            PayingDaemon { canned: CannedDaemon::default(), block: paying_block(), served: Default::default() };
        Ok(Box::new(daemon) as Box<dyn WalletDaemon>)
    });
    let config = ApiConfig { rpc_password: API_KEY.into(), tx_notify: recorder(&out), ..Default::default() };
    let state = Arc::new(ApiState::new(config, factory));
    assert!(state.tx_notifier().is_some_and(|n| n.enabled()));

    let open = format!(r#"{{"filename":{},"password":"{PASSWORD}"}}"#, quote(&wallet));
    assert_eq!(dispatch_on(&state, "POST", "/wallet/open", &open).0, 200);
    let mut server =
        serve::start(Arc::clone(&state), serve::ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() })
            .expect("bind");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let written = loop {
        if let Ok(text) = std::fs::read_to_string(&out) {
            if text.contains('|') {
                break text;
            }
        }
        assert!(std::time::Instant::now() < deadline, "no notification within 20 s");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(written.trim_end(), format!("{PAYMENT_HASH}|9000"), "%s is the hash, %a the amount received");

    // And the transaction it announced is the one the wallet now shows.
    let (status, body) = dispatch_on(&state, "GET", "/transactions", "");
    assert_eq!(status, 200);
    assert!(body.contains(PAYMENT_HASH), "{body}");

    // The command writes its file before it exits, and the runner counts the
    // delivery only once it has seen the exit, up to a poll later.
    let notifier = state.tx_notifier().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while notifier.sent() + notifier.failed() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    server.stop();
    assert_eq!((notifier.sent(), notifier.failed()), (1, 0), "one transaction, one notification");
}
