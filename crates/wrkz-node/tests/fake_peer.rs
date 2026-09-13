// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The sync state machine driven by a fake C++ peer, entirely offline.
//!
//! The fake peer is a real TCP server that speaks Levin with the encoders of
//! `wrkz_p2p::msg`, so what the engine sees on the wire is exactly what a C++
//! daemon would send. Its chain is the mainnet vector
//! `spec/vectors/mainnet_rawblocks_0_to_5.json`, so the blocks the engine adds
//! are real blocks that pass the real rules with the mainnet checkpoints on.
//!
//! Covered here (spec/08 "Sync state machine", "Block relay"):
//!
//! - handshake, then chain request, get-objects and blocks 1-5 applied;
//! - a `NOTIFY_RESPONSE_CHAIN_ENTRY` carrying the full 10,000 ids, and the
//!   first batch sized at `--sync-batch-min`;
//! - a missed id: the peer declares blocks missing, the engine re-requests the
//!   chain instead of dropping it, and finishes the sync on the second pass;
//! - `NOTIFY_NEW_BLOCK` at the tip;
//! - a lite block whose transaction is unknown, the `NOTIFY_MISSING_TXS` round
//!   trip, and the orphan reaction to the reassembled block;
//! - a peer sending garbage is dropped and the others are unaffected;
//! - a peer that stalls is dropped on the idle timeout;
//! - `COMMAND_TIMED_SYNC` in both directions;
//! - the peer state file written and read back.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_node::pool::BoundedTxSet;
use wrkz_node::{Node, NodeConfig, PeerState, TxPool};
use wrkz_p2p::conn::Connection;
use wrkz_p2p::levin::{self, Header};
use wrkz_p2p::msg::{self, BasicNodeData, CoreSyncData, LiteBlock, NewBlock, PeerlistEntry, RawBlockLegacy};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::Hash;
use wrkz_storage::batch::BatchStore;
use wrkz_storage::{KvStore, MemStore};

// ---------------------------------------------------------------------------
// vectors
// ---------------------------------------------------------------------------

fn vectors() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

fn raw_blocks(file: &str) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(vectors().join(file)).unwrap()).unwrap();
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

/// Blocks 0-5 of mainnet, as `(hash, block blob, transaction blobs)`.
fn mainnet_0_to_5() -> Vec<(Hash, Vec<u8>, Vec<Vec<u8>>)> {
    raw_blocks("mainnet_rawblocks_0_to_5.json")
        .into_iter()
        .map(|(blob, txs)| (BlockTemplate::from_bytes(&blob).unwrap().hash().unwrap(), blob, txs))
        .collect()
}

/// The tip vector's block 4,213,650 with its one real transaction.
fn block_with_a_transaction() -> (Hash, Vec<u8>, Vec<Vec<u8>>) {
    let (blob, txs) = raw_blocks("mainnet_rawblocks_4213648_to_4213650_v7.json")
        .into_iter()
        .find(|(_, txs)| !txs.is_empty())
        .expect("the tip vector carries a block with a transaction");
    (BlockTemplate::from_bytes(&blob).unwrap().hash().unwrap(), blob, txs)
}

// ---------------------------------------------------------------------------
// the fake peer
// ---------------------------------------------------------------------------

/// What the fake peer should do, changed by the test between exchanges.
#[derive(Default)]
struct Script {
    /// Extra ids appended to every chain entry, none of which it will serve.
    padding_ids: Vec<Hash>,
    /// Block hashes to declare `missed` instead of sending, once.
    withhold: Vec<Hash>,
    /// Answer nothing at all after the handshake.
    stall: bool,
    /// Send this raw payload as a `NOTIFY_RESPONSE_CHAIN_ENTRY` instead of a
    /// real chain entry.
    garbage_chain_entry: bool,
    /// Transactions the peer will hand over on `NOTIFY_MISSING_TXS`.
    txs: HashMap<Hash, Vec<u8>>,
}

