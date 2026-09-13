// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The serving side: an inbound peer handshakes with us, passes the back ping,
//! pulls the chain, and stays in state `normal` (spec/08 acceptance 1 and 3,
//! offline).
//!
//! The client here is what a C++ `NodeServer` does on an outgoing connection:
//! invoke `COMMAND_HANDSHAKE`, answer the back ping on the port it advertised,
//! then `NOTIFY_REQUEST_CHAIN` and `NOTIFY_REQUEST_GET_OBJECTS`. Everything
//! runs on loopback with no network.

use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;
use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_node::pool::BoundedTxSet;
use wrkz_node::{Node, NodeConfig, PeerState};
use wrkz_p2p::conn::{self, Connection};
use wrkz_p2p::levin;
use wrkz_p2p::msg::{self, BasicNodeData, CoreSyncData};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::{CRYPTONOTE_NETWORK, P2P_CURRENT_VERSION, P2P_MINIMUM_VERSION};
use wrkz_primitives::Hash;
use wrkz_storage::MemStore;

fn raw_blocks() -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/mainnet_rawblocks_0_to_5.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                hex::decode(item["block"].as_str().unwrap()).unwrap(),
                item["transactions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| hex::decode(t.as_str().unwrap()).unwrap())
                    .collect(),
            )
        })
        .collect()
}

