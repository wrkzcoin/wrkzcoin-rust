// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-service` driven through its dispatcher, with no socket and no daemon.
//!
//! The container is one of `wrkz-wallet`'s fixtures, which the real C++
//! `wrkz-wallet-api` wrote (see `crates/wrkz-wallet/tests/fixtures/README.md`),
//! so what these tests open is a container the C++ produced. The daemon is a
//! canned one that answers `/info` with a fixed height, so the whole surface
//! runs offline and deterministically.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use wrkz_rpc::json::Json;
use wrkz_service::errors::{ERR_INVALID_PASSWORD, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE_ERROR};
use wrkz_service::serve::{self, ServeConfig};
use wrkz_service::{dispatch, ServiceConfig, ServiceState};
use wrkz_wallet::api::{DaemonFactory, WalletDaemon};
use wrkz_wallet::daemon::{
    DaemonError, GlobalIndexes, Info, RandomOuts, SendResult, SyncRequest, TransactionsStatus, WalletSyncData,
};
use wrkz_wallet::sync::SyncDaemon;
use wrkz_wallet::transfer::TransferDaemon;
use zeroize::Zeroizing;

const RPC_PASSWORD: &str = "test-rpc-password";
const CONTAINER_PASSWORD: &str = "password";
const NETWORK_HEIGHT: u64 = 4_213_000;
/// The seed fixture's primary address.
const ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
const VIEW_SECRET: &str = "779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700";

////////////////////////
/* CANNED DAEMON      */
////////////////////////

struct CannedDaemon;

impl SyncDaemon for CannedDaemon {
    fn wallet_sync_data(&self, req: &SyncRequest) -> Result<WalletSyncData, DaemonError> {
        // One block per requested height, with a hash derived from it, so the
        // block-hash paths have something deterministic to return.
        let items = (0..req.block_count.min(8))
            .map(|i| {
                let height = req.start_height + i;
                wrkz_wallet::daemon::SyncBlock {
                    block_hash: format!("{height:064x}"),
                    block_height: height,
                    block_timestamp: 1_800_000_000 + height,
                    coinbase_tx: None,
                    transactions: Vec::new(),
                }
            })
            .collect();
        Ok(WalletSyncData { items, scanned_to_height: None, synced: true, top_block: None, status: "OK".into() })
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
            height: NETWORK_HEIGHT + 1,
            network_height: NETWORK_HEIGHT + 1,
            difficulty: 60_000,
            incoming_connections_count: 4,
            outgoing_connections_count: 4,
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
    Box::new(|_host, _port, _ssl| Ok(Box::new(CannedDaemon) as Box<dyn WalletDaemon>))
}

////////////////////////
/* HARNESS            */
////////////////////////

/// A temporary directory that removes itself.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("wrkz-service-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    /// Copy one of `wrkz-wallet`'s C++-written fixtures in.
    fn with_fixture(&self, fixture: &str) -> String {
        let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("wrkz-wallet")
            .join("tests")
            .join("fixtures")
            .join(fixture);
        let dst = self.0.join(fixture);
        std::fs::copy(&src, &dst).expect("copy fixture");
        dst.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    state: Arc<ServiceState>,
    _dir: TempDir,
}

impl Harness {
    fn new(fixture: &str) -> Harness {
        let dir = TempDir::new();
        let path = dir.with_fixture(fixture);
        let wallet = wrkz_wallet::file::Wallet::open(&path, CONTAINER_PASSWORD).expect("the fixture opens");
        let open = wrkz_wallet::api::open_container(
            wallet,
            path,
            Zeroizing::new(CONTAINER_PASSWORD.to_string()),
            "127.0.0.1".into(),
            17856,
            false,
            false,
            &canned_factory(),
        )
        .expect("the canned daemon answers");
        let config = ServiceConfig { rpc_password: RPC_PASSWORD.into(), ..ServiceConfig::default() };
        Harness { state: Arc::new(ServiceState::new(config, open)), _dir: dir }
    }