/// A C++ peer, as far as the wire is concerned.
struct FakePeer {
    addr: SocketAddr,
    script: Arc<Mutex<Script>>,
    /// Frames the node sent us, for assertions.
    seen: Receiver<(u32, Vec<u8>)>,
    /// Frames the test wants pushed to the node.
    push: Sender<(u32, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    /// The height the peer last advertised, so a test can raise it.
    advertised: Arc<AtomicU32>,
    /// Every command ever drained, so an assertion does not depend on which
    /// drain happened to catch it. See [`FakePeer::drain`].
    history: Mutex<Vec<u32>>,
}

impl FakePeer {
    /// Serve `chain` (index 0 is genesis) on a loopback port.
    fn start(chain: Vec<(Hash, Vec<u8>, Vec<Vec<u8>>)>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let script = Arc::new(Mutex::new(Script::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let advertised = Arc::new(AtomicU32::new(chain.len() as u32));
        let (seen_tx, seen) = channel();
        let (push, push_rx) = channel::<(u32, Vec<u8>)>();

        let s = script.clone();
        let st = stop.clone();
        let adv = advertised.clone();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            // A long read timeout on purpose: a Levin read that times out
            // mid-frame has consumed bytes it cannot put back, so a reader that
            // retries desynchronises. The C++ node closes the connection
            // instead, and so does ours; this harness simply never trips it.
            let Ok(mut conn) = Connection::from_stream(stream, Duration::from_secs(30)) else { return };
            let Ok(write_half) = conn.try_clone() else { return };
            // One writer, shared between the request handler and the test's
            // pushes, exactly as the node keeps exactly one writer per socket.
            let writer = Arc::new(Mutex::new(write_half));

            let pusher = writer.clone();
            let pst = st.clone();
            std::thread::spawn(move || {
                while !pst.load(Ordering::Relaxed) {
                    match push_rx.recv_timeout(Duration::from_millis(20)) {
                        Ok((command, payload)) => {
                            if pusher.lock().unwrap().notify(command, &payload).is_err() {
                                return;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => return,
                    }
                }
            });

            let mut server = Server { chain, script: s, seen: seen_tx, advertised: adv, writer };
            while !st.load(Ordering::Relaxed) {
                match conn.read_frame() {
                    Ok((header, payload)) => {
                        if server.handle(&header, &payload).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Ok(Self { addr, script, seen, push, stop, advertised, history: Mutex::new(Vec::new()) })
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap()
    }

    /// Queue a notification for the peer to send.
    fn push(&self, command: u32, payload: Vec<u8>) {
        let _ = self.push.send((command, payload));
    }

    /// Every frame received since the last call, drained.
    ///
    /// Destructive, and every caller of it competes for the same frames: two
    /// commands that arrive in one batch are handed to whoever drains first,
    /// and a helper that keeps only the one it wanted throws the other away.
    /// That made several assertions here racy, so every drained command is
    /// also recorded in [`FakePeer::history`], and "did the engine ever send
    /// X" is [`FakePeer::saw`] rather than a second wait.
    fn drain(&self) -> Vec<(u32, Vec<u8>)> {
        let frames: Vec<(u32, Vec<u8>)> = self.seen.try_iter().collect();
        if !frames.is_empty() {
            let mut history = self.history.lock().unwrap_or_else(|p| p.into_inner());
            history.extend(frames.iter().map(|(c, _)| *c));
        }
        frames
    }

    /// True if this command has been drained at any point in the test.
    ///
    /// Drains first, so a frame already on the wire counts even if nothing has
    /// drained since it arrived.
    fn saw(&self, command: u32) -> bool {
        self.drain();
        self.history.lock().unwrap_or_else(|p| p.into_inner()).contains(&command)
    }
}

impl Drop for FakePeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Server {
    chain: Vec<(Hash, Vec<u8>, Vec<Vec<u8>>)>,
    script: Arc<Mutex<Script>>,
    seen: Sender<(u32, Vec<u8>)>,
    advertised: Arc<AtomicU32>,
    writer: Arc<Mutex<Connection>>,
}

impl Server {
    /// A peer that claims a height above the chain it serves has a top block
    /// we cannot possibly hold, which is what makes it look taller to the
    /// engine; a peer at its own chain length reports that chain's top.
    fn sync_data(&self) -> CoreSyncData {
        let height = self.advertised.load(Ordering::Relaxed);
        let top = if height as usize <= self.chain.len() {
            self.chain[height as usize - 1].0
        } else {
            let mut h = [0x5au8; 32];
            h[..4].copy_from_slice(&height.to_le_bytes());
            h
        };
        CoreSyncData { current_height: height, top_id: top, ..Default::default() }
    }

    fn notify(&self, command: u32, payload: &[u8]) -> io::Result<()> {
        self.writer.lock().unwrap().notify(command, payload)
    }

    fn reply(&self, command: u32, code: i32, payload: &[u8]) -> io::Result<()> {
        self.writer.lock().unwrap().reply(command, code, payload)
    }

    fn handle(&mut self, header: &Header, payload: &[u8]) -> io::Result<()> {
        let _ = self.seen.send((header.command, payload.to_vec()));
        if self.script.lock().unwrap().stall && header.command != msg::COMMAND_HANDSHAKE {
            return Ok(());
        }
        match header.command {
            msg::COMMAND_HANDSHAKE => {
                let node = BasicNodeData::ours(0xfeed_face_dead_beef, 17855);
                let peerlist =
                    [PeerlistEntry { ip: [203, 0, 113, 7], port: 17855, id: 42, last_seen: node.local_time - 5 }];
                let body = msg::handshake_response(&node, &self.sync_data(), &peerlist, &[]);
                self.reply(msg::COMMAND_HANDSHAKE, levin::RETCODE_SUCCESS, &body)
            }
            msg::COMMAND_TIMED_SYNC if header.have_to_return_data => {
                let body = msg::timed_sync_response_from(
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
                    &self.sync_data(),
                    &[],
                    &[],
                );
                self.reply(msg::COMMAND_TIMED_SYNC, levin::RETCODE_SUCCESS, &body)
            }
            msg::COMMAND_PING if header.have_to_return_data => {
                self.reply(msg::COMMAND_PING, levin::RETCODE_SUCCESS, &msg::ping_response(0xfeed_face_dead_beef))
            }
            msg::NOTIFY_REQUEST_CHAIN => {
                let ids = msg::parse_request_chain(payload).expect("the node sends a well-formed chain request");
                if self.script.lock().unwrap().garbage_chain_entry {
                    return self.notify(msg::NOTIFY_RESPONSE_CHAIN_ENTRY, b"not a KV document");
                }
                // `findBlockchainSupplement`: the first id we know.
                let start = ids
                    .iter()
                    .find_map(|h| self.chain.iter().position(|(hash, _, _)| hash == h))
                    .expect("the node's sparse chain ends at genesis");
                let mut out: Vec<Hash> = self.chain[start..].iter().map(|(h, _, _)| *h).collect();
                let padding = self.script.lock().unwrap().padding_ids.clone();
                out.extend(padding);
                out.truncate(10_000);
                let total = self.advertised.load(Ordering::Relaxed).max(start as u32 + out.len() as u32);
                let body = msg::chain_entry(start as u32, total, &out);
                self.notify(msg::NOTIFY_RESPONSE_CHAIN_ENTRY, &body)
            }
            msg::NOTIFY_REQUEST_GET_OBJECTS => {
                let wanted = msg::parse_request_get_objects(payload).expect("well-formed get objects");
                let withheld = std::mem::take(&mut self.script.lock().unwrap().withhold);
                let mut blocks = Vec::new();
                let mut missed = Vec::new();
                for hash in &wanted {
                    match self.chain.iter().find(|(h, _, _)| h == hash) {
                        Some((_, blob, txs)) if !withheld.contains(hash) => {
                            blocks.push(RawBlockLegacy { block: blob.clone(), txs: txs.clone() })
                        }
                        _ => missed.push(*hash),
                    }
                }
                let body = msg::get_objects_response(&blocks, &missed, self.advertised.load(Ordering::Relaxed));
                self.notify(msg::NOTIFY_RESPONSE_GET_OBJECTS, &body)
            }
            msg::NOTIFY_MISSING_TXS => {
                let request = msg::parse_missing_txs(payload).expect("well-formed missing txs");
                let script = self.script.lock().unwrap();
                let blobs: Vec<Vec<u8>> =
                    request.missing_txs.iter().filter_map(|h| script.txs.get(h).cloned()).collect();
                drop(script);
                self.notify(msg::NOTIFY_NEW_TRANSACTIONS, &msg::new_transactions(&blobs))
            }
            // NOTIFY_REQUEST_TX_POOL and everything else needs no answer.
            other if header.have_to_return_data => self.reply(other, levin::ERROR_HANDLER_NOT_DEFINED, &[]),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// the node under test
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wrkz-node-test-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_config(name: &str) -> NodeConfig {
    NodeConfig {
        data_dir: temp_dir(name),
        p2p_port: 0,
        listen: false,
        use_default_seeds: false,
        allow_local_ip: true,
        max_outgoing: 4,
        max_incoming: 4,
        tick_interval: Duration::from_millis(20),
        timed_sync_interval: Duration::from_secs(3600),
        // Generous on purpose. Nothing here tests the handshake timeout — the
        // drop tests drive `idle_timeout` — and the fake peer has to be
        // scheduled, accept, read a frame and reply within it. At 500 ms this
        // lost the race about one run in four on a loaded machine, dropping the
        // connection before the sync it was meant to test had begun.
        handshake_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(3600),
        ..Default::default()
    }
}

fn new_node(cfg: NodeConfig) -> Node<MemStore, BoundedTxSet> {
    let mut chain =
        ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
    // The vector blocks are from 2018 and 2026; the future-time-limit rule is
    // judged against the validating node's clock, so pin it above every
    // timestamp in play, exactly as the chain crate's own vector tests do.
    chain.set_clock(Some(1_900_000_000));
    Node::new(chain, BoundedTxSet::new(64, 1 << 20), cfg)
}

/// Step the node until `done` or the budget runs out.
fn run_until<S: KvStore>(
    node: &mut Node<S, BoundedTxSet>,
    budget: Duration,
    mut done: impl FnMut(&Node<S, BoundedTxSet>) -> bool,
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

const BUDGET: Duration = Duration::from_secs(20);

/// Step the node until the fake peer has received `command`, collecting every
/// frame seen on the way. A frame the engine has queued is not on the wire
/// yet, so an assertion on "did it send X" has to wait rather than sample.
fn wait_for_frame(
    node: &mut Node<MemStore, BoundedTxSet>,
    peer: &FakePeer,
    command: u32,
    budget: Duration,
) -> Option<Vec<u8>> {
    let mut found = None;
    run_until(node, budget, |_| {
        for (c, payload) in peer.drain() {
            if c == command && found.is_none() {
                found = Some(payload);
            }
        }
        found.is_some()
    });
    found
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// spec/08 acceptance 2, in miniature: handshake, `NOTIFY_REQUEST_CHAIN`,
/// `NOTIFY_REQUEST_GET_OBJECTS`, and blocks 1-5 added through `ChainState`
/// with the mainnet checkpoints on.
#[test]
fn syncs_blocks_one_to_five_from_a_fake_peer() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    let mut node = new_node(test_config("sync5"));
    node.start().unwrap();
    node.connect_to(peer.addr);

    let mut seen: Vec<u32> = Vec::new();
    assert!(
        run_until(&mut node, BUDGET, |n| {
            seen.extend(peer.drain().into_iter().map(|(c, _)| c));
            n.chain().tip_index() == Some(5)
        }),
        "did not reach height 5"
    );
    assert_eq!(node.chain().tip_info().unwrap().block_hash, chain[5].0);
    assert!(seen.contains(&msg::COMMAND_HANDSHAKE));
    assert!(seen.contains(&msg::NOTIFY_REQUEST_CHAIN));
    assert!(seen.contains(&msg::NOTIFY_REQUEST_GET_OBJECTS));
    // The peer told us its top, so the connection ends on relay duty.
    assert!(run_until(&mut node, BUDGET, |n| n.peer_states().iter().all(|(_, s)| *s == PeerState::Normal)));
    assert!(node.is_synchronized(), "reaching the peer's top declares us synchronized");
    assert_eq!(node.observed_height(), 5);

    // Reaching the peer's top asks for its pool, which is the C++
    // `requestMissingPoolTransactions` on the synchronized transition. It is
    // the last frame of the exchange, so seeing it means the whole sequence
    // (handshake, chain request, get-objects) went out before it.
    //
    // `peer.drain()` is destructive and the loop above drained on every step,
    // so on a machine where the transition happened before we reached height 5
    // this frame is already in `seen` and waiting for a *second* one would hang
    // until the budget ran out. Accept either, which is what "did it send it"
    // actually means.
    assert!(
        seen.contains(&msg::NOTIFY_REQUEST_TX_POOL)
            || wait_for_frame(&mut node, &peer, msg::NOTIFY_REQUEST_TX_POOL, BUDGET).is_some(),
        "the synchronized transition asks for the peer's pool"
    );
}

/// The daemon's store: blocks 1-5 arrive in one get-objects response and reach
/// the engine as one write batch rather than five, and what reached it is the
/// chain — the committed resume height is 5.
#[test]
fn a_downloaded_batch_reaches_the_engine_as_one_write_batch() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    let store = BatchStore::with_limits(MemStore::default(), 1_000, 1 << 30);
    let mut state = ChainState::open_or_genesis(store, Config::default(), Checkpoints::mainnet()).unwrap();
    state.set_clock(Some(1_900_000_000));
    let mut node = Node::new(state, BoundedTxSet::new(64, 1 << 20), test_config("batched"));
    node.start().unwrap();
    node.connect_to(peer.addr);

    // `run_until` checks before it steps, so by the time it sees height 5 the
    // step that applied the batch has returned, and committed.
    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)), "did not reach height 5");
    let state = node.chain();
    let store = state.store();
    assert_eq!(store.pending_bytes(), 0, "the step that applied the batch committed it");
    let stats = store.stats();
    assert_eq!(stats.points, 5, "one block boundary per block applied: {stats:?}");
    assert!(stats.flushes < stats.points, "five blocks, fewer engine batches: {stats:?}");
    let tip = store.base().get(&wrkz_chain::keys::meta(wrkz_chain::keys::META_TIP)).unwrap();
    assert_eq!(tip, Some(5u32.to_le_bytes().to_vec()), "the engine holds the resume height of the whole batch");
}

/// `handle_response_chain_entry` accepts the full
/// `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT`, and the first batch is
/// `--sync-batch-min` blocks (`request_missing_objects`).
#[test]
fn a_ten_thousand_id_chain_entry_is_batched_at_the_c_batch_size() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    // 9,994 ids the peer will never serve, so the entry is exactly 10,000 long.
    peer.script().padding_ids = (0..10_000u32)
        .map(|i| {
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_le_bytes());
            h[31] = 0xaa;
            h
        })
        .collect();
    peer.advertised.store(10_000, Ordering::Relaxed);

    let mut cfg = test_config("tenk");
    cfg.tuning.batch_min = 120;
    cfg.tuning.batch_max = 600;
    let mut node = new_node(cfg);
    node.start().unwrap();
    node.connect_to(peer.addr);

    let payload =
        wait_for_frame(&mut node, &peer, msg::NOTIFY_REQUEST_GET_OBJECTS, BUDGET).expect("the node asked for a batch");
    let request = msg::parse_request_get_objects(&payload).unwrap();
    assert_eq!(request.len(), 120, "the first batch is --sync-batch-min blocks");
    // It asks for the real blocks first, in chain-entry order.
    assert_eq!(&request[..5], &chain[1..6].iter().map(|(h, _, _)| *h).collect::<Vec<_>>()[..]);
    assert_eq!(request[5][31], 0xaa, "then the padding ids, in order");
}

/// The reorganised-peer path of `handle_response_get_objects`: every block the
/// peer failed to send is in `missed_ids`, so it is not misbehaviour. The
/// engine clears its lists, re-requests the chain, and finishes on the second
/// pass instead of dropping the peer.
#[test]
fn a_missed_id_re_requests_the_chain_without_dropping_the_peer() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    // Blocks 3, 4 and 5 are "gone" for the first get-objects only.
    peer.script().withhold = chain[3..].iter().map(|(h, _, _)| *h).collect();

    let mut node = new_node(test_config("missed"));
    node.start().unwrap();
    node.connect_to(peer.addr);

    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)), "did not recover after the missed ids");
    assert_eq!(node.peer_count(), 1, "the peer was kept, not dropped");
    let chain_requests = peer.drain().into_iter().filter(|(c, _)| *c == msg::NOTIFY_REQUEST_CHAIN).count();
    assert!(chain_requests >= 1, "the engine re-requested the chain");
}

/// `handle_notify_new_block` (`:692`): a block relayed at our tip is added and
/// relayed on, but only from a connection in state `normal`.
#[test]
fn a_relayed_block_at_the_tip_is_accepted() {
    let full = mainnet_0_to_5();
    // The peer serves only blocks 0-4 and claims height 5.
    let peer = FakePeer::start(full[..5].to_vec()).unwrap();
    let mut node = new_node(test_config("newblock"));
    node.start().unwrap();
    node.connect_to(peer.addr);

    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(4)));
    assert!(run_until(&mut node, BUDGET, |n| n.peer_states().iter().all(|(_, s)| *s == PeerState::Normal)));

