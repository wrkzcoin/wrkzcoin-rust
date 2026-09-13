// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `--add-exclusive-node` and `--add-priority-node` against real listeners on
//! loopback. A pinned node is dialled whatever `--out-peers` says, its
//! connection is kept, and it is dialled again whenever that connection ends
//! (`connect_to_peerlist`, `NetNode.cpp:2851-2862`).

use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_node::node::PinnedNode;
use wrkz_node::pool::BoundedTxSet;
use wrkz_node::{Node, NodeConfig};
use wrkz_p2p::conn::Connection;
use wrkz_p2p::levin;
use wrkz_p2p::msg::{self, BasicNodeData, CoreSyncData};
use wrkz_primitives::Hash;
use wrkz_storage::MemStore;

const BUDGET: Duration = Duration::from_secs(20);

/// A peer that answers the handshake, and nothing else, counting handshakes.
struct Listener {
    addr: SocketAddr,
    handshakes: Arc<AtomicUsize>,
}

impl Listener {
    /// `hang_up` closes every connection shortly after its handshake.
    fn start(top: Hash, hang_up: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().unwrap();
        let handshakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&handshakes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    let Ok(mut conn) = Connection::from_stream(stream, Duration::from_secs(30)) else { return };
                    while let Ok((header, _)) = conn.read_frame() {
                        if header.command == msg::COMMAND_HANDSHAKE {
                            let node = BasicNodeData::ours(0x5eed_0000_0000_0001, 0);
                            let sync = CoreSyncData { current_height: 1, top_id: top, ..Default::default() };
                            let body = msg::handshake_response(&node, &sync, &[], &[]);
                            if conn.reply(msg::COMMAND_HANDSHAKE, levin::RETCODE_SUCCESS, &body).is_err() {
                                return;
                            }
                            counter.fetch_add(1, Ordering::SeqCst);
                            if hang_up {
                                // Long enough for the engine to have read the
                                // answer before the connection goes.
                                std::thread::sleep(Duration::from_millis(300));
                                conn.shutdown();
                                return;
                            }
                        } else if header.have_to_return_data {
                            let _ = conn.reply(header.command, levin::ERROR_HANDLER_NOT_DEFINED, &[]);
                        }
                    }
                });
            }
        });
        Self { addr, handshakes }
    }

    fn handshakes(&self) -> usize {
        self.handshakes.load(Ordering::SeqCst)
    }
}

fn genesis_state() -> ChainState<MemStore> {
    ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).expect("genesis")
}

fn genesis_hash() -> Hash {
    genesis_state().tip_info().map(|i| i.block_hash).unwrap_or_default()
}

/// A node that listens nowhere, knows no seeds, and would dial nobody from its
/// lists: `--out-peers 0`.
fn engine(name: &str, with: impl FnOnce(NodeConfig) -> NodeConfig) -> Node<MemStore, BoundedTxSet> {
    let dir = std::env::temp_dir().join(format!("wrkz-pinned-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = NodeConfig {
        data_dir: dir,
        p2p_port: 0,
        listen: false,
        use_default_seeds: false,
        max_outgoing: 0,
        tick_interval: Duration::from_millis(20),
        timed_sync_interval: Duration::from_secs(3600),
        handshake_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(3600),
        ..Default::default()
    };
    Node::new(genesis_state(), BoundedTxSet::new(64, 1 << 20), with(cfg))
}

fn run_for(
    node: &mut Node<MemStore, BoundedTxSet>,
    budget: Duration,
    mut done: impl FnMut(&Node<MemStore, BoundedTxSet>) -> bool,
) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if done(node) {
            return true;
        }
        node.step(Duration::from_millis(20));
    }
    done(node)
}

#[test]
fn an_exclusive_node_is_dialled_past_out_peers_and_kept() {
    let peer = Listener::start(genesis_hash(), false);
    let mut node = engine("exclusive", |c| NodeConfig { exclusive_nodes: vec![PinnedNode::at(peer.addr)], ..c });
    node.start().unwrap();
    assert!(run_for(&mut node, BUDGET, |n| n.connection_counts().1 == 1), "never connected");
    assert_eq!(peer.handshakes(), 1);

    // Several rounds later it is still the one connection: a node that is
    // connected is not dialled again.
    run_for(&mut node, Duration::from_millis(2500), |_| false);
    assert_eq!(node.connection_counts().1, 1);
    assert_eq!(peer.handshakes(), 1);
}

#[test]
fn a_priority_node_is_dialled_again_whenever_its_connection_ends() {
    let peer = Listener::start(genesis_hash(), true);
    let mut node = engine("priority", |c| NodeConfig { priority_nodes: vec![PinnedNode::at(peer.addr)], ..c });
    node.start().unwrap();
    assert!(run_for(&mut node, BUDGET, |_| peer.handshakes() >= 3), "dialled {} times", peer.handshakes());
}
