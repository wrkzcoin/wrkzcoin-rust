// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet-api`'s request log, in a test binary of its own.
//!
//! Logging is process-wide: turned on at debug inside `tests/api.rs`, it would
//! put every other test's requests on the runner's standard error. Here it is
//! the only thing running.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use wrkz_rpc::log::Level;
use wrkz_wallet::api::{serve, ApiConfig, ApiState, DaemonFactory};

const API_KEY: &str = "log-test-api-key-4471";

fn raw(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
}

#[test]
fn a_request_is_logged_without_its_key_its_query_or_its_body() {
    let dir = std::env::temp_dir().join(format!("wrkz-wallet-api-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("api.log");
    wrkz_wallet::logging::configure(Some(Level::Debug), Some(&log)).expect("the log file opens");

    let no_daemon: DaemonFactory = Box::new(|_: &str, _: u16, _: bool| Err("no daemon here".to_string()));
    let config = ApiConfig { rpc_password: API_KEY.into(), ..Default::default() };
    let state = Arc::new(ApiState::new(config, no_daemon));
    let mut server =
        serve::start(state, serve::ServeConfig { bind: "127.0.0.1:0".into(), ..Default::default() }).expect("bind");
    let addr = server.local_addr();

    // A wrong key, with a secret in the query string.
    let answer = raw(
        addr,
        "GET /status?apiKey=QUERY-SECRET-1 HTTP/1.1\r\nHost: x\r\nX-API-KEY: WRONG-KEY-2\r\nConnection: close\r\n\r\n",
    );
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");

    // The right key, and a body that must never reach the log.
    let body = r#"{"address":"BODY-SECRET-3"}"#;
    let answer = raw(
        addr,
        &format!(
            "POST /addresses/validate HTTP/1.1\r\nHost: x\r\nX-API-KEY: {API_KEY}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");

    let answer =
        raw(addr, &format!("GET /status HTTP/1.1\r\nHost: x\r\nX-API-KEY: {API_KEY}\r\nConnection: close\r\n\r\n"));
    assert!(answer.starts_with("HTTP/1.1 403"), "no wallet is open: {answer}");

    server.stop();
    wrkz_rpc::log::flush_file();
    let text = std::fs::read_to_string(&log).expect("the log was written");

    assert!(text.contains("GET /status -> 401"), "{text}");
    assert!(text.contains("POST /addresses/validate -> 400"), "{text}");
    assert!(text.contains("GET /status -> 403"), "{text}");
    assert!(text.contains("INFO"), "a rejected key is logged at info: {text}");
    for secret in [API_KEY, "WRONG-KEY-2", "QUERY-SECRET-1", "BODY-SECRET-3"] {
        assert!(!text.contains(secret), "the log holds {secret}:\n{text}");
    }

    // Off is off: nothing more is written, whatever happens.
    wrkz_wallet::logging::set_level(None);
    wrkz_wallet::logging::log(Level::Error, format_args!("SHOULD-NOT-APPEAR"));
    wrkz_rpc::log::flush_file();
    assert!(!std::fs::read_to_string(&log).unwrap().contains("SHOULD-NOT-APPEAR"));

    let _ = std::fs::remove_dir_all(&dir);
}
