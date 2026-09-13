// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The wallet's two remote services — the daemon and, optionally, a
//! transaction proof-of-work server — over one small HTTP trait, so the same
//! request code runs natively ([`UreqTransport`]) and inside a browser's Web
//! Worker (XMLHttpRequest, in apps/pluton).
//!
//! [`HttpDaemon`] makes the requests [`crate::daemon::Daemon`] makes — the same
//! paths, bodies and error classification (spec/09) — for every call sync and
//! transaction construction need, and carries the proof-of-work server that
//! [`TransferDaemon::remote_pow`] consults. `Daemon` stays as it is for the
//! command-line programs.

use std::cell::RefCell;
use std::time::Duration;

use serde::de::DeserializeOwned;
use wrkz_primitives::constants::TX_POW_NONCE_SIZE;

use crate::daemon::{
    self, DaemonError, GlobalIndexes, Info, RandomOuts, SendResult, SyncRequest, TransactionsStatus, WalletSyncData,
    MAX_RESPONSE_BYTES,
};
use crate::sync::SyncDaemon;
use crate::transfer::TransferDaemon;
use crate::txpow::TxPowServer;

/// How long one daemon request may take, as `Daemon`'s agent allows.
pub const DAEMON_TIMEOUT: Duration = Duration::from_secs(60);

/// One HTTP exchange, as a [`HttpTransport`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: String,
}

/// The HTTP the wallet needs. `None` means the request never reached the
/// server: refused, timed out, TLS failed.
pub trait HttpTransport {
    /// `method` is `GET`, `POST` or `DELETE`; a body is JSON. `api_key`, when
    /// given, goes out as the `X-API-KEY` header.
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        api_key: Option<&str>,
        timeout: Duration,
    ) -> Option<HttpReply>;
}

impl<T: HttpTransport + ?Sized> HttpTransport for &T {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        api_key: Option<&str>,
        timeout: Duration,
    ) -> Option<HttpReply> {
        (**self).request(method, url, body, api_key, timeout)
    }
}

/// Check what a user typed and turn it into the base every request is made
/// against: `https://node.example.com:17856/` becomes
/// `https://node.example.com:17856`. `None` unless it is an `http://` or
/// `https://` URL with a host.
pub fn normalize_url(input: &str) -> Option<String> {
    let url = input.trim().trim_end_matches('/');
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    (!rest.is_empty() && !rest.starts_with('/') && !rest.contains(char::is_whitespace)).then(|| url.to_string())
}

/// What happened the last time a transaction asked the proof-of-work server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PowOutcome {
    /// The server supplied the nonce (it is checked before use).
    Solved,
    /// The server could not help; the nonce was searched on this machine.
    ComputedLocally(String),
}

/// [`SyncDaemon`] and [`TransferDaemon`] over any [`HttpTransport`].
pub struct HttpDaemon<T> {
    base: String,
    transport: T,
    pow_server: Option<TxPowServer<T>>,
    last_pow: RefCell<Option<PowOutcome>>,
}

impl<T: HttpTransport> HttpDaemon<T> {
    /// A daemon at `url` (see [`normalize_url`]).
    pub fn new(url: &str, transport: T) -> Option<Self> {
        Some(Self { base: normalize_url(url)?, transport, pow_server: None, last_pow: RefCell::new(None) })
    }

    /// Ask `server` for every transaction proof of work, or nobody.
    pub fn set_pow_server(&mut self, server: Option<TxPowServer<T>>) {
        self.pow_server = server;
    }

    /// The normalized base URL.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// What the last transaction's proof of work did, once; `None` when no
    /// server was asked since the last call.
    pub fn take_pow_outcome(&self) -> Option<PowOutcome> {
        self.last_pow.borrow_mut().take()
    }

