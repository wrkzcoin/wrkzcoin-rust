// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The server over a real socket: the HTTP framing, keep-alive, the limits, and
//! the property that a slow or hostile client cannot stall the others.
//!
//! `tests/endpoints.rs` drives the same handlers without a socket; this is the
//! part that only a socket can prove.

mod fake;

use fake::FakeNode;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wrkz_rpc::http::HttpLimits;
use wrkz_rpc::server::{self, RunningServer, ServerConfig};

fn start(config: ServerConfig) -> RunningServer {
    server::start(Arc::new(FakeNode::default()), ServerConfig { bind: "127.0.0.1:0".into(), ..config })
        .expect("the listener binds")
}

/// Send raw bytes and read everything back until the peer closes or the read
/// times out.
fn raw(addr: std::net::SocketAddr, request: &[u8], timeout: Duration) -> String {
    let mut s = TcpStream::connect(addr).expect("connects");
    s.set_read_timeout(Some(timeout)).unwrap();
    s.set_write_timeout(Some(timeout)).unwrap();
    s.write_all(request).unwrap();
    s.flush().unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn get(addr: std::net::SocketAddr, path: &str) -> String {
    raw(addr, format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(), Duration::from_secs(5))
}

fn status_of(response: &str) -> u16 {
    response.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn body_of(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

/// One POST over its own connection, returning the head as text and the body
/// as bytes — a gzipped body is not text.
fn post_bytes(addr: std::net::SocketAddr, path: &str, body: &str, extra_headers: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).expect("connects");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").expect("a response head");
    (String::from_utf8_lossy(&out[..split]).into_owned(), out[split + 4..].to_vec())
}

/// Forty blocks far below the fake's tip: a body worth compressing, and a
/// range the cache may keep.
fn sync_node(sync_start: Option<u64>) -> FakeNode {
    FakeNode { sync_items: (1000..1040).map(fake::sync_block).collect(), sync_start, ..FakeNode::default() }
}

fn serve(node: Arc<FakeNode>, config: ServerConfig) -> RunningServer {
    server::start(node as Arc<dyn wrkz_rpc::NodeApi>, ServerConfig { bind: "127.0.0.1:0".into(), ..config })
        .expect("the listener binds")
}

#[test]
fn metrics_exist_only_when_enabled_and_sit_behind_the_token() {
    let off = start(ServerConfig::default());
    assert_eq!(status_of(&get(off.local_addr(), "/metrics")), 404, "not a C++ route unless asked for");

    let on = start(ServerConfig { metrics: true, ..Default::default() });
    assert_eq!(status_of(&get(on.local_addr(), "/height")), 200);
    let response = get(on.local_addr(), "/metrics");
    assert_eq!(status_of(&response), 200, "{response}");
    assert!(response.contains("Content-Type: text/plain; version=0.0.4"), "{response}");
    let body = body_of(&response);
    assert!(body.contains("\nwrkz_height 4213001\n"), "{body}");
    assert!(body.contains("wrkz_connections{direction=\"outgoing\"} 3\n"), "{body}");
    assert!(body.contains("# TYPE wrkz_rpc_requests_total counter\nwrkz_rpc_requests_total 1\n"), "{body}");
    assert!(body.contains("wrkz_synchronized 1\n"), "{body}");

    let guarded = start(ServerConfig { metrics: true, access_token: "s3cret".into(), ..Default::default() });
    assert_eq!(status_of(&get(guarded.local_addr(), "/metrics")), 401, "the token applies here too");
    let bearer = raw(
        guarded.local_addr(),
        b"GET /metrics HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer s3cret\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(status_of(&bearer), 200, "{bearer}");
}

#[test]
fn health_exists_only_when_enabled_and_says_whether_the_node_is_synced() {
    let off = start(ServerConfig::default());
    assert_eq!(status_of(&get(off.local_addr(), "/health")), 404, "not a C++ route unless asked for");

    let on = start(ServerConfig { health: true, ..Default::default() });
    let response = get(on.local_addr(), "/health");
    assert_eq!(status_of(&response), 200, "{response}");
    let body = body_of(&response);
    assert!(body.contains("\"status\":\"OK\""), "{body}");
    assert!(body.contains("\"synced\":true"), "{body}");
    assert!(body.contains("\"height\":4213001"), "{body}");

    let syncing = serve(
        Arc::new(FakeNode { synced: false, ..FakeNode::default() }),
        ServerConfig { health: true, ..Default::default() },
    );
    let response = get(syncing.local_addr(), "/health");
    assert_eq!(status_of(&response), 503, "a node still catching up is not healthy yet: {response}");
    assert!(body_of(&response).contains("\"status\":\"SYNCING\""), "{response}");

    let guarded = start(ServerConfig { health: true, access_token: "s3cret".into(), ..Default::default() });
    assert_eq!(status_of(&get(guarded.local_addr(), "/health")), 401, "the token applies here too");
}

#[test]
fn a_client_that_accepts_gzip_gets_the_same_body_compressed() {
    let server = serve(Arc::new(sync_node(None)), ServerConfig::default());
    let (plain_head, plain) = post_bytes(server.local_addr(), "/getwalletsyncdata", "{}", "");
    assert!(plain_head.starts_with("HTTP/1.1 200"), "{plain_head}");
    assert!(!plain_head.contains("Content-Encoding"), "no Accept-Encoding, no compression: {plain_head}");
    assert!(plain.len() > wrkz_rpc::http::MIN_GZIP_BYTES, "a body worth compressing: {} bytes", plain.len());

    let (gz_head, gz) = post_bytes(server.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: gzip\r\n");
    assert!(gz_head.contains("Content-Encoding: gzip"), "{gz_head}");
    assert!(gz_head.contains("Vary: Accept-Encoding"), "{gz_head}");
    assert!(gz_head.contains(&format!("Content-Length: {}", gz.len())), "framed by the compressed length: {gz_head}");
    assert!(gz.len() * 2 < plain.len(), "{} compressed against {} plain", gz.len(), plain.len());
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, plain, "byte for byte the same answer");

    // A server told not to compress never does, whatever the client accepts.
    let off = serve(Arc::new(sync_node(None)), ServerConfig { compression: "none".into(), ..Default::default() });
    let (head, body) = post_bytes(off.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: gzip\r\n");
    assert!(!head.contains("Content-Encoding"), "{head}");
    assert_eq!(body, plain);
}

/// Decode a `Transfer-Encoding: chunked` body.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = body.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&body[..eol]).ok().and_then(|s| usize::from_str_radix(s.trim(), 16).ok());
        let Some(size) = size else { break };
        body = &body[eol + 2..];
        if size == 0 {
            break;
        }
        assert!(body.len() >= size + 2, "a chunk shorter than its header claims");
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
    out
}

#[test]
fn a_large_body_streams_chunked_when_the_operator_asks_for_it() {
    // The default: `Content-Length`, exactly as cpp-httplib frames it.
    let framed = serve(Arc::new(sync_node(None)), ServerConfig::default());
    let (head, gz) = post_bytes(framed.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: gzip\r\n");
    assert!(head.contains(&format!("Content-Length: {}", gz.len())), "{head}");
    assert!(!head.contains("Transfer-Encoding"), "off by default: {head}");
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut plain).unwrap();

    // Threshold below the body: same bytes, streamed.
    let streamed = serve(
        Arc::new(sync_node(None)),
        ServerConfig { stream_threshold_bytes: wrkz_rpc::http::MIN_GZIP_BYTES, ..Default::default() },
    );
    let (head, chunked) = post_bytes(streamed.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: gzip\r\n");
    assert!(head.contains("Transfer-Encoding: chunked"), "{head}");
    assert!(head.contains("Content-Encoding: gzip"), "{head}");
    assert!(head.contains("Vary: Accept-Encoding"), "{head}");
    assert!(!head.contains("Content-Length"), "chunked and Content-Length are exclusive: {head}");
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(dechunk(&chunked).as_slice()).read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, plain, "streamed or not, the same answer");

    // A client that accepts nothing gets identity and `Content-Length`, even
    // over the threshold: there is nothing to stream through.
    let (head, body) = post_bytes(streamed.local_addr(), "/getwalletsyncdata", "{}", "");
    assert!(!head.contains("Transfer-Encoding"), "{head}");
    assert!(head.contains(&format!("Content-Length: {}", body.len())), "{head}");
    assert_eq!(body, plain);
}

/// Only with the crate's `zstd` feature; a build without it never offers the
/// coding and a client asking for it gets gzip, which is the C++ behaviour.
#[cfg(feature = "zstd")]
#[test]
fn a_client_that_asks_for_zstd_gets_zstd_and_one_that_does_not_gets_gzip() {
    let server = serve(Arc::new(sync_node(None)), ServerConfig::default());
    let (_, plain) = post_bytes(server.local_addr(), "/getwalletsyncdata", "{}", "");

    let (head, body) = post_bytes(server.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: gzip, zstd\r\n");
    assert!(head.contains("Content-Encoding: zstd"), "zstd wins when both are offered: {head}");
    assert!(head.contains(&format!("Content-Length: {}", body.len())), "{head}");
    let decoded = zstd::stream::decode_all(body.as_slice()).expect("decodes");
    assert_eq!(decoded, plain, "byte for byte the same answer");

    // `zstd;q=0` is "anything but zstd", so gzip it is.
    let (head, gz) = post_bytes(server.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: gzip, zstd;q=0\r\n");
    assert!(head.contains("Content-Encoding: gzip"), "{head}");
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut out).unwrap();
    assert_eq!(out, plain);

    // And streamed, the same answer again.
    let streamed = serve(
        Arc::new(sync_node(None)),
        ServerConfig { stream_threshold_bytes: wrkz_rpc::http::MIN_GZIP_BYTES, ..Default::default() },
    );
    let (head, chunked) = post_bytes(streamed.local_addr(), "/getwalletsyncdata", "{}", "Accept-Encoding: zstd\r\n");
    assert!(head.contains("Transfer-Encoding: chunked"), "{head}");
    assert!(head.contains("Content-Encoding: zstd"), "{head}");
    assert_eq!(zstd::stream::decode_all(dechunk(&chunked).as_slice()).expect("decodes"), plain);
}

#[test]
fn a_repeated_wallet_sync_request_is_answered_from_the_cache() {
    use std::sync::atomic::Ordering::SeqCst;
    let node = Arc::new(sync_node(Some(1000)));
    let server = serve(Arc::clone(&node), ServerConfig::default());
    let request = r#"{"startHeight":1000,"blockCount":100}"#;

    let (_, first) = post_bytes(server.local_addr(), "/getwalletsyncdata", request, "");
    assert_eq!(node.sync_calls.load(SeqCst), 1);
    let (head, second) = post_bytes(server.local_addr(), "/getwalletsyncdata", request, "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("Content-Type: application/json"), "{head}");
    assert_eq!(second, first, "the cached answer is the built one, byte for byte");
    assert_eq!(node.sync_calls.load(SeqCst), 1, "served from the cache, not rebuilt");

    // Anything that changes the bytes is a different entry.
    let base64 = r#"{"startHeight":1000,"blockCount":100,"encoding":"base64"}"#;
    let (_, encoded) = post_bytes(server.local_addr(), "/getwalletsyncdata", base64, "");
    assert_ne!(encoded, first);
    assert_eq!(node.sync_calls.load(SeqCst), 2);

    // A cached answer is compressed like any other.
    let (gz_head, _) = post_bytes(server.local_addr(), "/getwalletsyncdata", request, "Accept-Encoding: gzip\r\n");
    assert!(gz_head.contains("Content-Encoding: gzip"), "{gz_head}");
    assert_eq!(node.sync_calls.load(SeqCst), 2);
}

#[test]
fn near_the_tip_or_with_the_cache_off_every_request_is_built() {
    use std::sync::atomic::Ordering::SeqCst;
    let request = r#"{"startHeight":1000,"blockCount":100}"#;

    // The fake's tip is 4,213,000: a range ending 91 blocks below it is inside
    // the reorganisation margin and must not be kept.
    let near = Arc::new(FakeNode {
        sync_items: (4_212_900..4_212_910).map(fake::sync_block).collect(),
        sync_start: Some(4_212_900),
        ..FakeNode::default()
    });
    let server = serve(Arc::clone(&near), ServerConfig::default());
    post_bytes(server.local_addr(), "/getwalletsyncdata", request, "");
    post_bytes(server.local_addr(), "/getwalletsyncdata", request, "");
    assert_eq!(near.sync_calls.load(SeqCst), 2);

    let off = Arc::new(sync_node(Some(1000)));
    let server = serve(Arc::clone(&off), ServerConfig { sync_cache_bytes: 0, ..Default::default() });
    post_bytes(server.local_addr(), "/getwalletsyncdata", request, "");
    post_bytes(server.local_addr(), "/getwalletsyncdata", request, "");
    assert_eq!(off.sync_calls.load(SeqCst), 2);
}

#[test]
fn a_request_over_a_socket_gets_a_framed_response() {
    let server = start(ServerConfig::default());
    let response = get(server.local_addr(), "/height");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: application/json\r\n"));
    assert!(response.contains("Content-Length: 57\r\n"));
    assert_eq!(body_of(&response), r#"{"height":4213001,"network_height":4213001,"status":"OK"}"#);
}

#[test]
fn a_post_with_a_body_round_trips() {
    let server = start(ServerConfig::default());
    let body = r#"{"jsonrpc":"2.0","id":5,"method":"getblockcount","params":{}}"#;
    let request = format!(
        "POST /json_rpc HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let response = raw(server.local_addr(), request.as_bytes(), Duration::from_secs(5));
    assert_eq!(status_of(&response), 200);
    assert_eq!(body_of(&response), r#"{"id":5,"jsonrpc":"2.0","result":{"count":4213001,"status":"OK"}}"#);
}

#[test]
fn keep_alive_serves_several_requests_on_one_connection() {
    let server = start(ServerConfig::default());
    let mut s = TcpStream::connect(server.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    for _ in 0..3 {
        s.write_all(b"GET /height HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        s.flush().unwrap();
        // Read exactly one response: headers, then `Content-Length` bytes.
        let mut head = Vec::new();
        loop {
            let mut b = [0u8; 1];
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.contains("Keep-Alive: timeout=3, max=1000\r\n"), "{head}");
        let len: usize = head
            .split("Content-Length: ")
            .nth(1)
            .and_then(|s| s.split("\r\n").next())
            .and_then(|s| s.parse().ok())
            .expect("a length");
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).unwrap();
        assert_eq!(String::from_utf8(body).unwrap(), r#"{"height":4213001,"network_height":4213001,"status":"OK"}"#);
    }
    // `Connection: close` ends it.
    s.write_all(b"GET /height HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).unwrap();
    assert!(String::from_utf8_lossy(&rest).contains("Connection: close"));
}

#[test]
fn keep_alive_can_be_turned_off() {
    let server = start(ServerConfig { keep_alive: false, ..Default::default() });
    let response = raw(server.local_addr(), b"GET /height HTTP/1.1\r\nHost: x\r\n\r\n", Duration::from_secs(5));
    assert!(response.contains("Connection: close\r\n"), "{response}");
    assert!(!response.contains("Keep-Alive:"));
}

#[test]
fn an_oversized_declared_body_is_refused_before_it_is_read() {
    let server =
        start(ServerConfig { limits: HttpLimits { max_body: 64, ..Default::default() }, ..Default::default() });
    // The client promises a gigabyte and sends nothing: the server must answer
    // 413 immediately rather than waiting for, or allocating, the body.
    let started = Instant::now();
    let response = raw(
        server.local_addr(),
        b"POST /get_transactions_status HTTP/1.1\r\nHost: x\r\nContent-Length: 1073741824\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(status_of(&response), 413);
    assert_eq!(body_of(&response), r#"{"error":"RPC request body too large","status":"Failed"}"#);
    assert!(started.elapsed() < Duration::from_secs(2), "the answer is immediate, not after a read");
}

#[test]
fn hostile_heads_are_refused_over_the_socket() {
    let server = start(ServerConfig {
        limits: HttpLimits { max_headers: 4, max_header_line: 64, ..Default::default() },
        ..Default::default()
    });
    let many: String = (0..50).map(|i| format!("H{i}: v\r\n")).collect();
    let response =
        raw(server.local_addr(), format!("GET /height HTTP/1.1\r\n{many}\r\n").as_bytes(), Duration::from_secs(5));
    assert_eq!(status_of(&response), 431);

    let response = raw(server.local_addr(), b"total nonsense\r\n\r\n", Duration::from_secs(5));
    assert_eq!(status_of(&response), 400);

    let response = raw(
        server.local_addr(),
        b"POST /height HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(status_of(&response), 501);
}

/// A client that opens a connection and never sends anything must not hold a
/// worker past the read timeout, and must not stop anyone else being served.
#[test]
fn a_silent_client_does_not_stall_the_others() {
    let server = start(ServerConfig { workers: 2, read_timeout: Duration::from_millis(300), ..Default::default() });
    // Two idle connections, which is every worker.
    let idle: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(server.local_addr()).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(50));

    // The third client waits for a worker, but only for one read timeout.
    let started = Instant::now();
    let response = get(server.local_addr(), "/height");
    assert_eq!(status_of(&response), 200, "served after the idle connections time out");
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    drop(idle);
}

/// A partial request that stops mid-headers is dropped at the read timeout
/// rather than held open.
#[test]
fn a_half_written_request_times_out() {
    let server = start(ServerConfig { read_timeout: Duration::from_millis(300), ..Default::default() });
    let mut s = TcpStream::connect(server.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /height HTTP/1.1\r\nHost: x\r\n").unwrap();
    s.flush().unwrap();
    let started = Instant::now();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    assert!(started.elapsed() < Duration::from_secs(3), "the server let go");
    // Nothing useful came back, and the server is still serving.
    assert_eq!(status_of(&get(server.local_addr(), "/height")), 200);
    let _ = s.shutdown(Shutdown::Both);
}

/// Send `prefix`, then one more byte every 100 ms — each read on the server
/// waits far less than its timeout — until the server answers, closes or five
/// seconds pass. Returns how long the server put up with it.
fn trickle(addr: std::net::SocketAddr, prefix: &[u8]) -> Duration {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    s.write_all(prefix).unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if s.write_all(b"x").is_err() {
            break;
        }
        match s.read(&mut [0u8; 512]) {
            // The server's 400, or its close.
            Ok(_) => break,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    started.elapsed()
}

/// A client that never pauses long enough to trip the read timeout but never
/// finishes either is cut off at the deadline for the head, or for the body,
/// rather than holding a worker for as long as it cares to.
#[test]
fn a_trickled_request_is_cut_off_at_its_deadline() {
    let server = start(ServerConfig { read_timeout: Duration::from_millis(500), ..Default::default() });
    let head = trickle(server.local_addr(), b"GET /height HTTP/1.1\r\nX-Slow: ");
    assert!(head < Duration::from_secs(3), "a trickled head was held for {head:?}");
    let body = trickle(
        server.local_addr(),
        b"POST /get_transactions_status HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n\r\n",
    );
    assert!(body < Duration::from_secs(3), "a trickled body was held for {body:?}");
    // And the server is still serving.
    assert_eq!(status_of(&get(server.local_addr(), "/height")), 200);
}

/// A full queue is answered, not dropped: the `503` the module docs promise,
/// with `Connection: close`, written by the acceptor without a worker.
#[test]
fn a_full_queue_is_answered_503_not_dropped() {
    let server = start(ServerConfig {
        workers: 1,
        queue_capacity: 1,
        read_timeout: Duration::from_secs(5),
        ..Default::default()
    });
    let addr = server.local_addr();
    // One silent connection holds the only worker, the next fills the queue.
    let held = TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let queued = TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let mut shed = TcpStream::connect(addr).unwrap();
    shed.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let started = Instant::now();
    let mut out = Vec::new();
    let _ = shed.read_to_end(&mut out);
    let response = String::from_utf8_lossy(&out);
    assert_eq!(status_of(&response), 503, "{response}");
    assert!(response.contains("Connection: close\r\n"), "{response}");
    assert_eq!(body_of(&response), r#"{"error":"RPC server is busy, please retry shortly","status":"Failed"}"#);
    assert!(started.elapsed() < Duration::from_secs(2), "answered at once, not after a worker freed up");
    drop((held, queued));
}

#[test]
fn several_clients_are_served_concurrently() {
    let server = start(ServerConfig { workers: 4, ..Default::default() });
    let addr = server.local_addr();
    let handles: Vec<_> = (0..16).map(|_| std::thread::spawn(move || status_of(&get(addr, "/info")))).collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 200);
    }
}

#[test]
fn stopping_the_server_releases_the_port_and_joins_its_threads() {
    let mut server = start(ServerConfig::default());
    let addr = server.local_addr();
    assert_eq!(status_of(&get(addr, "/height")), 200);
    server.stop();
    // A second stop is a no-op, and the address is free again.
    server.stop();
    let reopened =
        server::start(Arc::new(FakeNode::default()), ServerConfig { bind: addr.to_string(), ..Default::default() });
    assert!(reopened.is_ok(), "the port was released");
}
