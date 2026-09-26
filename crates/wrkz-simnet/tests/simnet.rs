// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Whole simnet nodes in one process: relay, catching up after a partition,
//! a reorganisation to the longer side, the WebSocket stream, a wallet that
//! finds its reward and is woken by the stream, and the network id that keeps
//! a mainnet node out.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_mempool::TransactionPool;
use wrkz_node::mempool::SharedMempool;
use wrkz_node::node::PinnedNode;
use wrkz_node::{Node, NodeConfig};
use wrkz_simnet::{NodeOptions, SimKeys, Simnet};
use wrkz_storage::MemStore;
use wrkz_wallet::daemon::Daemon;
use wrkz_wallet::file::{SecretKey, Wallet};
use wrkz_wallet::sync::{SyncStep, Synchronizer};
use wrkz_wallet::tip_watch::TipWatch;
use wrkz_ws::frame::{Message, MessageReader, Role};
use wrkz_ws::handshake;

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn a_block_mined_on_one_node_reaches_every_node() {
    let net = Simnet::builder().nodes(3).line().build().unwrap();
    assert!(net.wait_for_connections(1, WAIT), "the line connects");
    let miner = SimKeys::random();
    let mined = net.node(0).mine_many(&miner.address, 5).unwrap();
    let (height, top) = net.wait_for_agreement(&[0, 1, 2], WAIT).unwrap();
    assert_eq!(height, 5);
    assert_eq!(top, *mined.last().unwrap());
}

#[test]
fn a_node_cut_off_catches_up_once_healed() {
    let net = Simnet::builder().nodes(3).line().build().unwrap();
    assert!(net.wait_for_connections(1, WAIT));
    let miner = SimKeys::random();
    net.node(0).mine_many(&miner.address, 3).unwrap();
    net.wait_for_agreement(&[0, 1, 2], WAIT).unwrap();

    net.cut(1, 2);
    net.node(0).mine_many(&miner.address, 40).unwrap();
    net.wait_for_agreement(&[0, 1], WAIT).unwrap();
    assert_eq!(net.node(2).height(), 3, "a cut link carries nothing");

    net.heal(1, 2);
    let (height, _) = net.wait_for_agreement(&[0, 1, 2], WAIT).unwrap();
    assert_eq!(height, 43);
}

#[test]
fn a_healed_partition_reorganises_onto_the_longer_side() {
    let net = Simnet::builder().nodes(2).line().build().unwrap();
    assert!(net.wait_for_connections(1, WAIT));
    let (a, b) = (SimKeys::random(), SimKeys::random());
    net.node(0).mine_many(&a.address, 2).unwrap();
    net.wait_for_agreement(&[0, 1], WAIT).unwrap();

    net.cut(0, 1);
    net.node(0).mine_many(&a.address, 3).unwrap();
    let longer = net.node(1).mine_many(&b.address, 6).unwrap();
    assert_eq!((net.node(0).height(), net.node(1).height()), (5, 8));

    // Node 0's stream should report the switch.
    let mut events = subscribe(&net.node(0).ws_url().unwrap(), "chainswitch");

    net.heal(0, 1);
    let (height, top) = net.wait_for_agreement(&[0, 1], WAIT).unwrap();
    assert_eq!((height, top), (8, *longer.last().unwrap()), "every block difficulty 1: the longer chain wins");
    let switch = events.next_text();
    assert!(switch.starts_with("{\"topic\":\"chainswitch\",\"data\":{\"common_root_height\":2,"), "{switch}");
}

#[test]
fn the_stream_announces_a_block_mined_elsewhere() {
    let net = Simnet::builder().nodes(2).line().build().unwrap();
    assert!(net.wait_for_connections(1, WAIT));
    let mut events = subscribe(&net.node(1).ws_url().unwrap(), "hashblock");
    let mined = net.node(0).mine(&SimKeys::random().address).unwrap();
    let text = events.next_text();
    assert_eq!(
        text,
        format!("{{\"topic\":\"hashblock\",\"data\":{{\"height\":1,\"hash\":\"{}\"}}}}", hex::encode(mined))
    );
}