    fn call(&self, method: &str, path: &str, body: Option<&serde_json::Value>) -> daemon::Result<String> {
        let body = body.map(serde_json::Value::to_string);
        let url = format!("{}{}", self.base, path);
        let reply = self
            .transport
            .request(method, &url, body.as_deref(), None, DAEMON_TIMEOUT)
            .ok_or_else(|| DaemonError::Transport(format!("no response from {}", self.base)))?;
        if reply.body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(DaemonError::ResponseTooLarge);
        }
        // `Daemon::map_err`.
        match reply.status {
            200 => Ok(reply.body),
            429 => Err(DaemonError::RateLimited),
            400 => Err(DaemonError::BadRequest(reply.body)),
            404 => Err(DaemonError::NotFound),
            code => Err(DaemonError::Http(code, reply.body)),
        }
    }

    /// A reply whose own `status` must be `OK` (`Daemon::checked`).
    fn checked<R: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> daemon::Result<R> {
        let text = self.call(method, path, body.as_ref())?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| DaemonError::Json(e.to_string()))?;
        match value.get("status").and_then(serde_json::Value::as_str) {
            Some("OK") => serde_json::from_value(value).map_err(|e| DaemonError::Json(e.to_string())),
            Some(other) => Err(DaemonError::Status(other.to_string())),
            None => Err(DaemonError::Json("missing field `status`".into())),
        }
    }
}

impl<T: HttpTransport> SyncDaemon for HttpDaemon<T> {
    fn wallet_sync_data(&self, req: &SyncRequest) -> daemon::Result<WalletSyncData> {
        let body = serde_json::to_value(req).map_err(|e| DaemonError::Json(e.to_string()))?;
        self.checked("POST", "/getwalletsyncdata", Some(body))
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> daemon::Result<GlobalIndexes> {
        self.checked(
            "POST",
            "/get_global_indexes_for_range",
            Some(serde_json::json!({ "startHeight": start, "endHeight": end })),
        )
    }

    fn transactions_status(&self, hashes: &[String]) -> daemon::Result<TransactionsStatus> {
        self.checked("POST", "/get_transactions_status", Some(serde_json::json!({ "transactionHashes": hashes })))
    }

    fn info(&self) -> daemon::Result<Info> {
        self.checked("GET", "/info", None)
    }
}

impl<T: HttpTransport> TransferDaemon for HttpDaemon<T> {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> daemon::Result<RandomOuts> {
        self.checked(
            "POST",
            "/getrandom_outs",
            Some(serde_json::json!({ "amounts": amounts, "outs_count": outs_count })),
        )
    }

    /// HTTP 200 either way; `status` is `OK` or `Failed` with `error`.
    fn send_raw_transaction(&self, tx_hex: &str) -> daemon::Result<SendResult> {
        let text = self.call("POST", "/sendrawtransaction", Some(&serde_json::json!({ "tx_as_hex": tx_hex })))?;
        serde_json::from_str(&text).map_err(|e| DaemonError::Json(e.to_string()))
    }

    fn remote_pow(&self, prefix: &[u8], difficulty: u64, height: u64) -> Option<[u8; TX_POW_NONCE_SIZE]> {
        let server = self.pow_server.as_ref()?;
        let result = server.solve(prefix, difficulty, height);
        let outcome = match &result {
            Ok(nonce) if crate::transfer::nonce_satisfies(prefix, nonce, difficulty) => PowOutcome::Solved,
            Ok(_) => PowOutcome::ComputedLocally("the server's nonce did not meet the difficulty".into()),
            Err(e) => PowOutcome::ComputedLocally(e.to_string()),
        };
        *self.last_pow.borrow_mut() = Some(outcome);
        result.ok()
    }
}

/// [`HttpTransport`] over `ureq`, for desktop and Android.
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
    max_body: usize,
}

#[cfg(feature = "native")]
impl UreqTransport {
    /// For a daemon: bodies up to [`MAX_RESPONSE_BYTES`], gzip, redirects
    /// followed, as [`crate::daemon::Daemon`].
    pub fn for_daemon() -> Self {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).build();
        Self { agent, max_body: MAX_RESPONSE_BYTES as usize }
    }

    /// For a proof-of-work server: small bodies and no redirects, as the C++
    /// client (`set_follow_location(false)`).
    pub fn for_pow_server() -> Self {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).redirects(0).build();
        Self { agent, max_body: crate::txpow::MAX_REPLY_BYTES }
    }
}