    let nb = NewBlock {
        block: RawBlockLegacy { block: full[5].1.clone(), txs: full[5].2.clone() },
        current_blockchain_height: 6,
        hop: 1,
    };
    peer.advertised.store(6, Ordering::Relaxed);
    peer.push(msg::NOTIFY_NEW_BLOCK, msg::new_block(&nb));

    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)), "the relayed block was not added");
    assert_eq!(node.chain().tip_info().unwrap().block_hash, full[5].0);
    // `relayBlock` sends the lite form to peers at P2P version >= 4, which the
    // fake peer is; it comes back to the sender, exactly as the C++ does.
    assert!(run_until(&mut node, Duration::from_secs(2), |_| peer
        .drain()
        .into_iter()
        .any(|(c, _)| c == msg::NOTIFY_NEW_LITE_BLOCK)));
}

/// `doPushLiteBlock` (`:1155`): a lite block whose transaction we do not hold
/// produces `NOTIFY_MISSING_TXS`; the answer arrives as
/// `NOTIFY_NEW_TRANSACTIONS` and the block is reassembled. The block here is
/// from height 4,213,650, so the reassembled block is rejected as an orphan,
/// which puts the peer back on a chain request rather than dropping it.
#[test]
fn a_lite_block_with_an_unknown_transaction_round_trips() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    let (tip_hash, tip_blob, tip_txs) = block_with_a_transaction();
    let tx_blob = tip_txs[0].clone();
    let tx_hash = wrkz_primitives::tx::Transaction::from_bytes(&tx_blob).unwrap().hash().unwrap();
    peer.script().txs.insert(tx_hash, tx_blob.clone());

    let mut node = new_node(test_config("lite"));
    node.start().unwrap();
    node.connect_to(peer.addr);
    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)));
    assert!(run_until(&mut node, BUDGET, |n| n.peer_states().iter().all(|(_, s)| *s == PeerState::Normal)));
    peer.drain();

    let lb = LiteBlock { current_blockchain_height: 4_213_651, hop: 1, block_template: tip_blob };
    peer.push(msg::NOTIFY_NEW_LITE_BLOCK, msg::lite_block(&lb));

    // 1. the engine asks for the transaction it does not hold
    let payload =
        wait_for_frame(&mut node, &peer, msg::NOTIFY_MISSING_TXS, BUDGET).expect("the engine sent NOTIFY_MISSING_TXS");
    let asked = msg::parse_missing_txs(&payload).unwrap();
    assert_eq!(asked.block_hash, tip_hash);
    assert_eq!(asked.missing_txs, vec![tx_hash]);
    assert_eq!(asked.current_blockchain_height, 4_213_651);

    // 2. the peer answers, the engine reassembles, and the block is an orphan
    //    at our height, so the engine asks for the chain and keeps the peer.
    // `saw` consults everything drained so far, so a chain request that came
    // in the same batch as the NOTIFY_MISSING_TXS above still counts. Waiting
    // for a fresh one raced with that batch and hung until the budget ran out.
    let saw_chain_request =
        run_until(&mut node, BUDGET, |n| n.peer_count() == 0 || peer.saw(msg::NOTIFY_REQUEST_CHAIN));
    assert!(saw_chain_request, "an orphaned lite block makes the engine re-request the chain");
    assert_eq!(node.peer_count(), 1, "an orphaned lite block does not cost the connection");
    // The transaction the peer supplied was only used to rebuild the block; it
    // is not pool-admitted by this path, which is what the C++ does too.
    assert_eq!(node.pool().len(), 0);
}

