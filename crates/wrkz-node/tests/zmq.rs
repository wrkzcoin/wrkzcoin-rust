// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The ZMQ publisher over a real socket, from a subscriber written against
//! RFC 37 rather than against the publisher's own code: the greeting, READY,
//! SUBSCRIBE and its ZMTP 3.0 spelling, CANCEL, PING, the frames of a published
//! message, a refused socket type, a subscriber that stops reading — and a
//! block mined through the RPC node reaching a subscriber as `hashblock` and
//! `chain_main`.
//!
//! libzmq itself is exercised by the ignored test at the bottom, through
//! pyzmq and `scripts/zmq-subscribe.py`.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;
use wrkz_chain::{ChainState, Checkpoints};
use wrkz_mempool::TransactionPool;
use wrkz_node::zmq::ZmqPublisher;
use wrkz_primitives::block::BlockTemplate;
use wrkz_rpc::api::SubmitOutcome;
use wrkz_rpc::events::{ChainEvent, Events, PoolRemoval};
use wrkz_rpc::node::{serving_config, ChainNode};
use wrkz_rpc::NodeApi;
use wrkz_storage::MemStore;

const MINER_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

fn start() -> (ZmqPublisher, SocketAddr) {
    let publisher = ZmqPublisher::start("tcp://127.0.0.1:0").expect("binds");
    let addr = publisher.local_addr().expect("a TCP address");
    (publisher, addr)
}

