// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Client for an external transaction proof-of-work server
//! (`wrkz-txpow-server`, the C++ `src/txpowserver`), wire-compatible with the
//! C++ client (`src/nigel/TxPowClient.cpp`):
//!
//! | request | reply |
//! | --- | --- |
//! | `POST {base}/pow` `{"prefix": hex, "wait_ms": 20000, "height": h}` | `{"status": "pending" \| "done" \| "error", "job_id", "nonce"}` |
//! | `GET {base}/pow/{job_id}?wait_ms=20000` | the same |
//! | `DELETE {base}/pow/{job_id}` | frees the server's queue slot |
//! | `GET {base}/health` | `{"status": "OK", "threads", "queue", "capacity"}` |
//!
//! The server sees only the unsigned transaction prefix — the bytes the daemon
//! sees at broadcast — and returns eight nonce bytes. Nothing it answers is
//! trusted: [`crate::transfer`] checks the nonce with one hash and searches
//! locally when the server is unreachable, slow, refuses, or is wrong.
//!
//! Where this differs from the C++ client: the server is one URL (scheme,
//! host, port and mount path together) instead of host, port and an SSL flag;
//! an API key, which the server can require (`X-API-KEY`) and the C++ client
//! cannot send, is supported; and the height is sent, so the server works to
//! the difficulty the wallet will be judged at. The HTTP itself is a
//! [`HttpTransport`], so the same code runs natively
//! ([`crate::http::UreqTransport`]) and in a browser's Web Worker
//! (XMLHttpRequest, in apps/pluton).

use std::time::Duration;

use serde::Serialize;

use wrkz_primitives::constants::TX_POW_NONCE_SIZE;

use crate::http::{HttpReply, HttpTransport};
use crate::platform;

/// How long one long-poll asks the server to hold the request
/// (`WAIT_MS_PER_REQUEST`). The server caps it at its own maximum.
pub const WAIT_MS_PER_REQUEST: u64 = 20_000;

/// The total wait before giving up and computing locally (`timeoutSeconds`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Pause between polls when the server answers "pending" at once, so a
/// server with long polling disabled is not hammered (`POLL_PAUSE_MS`).
const POLL_PAUSE: Duration = Duration::from_millis(500);

/// A reply body larger than this is not a proof-of-work server's.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Why a server could not be used. Each ends with the wallet computing the
/// proof of work itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxPowError {
    /// Not an `http://` or `https://` URL with a host.
    InvalidUrl(String),
    /// No answer at all.
    Unreachable,
    /// The server said no, with its reason (a 401 for a wrong API key, 429
    /// when rate limited, a difficulty over its limit).
    Refused(String),
    /// Not a proof-of-work server's reply.
    Malformed(String),
    /// No nonce within the timeout.
    TimedOut,
}

impl std::fmt::Display for TxPowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxPowError::InvalidUrl(u) => write!(f, "'{u}' is not an http:// or https:// URL"),
            TxPowError::Unreachable => f.write_str("no response (connection refused, timed out, or TLS failed)"),
            TxPowError::Refused(why) => write!(f, "the server refused the job: {why}"),
            TxPowError::Malformed(why) => write!(f, "the server's reply was not understood: {why}"),
            TxPowError::TimedOut => f.write_str("no answer within the time limit"),
        }
    }
}

impl std::error::Error for TxPowError {}

/// What a connection test found (`TxPowClient::probe`), ready to show.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub ok: bool,
    pub url: String,
    pub latency_ms: u64,
    pub threads: u64,
    pub queue: u64,
    pub capacity: u64,
    pub error: Option<String>,
}

/// Check what a user typed and turn it into the base every request is made
/// against: `https://pow.example.com:8080/txpow/` becomes
/// `https://pow.example.com:8080/txpow`.
pub fn normalize_url(input: &str) -> Result<String, TxPowError> {
    crate::http::normalize_url(input).ok_or_else(|| TxPowError::InvalidUrl(input.trim().to_string()))
}