#[cfg(feature = "native")]
impl HttpTransport for UreqTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        api_key: Option<&str>,
        timeout: Duration,
    ) -> Option<HttpReply> {
        use std::io::Read;

        let mut req = self.agent.request(method, url).timeout(timeout);
        if let Some(key) = api_key {
            req = req.set("X-API-KEY", key);
        }
        let result = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(b),
            None => req.call(),
        };
        let resp = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(_)) => return None,
        };
        let status = resp.status();
        let mut buf = Vec::new();
        // One byte over the cap is enough for the caller to see it was exceeded.
        resp.into_reader().take(self.max_body as u64 + 1).read_to_end(&mut buf).ok()?;
        Some(HttpReply { status, body: String::from_utf8_lossy(&buf).into_owned() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// `(method, url, body)` of one request.
    type Asked = (String, String, Option<serde_json::Value>);

    /// A scripted reply, and what the error it causes must look like.
    type Case<'a> = (Option<(u16, &'a str)>, fn(&DaemonError) -> bool);

    /// Answers from a script and records what was asked.
    #[derive(Default)]
    struct Scripted {
        replies: RefCell<VecDeque<Option<HttpReply>>>,
        asked: RefCell<Vec<Asked>>,
    }

    impl Scripted {
        fn with(replies: Vec<Option<(u16, &str)>>) -> Self {
            let s = Self::default();
            for r in replies {
                s.replies.borrow_mut().push_back(r.map(|(status, body)| HttpReply { status, body: body.into() }));
            }
            s
        }
    }

    impl HttpTransport for Scripted {
        fn request(
            &self,
            method: &str,
            url: &str,
            body: Option<&str>,
            _api_key: Option<&str>,
            _timeout: Duration,
        ) -> Option<HttpReply> {
            let body = body.map(|b| serde_json::from_str(b).expect("a JSON body"));
            self.asked.borrow_mut().push((method.into(), url.into(), body));
            self.replies.borrow_mut().pop_front().flatten()
        }
    }

    fn daemon(t: &Scripted) -> HttpDaemon<&Scripted> {
        HttpDaemon::new("http://node.example.com:17856/", t).unwrap()
    }

    #[test]
    fn urls_are_normalized_or_refused() {
        assert_eq!(
            normalize_url(" https://node.example.com:17856/ ").as_deref(),
            Some("https://node.example.com:17856")
        );
        for bad in ["", "node.example.com:17856", "ftp://x", "https://", "http://a b"] {
            assert_eq!(normalize_url(bad), None, "{bad}");
        }
    }

    /// The request shapes are the wire contract (spec/09); they must be the
    /// ones `daemon::Daemon` sends.
    #[test]
    fn every_request_has_the_daemon_wire_shape() {
        let t = Scripted::with(vec![
            Some((200, r#"{"items":[],"status":"OK","synced":true}"#)),
            Some((200, r#"{"indexes":[],"status":"OK"}"#)),
            Some((200, r#"{"transactionsInPool":[],"transactionsInBlock":[],"transactionsUnknown":[],"status":"OK"}"#)),
            Some((200, r#"{"outs":[],"status":"OK"}"#)),
            Some((200, r#"{"status":"OK"}"#)),
        ]);
        let d = daemon(&t);
        let req = SyncRequest { start_height: 5, block_count: 100, ..Default::default() };
        let _ = d.wallet_sync_data(&req);
        let _ = d.global_indexes_for_range(10, 20);
        let _ = d.transactions_status(&["aa".into()]);
        let _ = d.random_outs(&[100, 200], 8);
        let _ = d.send_raw_transaction("beef");

        let asked = t.asked.borrow();
        let paths: Vec<_> = asked.iter().map(|(m, u, _)| format!("{m} {u}")).collect();
        assert_eq!(
            paths,
            [
                "POST http://node.example.com:17856/getwalletsyncdata",
                "POST http://node.example.com:17856/get_global_indexes_for_range",
                "POST http://node.example.com:17856/get_transactions_status",
                "POST http://node.example.com:17856/getrandom_outs",
                "POST http://node.example.com:17856/sendrawtransaction",
            ]
        );
        assert_eq!(asked[0].2.as_ref().unwrap(), &serde_json::to_value(&req).unwrap());
        assert_eq!(asked[1].2.as_ref().unwrap(), &serde_json::json!({ "startHeight": 10, "endHeight": 20 }));
        assert_eq!(asked[2].2.as_ref().unwrap(), &serde_json::json!({ "transactionHashes": ["aa"] }));
        assert_eq!(asked[3].2.as_ref().unwrap(), &serde_json::json!({ "amounts": [100, 200], "outs_count": 8 }));
        assert_eq!(asked[4].2.as_ref().unwrap(), &serde_json::json!({ "tx_as_hex": "beef" }));
    }

    #[test]
    fn info_is_a_get() {
        let body = r#"{"height":4218391,"network_height":4218391,"difficulty":30436438,"status":"OK","synced":true,
                      "sync_features":["heightRange","skipEmptyBlocks"]}"#;
        let t = Scripted::with(vec![Some((200, body))]);
        let info = daemon(&t).info().unwrap();
        let asked = t.asked.borrow();
        assert_eq!((asked[0].0.as_str(), asked[0].1.as_str()), ("GET", "http://node.example.com:17856/info"));
        assert!(asked[0].2.is_none());
        assert_eq!(info.top_index(), 4_218_390);
        assert!(info.supports("heightRange"));
    }

    /// The recorded mainnet replies parse through this client as through
    /// `Daemon` (`daemon::tests::sample_responses_parse`).
    #[test]
    fn the_mainnet_samples_parse() {
        let vectors = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors");
        let read = |f: &str| std::fs::read_to_string(vectors.join(f)).unwrap();
        let (sync, indexes, outs) = (
            read("mainnet_getwalletsyncdata_4213000.json"),
            read("mainnet_get_global_indexes_for_range.json"),
            read("mainnet_getrandom_outs.json"),
        );
        let t = Scripted::with(vec![Some((200, &sync)), Some((200, &indexes)), Some((200, &outs))]);
        let d = daemon(&t);
        let s = d.wallet_sync_data(&SyncRequest { start_height: 4_213_000, block_count: 2, ..Default::default() });
        assert_eq!(s.unwrap().items[0].block_height, 4_213_000);
        assert!(!d.global_indexes_for_range(4_213_000, 4_213_001).unwrap().indexes.is_empty());
        assert!(!d.random_outs(&[1], 2).unwrap().outs.is_empty());
    }

    /// `Daemon::map_err` and `Daemon::checked`, reply for reply.
    #[test]
    fn failures_are_classified_as_the_native_client_does() {
        let big = "x".repeat(MAX_RESPONSE_BYTES as usize + 1);
        let cases: Vec<Case> = vec![
            (None, |e| matches!(e, DaemonError::Transport(_))),
            (Some((429, "")), |e| matches!(e, DaemonError::RateLimited)),
            (Some((400, "too many")), |e| matches!(e, DaemonError::BadRequest(b) if b == "too many")),
            (Some((404, "")), |e| matches!(e, DaemonError::NotFound)),
            (Some((500, "boom")), |e| matches!(e, DaemonError::Http(500, b) if b == "boom")),
            (Some((200, "not json")), |e| matches!(e, DaemonError::Json(_))),
            (Some((200, r#"{"status":"Failed"}"#)), |e| matches!(e, DaemonError::Status(s) if s == "Failed")),
            (Some((200, &big)), |e| matches!(e, DaemonError::ResponseTooLarge)),
        ];
        for (reply, expected) in cases {
            let t = Scripted::with(vec![reply]);
            let err = daemon(&t).global_indexes_for_range(1, 2).unwrap_err();
            assert!(expected(&err), "{err:?}");
        }
    }

    #[test]
    fn a_refused_relay_is_an_answer_not_an_error() {
        let t = Scripted::with(vec![Some((200, r#"{"status":"Failed","error":"double spend"}"#))]);
        let sent = daemon(&t).send_raw_transaction("beef").unwrap();
        assert_eq!((sent.status.as_str(), sent.error.as_deref()), ("Failed", Some("double spend")));
    }

    #[test]
    fn the_pow_server_is_asked_only_when_one_is_set_and_its_outcome_is_reported() {
        let t =
            Scripted::with(vec![Some((401, r#"{"status":"error","error":"missing or incorrect X-API-KEY header"}"#))]);
        let mut d = daemon(&t);
        assert_eq!(d.remote_pow(&[0; 16], 1, 1), None);
        assert!(t.asked.borrow().is_empty(), "no server, no request");
        assert_eq!(d.take_pow_outcome(), None);

        d.set_pow_server(Some(TxPowServer::new("http://pow.example.com", Some("k"), &t).unwrap()));
        assert_eq!(d.remote_pow(&[0; 16], 1, 1), None);
        match d.take_pow_outcome() {
            Some(PowOutcome::ComputedLocally(why)) => assert!(why.contains("X-API-KEY"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(d.take_pow_outcome(), None, "reported once");
    }
}