    /// One request with the right password.
    fn call(&self, method: &str, params: &str) -> Json {
        self.raw(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"password":"{RPC_PASSWORD}","method":"{method}","params":{params}}}"#
        ))
    }

    fn raw(&self, body: &str) -> Json {
        dispatch(&self.state, body.as_bytes())
    }

    /// The `result` of a call that must have succeeded.
    fn ok(&self, method: &str, params: &str) -> Json {
        let answer = self.call(method, params);
        assert!(answer.get("error").is_none(), "{method} failed: {}", answer.to_string());
        answer.get("result").cloned().expect("a result")
    }

    /// The `(code, application_code)` of a call that must have failed.
    fn err(&self, method: &str, params: &str) -> (i64, Option<i64>) {
        let answer = self.call(method, params);
        let error =
            answer.get("error").unwrap_or_else(|| panic!("{method} should have failed: {}", answer.to_string()));
        let code = match error.get("code") {
            Some(Json::I64(c)) => *c,
            Some(Json::U64(c)) => *c as i64,
            other => panic!("no code: {other:?}"),
        };
        let application = error.get("data").and_then(|d| d.get("application_code")).and_then(|c| match c {
            Json::I64(v) => Some(*v),
            Json::U64(v) => Some(*v as i64),
            _ => None,
        });
        (code, application)
    }
}

////////////////////////
/* THE ENVELOPE       */
////////////////////////

#[test]
fn the_envelope_is_the_cpp_envelope() {
    let h = Harness::new("spec05-seed.wallet");

    // `id` is echoed verbatim, of whatever type, and `jsonrpc` is always 2.0.
    let answer = h.raw(&format!(
        r#"{{"jsonrpc":"2.0","id":"abc","password":"{RPC_PASSWORD}","method":"getAddresses","params":{{}}}}"#
    ));
    assert_eq!(answer.get("id"), Some(&Json::Str("abc".into())));
    assert_eq!(answer.get("jsonrpc"), Some(&Json::Str("2.0".into())));
    assert!(answer.get("result").is_some());

    // No `id` in, no `id` out.
    let answer = h.raw(&format!(r#"{{"password":"{RPC_PASSWORD}","method":"getAddresses","params":{{}}}}"#));
    assert!(answer.get("id").is_none(), "{}", answer.to_string());

    // A body that is not JSON: still an envelope, with `id` null.
    let answer = h.raw("not json at all");
    assert_eq!(answer.get("id"), Some(&Json::Null));
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_PARSE_ERROR)));
    assert_eq!(answer.get("error").and_then(|e| e.get("message")), Some(&Json::Str("Parse error".into())));

    // The request's own `jsonrpc` member is never looked at.
    let answer = h.raw(&format!(r#"{{"id":7,"password":"{RPC_PASSWORD}","method":"getAddresses"}}"#));
    assert!(answer.get("result").is_some(), "{}", answer.to_string());
    assert_eq!(answer.get("id"), Some(&Json::U64(7)));
}

#[test]
fn the_password_is_checked_before_the_method() {
    let h = Harness::new("spec05-seed.wallet");

    // No password.
    let answer = h.raw(r#"{"id":1,"method":"getAddresses"}"#);
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_INVALID_PASSWORD)));

    // Wrong password, and an unknown method: the password wins.
    let answer = h.raw(r#"{"id":1,"password":"wrong","method":"noSuchMethod"}"#);
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_INVALID_PASSWORD)));
    assert_eq!(
        answer.get("error").and_then(|e| e.get("message")),
        Some(&Json::Str("Invalid or no rpc password".into()))
    );

    // A password of the wrong type is no password.
    let answer = h.raw(r#"{"id":1,"password":42,"method":"getAddresses"}"#);
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_INVALID_PASSWORD)));
}