/// A configured proof-of-work server.
pub struct TxPowServer<T> {
    base: String,
    api_key: Option<String>,
    timeout: Duration,
    transport: T,
}

/// How one reply reads.
enum Reply {
    Done([u8; TX_POW_NONCE_SIZE]),
    Pending(String),
}

impl<T: HttpTransport> TxPowServer<T> {
    /// A server at `url` (see [`normalize_url`]). An empty `api_key` is none.
    pub fn new(url: &str, api_key: Option<&str>, transport: T) -> Result<Self, TxPowError> {
        Ok(Self {
            base: normalize_url(url)?,
            api_key: api_key.map(str::trim).filter(|k| !k.is_empty()).map(str::to_string),
            timeout: DEFAULT_TIMEOUT,
            transport,
        })
    }

    /// Change the total wait before the wallet gives up on the server.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The normalized base URL.
    pub fn url(&self) -> &str {
        &self.base
    }

    fn call(&self, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Option<HttpReply> {
        let url = format!("{}{}", self.base, path);
        self.transport.request(method, &url, body, self.api_key.as_deref(), timeout)
    }

    /// The nonce for `prefix` (which ends in the eight placeholder nonce
    /// bytes), or why there is none. `difficulty` is only reported; the
    /// server derives its own from `height`, and the caller checks the result.
    pub fn solve(&self, prefix: &[u8], difficulty: u64, height: u64) -> Result<[u8; TX_POW_NONCE_SIZE], TxPowError> {
        let _ = difficulty;
        let deadline = platform::now_millis().saturating_add(self.timeout.as_millis() as u64);
        // Seconds the transport waits for one long-poll to come back.
        let read_timeout = Duration::from_millis(WAIT_MS_PER_REQUEST) + Duration::from_secs(15);

        let submit = serde_json::json!({
            "prefix": hex::encode(prefix),
            "wait_ms": WAIT_MS_PER_REQUEST,
            "height": height,
        })
        .to_string();

        let mut job = match parse_reply(self.call("POST", "/pow", Some(&submit), read_timeout))? {
            Reply::Done(nonce) => return Ok(nonce),
            Reply::Pending(job) => job,
        };

        while platform::now_millis() < deadline {
            let started = platform::now_millis();
            let path = format!("/pow/{job}?wait_ms={WAIT_MS_PER_REQUEST}");
            match parse_reply(self.call("GET", &path, None, read_timeout))? {
                Reply::Done(nonce) => return Ok(nonce),
                Reply::Pending(next) => job = next,
            }
            if platform::now_millis().saturating_sub(started) < POLL_PAUSE.as_millis() as u64 {
                platform::sleep(POLL_PAUSE);
            }
        }

        // Best effort: free the server's queue slot.
        let _ = self.call("DELETE", &format!("/pow/{job}"), None, Duration::from_secs(5));
        Err(TxPowError::TimedOut)
    }

    /// One `GET /health`, without sending any work (`TxPowClient::probe`).
    pub fn probe(&self) -> Probe {
        let mut probe = Probe { url: self.base.clone(), ..Probe::default() };
        let started = platform::now_millis();
        let reply = self.call("GET", "/health", None, Duration::from_secs(10));
        probe.latency_ms = platform::now_millis().saturating_sub(started);

        let Some(reply) = reply else {
            probe.error = Some(TxPowError::Unreachable.to_string());
            return probe;
        };
        if reply.status != 200 {
            probe.error = Some(format!("HTTP {}", reply.status));
            return probe;
        }
        match serde_json::from_str::<serde_json::Value>(&reply.body) {
            Ok(h) if h["status"] == "OK" && h.get("threads").is_some() => {
                probe.ok = true;
                probe.threads = h["threads"].as_u64().unwrap_or(0);
                probe.queue = h["queue"].as_u64().unwrap_or(0);
                probe.capacity = h["capacity"].as_u64().unwrap_or(0);
            }
            Ok(_) => probe.error = Some("answered, but not like a Tx PoW server".into()),
            Err(_) => probe.error = Some("answered, but not with JSON; is this the right port or path?".into()),
        }
        probe
    }
}

/// `parseReply`: a nonce, a job still pending, or why the server cannot help.
fn parse_reply(reply: Option<HttpReply>) -> Result<Reply, TxPowError> {
    let reply = reply.ok_or(TxPowError::Unreachable)?;
    if reply.body.len() > MAX_REPLY_BYTES {
        return Err(TxPowError::Malformed("reply too large".into()));
    }
    let j: serde_json::Value = serde_json::from_str(&reply.body)
        .map_err(|_| TxPowError::Malformed(format!("HTTP {}, body is not JSON", reply.status)))?;
    let status = j["status"].as_str().unwrap_or_default();

    if reply.status != 200 || status == "error" {
        let why = j["error"].as_str().unwrap_or("no reason given");
        return Err(TxPowError::Refused(format!("HTTP {}: {why}", reply.status)));
    }
    match status {
        "pending" => match j["job_id"].as_str() {
            // The id goes into a URL path; a server's id is hex.
            Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => {
                Ok(Reply::Pending(id.to_string()))
            }
            _ => Err(TxPowError::Malformed("pending without a usable job_id".into())),
        },
        "done" => {
            let bytes = hex::decode(j["nonce"].as_str().unwrap_or_default())
                .map_err(|_| TxPowError::Malformed("nonce is not hex".into()))?;
            let nonce: [u8; TX_POW_NONCE_SIZE] =
                bytes.try_into().map_err(|_| TxPowError::Malformed("nonce is not 8 bytes".into()))?;
            Ok(Reply::Done(nonce))
        }
        other => Err(TxPowError::Refused(format!("job ended with status '{other}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// `(method, url, body, api_key)` of one request.
    type Asked = (String, String, Option<String>, Option<String>);

    /// A scripted reply, and what the error it causes must look like.
    type Case<'a> = (Option<(u16, &'a str)>, fn(&TxPowError) -> bool);

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
            api_key: Option<&str>,
            _timeout: Duration,
        ) -> Option<HttpReply> {
            self.asked.borrow_mut().push((
                method.into(),
                url.into(),
                body.map(str::to_string),
                api_key.map(str::to_string),
            ));
            self.replies.borrow_mut().pop_front().flatten()
        }
    }

    const PREFIX: &[u8] = &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0];