/// A peer that sends a frame the handlers cannot parse is dropped, and a
/// second peer on the same engine finishes its sync unaffected (spec/08
/// acceptance 4).
#[test]
fn a_garbage_peer_is_dropped_and_the_others_are_unaffected() {
    let chain = mainnet_0_to_5();
    let bad = FakePeer::start(chain.clone()).unwrap();
    bad.script().garbage_chain_entry = true;
    let good = FakePeer::start(chain.clone()).unwrap();

    let mut cfg = test_config("garbage");
    // One sync peer at a time would leave the second on relay duty; allow both
    // to pull so the good one is guaranteed to do the work.
    cfg.tuning.max_peers = 2;
    let mut node = new_node(cfg);
    node.start().unwrap();
    node.connect_to(bad.addr);
    assert!(run_until(&mut node, BUDGET, |n| n.peer_count() == 1));
    node.connect_to(good.addr);

    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)), "the good peer did not finish");
    let addrs: Vec<SocketAddr> = node.peer_states().into_iter().map(|(a, _)| a).collect();
    assert!(!addrs.contains(&bad.addr), "the garbage peer must be gone");
    assert!(addrs.contains(&good.addr), "the good peer must remain");
}

/// A peer that answers the handshake and then goes silent is dropped on the
/// idle timeout (`timeoutLoop`), and only it.
#[test]
fn a_stalling_peer_is_dropped_on_the_idle_timeout() {
    let chain = mainnet_0_to_5();
    let stalled = FakePeer::start(chain.clone()).unwrap();
    stalled.script().stall = true;
    let good = FakePeer::start(chain.clone()).unwrap();

    let mut cfg = test_config("stall");
    // The idle timeout applies to the good peer as well, between its frames.
    // At 300 ms a loaded machine (this file's tests run in parallel) dropped
    // it mid-sync about one suite run in three; 2 s is still a tenth of the
    // budget the stalled peer has to be dropped in.
    cfg.idle_timeout = Duration::from_secs(2);
    cfg.tuning.max_peers = 2;
    let mut node = new_node(cfg);
    node.start().unwrap();
    node.connect_to(stalled.addr);
    node.connect_to(good.addr);

    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)));
    let dropped = run_until(&mut node, BUDGET, |n| !n.peer_states().iter().any(|(a, _)| *a == stalled.addr));
    assert!(dropped, "the stalled peer should have hit the idle timeout");
}