/// Until `count` subscribers are past the handshake and subscribed: a message
/// published before then is, rightly, not theirs.
fn wait_for_subscribers(publisher: &ZmqPublisher, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while publisher.ready_subscribers() < count {
        assert!(Instant::now() < deadline, "{count} subscriber(s) never became ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What a frame from the publisher is.
#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Command { name: String, data: Vec<u8> },
    Data { more: bool, body: Vec<u8> },
}

/// A ZMTP 3.1 SUB socket, NULL mechanism, from the RFC.
struct Subscriber {
    stream: TcpStream,
}

impl Subscriber {
    fn connect(addr: SocketAddr) -> Self {
        Self::connect_as(addr, "SUB").expect("the publisher accepts a SUB socket")
    }

    /// Greet, READY as `socket_type`, and read the publisher's READY. `Err`
    /// carries the frame that came instead.
    fn connect_as(addr: SocketAddr, socket_type: &str) -> Result<Self, Frame> {
        let stream = TcpStream::connect(addr).expect("connects");
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut s = Self { stream };
        let mut greeting = [0u8; 64];
        greeting[0] = 0xff;
        greeting[9] = 0x7f;
        greeting[10] = 3;
        greeting[11] = 1;
        greeting[12..16].copy_from_slice(b"NULL");
        s.stream.write_all(&greeting).unwrap();
        let mut theirs = [0u8; 64];
        s.stream.read_exact(&mut theirs).expect("the publisher's greeting");
        assert_eq!((theirs[0], theirs[9] & 1, theirs[10]), (0xff, 1, 3), "a ZMTP 3 greeting");
        assert_eq!(&theirs[12..16], b"NULL");

        let mut ready = vec![11];
        ready.extend_from_slice(b"Socket-Type");
        ready.extend_from_slice(&(socket_type.len() as u32).to_be_bytes());
        ready.extend_from_slice(socket_type.as_bytes());
        s.command("READY", &ready);
        match s.frame() {
            Some(Frame::Command { name, data }) if name == "READY" => {
                let want = [&[11][..], b"Socket-Type", &[0, 0, 0, 3], b"PUB"].concat();
                assert_eq!(data, want, "the publisher announces a PUB socket");
                Ok(s)
            }
            Some(other) => Err(other),
            None => Err(Frame::Data { more: false, body: b"(closed)".to_vec() }),
        }
    }

    fn command(&mut self, name: &str, data: &[u8]) {
        let mut body = vec![name.len() as u8];
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(data);
        let mut frame = vec![0x04, body.len() as u8];
        frame.extend_from_slice(&body);
        self.stream.write_all(&frame).unwrap();
    }

    fn subscribe(&mut self, prefix: &str) {
        self.command("SUBSCRIBE", prefix.as_bytes());
    }

    /// The ZMTP 3.0 subscription: a one-frame message, 1 then the prefix.
    fn subscribe_as_zmtp_3_0(&mut self, prefix: &str) {
        let mut frame = vec![0x00, (prefix.len() + 1) as u8, 1];
        frame.extend_from_slice(prefix.as_bytes());
        self.stream.write_all(&frame).unwrap();
    }

    /// The next frame, or `None` once the connection is closed or quiet for
    /// the read timeout.
    fn frame(&mut self) -> Option<Frame> {
        let mut head = [0u8; 2];
        match self.stream.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return None,
            Err(_) => return None,
        }
        let size = if head[0] & 0x02 != 0 {
            let mut rest = [0u8; 7];
            self.stream.read_exact(&mut rest).ok()?;
            let mut size = [0u8; 8];
            size[0] = head[1];
            size[1..].copy_from_slice(&rest);
            u64::from_be_bytes(size) as usize
        } else {
            usize::from(head[1])
        };
        let mut body = vec![0u8; size];
        self.stream.read_exact(&mut body).ok()?;
        if head[0] & 0x04 != 0 {
            let len = usize::from(body[0]);
            let name = String::from_utf8(body[1..1 + len].to_vec()).unwrap();
            Some(Frame::Command { name, data: body[1 + len..].to_vec() })
        } else {
            Some(Frame::Data { more: head[0] & 0x01 != 0, body })
        }
    }

    /// The next published message: the topic and the JSON body.
    fn message(&mut self) -> Option<(String, Value)> {
        let Frame::Data { more: true, body: topic } = self.frame()? else { panic!("a topic frame with MORE set") };
        let Frame::Data { more: false, body } = self.frame()? else { panic!("a last body frame") };
        let body = serde_json::from_slice(&body).unwrap_or_else(|e| panic!("not JSON ({e})"));
        Some((String::from_utf8(topic).unwrap(), body))
    }

    fn quiet_for(&mut self, d: Duration) -> bool {
        self.stream.set_read_timeout(Some(d)).unwrap();
        let quiet = self.frame().is_none();
        self.stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        quiet
    }
}

fn block_added(index: u32) -> ChainEvent {
    ChainEvent::BlockAdded { index, hash: [index as u8; 32], transaction_hashes: vec![[0xcc; 32]] }
}

#[test]
fn a_subscriber_gets_the_topics_it_asked_for_and_nothing_else() {
    let (publisher, addr) = start();
    let mut blocks = Subscriber::connect(addr);
    blocks.subscribe("hashblock");
    let mut pool = Subscriber::connect(addr);
    pool.subscribe_as_zmtp_3_0("txpool");
    wait_for_subscribers(&publisher, 2);

    let listener = publisher.listener();
    listener.on_event(&block_added(5));
    listener.on_event(&ChainEvent::AlternativeBlockAdded { index: 6, hash: [6; 32] });
    listener.on_event(&ChainEvent::PoolAdded { hash: [0xaa; 32] });
    listener.on_event(&ChainEvent::PoolRemoved { hashes: vec![[0xaa; 32]], reason: PoolRemoval::InBlock });

    // A prefix match, as libzmq's: `hashblock` brings `hashblock_alt`, and
    // `chain_main` stays away.
    let (topic, body) = blocks.message().expect("hashblock");
    assert_eq!(topic, "hashblock");
    assert_eq!(body, serde_json::json!({ "height": 5, "hash": "05".repeat(32) }));
    assert_eq!(blocks.message().expect("hashblock_alt").0, "hashblock_alt");
    assert!(blocks.quiet_for(Duration::from_millis(300)), "nothing else for this one");

    let (topic, body) = pool.message().expect("txpool_add");
    assert_eq!(topic, "txpool_add");
    assert_eq!(body, serde_json::json!({ "hashes": ["aa".repeat(32)] }));
    let (topic, body) = pool.message().expect("txpool_del");
    assert_eq!(topic, "txpool_del");
    assert_eq!(body["reason"], "InBlock");
    assert!(pool.quiet_for(Duration::from_millis(300)));
}

#[test]
fn ping_is_answered_and_cancel_ends_a_subscription() {
    let (publisher, addr) = start();
    let mut sub = Subscriber::connect(addr);
    sub.subscribe("");
    wait_for_subscribers(&publisher, 1);

    // A TTL, then context the PONG must echo.
    sub.command("PING", &[0, 100, b'h', b'i']);
    assert_eq!(sub.frame(), Some(Frame::Command { name: "PONG".to_string(), data: b"hi".to_vec() }));

    sub.command("CANCEL", b"");
    let deadline = Instant::now() + Duration::from_secs(10);
    while publisher.ready_subscribers() != 0 {
        assert!(Instant::now() < deadline, "the cancel never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
    publisher.listener().on_event(&block_added(1));
    assert!(sub.quiet_for(Duration::from_millis(300)), "no subscription, no messages");
}

/// With the NULL mechanism each side sends its READY without waiting for the
/// other's (RFC 37), so a socket of the wrong type reads the publisher's READY
/// first; the refusal — an ERROR naming the type, then the close — follows.
#[test]
fn a_socket_that_is_not_a_subscriber_is_refused_with_a_reason() {
    let (publisher, addr) = start();
    let mut req = Subscriber::connect_as(addr, "REQ").expect("the publisher's own READY comes first");
    match req.frame() {
        Some(Frame::Command { name, data }) => {
            assert_eq!(name, "ERROR");
            let reason = String::from_utf8_lossy(&data[1..]).into_owned();
            assert!(reason.contains("REQ"), "{reason}");
        }
        other => panic!("expected ERROR, got {other:?}"),
    }
    assert_eq!(req.frame(), None, "and then the connection is closed");
    assert_eq!(publisher.ready_subscribers(), 0);
}

#[test]
fn a_subscriber_that_stops_reading_loses_messages_and_holds_nothing_up() {
    let (publisher, addr) = start();
    let mut stalled = Subscriber::connect(addr);
    stalled.subscribe("");
    wait_for_subscribers(&publisher, 1);

    let listener = publisher.listener();
    let began = Instant::now();
    for index in 0..20_000 {
        listener.on_event(&block_added(index));
    }
    assert!(began.elapsed() < Duration::from_secs(10), "publishing waited on a subscriber: {:?}", began.elapsed());
    assert_eq!(publisher.published(), 40_000, "hashblock and chain_main for each block");
    assert!(publisher.dropped() > 0, "the stalled subscriber's queue overflowed");
    // It is still connected, and still gets what fits.
    assert!(matches!(stalled.message(), Some((topic, _)) if topic == "hashblock"));
}

#[test]
fn a_block_mined_through_the_rpc_node_is_published() {
    let (publisher, addr) = start();
    let chain =
        ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).expect("genesis");
    let node = ChainNode::standalone(chain, TransactionPool::new(Default::default()))
        .with_events(Events::new(vec![publisher.listener()]));
    let mut sub = Subscriber::connect(addr);
    sub.subscribe("");
    wait_for_subscribers(&publisher, 1);

    // Block 1 is version 1 at difficulty 1: the template as it comes is a
    // valid block.
    let answer = node.block_template(MINER_ADDRESS, &[0; 8]).unwrap().unwrap();
    let block = BlockTemplate::from_bytes(&answer.blob).unwrap();
    assert!(matches!(node.submit_block(&answer.blob).unwrap(), SubmitOutcome::Added { relay: true }));
    let hash = hex::encode(block.hash().unwrap());
    let coinbase = hex::encode(block.base_transaction.hash().unwrap());

    let (topic, body) = sub.message().expect("hashblock");
    assert_eq!(topic, "hashblock");
    assert_eq!(body, serde_json::json!({ "height": 1, "hash": hash }));
    let (topic, body) = sub.message().expect("chain_main");
    assert_eq!(topic, "chain_main");
    assert_eq!(body, serde_json::json!({ "height": 1, "hash": hash, "transaction_hashes": [coinbase] }));
}

/// libzmq, through pyzmq: a real SUB socket subscribing to this publisher.
///
///     pip install pyzmq
///     WRKZ_ZMQ_PYTHON=python3 cargo test -p wrkz-node --test zmq -- --ignored
///
/// `WRKZ_ZMQ_PYTHON` names the interpreter; pyzmq may come from `PYTHONPATH`.
#[test]
#[ignore = "needs Python with pyzmq: set WRKZ_ZMQ_PYTHON"]
fn libzmq_subscribes_to_this_publisher() {
    let python = std::env::var("WRKZ_ZMQ_PYTHON").expect("WRKZ_ZMQ_PYTHON names a Python with pyzmq");
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/zmq-subscribe.py");
    let (publisher, addr) = start();
    let child = std::process::Command::new(python)
        .arg(script)
        .args(["--topic", "hashblock", "--topic", "chain_main", "--count", "3", "--timeout", "30"])
        .arg(format!("tcp://{addr}"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python starts");
    wait_for_subscribers(&publisher, 1);
    let listener = publisher.listener();
    listener.on_event(&ChainEvent::PoolAdded { hash: [1; 32] });
    listener.on_event(&block_added(9));
    listener.on_event(&ChainEvent::AlternativeBlockAdded { index: 9, hash: [2; 32] });

    let output = child.wait_with_output().expect("python finishes");
    assert!(output.status.success(), "the subscriber exited with {}", output.status);
    let lines: Vec<String> = String::from_utf8(output.stdout).unwrap().lines().map(str::to_string).collect();
    let hash = "09".repeat(32);
    assert_eq!(
        lines,
        [
            format!("hashblock {{\"height\":9,\"hash\":\"{hash}\"}}"),
            format!("chain_main {{\"height\":9,\"hash\":\"{hash}\",\"transaction_hashes\":[\"{}\"]}}", "cc".repeat(32)),
            format!("hashblock_alt {{\"height\":9,\"hash\":\"{}\"}}", "02".repeat(32)),
        ]
    );
}