#[test]
fn a_wallet_finds_its_reward_and_the_stream_wakes_it() {
    let net = Simnet::builder().node(NodeOptions::default()).build().unwrap();
    let node = net.node(0);
    let keys = SimKeys::random();
    // 40 blocks unlock the first reward.
    node.mine_many(&keys.address, 45).unwrap();

    let wallet = Wallet::import_from_keys(
        &SecretKey::from_bytes(keys.spend_secret),
        &SecretKey::from_bytes(keys.view_secret),
        0,
    )
    .unwrap();
    let url = node.rpc_url().unwrap();
    let mut sync = Synchronizer::new(Daemon::new(&url).unwrap(), wallet);
    sync.refresh_info().unwrap();
    assert!(matches!(sync.sync_until_synced(1000), SyncStep::Synced { .. }));
    let (unlocked, locked) = sync.wallet().balance(sync.daemon_state().network_block_count);
    assert!(unlocked > 0, "the first rewards are past the unlock window");
    assert!(locked > 0, "the last forty are not");
    let transactions = sync.wallet().transactions().len();
    assert_eq!(transactions, 45, "one coinbase per block");

    // Synced; now the stream says when there is more.
    let watch = TipWatch::start();
    watch.follow(Some(&url));
    let never = AtomicBool::new(false);
    assert!(net.wait_until(WAIT, |_| watch.is_live() || watch.events() > 0));
    let seen = watch.events();
    node.mine(&keys.address).unwrap();
    assert!(watch.wait(WAIT, seen, &never), "the block wakes the wallet");
    sync.refresh_info().unwrap();
    assert!(matches!(sync.sync_until_synced(100), SyncStep::Synced { .. }));
    assert_eq!(sync.wallet().transactions().len(), 46);
}

#[test]
fn a_mainnet_node_is_turned_away() {
    let net = Simnet::builder().nodes(1).build().unwrap();
    let simnet = net.node(0);

    // A mainnet node, dialling the simnet node directly.
    let chain = ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::none()).unwrap();
    let chain = Arc::new(RwLock::new(chain));
    let pool = Arc::new(Mutex::new(TransactionPool::new(Default::default())));
    let cfg = NodeConfig {
        data_dir: net.data_dir().join("mainnet"),
        p2p_port: 0,
        bind: "127.0.0.1".parse().unwrap(),
        use_default_seeds: false,
        exclusive_nodes: vec![PinnedNode::at(simnet.p2p_addr())],
        allow_local_ip: true,
        ..NodeConfig::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let mut node = Node::with_shared_chain(Arc::clone(&chain), SharedMempool::new(pool, chain), cfg);
    node.start().unwrap();
    node.run_for(Duration::from_secs(3));
    assert_eq!(node.peer_count(), 0, "a simnet node is not a mainnet peer");
    assert_eq!(simnet.connections(), 0, "and the simnet node kept no connection from it");
    node.shutdown();
}

/// A raw WebSocket subscription.
struct Events {
    reader: BufReader<TcpStream>,
    messages: MessageReader,
}

impl Events {
    /// The next message that is not the hello, a heartbeat or a ping.
    fn next_text(&mut self) -> String {
        loop {
            match self.messages.read(&mut self.reader).expect("a message") {
                Message::Text(t) if t.starts_with("{\"topic\":\"hello\"") || t.contains("\"heartbeat\"") => {}
                Message::Text(t) => return t,
                _ => {}
            }
        }
    }
}

fn subscribe(ws_url: &str, topics: &str) -> Events {
    let host = ws_url.strip_prefix("ws://").and_then(|r| r.strip_suffix("/ws")).unwrap();
    let stream = TcpStream::connect(host).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let mut writer = stream.try_clone().unwrap();
    let request = handshake::client_request(host, &format!("/ws?topics={topics}"), key, &[]).unwrap();
    writer.write_all(request.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let head = handshake::read_response_head(&mut reader, 8192).unwrap();
    handshake::check_response(&head, key).unwrap();
    // Keep the writing half open for the life of the subscription.
    std::mem::forget(writer);
    Events { reader, messages: MessageReader::new(Role::Client, 1 << 20) }
}