#[test]
fn legacy_security_serves_without_a_password() {
    let h = Harness::new("spec05-seed.wallet");
    let open = std::mem::replace(
        &mut *h.state.wallet.write().unwrap(),
        wrkz_wallet::api::open_container(
            wrkz_wallet::file::Wallet::create_new(0).unwrap(),
            String::new(),
            Zeroizing::new(String::new()),
            "127.0.0.1".into(),
            17856,
            false,
            false,
            &canned_factory(),
        )
        .unwrap(),
    );
    let config = ServiceConfig { legacy_security: true, ..ServiceConfig::default() };
    let state = Arc::new(ServiceState::new(config, open));
    let answer = dispatch(&state, br#"{"id":1,"method":"getAddresses"}"#);
    assert!(answer.get("result").is_some(), "{}", answer.to_string());
}

#[test]
fn a_missing_or_unknown_method_says_so() {
    let h = Harness::new("spec05-seed.wallet");

    let answer = h.raw(&format!(r#"{{"id":1,"password":"{RPC_PASSWORD}"}}"#));
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_INVALID_REQUEST)));
    assert_eq!(answer.get("error").and_then(|e| e.get("message")), Some(&Json::Str("Invalid Request".into())));

    let answer = h.raw(&format!(r#"{{"id":1,"password":"{RPC_PASSWORD}","method":123}}"#));
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_INVALID_REQUEST)));

    let answer = h.raw(&format!(r#"{{"id":1,"password":"{RPC_PASSWORD}","method":"noSuchMethod"}}"#));
    assert_eq!(answer.get("error").and_then(|e| e.get("code")), Some(&Json::I64(ERR_METHOD_NOT_FOUND)));
    assert_eq!(answer.get("error").and_then(|e| e.get("message")), Some(&Json::Str("Method not found".into())));
}

#[test]
fn an_application_error_is_minus_32700_with_an_application_code() {
    let h = Harness::new("spec05-seed.wallet");
    // `WRONG_HASH_FORMAT` is 3 in the service category.
    let (code, application) = h.err("getTransaction", r#"{"transactionHash":"not-a-hash"}"#);
    assert_eq!(code, ERR_PARSE_ERROR, "every application error carries the parse-error code");
    assert_eq!(application, Some(3));

    // A missing required member is the generic "Request error", with no data.
    let answer = h.call("getTransaction", "{}");
    let error = answer.get("error").expect("an error");
    assert_eq!(error.get("code"), Some(&Json::I64(ERR_PARSE_ERROR)));
    assert_eq!(error.get("message"), Some(&Json::Str("Request error".into())));
    assert!(error.get("data").is_none(), "a serialization error has no data: {}", answer.to_string());
}

////////////////////////
/* THE METHODS        */
////////////////////////

#[test]
fn every_method_the_cpp_registers_is_here_and_nothing_else_is() {
    let names: Vec<&str> = wrkz_service::methods::METHODS.iter().map(|(n, _)| *n).collect();
    // `PaymentServiceJsonRpcServer`'s constructor, in its order.
    assert_eq!(
        names,
        vec![
            "save",
            "export",
            "reset",
            "createAddress",
            "createAddressList",
            "deleteAddress",
            "getSpendKeys",
            "getBalance",
            "getBlockHashes",
            "getTransactionHashes",
            "getTransactions",
            "getUnconfirmedTransactionHashes",
            "getTransaction",
            "sendTransaction",
            "createDelayedTransaction",
            "getDelayedTransactionHashes",
            "deleteDelayedTransaction",
            "sendDelayedTransaction",
            "getViewKey",
            "getMnemonicSeed",
            "getStatus",
            "getAddresses",
            "createIntegratedAddress",
            "getFeeInfo",
            "getNodeFeeInfo",
        ]
    );
}

#[test]
fn the_read_only_methods_answer_from_the_container() {
    let h = Harness::new("spec05-seed.wallet");

    let addresses = h.ok("getAddresses", "{}");
    assert_eq!(addresses.get("addresses").and_then(|a| a.as_array()).map(|a| a.len()), Some(1));
    assert_eq!(addresses.get("addresses").and_then(|a| a.as_array()).and_then(|a| a[0].as_str()), Some(ADDRESS));

    let view = h.ok("getViewKey", "{}");
    assert_eq!(view.get("viewSecretKey").and_then(|v| v.as_str()), Some(VIEW_SECRET));

    let keys = h.ok("getSpendKeys", &format!(r#"{{"address":"{ADDRESS}"}}"#));
    assert!(keys.get("spendSecretKey").is_some());
    assert!(keys.get("spendPublicKey").is_some());

    let seed = h.ok("getMnemonicSeed", &format!(r#"{{"address":"{ADDRESS}"}}"#));
    let words = seed.get("mnemonicSeed").and_then(|v| v.as_str()).expect("a seed");
    assert_eq!(words.split_whitespace().count(), 25);

    let balance = h.ok("getBalance", "{}");
    assert_eq!(balance.get("availableBalance"), Some(&Json::U64(0)));
    assert_eq!(balance.get("lockedAmount"), Some(&Json::U64(0)));
    // Per address, the same, and an address that is not ours is BAD_ADDRESS (7).
    let per_address = h.ok("getBalance", &format!(r#"{{"address":"{ADDRESS}"}}"#));
    assert_eq!(per_address, balance);

    let status = h.ok("getStatus", "{}");
    // `Nigel::getDaemonInfo` subtracts one from each of `/info`'s counts
    // (`Nigel.cpp:867`), and `getStatus` reports what Nigel holds.
    assert_eq!(status.get("knownBlockCount"), Some(&Json::U64(NETWORK_HEIGHT)));
    assert_eq!(status.get("localDaemonBlockCount"), Some(&Json::U64(NETWORK_HEIGHT)));
    assert_eq!(status.get("peerCount"), Some(&Json::U64(8)));
    assert!(status.get("blockCount").is_some());
    assert!(status.get("lastBlockHash").is_some());

    // The fee is always empty: no WrkzCoin daemon serves `/fee`.
    for method in ["getFeeInfo", "getNodeFeeInfo"] {
        let fee = h.ok(method, "{}");
        assert_eq!(fee.get("address"), Some(&Json::Str(String::new())));
        assert_eq!(fee.get("amount"), Some(&Json::U64(0)));
    }
}

#[test]
fn create_integrated_address_round_trips() {
    let h = Harness::new("spec05-seed.wallet");
    let payment_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let made = h.ok("createIntegratedAddress", &format!(r#"{{"address":"{ADDRESS}","paymentId":"{payment_id}"}}"#));
    let integrated = made.get("integratedAddress").and_then(|v| v.as_str()).expect("an address");
    let parsed = wrkz_primitives::base58::parse_address(integrated).expect("parses");
    assert_eq!(parsed.payment_id.as_deref(), Some(payment_id));

    // Both members are required, and the payment id is checked.
    assert_eq!(h.err("createIntegratedAddress", &format!(r#"{{"address":"{ADDRESS}"}}"#)).1, None);
    let (_, application) =
        h.err("createIntegratedAddress", &format!(r#"{{"address":"{ADDRESS}","paymentId":"beef"}}"#));
    assert_eq!(application, Some(2), "WRONG_PAYMENT_ID_FORMAT");
}

#[test]
fn addresses_can_be_created_and_deleted() {
    let h = Harness::new("spec05-seed.wallet");
    let made = h.ok("createAddress", "{}");
    let address = made.get("address").and_then(|v| v.as_str()).expect("an address").to_string();
    assert_ne!(address, ADDRESS);
    assert_eq!(h.ok("getAddresses", "{}").get("addresses").and_then(|a| a.as_array()).map(|a| a.len()), Some(2));

    h.ok("deleteAddress", &format!(r#"{{"address":"{address}"}}"#));
    assert_eq!(h.ok("getAddresses", "{}").get("addresses").and_then(|a| a.as_array()).map(|a| a.len()), Some(1));

    // Both keys at once, and newAddress with scanHeight, are refused before
    // anything happens.
    assert_eq!(h.err("createAddress", r#"{"spendSecretKey":"aa","spendPublicKey":"bb"}"#).1, None);
    assert_eq!(h.err("createAddress", r#"{"newAddress":true,"scanHeight":100}"#).1, None);
    // A key that is not 64 hex characters is WRONG_KEY_FORMAT (1).
    assert_eq!(h.err("createAddress", r#"{"spendSecretKey":"nope"}"#).1, Some(1));
}

#[test]
fn create_address_list_refuses_a_repeated_key_before_importing_any() {
    let h = Harness::new("spec05-seed.wallet");
    let key = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
    let (_, application) = h.err("createAddressList", &format!(r#"{{"spendSecretKeys":["{key}","{key}"]}}"#));
    assert_eq!(application, Some(5), "DUPLICATE_KEY");
    assert_eq!(
        h.ok("getAddresses", "{}").get("addresses").and_then(|a| a.as_array()).map(|a| a.len()),
        Some(1),
        "nothing was imported"
    );
    // `spendSecretKeys` is required.
    assert_eq!(h.err("createAddressList", "{}").1, None);
}

#[test]
fn block_hashes_come_from_the_daemon_and_are_capped() {
    let h = Harness::new("spec05-seed.wallet");
    let answer = h.ok("getBlockHashes", r#"{"firstBlockIndex":100,"blockCount":3}"#);
    let hashes = answer.get("blockHashes").and_then(|v| v.as_array()).expect("an array");
    assert_eq!(hashes.len(), 3);
    assert_eq!(hashes[0].as_str(), Some(format!("{:064x}", 100).as_str()));
    assert_eq!(hashes[2].as_str(), Some(format!("{:064x}", 102).as_str()));

    // Both members are required.
    assert_eq!(h.err("getBlockHashes", r#"{"firstBlockIndex":1}"#).1, None);
    assert_eq!(h.err("getBlockHashes", r#"{"blockCount":1}"#).1, None);

    // Above the cap is WRONG_PARAMETERS (22) rather than a million round trips.
    let (_, application) = h.err("getBlockHashes", r#"{"firstBlockIndex":0,"blockCount":100000}"#);
    assert_eq!(application, Some(22));
}

#[test]
fn the_transaction_queries_need_exactly_one_of_hash_or_index() {
    let h = Harness::new("spec05-seed.wallet");
    for method in ["getTransactions", "getTransactionHashes"] {
        // Neither.
        assert_eq!(h.err(method, r#"{"blockCount":10}"#).1, None, "{method} with neither");
        // Both.
        let both = format!(r#"{{"blockCount":10,"firstBlockIndex":1,"blockHash":"{}"}}"#, "0".repeat(64));
        assert_eq!(h.err(method, &both).1, None, "{method} with both");
        // `blockCount` missing.
        assert_eq!(h.err(method, r#"{"firstBlockIndex":1}"#).1, None, "{method} with no blockCount");
        // A window this container has nothing in: OBJECT_NOT_FOUND (4), which
        // is what the C++ throws rather than answering with an empty list.
        assert_eq!(h.err(method, r#"{"firstBlockIndex":1,"blockCount":10}"#).1, Some(4), "{method} on an empty range");
    }
}

#[test]
fn an_unknown_transaction_and_an_empty_pool_answer_the_cpp_way() {
    let h = Harness::new("spec05-seed.wallet");
    let unknown = "0".repeat(64);
    // OBJECT_NOT_FOUND in the *wallet* category is 23.
    assert_eq!(h.err("getTransaction", &format!(r#"{{"transactionHash":"{unknown}"}}"#)).1, Some(23));

    let pool = h.ok("getUnconfirmedTransactionHashes", "{}");
    assert_eq!(pool.get("transactionHashes"), Some(&Json::Array(Vec::new())));
    // A filter address that is not an address at all is BAD_ADDRESS (7).
    assert_eq!(h.err("getUnconfirmedTransactionHashes", r#"{"addresses":["nope"]}"#).1, Some(7));
}

#[test]
fn delayed_transactions_are_listed_and_can_be_deleted() {
    let h = Harness::new("spec05-seed.wallet");
    let empty = h.ok("getDelayedTransactionHashes", "{}");
    assert_eq!(empty.get("transactionHashes"), Some(&Json::Array(Vec::new())));

    let unknown = "0".repeat(64);
    assert_eq!(h.err("deleteDelayedTransaction", &format!(r#"{{"transactionHash":"{unknown}"}}"#)).1, Some(4));
    assert_eq!(h.err("sendDelayedTransaction", &format!(r#"{{"transactionHash":"{unknown}"}}"#)).1, Some(4));
    // The hash itself is validated first.
    assert_eq!(h.err("deleteDelayedTransaction", r#"{"transactionHash":"short"}"#).1, Some(3));
    // And the member is required.
    assert_eq!(h.err("deleteDelayedTransaction", "{}").1, None);
}

#[test]
fn sending_needs_transfers_and_refuses_extra_with_a_payment_id() {
    let h = Harness::new("spec05-seed.wallet");
    // `transfers` is the one required member.
    assert_eq!(h.err("sendTransaction", "{}").1, None);
    // `extra` and `paymentId` together.
    let both = format!(
        r#"{{"transfers":[{{"address":"{ADDRESS}","amount":100}}],"extra":"aabb","paymentId":"{}"}}"#,
        "0".repeat(64)
    );
    assert_eq!(h.err("sendTransaction", &both).1, None);
    // A transfer without an amount.
    let no_amount = format!(r#"{{"transfers":[{{"address":"{ADDRESS}"}}]}}"#);
    assert_eq!(h.err("sendTransaction", &no_amount).1, None);
    // With nothing to spend, the container says so rather than building one:
    // WRONG_AMOUNT (9) is what "not enough balance" maps to.
    let broke = format!(r#"{{"transfers":[{{"address":"{ADDRESS}","amount":100}}]}}"#);
    let (code, application) = h.err("sendTransaction", &broke);
    assert_eq!(code, ERR_PARSE_ERROR);
    assert_eq!(application, Some(9));
}

#[test]
fn save_export_and_reset_do_what_they_say() {
    let h = Harness::new("spec05-seed.wallet");
    h.ok("save", "{}");

    h.ok("export", r#"{"fileName":"exported.json"}"#);
    let exported = {
        let open = h.state.read();
        std::path::Path::new(&open.filename).parent().expect("a directory").join("exported.json")
    };
    let text = std::fs::read_to_string(&exported).expect("the export exists");
    assert!(text.contains("walletFileFormatVersion"), "the container's own JSON: {}", &text[..80.min(text.len())]);
    // `fileName` is required.
    assert_eq!(h.err("export", "{}").1, None);

    // A reset moves every subwallet's scan start and forgets what was synced,
    // as `SubWallets::reset` plus `WalletSynchronizer::reset` do; the
    // addresses stay.
    h.ok("reset", r#"{"scanHeight":123}"#);
    assert_eq!(h.ok("getAddresses", "{}").get("addresses").and_then(|a| a.as_array()).map(|a| a.len()), Some(1));
    let open = h.state.read();
    assert_eq!(open.wallet().wallet_synchronizer.start_height, 123);
    assert!(open.wallet().sub_wallets.sub_wallet.iter().all(|s| s.sync_start_height == 123));
    assert_eq!(open.wallet().wallet_height(), 0, "nothing is synced any more");
}

#[test]
fn a_view_only_container_refuses_what_needs_a_spend_key() {
    let h = Harness::new("spec05-view.wallet");
    // TRACKING_MODE is 21.
    let (_, application) = h.err("getMnemonicSeed", &format!(r#"{{"address":"{ADDRESS}"}}"#));
    assert!(
        application == Some(21) || application == Some(6),
        "tracking mode or not deterministic, got {application:?}"
    );
    // Reading is fine.
    assert!(h.ok("getViewKey", "{}").get("viewSecretKey").is_some());
}

////////////////////////
/* THE LISTENER       */
////////////////////////

/// One HTTP exchange over any stream, and the JSON it answered with.
fn json_exchange<S: std::io::Read + std::io::Write>(stream: S, path: &str, body: &str) -> (u16, Option<Json>) {
    let (status, out) =
        wrkz_wallet::ipc::exchange(stream, "POST", path, "localhost", Some(body.as_bytes()), &[], 1 << 20)
            .expect("exchange");
    (status, wrkz_rpc::json::parse(&out, wrkz_rpc::json::ParseLimits::default()).ok())
}

fn error_code(answer: &Json) -> Option<i64> {
    match answer.get("error")?.get("code")? {
        Json::I64(c) => Some(*c),
        Json::U64(c) => Some(*c as i64),
        _ => None,
    }
}

#[test]
fn json_rpc_is_served_over_tcp_with_the_password_in_the_body() {
    let h = Harness::new("spec05-seed.wallet");
    let config = ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() };
    let mut running = serve::start(Arc::clone(&h.state), config).expect("bind");
    let addr = running.local_addr().expect("a TCP listener");
    let connect = || {
        let stream = std::net::TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        stream
    };

    let (status, answer) = json_exchange(connect(), "/json_rpc", r#"{"id":1,"method":"getAddresses"}"#);
    assert_eq!(status, 200, "a refusal is still a 200 envelope");
    assert_eq!(answer.as_ref().and_then(error_code), Some(ERR_INVALID_PASSWORD));

    let body = format!(r#"{{"id":1,"password":"{RPC_PASSWORD}","method":"getAddresses"}}"#);
    let (status, answer) = json_exchange(connect(), "/json_rpc", &body);
    assert_eq!(status, 200);
    assert!(answer.unwrap().get("result").is_some());

    let (status, _) = json_exchange(connect(), "/", &body);
    assert_eq!(status, 404, "only /json_rpc is routed");

    running.stop();
}

#[cfg(unix)]
#[test]
fn json_rpc_is_served_on_a_local_socket_instead_of_a_port() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let h = Harness::new("spec05-seed.wallet");
    let dir = TempDir::new();
    let socket = dir.0.join("service.sock").to_string_lossy().into_owned();
    let config = ServeConfig {
        bind: String::new(),
        ipc: Some(wrkz_wallet::listen::IpcConfig { path: socket.clone(), mode: 0o600, group: String::new() }),
        ..Default::default()
    };
    let mut running = serve::start(Arc::clone(&h.state), config).expect("the socket binds");
    assert!(running.local_addr().is_none(), "no TCP port, as with the C++'s --bind-ipc-path");
    assert_eq!(running.ipc_path(), Some(socket.as_str()));

    let meta = std::fs::metadata(&socket).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let connect = || {
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        stream
    };

    // The socket's mode decides who connects; the password still decides who
    // may do anything, as `PaymentServiceJsonRpcServer` checks it whatever the
    // transport.
    let (_, answer) = json_exchange(connect(), "/json_rpc", r#"{"id":1,"method":"getAddresses"}"#);
    assert_eq!(answer.as_ref().and_then(error_code), Some(ERR_INVALID_PASSWORD));
    let body = format!(r#"{{"id":1,"password":"{RPC_PASSWORD}","method":"getAddresses"}}"#);
    let (status, answer) = json_exchange(connect(), "/json_rpc", &body);
    assert_eq!(status, 200);
    assert!(answer.unwrap().to_string().contains(ADDRESS));

    running.stop();
    assert!(!std::path::Path::new(&socket).exists(), "the socket file is removed on the way out");
}

////////////////////////
/* NOTIFICATIONS      */
////////////////////////

const PAYMENT_HASH: &str = "5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed";
const SPEND_PUBLIC: &str = "857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3";
const VIEW_PUBLIC: &str = "0489cb98c7108372eaff2cdeddc5e76166b017a847537bf8499d61465395e942";

/// Serves one block, at `block_height`, holding a transaction that pays the
/// seed fixture 9000, and reports `network_height` from `/info`.
struct PayingDaemon {
    block_height: u64,
    network_height: u64,
    served: AtomicBool,
}

fn paying_block(height: u64) -> wrkz_wallet::daemon::SyncBlock {
    use wrkz_pow::curve;
    use wrkz_wallet::daemon::{SyncBlock, SyncInput, SyncOutput, SyncTransaction};

    let key = |hex_text: &str| -> [u8; 32] { hex::decode(hex_text).unwrap().try_into().unwrap() };
    let (tx_secret, tx_public) = curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(b"service notify test"));
    let derivation = curve::generate_key_derivation(&key(VIEW_PUBLIC), &tx_secret).expect("a point");
    let output = curve::derive_public_key(&derivation, 0, &key(SPEND_PUBLIC)).expect("a point");

    SyncBlock {
        block_hash: "b10c".repeat(16),
        block_height: height,
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
        let block = paying_block(self.block_height);
        if !self.served.swap(true, Ordering::SeqCst) {
            return Ok(WalletSyncData {
                scanned_to_height: Some(block.block_height),
                items: vec![block],
                synced: false,
                top_block: None,
                status: "OK".into(),
            });
        }
        Ok(WalletSyncData {
            items: Vec::new(),
            scanned_to_height: Some(block.block_height),
            synced: true,
            top_block: Some(wrkz_wallet::daemon::TopBlock { hash: block.block_hash, height: block.block_height }),
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
        CannedDaemon.transactions_status(hashes)
    }

    fn info(&self) -> Result<Info, DaemonError> {
        let mut info = CannedDaemon.info()?;
        info.height = self.network_height + 1;
        info.network_height = self.network_height + 1;
        Ok(info)
    }
}

impl TransferDaemon for PayingDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> Result<RandomOuts, DaemonError> {
        CannedDaemon.random_outs(amounts, outs_count)
    }

    fn send_raw_transaction(&self, tx_hex: &str) -> Result<SendResult, DaemonError> {
        CannedDaemon.send_raw_transaction(tx_hex)
    }
}

/// A webhook receiver: every POST's body, in the order they arrive.
struct Hooks {
    base: String,
    bodies: std::sync::mpsc::Receiver<String>,
}

impl Hooks {
    fn start() -> Hooks {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sender, bodies) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                let body = loop {
                    let text = String::from_utf8_lossy(&got).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if got.len() >= end + 4 + length {
                            break text[end + 4..].to_string();
                        }
                    }
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break String::new(),
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                };
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                if sender.send(body).is_err() {
                    break;
                }
            }
        });
        Hooks { base, bodies }
    }

    /// The next `n` bodies, or panic after ten seconds.
    fn take(&self, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                self.bodies
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|_| panic!("notification {} of {n} never came", i + 1))
            })
            .collect()
    }

    /// Nothing more arrives within `wait`.
    fn quiet_for(&self, wait: std::time::Duration) {
        if let Ok(extra) = self.bodies.recv_timeout(wait) {
            panic!("an unexpected notification: {extra}");
        }
    }
}

/// A service over the seed fixture, syncing from `daemon`, with both hooks
/// pointed at `hooks`.
fn notifying(hooks: &Hooks, block_height: u64, network_height: u64, notify_during_sync: bool) -> Harness {
    let dir = TempDir::new();
    let path = dir.with_fixture("spec05-seed.wallet");
    let wallet = wrkz_wallet::file::Wallet::open(&path, CONTAINER_PASSWORD).expect("the fixture opens");
    let factory: DaemonFactory = Box::new(move |_host, _port, _ssl| {
        let daemon = PayingDaemon { block_height, network_height, served: AtomicBool::new(false) };
        Ok(Box::new(daemon) as Box<dyn WalletDaemon>)
    });
    let open = wrkz_wallet::api::open_container(
        wallet,
        path,
        Zeroizing::new(CONTAINER_PASSWORD.to_string()),
        "127.0.0.1".into(),
        17856,
        false,
        false,
        &factory,
    )
    .expect("the daemon answers");
    let config = ServiceConfig {
        rpc_password: RPC_PASSWORD.into(),
        tx_notify: format!("{}/tx", hooks.base),
        tx_confirmed_notify: format!("{}/confirmed", hooks.base),
        notify_during_sync,
        ..ServiceConfig::default()
    };
    Harness { state: Arc::new(ServiceState::new(config, open)), _dir: dir }
}

/// One step of the service's sync loop.
fn sync_step(h: &Harness) {
    let mut open = h.state.write();
    open.sync_round();
    h.state.notify.sync_step(&open);
}

fn event_of(body: &str) -> String {
    let json = wrkz_rpc::json::parse(body.as_bytes(), wrkz_rpc::json::ParseLimits::default()).expect("a JSON body");
    json.get("event").and_then(Json::as_str).expect("an event").to_string()
}

#[test]
fn a_transaction_sync_finds_fires_tx_and_then_tx_confirmed() {
    let hooks = Hooks::start();
    let h = notifying(&hooks, NETWORK_HEIGHT, NETWORK_HEIGHT, false);
    assert!(h.state.notify.enabled());

    sync_step(&h);
    let mut bodies = hooks.take(2);
    // Two hooks, two workers: the order between them is not promised.
    bodies.sort_by_key(|b| event_of(b));
    assert_eq!(event_of(&bodies[0]), "tx");
    assert_eq!(event_of(&bodies[1]), "tx_confirmed");
    for body in &bodies {
        assert!(body.contains(&format!(r#""hash":"{PAYMENT_HASH}""#)), "{body}");
        assert!(body.contains(&format!(r#""height":{NETWORK_HEIGHT}"#)), "{body}");
        assert!(body.contains(r#""amount":9000,"#), "{body}");
        assert!(body.contains(r#""confirmed":true"#), "{body}");
    }

    sync_step(&h);
    hooks.quiet_for(std::time::Duration::from_millis(500));
}

#[test]
fn a_send_is_announced_when_relayed_and_confirmed_once_when_mined() {
    use wrkz_wallet::file::{Hex32, Transaction, Transfer};

    let hooks = Hooks::start();
    let h = notifying(&hooks, NETWORK_HEIGHT, NETWORK_HEIGHT, false);
    let hash = Hex32::from_hex(PAYMENT_HASH).unwrap();

    // What `sendTransaction` leaves behind: a locked transaction, announced.
    {
        let mut open = h.state.write();
        let spend = open.wallet().primary_sub_wallet().unwrap().public_spend_key;
        open.wallet_mut().sub_wallets.locked_transactions.push(Transaction {
            block_height: 0,
            fee: 1000,
            hash,
            is_coinbase_transaction: false,
            payment_id: String::new(),
            timestamp: 0,
            transfers: vec![Transfer { amount: -10_000, public_key: spend }],
            unlock_time: 0,
        });
        h.state.notify.sent(&open, &hash);
    }
    let sent = hooks.take(1);
    assert_eq!(event_of(&sent[0]), "tx");
    assert!(sent[0].contains(r#""height":0,"#) && sent[0].contains(r#""confirmed":false"#), "{}", sent[0]);
    assert!(sent[0].contains(r#""amount":-10000,"#), "{}", sent[0]);

    // Mined: the confirmation, and no second `tx`.
    sync_step(&h);
    let confirmed = hooks.take(1);
    assert_eq!(event_of(&confirmed[0]), "tx_confirmed", "{}", confirmed[0]);
    assert!(confirmed[0].contains(r#""confirmed":true"#));
    hooks.quiet_for(std::time::Duration::from_millis(500));
}

#[test]
fn a_rescan_is_quiet_unless_notify_during_sync() {
    // The block is two days of blocks below the daemon's height.
    let behind = NETWORK_HEIGHT + 2 * wrkz_wallet::api::notify::WALLET_NOTIFY_SYNC_LAG_BLOCKS;

    let hooks = Hooks::start();
    let h = notifying(&hooks, NETWORK_HEIGHT, behind, false);
    sync_step(&h);
    hooks.quiet_for(std::time::Duration::from_millis(800));

    let hooks = Hooks::start();
    let h = notifying(&hooks, NETWORK_HEIGHT, behind, true);
    sync_step(&h);
    assert_eq!(hooks.take(2).len(), 2, "--notify-during-sync announces it anyway");
}