/// `COMMAND_TIMED_SYNC` every interval to a connection in state normal, and its
/// response feeds the sync data back in as *not initial* (spec/08).
#[test]
fn timed_sync_runs_in_both_directions() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    let mut cfg = test_config("timedsync");
    cfg.timed_sync_interval = Duration::from_millis(50);
    let mut node = new_node(cfg);
    node.start().unwrap();
    node.connect_to(peer.addr);
    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)));
    peer.drain();

    // We send it.
    let payload =
        wait_for_frame(&mut node, &peer, msg::COMMAND_TIMED_SYNC, BUDGET).expect("the node sent COMMAND_TIMED_SYNC");
    let ours = msg::parse_timed_sync_request(&payload).unwrap();
    assert_eq!(ours.current_height, 6, "we advertise tip index + 1");
    assert_eq!(ours.top_id, chain[5].0);

    // The peer's answer raises the observed height without a handshake.
    peer.advertised.store(9_999, Ordering::Relaxed);
    assert!(
        run_until(&mut node, BUDGET, |n| n.observed_height() == 9_998),
        "the timed sync response should update the observed height"
    );
    // And it also puts the connection back on sync duty, since a peer that
    // claims 9,999 blocks has a top block we do not hold.
    assert!(run_until(&mut node, BUDGET, |n| n
        .peer_states()
        .iter()
        .any(|(_, s)| *s == PeerState::Synchronizing || *s == PeerState::SyncRequired)));
}