    #[test]
    fn urls_are_normalized_or_refused() {
        assert_eq!(
            normalize_url(" https://pow.example.com:8080/txpow/ ").unwrap(),
            "https://pow.example.com:8080/txpow"
        );
        assert_eq!(normalize_url("http://10.0.0.7:17860").unwrap(), "http://10.0.0.7:17860");
        for bad in ["", "pow.example.com", "ftp://x", "https://", "https:///path", "http://a b"] {
            assert!(matches!(normalize_url(bad), Err(TxPowError::InvalidUrl(_))), "{bad}");
        }
    }

    #[test]
    fn an_immediate_answer_is_returned_with_the_wire_shape_of_the_cpp_client() {
        let t = Scripted::with(vec![Some((200, r#"{"status":"done","job_id":"ab12","nonce":"0102030405060708"}"#))]);
        let server = TxPowServer::new("https://pow.example.com/txpow", Some("secret"), &t).unwrap();
        assert_eq!(server.solve(PREFIX, 40_000, 4_218_390).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);

        let asked = t.asked.borrow();
        let (method, url, body, key) = &asked[0];
        assert_eq!((method.as_str(), url.as_str()), ("POST", "https://pow.example.com/txpow/pow"));
        assert_eq!(key.as_deref(), Some("secret"));
        let body: serde_json::Value = serde_json::from_str(body.as_deref().unwrap()).unwrap();
        assert_eq!(body["prefix"], "0102030000000000000000");
        assert_eq!(body["wait_ms"], WAIT_MS_PER_REQUEST);
        assert_eq!(body["height"], 4_218_390);
    }

    #[test]
    fn a_pending_job_is_polled_until_done() {
        let t = Scripted::with(vec![
            Some((200, r#"{"status":"pending","job_id":"ab12"}"#)),
            Some((200, r#"{"status":"pending","job_id":"ab12"}"#)),
            Some((200, r#"{"status":"done","job_id":"ab12","nonce":"ffffffffffffffff"}"#)),
        ]);
        let server = TxPowServer::new("http://pow.example.com", None, &t).unwrap();
        assert_eq!(server.solve(PREFIX, 1, 1).unwrap(), [0xff; 8]);
        let asked = t.asked.borrow();
        assert_eq!(asked.len(), 3);
        assert_eq!(asked[1].0, "GET");
        assert_eq!(asked[1].1, format!("http://pow.example.com/pow/ab12?wait_ms={WAIT_MS_PER_REQUEST}"));
        assert_eq!(asked[1].3, None, "no key configured, none sent");
    }

    #[test]
    fn every_failure_is_reported_not_trusted() {
        let cases: Vec<Case> = vec![
            (None, |e| *e == TxPowError::Unreachable),
            (
                Some((401, r#"{"status":"error","error":"missing or incorrect X-API-KEY header"}"#)),
                |e| matches!(e, TxPowError::Refused(w) if w.contains("401") && w.contains("X-API-KEY")),
            ),
            (Some((200, "<html>")), |e| matches!(e, TxPowError::Malformed(_))),
            (Some((200, r#"{"status":"done","nonce":"0102"}"#)), |e| matches!(e, TxPowError::Malformed(_))),
            (Some((200, r#"{"status":"done","nonce":"zz02030405060708"}"#)), |e| matches!(e, TxPowError::Malformed(_))),
            (Some((200, r#"{"status":"pending","job_id":"../etc"}"#)), |e| matches!(e, TxPowError::Malformed(_))),
            (Some((200, r#"{"status":"cancelled","job_id":"ab"}"#)), |e| matches!(e, TxPowError::Refused(_))),
        ];
        for (reply, expected) in cases {
            let t = Scripted::with(vec![reply]);
            let server = TxPowServer::new("http://pow.example.com", None, &t).unwrap();
            let err = server.solve(PREFIX, 1, 1).unwrap_err();
            assert!(expected(&err), "{reply:?} gave {err:?}");
        }
    }

    #[test]
    fn a_job_past_the_deadline_is_cancelled() {
        let t = Scripted::with(vec![Some((200, r#"{"status":"pending","job_id":"ab12"}"#))]);
        let server = TxPowServer::new("http://pow.example.com", None, &t).unwrap().with_timeout(Duration::ZERO);
        assert_eq!(server.solve(PREFIX, 1, 1).unwrap_err(), TxPowError::TimedOut);
        let asked = t.asked.borrow();
        assert_eq!((asked[1].0.as_str(), asked[1].1.as_str()), ("DELETE", "http://pow.example.com/pow/ab12"));
    }

    #[test]
    fn probe_reads_health() {
        let t = Scripted::with(vec![Some((200, r#"{"status":"OK","threads":8,"queue":1,"capacity":64}"#))]);
        let p = TxPowServer::new("http://pow.example.com/", None, &t).unwrap().probe();
        assert!(p.ok, "{p:?}");
        assert_eq!((p.threads, p.queue, p.capacity), (8, 1, 64));
        assert_eq!(p.url, "http://pow.example.com");

        let t = Scripted::with(vec![Some((200, r#"{"status":"OK"}"#))]);
        let p = TxPowServer::new("http://pow.example.com", None, &t).unwrap().probe();
        assert!(!p.ok && p.error.as_deref() == Some("answered, but not like a Tx PoW server"));
    }
}
