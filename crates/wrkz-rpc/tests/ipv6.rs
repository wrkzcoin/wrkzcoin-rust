// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The RPC on two listeners: `--rpc-bind-ipv6-address` serving the same routes
//! from the same worker pool, and `::1` treated as loopback everywhere
//! `127.0.0.1` is.
//!
//! The C++ starts a second `httplib::Server` with `set_address_family(AF_INET6)`
//! and `set_ipv6_v6only(true)` (`RpcServer.cpp:138`), on the same port
//! (`listenIpv6` uses `m_port`), and only when an address was configured
//! (`m_ipv6Host`, `RpcServer.cpp:97`). This checks the same three properties.
//!
//! Skips with a message when the host has no IPv6 loopback.

mod fake;

use fake::FakeNode;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;
use wrkz_rpc::server::{self, is_loopback_ip, RunningServer, ServerConfig};

fn ipv6_available(what: &str) -> bool {
    if wrkz_p2p::bind::ipv6_loopback_available() {
        return true;
    }
    eprintln!("skipping {what}: this host has no IPv6 loopback");
    false
}

fn start(config: ServerConfig) -> std::io::Result<RunningServer> {
    server::start(Arc::new(FakeNode::default()), config)
}

fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connects");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    s.flush().unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn body_of(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

/// Deliverable 3: both families answer the same routes, and the IPv6 listener
/// is IPv6-only, so binding the IPv4 wildcard on that port still works.
#[test]
fn the_same_routes_are_served_on_both_families() {
    if !ipv6_available("the_same_routes_are_served_on_both_families") {
        return;
    }
    let server = start(ServerConfig {
        bind: "127.0.0.1:0".into(),
        bind_ipv6: "[::1]:0".into(),
        // One worker: if the two listeners did not share the pool, the second
        // request would never be answered.
        workers: 1,
        ..Default::default()
    })
    .expect("both listeners bind");

    let v4 = server.local_addr();
    let v6 = server.local_addr6().expect("the IPv6 listener is up");
    assert!(v4.is_ipv4() && v6.is_ipv6(), "{v4} / {v6}");

    let a = get(v4, "/height");
    let b = get(v6, "/height");
    assert!(a.starts_with("HTTP/1.1 200 OK\r\n"), "{a}");
    assert!(b.starts_with("HTTP/1.1 200 OK\r\n"), "{b}");
    assert_eq!(body_of(&a), body_of(&b), "the same route, the same answer, whichever socket you reach it on");

    // And a JSON-RPC POST, so it is not only the trivial GET path.
    let payload = r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":{}}"#;
    let mut s = TcpStream::connect(v6).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(
        format!(
            "POST /json_rpc HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(r#""count":4213001"#), "{text}");
}

/// No address, no second listener — the C++ default.
#[test]
fn no_ipv6_listener_unless_an_address_is_configured() {
    let server = start(ServerConfig { bind: "127.0.0.1:0".into(), ..Default::default() }).unwrap();
    assert_eq!(server.local_addr6(), None, "the default is off, as in the C++");
}

/// A bind address that is not IPv6 is refused at start-up rather than silently
/// becoming a second IPv4 listener.
#[test]
fn a_non_ipv6_bind_address_is_refused() {
    let outcome =
        start(ServerConfig { bind: "127.0.0.1:0".into(), bind_ipv6: "127.0.0.1:0".into(), ..Default::default() });
    let e = match outcome {
        Err(e) => e,
        Ok(_) => panic!("an IPv4 address is not an IPv6 bind address"),
    };
    assert!(e.to_string().contains("not an IPv6 address"), "{e}");
}

/// Deliverable 3's audit item: `::1` is loopback wherever `127.0.0.1` is. The
/// C++ compares the two strings (`RpcServer.cpp:570`); parsing instead also
/// catches the spellings a proxy or a dual-stack socket can produce.
#[test]
fn both_families_of_loopback_are_recognised() {
    for yes in ["127.0.0.1", "127.0.0.53", "::1", "0:0:0:0:0:0:0:1", "[::1]", "::ffff:127.0.0.1", " ::1 ", "::1%lo0"] {
        assert!(is_loopback_ip(yes), "{yes} is loopback");
    }
    for no in ["", "0.0.0.0", "::", "1.2.3.4", "2a01:4f8:c012:5d10::1", "localhost", "not an address"] {
        assert!(!is_loopback_ip(no), "{no} is not loopback");
    }
}

/// The rate limit exempts loopback (`RpcServer.cpp:570`). A client on `::1`
/// gets the same exemption a client on `127.0.0.1` does — before this, the
/// exemption was a string comparison that a `0:0:0:0:0:0:0:1` peer address
/// would have missed.
#[test]
fn a_client_on_ipv6_loopback_is_exempt_from_the_rate_limit() {
    if !ipv6_available("a_client_on_ipv6_loopback_is_exempt") {
        return;
    }
    let server = start(ServerConfig {
        bind: "127.0.0.1:0".into(),
        bind_ipv6: "[::1]:0".into(),
        max_requests_per_minute: 2,
        ..Default::default()
    })
    .unwrap();
    let v6 = server.local_addr6().unwrap();
    for i in 0..6 {
        let r = get(v6, "/height");
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "request {i} over ::1 must not be rate limited: {r}");
    }
}

/// Every limit is shared: the access token is checked on the IPv6 listener too,
/// because both acceptors hand their sockets to the same `Context`.
#[test]
fn the_access_token_is_enforced_on_the_ipv6_listener() {
    if !ipv6_available("the_access_token_is_enforced_on_the_ipv6_listener") {
        return;
    }
    let server = start(ServerConfig {
        bind: "127.0.0.1:0".into(),
        bind_ipv6: "[::1]:0".into(),
        access_token: "secret".into(),
        ..Default::default()
    })
    .unwrap();
    let v6 = server.local_addr6().unwrap();
    let r = get(v6, "/height");
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");

    let mut s = TcpStream::connect(v6).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /height HTTP/1.1\r\nHost: x\r\nX-API-Key: secret\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
}
