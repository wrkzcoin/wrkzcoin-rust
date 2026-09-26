// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `GET /ws` over a real socket: the handshake and its refusals, the hello,
//! events as the ZMQ bodies, topic filters, the ping and heartbeat, the caps,
//! and the goodbye when the server stops.

mod fake;

use fake::FakeNode;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;
use wrkz_rpc::events::{ChainEvent, PoolRemoval};
use wrkz_rpc::server::{self, RunningServer, ServerConfig};
use wrkz_rpc::ws::{WsConfig, WsHub};
use wrkz_ws::frame::{close, encode_frame, Message, MessageReader, Opcode, Role};
use wrkz_ws::handshake;

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const MASK: [u8; 4] = [1, 2, 3, 4];

fn start(config: ServerConfig, ws: Option<WsConfig>) -> (RunningServer, Option<WsHub>) {
    let hub = ws.map(WsHub::new);
    let server = server::start_with(
        Arc::new(FakeNode::default()),
        ServerConfig { bind: "127.0.0.1:0".into(), ..config },
        hub.clone(),
    )
    .expect("the listener binds");
    (server, hub)
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    messages: MessageReader,
}

impl Client {
    fn next(&mut self) -> Message {
        self.messages.read(&mut self.reader).expect("a message")
    }

    /// The next text message, answering nothing and skipping pings.
    fn text(&mut self) -> String {
        loop {
            match self.next() {
                Message::Text(t) => return t,
                Message::Ping(_) => {}
                other => panic!("expected text, got {other:?}"),
            }
        }
    }

    fn send(&mut self, opcode: Opcode, payload: &[u8]) {
        self.writer.write_all(&encode_frame(opcode, payload, Some(MASK))).unwrap();
    }
}