/// The peer state file is written where the C++ node reads it, and reading it
/// back gives the same lists and peer id (spec/08 "Peer state file").
#[test]
fn the_peer_state_file_round_trips_on_disk() {
    use wrkz_node::PeerManager;
    let dir = temp_dir("peerstate");
    let path = dir.join(wrkz_primitives::constants::P2P_NET_DATA_FILENAME);

    let mut pm = PeerManager::open(&path, true, false);
    let peer_id = pm.peer_id();
    pm.set_peer_just_seen(0xabc, "203.0.113.9:17855".parse().unwrap(), 1_700_000_000);
    pm.merge_peerlist(
        &[PeerlistEntry { ip: [198, 51, 100, 4], port: 17855, id: 0xdef, last_seen: 100 }],
        200,
        1_700_000_000,
    );
    pm.save().unwrap();
    assert!(path.exists(), "the file is written under the C++ name");

    let back = PeerManager::open(&path, true, false);
    assert_eq!(back.peer_id(), peer_id);
    assert_eq!(back.white_count(), 1);
    assert_eq!(back.gray_count(), 1);

    // `--p2p-reset-peerstate`: a new peer id and empty lists.
    let reset = PeerManager::open(&path, true, true);
    assert_ne!(reset.peer_id(), peer_id);
    assert_eq!((reset.white_count(), reset.gray_count()), (0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sync peer that never answers its chain request loses the sync slot at the
/// request deadline — the idle timeout is an hour here, so nothing else could
/// take it — and the slot goes to the peer that was waiting on relay duty.
#[test]
fn a_stalled_chain_request_hands_the_sync_to_another_peer() {
    let chain = mainnet_0_to_5();
    let stalled = FakePeer::start(chain.clone()).unwrap();
    stalled.script().stall = true;
    let good = FakePeer::start(chain.clone()).unwrap();

    let mut cfg = test_config("deadline");
    cfg.tuning.max_peers = 1;
    cfg.chain_request_timeout = Duration::from_millis(300);
    let mut node = new_node(cfg);
    node.start().unwrap();
    node.connect_to(stalled.addr);
    assert!(
        run_until(&mut node, BUDGET, |n| n.peer_states().iter().any(|(_, s)| *s == PeerState::Synchronizing)),
        "the first peer takes the one sync slot"
    );
    node.connect_to(good.addr);

    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)), "the waiting peer never got the slot");
    assert!(good.saw(msg::NOTIFY_REQUEST_GET_OBJECTS), "the blocks came from the good peer");
    let states = node.peer_states();
    let stalled_state = states.iter().find(|(a, _)| *a == stalled.addr).map(|(_, s)| *s);
    assert_ne!(stalled_state, Some(PeerState::Synchronizing), "the stalled peer is off sync: {states:?}");
}