/// A node already holding mainnet blocks 0-5, listening on a loopback port.
fn node_at_height_five() -> Node<MemStore, BoundedTxSet> {
    let mut chain =
        ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
    chain.set_clock(Some(1_900_000_000));
    for (blob, txs) in raw_blocks().into_iter().skip(1) {
        chain.add_block(&blob, &txs).expect("the vector blocks apply");
    }
    assert_eq!(chain.tip_index(), Some(5));

    let dir = std::env::temp_dir().join(format!("wrkz-node-serving-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = NodeConfig {
        data_dir: dir,
        p2p_port: 0,
        bind: "127.0.0.1".parse().unwrap(),
        listen: true,
        use_default_seeds: false,
        allow_local_ip: true,
        max_outgoing: 0, // never dial; this test is about being dialled
        tick_interval: Duration::from_millis(20),
        ..Default::default()
    };
    Node::new(chain, BoundedTxSet::new(16, 1 << 20), cfg)
}

/// The scripted C++ side, run on its own thread so the node can be stepped in
/// the test thread. Returns what it observed.
struct Observed {
    handshake: msg::HandshakeResponse,
    back_ping_peer_id: Option<u64>,
    chain_entry: msg::ChainEntry,
    blocks: Vec<Vec<u8>>,
    timed_sync_height: u32,
}

/// The client runs on its own thread and holds its connection open until the
/// returned `Sender` is dropped, so the test can assert on the *live*
/// connection's state rather than on one the node has already reaped.
fn spawn_client(node_addr: SocketAddr, genesis: Hash) -> (Receiver<Result<Observed, String>>, Sender<()>) {
    let (tx, rx) = channel();
    let (keep_tx, keep_rx) = channel::<()>();
    std::thread::spawn(move || {
        let (outcome, conn) = match client(node_addr, genesis) {
            Ok((observed, conn)) => (Ok(observed), Some(conn)),
            Err(e) => (Err(e), None),
        };
        let _ = tx.send(outcome);
        // Blocks until the test drops its Sender.
        let _ = keep_rx.recv();
        drop(conn);
    });
    (rx, keep_tx)
}

fn client(node_addr: SocketAddr, genesis: Hash) -> Result<(Observed, Connection), String> {
    let our_peer_id: u64 = 0x1234_5678_9abc_def0;
    // The back ping dials the port we advertise, so it has to be a real one.
    let ping_listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let my_port = ping_listener.local_addr().map_err(|e| e.to_string())?.port();
    let (ping_tx, ping_rx) = channel::<u64>();
    std::thread::spawn(move || {
        let Ok((stream, _)) = ping_listener.accept() else { return };
        let Ok(mut c) = Connection::from_stream(stream, Duration::from_secs(5)) else { return };
        if let Ok((header, _)) = c.read_frame() {
            if header.command == msg::COMMAND_PING {
                let _ = c.reply(msg::COMMAND_PING, levin::RETCODE_SUCCESS, &msg::ping_response(our_peer_id));
                let _ = ping_tx.send(our_peer_id);
            }
        }
    });

    let mut conn = Connection::connect(node_addr, conn::CONNECT_TIMEOUT, Duration::from_secs(10))
        .map_err(|e| format!("connect: {e}"))?;

    // --- COMMAND_HANDSHAKE ---
    let node_data = BasicNodeData {
        network_id: CRYPTONOTE_NETWORK,
        version: P2P_CURRENT_VERSION,
        peer_id: our_peer_id,
        local_time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        my_port: my_port as u32,
    };
    let our_sync = CoreSyncData { current_height: 1, top_id: genesis, ..Default::default() };
    let (header, body) = conn
        .invoke(msg::COMMAND_HANDSHAKE, &msg::handshake_request(&node_data, &our_sync), Duration::from_secs(10))
        .map_err(|e| format!("handshake: {e}"))?;
    if header.return_code != levin::RETCODE_SUCCESS {
        return Err(format!("handshake return code {}", header.return_code));
    }
    let handshake = msg::parse_handshake_response(&body).map_err(|e| format!("handshake response: {e}"))?;
    if handshake.node_data.network_id != CRYPTONOTE_NETWORK {
        return Err("wrong network id".to_string());
    }
    if handshake.node_data.version < P2P_MINIMUM_VERSION {
        return Err(format!("version {}", handshake.node_data.version));
    }
    // The back ping should arrive within its 10 s budget.
    let back_ping_peer_id = ping_rx.recv_timeout(Duration::from_secs(10)).ok();

    // --- NOTIFY_REQUEST_CHAIN ---
    conn.notify(msg::NOTIFY_REQUEST_CHAIN, &msg::request_chain(&[genesis])).map_err(|e| e.to_string())?;
    let chain_entry = msg::parse_chain_entry(&wait(&mut conn, msg::NOTIFY_RESPONSE_CHAIN_ENTRY, our_peer_id)?)
        .map_err(|e| format!("chain entry: {e}"))?;

    // --- NOTIFY_REQUEST_GET_OBJECTS ---
    let want: Vec<Hash> = chain_entry.block_ids[1..].to_vec();
    conn.notify(msg::NOTIFY_REQUEST_GET_OBJECTS, &msg::request_get_objects(&want)).map_err(|e| e.to_string())?;
    let objects = msg::parse_get_objects_response(&wait(&mut conn, msg::NOTIFY_RESPONSE_GET_OBJECTS, our_peer_id)?)
        .map_err(|e| format!("get objects: {e}"))?;
    if !objects.missed_ids.is_empty() {
        return Err(format!("{} ids missed", objects.missed_ids.len()));
    }

    // --- COMMAND_TIMED_SYNC ---
    let (header, body) = conn
        .invoke(msg::COMMAND_TIMED_SYNC, &msg::timed_sync_request(&our_sync), Duration::from_secs(10))
        .map_err(|e| format!("timed sync: {e}"))?;
    if header.return_code != levin::RETCODE_SUCCESS {
        return Err(format!("timed sync return code {}", header.return_code));
    }
    let timed = msg::parse_timed_sync_response(&body).map_err(|e| format!("timed sync response: {e}"))?;

    Ok((
        Observed {
            handshake,
            back_ping_peer_id,
            chain_entry,
            blocks: objects.blocks.into_iter().map(|b| b.block).collect(),
            timed_sync_height: timed.payload_data.current_height,
        },
        conn,
    ))
}

/// Read frames until `command` arrives, answering the node's own requests the
/// way a C++ peer would.
fn wait(conn: &mut Connection, command: u32, our_peer_id: u64) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match conn.read_frame() {
            Ok((header, payload)) => {
                if header.command == command && !header.is_response() {
                    return Ok(payload);
                }
                if header.have_to_return_data {
                    let body = match header.command {
                        msg::COMMAND_PING => msg::ping_response(our_peer_id),
                        _ => Vec::new(),
                    };
                    let code = if header.command == msg::COMMAND_PING {
                        levin::RETCODE_SUCCESS
                    } else {
                        levin::ERROR_HANDLER_NOT_DEFINED
                    };
                    conn.reply(header.command, code, &body).map_err(|e| e.to_string())?;
                }
            }
            // A Levin read that times out has consumed bytes it cannot put
            // back, so retrying desynchronises the stream; the socket timeout
            // here is longer than the deadline, and hitting it is a failure.
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Err(format!("no command {command} in time"))
}

/// A C++ node dials us, handshakes, passes the back ping, downloads blocks 1-5
/// and stays in state `normal`.
#[test]
fn an_inbound_peer_handshakes_back_pings_and_pulls_the_chain() {
    let mut node = node_at_height_five();
    node.start().unwrap();
    let addr = node.listen_addr().expect("the node is listening");
    let genesis = node.chain().block_info(0).unwrap().unwrap().block_hash;
    let expected: Vec<Vec<u8>> = raw_blocks().into_iter().skip(1).map(|(b, _)| b).collect();

    let (result, keep_alive) = spawn_client(addr, genesis);
    let deadline = Instant::now() + Duration::from_secs(60);
    let observed = loop {
        match result.try_recv() {
            Ok(r) => break r.expect("the client exchange succeeded"),
            Err(_) if Instant::now() < deadline => {
                node.step(Duration::from_millis(20));
            }
            Err(e) => panic!("the client never finished: {e}"),
        }
    };

    // The handshake answers with our node data, our sync data and a peer list.
    assert_eq!(observed.handshake.node_data.network_id, CRYPTONOTE_NETWORK);
    assert_eq!(observed.handshake.node_data.version, P2P_CURRENT_VERSION);
    assert_eq!(observed.handshake.node_data.peer_id, node.peer_manager().peer_id());
    assert_eq!(observed.handshake.node_data.my_port as u16, addr.port());
    assert_eq!(observed.handshake.payload_data.current_height, 6);
    assert_eq!(observed.handshake.payload_data.top_id, node.top_hash());
    assert_eq!(observed.handshake.payload_data.capability_flags, 0, "a full node advertises no capability flags");

    // The back ping is what a peer must pass to reach anyone's white list.
    assert_eq!(observed.back_ping_peer_id, Some(0x1234_5678_9abc_def0), "the node did not back ping");

    // `findBlockchainSupplement`: from the common block, inclusive.
    assert_eq!(observed.chain_entry.start_height, 0);
    assert_eq!(observed.chain_entry.total_height, 6);
    assert_eq!(observed.chain_entry.block_ids.len(), 6);
    assert_eq!(observed.chain_entry.block_ids[0], genesis);

    // The blocks come back byte for byte as the vector holds them.
    assert_eq!(observed.blocks, expected);
    for (i, blob) in observed.blocks.iter().enumerate() {
        assert_eq!(BlockTemplate::from_bytes(blob).unwrap().hash().unwrap(), observed.chain_entry.block_ids[i + 1]);
    }

    assert_eq!(observed.timed_sync_height, 6);

    // spec/08 acceptance 3: the connection sits in `normal`. The peer told us
    // it holds only genesis, so `process_payload_sync_data` saw a top block we
    // already have and put it on relay duty.
    let states = node.peer_states();
    assert_eq!(states.len(), 1, "one inbound connection");
    assert_eq!(states[0].1, PeerState::Normal, "a peer we can serve stays normal");
    drop(keep_alive);
}

/// Before its handshake an inbound peer may ping — another node's back ping is
/// exactly that — but anything else closes the connection: here, a chain
/// request, which the C++ would have answered.
#[test]
fn traffic_before_the_handshake_is_refused() {
    let mut node = node_at_height_five();
    node.start().unwrap();
    let addr = node.listen_addr().unwrap();
    let genesis = node.chain().block_info(0).unwrap().unwrap().block_hash;
    let our_peer_id = node.peer_manager().peer_id();

    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<(u64, bool), String> {
            let mut conn =
                Connection::connect(addr, conn::CONNECT_TIMEOUT, Duration::from_secs(10)).map_err(|e| e.to_string())?;
            let (_, body) = conn
                .invoke(msg::COMMAND_PING, &msg::ping_request(), Duration::from_secs(10))
                .map_err(|e| format!("ping: {e}"))?;
            let ping = msg::parse_ping_response(&body).map_err(|e| e.to_string())?;
            conn.notify(msg::NOTIFY_REQUEST_CHAIN, &msg::request_chain(&[genesis])).map_err(|e| e.to_string())?;
            // The node closes the connection instead of answering.
            let closed = loop {
                match conn.read_frame() {
                    Ok((h, _)) if h.command == msg::NOTIFY_RESPONSE_CHAIN_ENTRY => break false,
                    Ok(_) => continue,
                    Err(_) => break true,
                }
            };
            Ok((ping.peer_id, closed))
        })();
        let _ = tx.send(outcome);
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let (ping_id, closed) = loop {
        match rx.try_recv() {
            Ok(r) => break r.expect("the client ran"),
            Err(_) if Instant::now() < deadline => {
                node.step(Duration::from_millis(20));
            }
            Err(e) => panic!("client never finished: {e}"),
        }
    };
    assert_eq!(ping_id, our_peer_id, "a ping before the handshake is answered");
    assert!(closed, "a chain request before the handshake closes the connection");
}