/// Upgrade with `extra` headers on `target`; the answer's head, and the
/// connection when it is a `101`.
fn connect(addr: SocketAddr, target: &str, extra: &[(&str, &str)]) -> (handshake::ResponseHead, Option<Client>) {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writer.write_all(handshake::client_request("x", target, KEY, extra).unwrap().as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let head = handshake::read_response_head(&mut reader, 8192).unwrap();
    if head.status != 101 {
        return (head, None);
    }
    handshake::check_response(&head, KEY).expect("a valid 101");
    let client = Client { reader, writer, messages: MessageReader::new(Role::Client, 1 << 20) };
    (head, Some(client))
}

fn subscribe(addr: SocketAddr, target: &str) -> Client {
    let (head, client) = connect(addr, target, &[]);
    client.unwrap_or_else(|| panic!("expected 101, got {}", head.status))
}

fn wait_for_clients(hub: &WsHub, n: usize) {
    for _ in 0..200 {
        if hub.clients() == n {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("expected {n} subscribers, have {}", hub.clients());
}

#[test]
fn without_the_flag_the_path_is_a_404() {
    let (server, _) = start(ServerConfig::default(), None);
    let (head, _) = connect(server.local_addr(), "/ws", &[]);
    assert_eq!(head.status, 404);
}

#[test]
fn the_hello_then_events_as_the_zmq_bodies() {
    let (server, hub) = start(ServerConfig::default(), Some(WsConfig::default()));
    let hub = hub.unwrap();
    let mut client = subscribe(server.local_addr(), "/ws");
    let tip = "c5".repeat(32);
    assert_eq!(
        client.text(),
        format!(
            "{{\"topic\":\"hello\",\"data\":{{\"height\":4213000,\"hash\":\"{tip}\",\"topics\":[\"hashblock\",\
             \"chain_main\",\"hashblock_alt\",\"chainswitch\",\"txpool_add\",\"txpool_del\"]}}}}"
        )
    );
    wait_for_clients(&hub, 1);

    let listener = hub.listener();
    listener.on_event(&ChainEvent::BlockAdded { index: 7, hash: [0xaa; 32], transaction_hashes: vec![[0xbb; 32]] });
    let a = "aa".repeat(32);
    let b = "bb".repeat(32);
    assert_eq!(client.text(), format!("{{\"topic\":\"hashblock\",\"data\":{{\"height\":7,\"hash\":\"{a}\"}}}}"));
    assert_eq!(
        client.text(),
        format!(
            "{{\"topic\":\"chain_main\",\"data\":{{\"height\":7,\"hash\":\"{a}\",\"transaction_hashes\":[\"{b}\"]}}}}"
        )
    );
    listener.on_event(&ChainEvent::PoolRemoved { hashes: vec![[0xbb; 32]], reason: PoolRemoval::InBlock });
    assert_eq!(
        client.text(),
        format!("{{\"topic\":\"txpool_del\",\"data\":{{\"hashes\":[\"{b}\"],\"reason\":\"InBlock\"}}}}")
    );
    assert_eq!(hub.published(), 3);
}

#[test]
fn a_topic_filter_is_honoured_and_a_misspelling_refused() {
    let (server, hub) = start(ServerConfig::default(), Some(WsConfig::default()));
    let hub = hub.unwrap();
    let (head, _) = connect(server.local_addr(), "/ws?topics=hashblok", &[]);
    assert_eq!(head.status, 400);

    let mut client = subscribe(server.local_addr(), "/ws?topics=txpool");
    assert!(client.text().contains("\"topics\":[\"txpool_add\",\"txpool_del\"]"));
    wait_for_clients(&hub, 1);
    let listener = hub.listener();
    listener.on_event(&ChainEvent::BlockAdded { index: 1, hash: [1; 32], transaction_hashes: Vec::new() });
    listener.on_event(&ChainEvent::PoolAdded { hash: [2; 32] });
    // The block was not for this subscriber, so the pool event is next.
    assert!(client.text().starts_with("{\"topic\":\"txpool_add\""));
}

#[test]
fn a_ping_is_answered_and_a_close_echoed() {
    let (server, hub) = start(ServerConfig::default(), Some(WsConfig::default()));
    let hub = hub.unwrap();
    let mut client = subscribe(server.local_addr(), "/ws");
    client.text();
    client.send(Opcode::Ping, b"are you there");
    assert_eq!(client.next(), Message::Pong(b"are you there".to_vec()));
    client.send(Opcode::Close, &wrkz_ws::frame::close_payload(close::NORMAL, "bye"));
    assert_eq!(client.next(), Message::Close(Some(close::NORMAL)));
    wait_for_clients(&hub, 0);
}

#[test]
fn a_quiet_stream_carries_a_ping_and_a_heartbeat() {
    let config = WsConfig { ping_interval: Duration::from_secs(1), ..WsConfig::default() };
    let (server, _) = start(ServerConfig::default(), Some(config));
    let mut client = subscribe(server.local_addr(), "/ws?topics=chainswitch");
    client.text();
    assert_eq!(client.next(), Message::Ping(Vec::new()));
    // Sent whatever the topics.
    assert_eq!(client.text(), "{\"topic\":\"heartbeat\",\"data\":{}}");
}

#[test]
fn an_unmasked_client_frame_is_a_protocol_error() {
    let (server, hub) = start(ServerConfig::default(), Some(WsConfig::default()));
    let hub = hub.unwrap();
    let mut client = subscribe(server.local_addr(), "/ws");
    client.text();
    client.writer.write_all(&encode_frame(Opcode::Text, b"hi", None)).unwrap();
    assert_eq!(client.next(), Message::Close(Some(close::PROTOCOL_ERROR)));
    wait_for_clients(&hub, 0);
}

#[test]
fn the_handshake_refusals() {
    let (server, _) =
        start(ServerConfig { access_token: "secret".into(), ..ServerConfig::default() }, Some(WsConfig::default()));
    let addr = server.local_addr();
    // The token is required as on every route, and accepted as on every route.
    assert_eq!(connect(addr, "/ws", &[]).0.status, 401);
    assert_eq!(connect(addr, "/ws", &[("X-API-Key", "secret")]).0.status, 101);

    // A plain GET, and an old version, are told what is required.
    let plain = {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\nX-API-Key: secret\r\n\r\n").unwrap();
        handshake::read_response_head(&mut BufReader::new(s), 8192).unwrap()
    };
    assert_eq!(plain.status, 426);
    assert_eq!(plain.header("Sec-WebSocket-Version"), Some("13"));
}

#[test]
fn a_browser_origin_needs_cors() {
    let (closed, _) = start(ServerConfig::default(), Some(WsConfig::default()));
    assert_eq!(connect(closed.local_addr(), "/ws", &[("Origin", "https://wallet.example")]).0.status, 403);
    assert_eq!(connect(closed.local_addr(), "/ws", &[]).0.status, 101);

    let (open, _) = start(
        ServerConfig { cors_header: "https://wallet.example".into(), ..ServerConfig::default() },
        Some(WsConfig::default()),
    );
    assert_eq!(connect(open.local_addr(), "/ws", &[("Origin", "https://wallet.example")]).0.status, 101);
    assert_eq!(connect(open.local_addr(), "/ws", &[("Origin", "https://evil.example")]).0.status, 403);
}

#[test]
fn past_the_cap_the_upgrade_is_a_503() {
    let (server, hub) = start(ServerConfig::default(), Some(WsConfig { max_clients: 1, ..WsConfig::default() }));
    let hub = hub.unwrap();
    let _first = subscribe(server.local_addr(), "/ws");
    wait_for_clients(&hub, 1);
    assert_eq!(connect(server.local_addr(), "/ws", &[]).0.status, 503);
}

#[test]
fn stopping_the_server_says_going_away() {
    let (mut server, hub) = start(ServerConfig::default(), Some(WsConfig::default()));
    let hub = hub.unwrap();
    let mut client = subscribe(server.local_addr(), "/ws");
    client.text();
    wait_for_clients(&hub, 1);
    server.stop();
    assert_eq!(client.next(), Message::Close(Some(close::GOING_AWAY)));
    assert_eq!(hub.clients(), 0);
}

#[test]
fn a_subscription_does_not_hold_a_worker() {
    // One worker: were a subscriber to keep it, the plain request after it
    // would never be answered.
    let (server, hub) = start(ServerConfig { workers: 1, ..ServerConfig::default() }, Some(WsConfig::default()));
    let hub = hub.unwrap();
    let mut client = subscribe(server.local_addr(), "/ws");
    client.text();
    wait_for_clients(&hub, 1);
    let mut s = TcpStream::connect(server.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /height HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let head = handshake::read_response_head(&mut BufReader::new(s), 8192).unwrap();
    assert_eq!(head.status, 200);
}