/// A `NOTIFY_RESPONSE_CHAIN_ENTRY` nobody asked for is dropped along with the
/// peer that sent it, instead of being appended to the sync list.
#[test]
fn an_unsolicited_chain_entry_drops_the_peer() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    let mut node = new_node(test_config("unsolicited"));
    node.start().unwrap();
    node.connect_to(peer.addr);
    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)));
    assert!(run_until(&mut node, BUDGET, |n| n.peer_states().iter().all(|(_, s)| *s == PeerState::Normal)));

    let ids: Vec<Hash> = chain.iter().map(|(h, _, _)| *h).collect();
    peer.push(msg::NOTIFY_RESPONSE_CHAIN_ENTRY, msg::chain_entry(0, 6, &ids));
    assert!(run_until(&mut node, BUDGET, |n| n.peer_count() == 0), "the unsolicited sender is dropped");
    // loopback is not scored unless the configuration says so
    assert!(!node.ban_list().is_banned(peer.addr.ip()));
}

/// With loopback scored, a message that does not decode bans the address for
/// a day, the ban is written to its own file in the data directory at once,
/// and a new connection to that address is refused.
#[test]
fn a_malformed_message_bans_the_address() {
    let chain = mainnet_0_to_5();
    let peer = FakePeer::start(chain.clone()).unwrap();
    let mut cfg = test_config("malformed-ban");
    cfg.ban_loopback = true;
    let dir = cfg.data_dir.clone();
    let mut node = new_node(cfg);
    node.start().unwrap();
    node.connect_to(peer.addr);
    assert!(run_until(&mut node, BUDGET, |n| n.chain().tip_index() == Some(5)));
    assert!(run_until(&mut node, BUDGET, |n| n.peer_states().iter().all(|(_, s)| *s == PeerState::Normal)));

    peer.push(msg::NOTIFY_NEW_BLOCK, b"not a KV document".to_vec());
    assert!(run_until(&mut node, BUDGET, |n| n.peer_count() == 0), "the sender is dropped");
    let bans = node.ban_list().entries();
    assert_eq!(bans.len(), 1, "{bans:?}");
    assert_eq!(bans[0].0, peer.addr.ip());
    assert!(bans[0].1 > 23 * 3600, "a misbehaviour ban lasts a day: {}s", bans[0].1);
    let file = std::fs::read_to_string(dir.join(wrkz_node::peers::BANS_FILENAME)).expect("the ban file is written");
    assert!(file.starts_with("127.0.0.1 "), "{file}");

    let again = FakePeer::start(chain).unwrap();
    node.connect_to(again.addr);
    run_until(&mut node, Duration::from_secs(2), |n| n.peer_count() != 0);
    assert_eq!(node.peer_count(), 0, "a banned address is not let back in");
}