/// A peer that handshakes with the wrong network id is refused, and the
/// listener survives it (spec/08: the network id is the only "password").
#[test]
fn a_wrong_network_id_is_refused() {
    let mut node = node_at_height_five();
    node.start().unwrap();
    let addr = node.listen_addr().unwrap();

    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<i32, String> {
            let mut conn =
                Connection::connect(addr, conn::CONNECT_TIMEOUT, Duration::from_secs(5)).map_err(|e| e.to_string())?;
            let node_data = BasicNodeData {
                network_id: [0xff; 16],
                version: P2P_CURRENT_VERSION,
                peer_id: 1,
                local_time: 0,
                my_port: 0,
            };
            let sync = CoreSyncData { current_height: 1, ..Default::default() };
            conn.send(
                &wrkz_p2p::levin::Header::request(msg::COMMAND_HANDSHAKE, true),
                &msg::handshake_request(&node_data, &sync),
            )
            .map_err(|e| e.to_string())?;
            // The node closes the connection instead of answering.
            match conn.read_frame() {
                Ok((header, _)) => Ok(header.return_code),
                Err(_) => Ok(0),
            }
        })();
        let _ = tx.send(outcome);
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rx.try_recv() {
            Ok(r) => {
                r.expect("the client ran");
                break;
            }
            Err(_) if Instant::now() < deadline => {
                node.step(Duration::from_millis(20));
            }
            Err(e) => panic!("client never finished: {e}"),
        }
    }
    // The connection is gone and the node is still serving.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !node.peer_states().is_empty() && Instant::now() < deadline {
        node.step(Duration::from_millis(20));
    }
    assert!(node.peer_states().is_empty(), "the wrong-network peer must be dropped");
    assert_eq!(node.chain().tip_index(), Some(5), "the chain is untouched");
}