/// The live acceptance of spec/08 in miniature: sync blocks 1-200 from a real
/// seed node over Levin alone. Needs outbound TCP to port 17855.
///
///     cargo test -p wrkz-node -- --ignored --nocapture
#[test]
#[ignore = "needs outbound TCP to node-fin.wrkz.work:17855"]
fn live_sync_of_the_first_two_hundred_blocks() {
    wrkz_node::log::set_level(wrkz_node::Level::Debug);
    let mut cfg = NodeConfig {
        data_dir: temp_dir("live"),
        p2p_port: 0,
        listen: false,
        seeds: vec!["node-fin.wrkz.work:17855".to_string()],
        use_default_seeds: false,
        max_outgoing: 2,
        sync_to: Some(200),
        exit_when_synced: true,
        ..Default::default()
    };
    cfg.tick_interval = Duration::from_millis(100);
    let mut node = new_node(cfg);
    node.start().unwrap();
    let stopped = node.run_for(Duration::from_secs(180));
    println!("live sync stopped at height {} (top {})", node.height(), hex::encode(node.top_hash()));
    assert!(stopped, "the node did not reach block 200 within the budget");
    assert!(node.chain().tip_index().unwrap_or(0) >= 200);
    // Block 5's hash from the spec/09 header table, so the chain we pulled is
    // the real one and not merely 200 blocks of something.
    assert_eq!(
        hex::encode(node.chain().block_info(5).unwrap().unwrap().block_hash),
        "513e3cbe87ff9ca63ee30197ac358de63ac30216b68359231917c1e10169e1cb"
    );
}
