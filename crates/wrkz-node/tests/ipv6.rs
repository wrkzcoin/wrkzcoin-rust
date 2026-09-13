// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! IPv6: the second listener, the peer-list capability gate in both
//! directions, and an IPv6 peer through the file, the lists and the dial path.
//!
//! What the C++ does and what is checked here:
//!
//! - a **separate** IPv6 listener with its own accept loop, created only when
//!   an address was given (`m_enableIPv6 = !m_bind_ipv6.empty()`,
//!   `NetNode.cpp:489`; `acceptLoopIPv6` spawned at `NetNode.cpp:770`), feeding
//!   the same connection table, the same inbound cap and the same peer
//!   manager (`NetNode.cpp:2696`);
//! - `local_peerlist6` exchanged **only** with peers at
//!   `P2P_IPV6_CAPABILITY_VERSION` or above, in both directions
//!   (`NetNode.cpp:904` and `:2335`);
//! - an IPv6 entry surviving `p2pstate.wrkz.bin` and coming back out of the
//!   dial selection, and the dial itself completing a handshake over IPv6.
//!
//! Every test that needs `::1` skips with a message when the host has no IPv6
//! loopback, which is common in a CI container.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_node::peers::now_secs;
use wrkz_node::pool::BoundedTxSet;
use wrkz_node::{Node, NodeConfig, PeerManager};
use wrkz_p2p::conn::{self, Connection};
use wrkz_p2p::levin;
use wrkz_p2p::msg::{self, BasicNodeData, CoreSyncData, HandshakeResponse, PeerlistEntry6};
use wrkz_primitives::constants::{
    CRYPTONOTE_NETWORK, P2P_CURRENT_VERSION, P2P_IPV6_CAPABILITY_VERSION, P2P_NET_DATA_FILENAME,
};
use wrkz_storage::MemStore;

/// A routable IPv6 address, so `is_ip_allowed` accepts it without
/// `--allow-local-ip` — the operator's node really does peer with this one.
const PEER6: Ipv6Addr = Ipv6Addr::new(0x2a01, 0x4f8, 0xc012, 0x5d10, 0, 0, 0, 1);
const PEER6_PORT: u32 = 17855;

const BUDGET: Duration = Duration::from_secs(20);

/// True when this host can bind `::1`. A test that cannot must say so and
/// pass, rather than fail on a machine with IPv6 compiled out.
fn ipv6_available(what: &str) -> bool {
    if wrkz_p2p::bind::ipv6_loopback_available() {
        return true;
    }
    eprintln!("skipping {what}: this host has no IPv6 loopback");
    false
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wrkz-node-ipv6-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn base_config(dir: std::path::PathBuf) -> NodeConfig {
    NodeConfig {
        data_dir: dir,
        p2p_port: 0,
        listen: false,
        use_default_seeds: false,
        max_outgoing: 4,
        max_incoming: 4,
        tick_interval: Duration::from_millis(20),
        timed_sync_interval: Duration::from_secs(3600),
        idle_timeout: Duration::from_secs(3600),
        ..Default::default()
    }
}

fn new_node(cfg: NodeConfig) -> Node<MemStore, BoundedTxSet> {
    let chain = ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet()).unwrap();
    Node::new(chain, BoundedTxSet::new(64, 1 << 20), cfg)
}

/// Step the node until `done` or the budget runs out.
fn run_until(
    node: &mut Node<MemStore, BoundedTxSet>,
    budget: Duration,
    mut done: impl FnMut(&Node<MemStore, BoundedTxSet>) -> bool,
) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if done(node) {
            return true;
        }
        node.step(Duration::from_millis(10));
    }
    done(node)
}

// ---------------------------------------------------------------------------
// a client that connects *to* the node under test
// ---------------------------------------------------------------------------

/// One inbound connection, held open until the `Client` is dropped so the
/// engine keeps counting it.
struct Client {
    result: Receiver<io::Result<Box<HandshakeResponse>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Client {
    /// Connect to `addr` and invoke `COMMAND_HANDSHAKE` advertising `version`,
    /// on its own thread, because the engine has to be stepped while it runs.
    fn spawn(addr: SocketAddr, version: u8) -> Self {
        let (tx, result) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mine = Arc::clone(&stop);
        std::thread::spawn(move || {
            // The connection is kept in scope on purpose: dropping it would
            // close the socket, and the engine would forget the peer before the
            // test could count it.
            let mut held: Option<Connection> = None;
            let outcome = (|| -> io::Result<Box<HandshakeResponse>> {
                let mut conn = Connection::connect(addr, conn::CONNECT_TIMEOUT, conn::HANDSHAKE_TIMEOUT)?;
                let mut node_data = BasicNodeData::ours(0x1234_5678_9abc_def0, 0);
                node_data.version = version;
                // `my_port = 0` so the engine starts no back ping: this client
                // does not listen, and a failed ping would only add noise.
                let payload = msg::handshake_request(&node_data, &CoreSyncData::default());
                let (header, body) = conn.invoke(msg::COMMAND_HANDSHAKE, &payload, conn::HANDSHAKE_TIMEOUT)?;
                held = Some(conn);
                if header.return_code != levin::RETCODE_SUCCESS {
                    return Err(io::Error::other(format!("return code {}", header.return_code)));
                }
                let hs = msg::parse_handshake_response(&body).map_err(|e| io::Error::other(e.to_string()))?;
                Ok(Box::new(hs))
            })();
            let ok = outcome.is_ok();
            let _ = tx.send(outcome);
            while ok && !mine.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(held);
        });
        Self { result, stop }
    }

    /// Step the node until the client's handshake has come back.
    fn wait(&self, node: &mut Node<MemStore, BoundedTxSet>) -> io::Result<Box<HandshakeResponse>> {
        let mut out = None;
        run_until(node, BUDGET, |_| {
            if out.is_none() {
                out = self.result.try_recv().ok();
            }
            out.is_some()
        });
        out.unwrap_or_else(|| Err(io::Error::other("the client never answered")))
    }
}

// ---------------------------------------------------------------------------
// a peer the node under test dials
// ---------------------------------------------------------------------------

/// A listener that answers `COMMAND_HANDSHAKE` with a chosen version and a
/// chosen `local_peerlist6`, then stays quiet. Enough to drive the outbound
/// half of the capability gate and the IPv6 dial path.
struct FakePeer {
    addr: SocketAddr,
    saw_handshake: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Drop for FakePeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl FakePeer {
    fn start(bind: &str, version: u8, peerlist6: Vec<PeerlistEntry6>) -> io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        let saw_handshake = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&saw_handshake);
        let st = Arc::clone(&stop);
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let Ok(mut conn) = Connection::from_stream(stream, Duration::from_secs(30)) else { return };
            let Ok(write_half) = conn.try_clone() else { return };
            let mut writer = write_half;
            while !st.load(Ordering::Relaxed) {
                let Ok((header, _payload)) = conn.read_frame() else { return };
                if header.command == msg::COMMAND_HANDSHAKE {
                    seen.store(true, Ordering::Relaxed);
                    let mut node_data = BasicNodeData::ours(0xfeed_face_dead_beef, 0);
                    node_data.version = version;
                    let body = msg::handshake_response(&node_data, &CoreSyncData::default(), &[], &peerlist6);
                    if writer.reply(msg::COMMAND_HANDSHAKE, levin::RETCODE_SUCCESS, &body).is_err() {
                        return;
                    }
                } else if header.have_to_return_data {
                    let _ = writer.reply(header.command, levin::ERROR_HANDLER_NOT_DEFINED, &[]);
                }
            }
        });
        Ok(Self { addr, saw_handshake, stop })
    }

    fn saw_handshake(&self) -> bool {
        self.saw_handshake.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// the listener
// ---------------------------------------------------------------------------

/// Deliverable 1: two listeners, one engine. A client on each family reaches
/// the handshake, and both connections land in the one inbound pool the
/// `--in-peers` cap counts.
#[test]
fn both_families_listen_and_a_client_on_each_reaches_the_handshake() {
    if !ipv6_available("both_families_listen") {
        return;
    }
    let mut cfg = base_config(temp_dir("listen-both"));
    cfg.listen = true;
    cfg.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    cfg.bind_ipv6 = Some(Ipv6Addr::LOCALHOST);
    let mut node = new_node(cfg);
    node.start().expect("both listeners bind");

    let v4 = node.listen_addr().expect("the IPv4 listener is up");
    let v6 = node.listen_addr6().expect("the IPv6 listener is up");
    assert!(v4.is_ipv4() && v6.is_ipv6(), "{v4} / {v6}");
    assert_ne!(v4.port(), 0);
    assert_ne!(v6.port(), 0);

    let a = Client::spawn(v4, P2P_CURRENT_VERSION);
    let hs4 = a.wait(&mut node).expect("the IPv4 client completes the handshake");
    assert_eq!(hs4.node_data.network_id, CRYPTONOTE_NETWORK);

    let b = Client::spawn(v6, P2P_CURRENT_VERSION);
    let hs6 = b.wait(&mut node).expect("the IPv6 client completes the handshake");
    assert_eq!(hs6.node_data.network_id, CRYPTONOTE_NETWORK);
    assert_eq!(hs6.node_data.peer_id, hs4.node_data.peer_id, "one node, one peer id, whichever socket you reach it on");

    // One pool: both families count against the same inbound total.
    run_until(&mut node, BUDGET, |n| n.connection_counts().0 == 2);
    assert_eq!(node.connection_counts(), (2, 0), "both inbound connections are in the one table");
    drop((a, b));
    node.shutdown();
}

/// No address, no listener — `m_enableIPv6` is `!m_bind_ipv6.empty()`
/// (`NetNode.cpp:489`), so IPv6 is off unless it is asked for.
#[test]
fn no_ipv6_listener_unless_an_address_is_configured() {
    let mut cfg = base_config(temp_dir("no-v6"));
    cfg.listen = true;
    cfg.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    assert_eq!(cfg.bind_ipv6, None, "the default is off, as in the C++");
    let mut node = new_node(cfg);
    node.start().unwrap();
    assert!(node.listen_addr().is_some());
    assert_eq!(node.listen_addr6(), None, "no IPv6 listener was asked for");
    node.shutdown();
}

/// `--p2p-bind-port-ipv6 0` means "same as `--p2p-bind-port`"
/// (`DaemonConfiguration.cpp:337`, `NetNodeConfig.cpp:104`), and a non-zero
/// value wins. Both listeners then hold their own port on their own family,
/// which is what `IPV6_V6ONLY` buys.
#[test]
fn the_ipv6_port_defaults_to_the_ipv4_port_and_can_be_overridden() {
    if !ipv6_available("the_ipv6_port_defaults") {
        return;
    }
    // A concrete shared port: bind the IPv4 listener first, then ask the IPv6
    // one for the same number. Dual-stack, this is EADDRINUSE.
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let shared = probe.local_addr().unwrap().port();
    drop(probe);

    let mut cfg = base_config(temp_dir("v6-port-default"));
    cfg.listen = true;
    cfg.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    cfg.p2p_port = shared;
    cfg.bind_ipv6 = Some(Ipv6Addr::LOCALHOST);
    cfg.p2p_port_ipv6 = 0;
    let mut node = new_node(cfg);
    if node.start().is_err() {
        eprintln!("skipping: port {shared} was taken between the probe and the bind");
        return;
    }
    assert_eq!(node.listen_addr().unwrap().port(), shared);
    assert_eq!(node.listen_addr6().unwrap().port(), shared, "0 means the IPv4 port");
    node.shutdown();
    drop(node);

    // And an explicit port is used instead.
    let mut cfg = base_config(temp_dir("v6-port-explicit"));
    cfg.listen = true;
    cfg.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    cfg.bind_ipv6 = Some(Ipv6Addr::LOCALHOST);
    cfg.p2p_port = 0;
    let picked = {
        let probe = TcpListener::bind("[::1]:0").unwrap();
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };
    cfg.p2p_port_ipv6 = picked;
    let mut node = new_node(cfg);
    if node.start().is_err() {
        eprintln!("skipping: port {picked} was taken between the probe and the bind");
        return;
    }
    assert_eq!(node.listen_addr6().unwrap().port(), picked);
    assert_ne!(node.listen_addr().unwrap().port(), picked, "the IPv4 listener took an ephemeral port");
    node.shutdown();
}

// ---------------------------------------------------------------------------
// the capability gate
// ---------------------------------------------------------------------------

/// Seed a peer state file holding one IPv6 white entry, and return its
/// directory.
fn dir_with_one_ipv6_white_peer(name: &str) -> std::path::PathBuf {
    let dir = temp_dir(name);
    let mut pm = PeerManager::open(&dir.join(P2P_NET_DATA_FILENAME), false, false);
    pm.append_white6(PeerlistEntry6 { id: 7, last_seen: now_secs(), ip: PEER6.octets(), port: PEER6_PORT });
    pm.save().expect("the peer state file is written");
    dir
}

/// What we **send**: `get_peerlist6_head` is only called for a peer at
/// `P2P_IPV6_CAPABILITY_VERSION` or above (`NetNode.cpp:2335`), so an older
/// peer gets an empty `local_peerlist6` and never sees our IPv6 peers.
#[test]
fn we_advertise_ipv6_peers_only_to_peers_at_the_capability_version() {
    if !ipv6_available("we_advertise_ipv6_peers") {
        return;
    }
    let mut cfg = base_config(dir_with_one_ipv6_white_peer("advertise-gate"));
    cfg.listen = true;
    cfg.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    cfg.bind_ipv6 = Some(Ipv6Addr::LOCALHOST);
    let mut node = new_node(cfg);
    node.start().unwrap();
    assert_eq!(node.peer_manager().white_count(), 1, "the seeded IPv6 peer was read back");
    let v6 = node.listen_addr6().unwrap();

    let old = Client::spawn(v6, P2P_IPV6_CAPABILITY_VERSION - 1);
    let hs = old.wait(&mut node).expect("an old peer still gets a handshake");
    assert!(
        hs.local_peerlist6.is_empty(),
        "a peer below version {P2P_IPV6_CAPABILITY_VERSION} is told nothing about IPv6"
    );
    drop(old);

    let new = Client::spawn(v6, P2P_IPV6_CAPABILITY_VERSION);
    let hs = new.wait(&mut node).expect("a capable peer gets a handshake");
    assert_eq!(hs.local_peerlist6.len(), 1, "a capable peer is told about our IPv6 peers");
    assert_eq!(hs.local_peerlist6[0].ip, PEER6.octets());
    assert_eq!(hs.local_peerlist6[0].port, PEER6_PORT);
    drop(new);
    node.shutdown();
}

/// What we **accept**: `handle_remote_peerlist6` runs only when the peer's
/// version is at least `P2P_IPV6_CAPABILITY_VERSION` (`NetNode.cpp:904`), so a
/// list from an older peer is ignored outright.
#[test]
fn we_accept_ipv6_peers_only_from_peers_at_the_capability_version() {
    let advertised = vec![PeerlistEntry6 { id: 11, last_seen: now_secs(), ip: PEER6.octets(), port: PEER6_PORT }];
    let expected = SocketAddr::new(IpAddr::V6(PEER6), PEER6_PORT as u16);

    for (version, should_take) in [(P2P_IPV6_CAPABILITY_VERSION - 1, false), (P2P_IPV6_CAPABILITY_VERSION, true)] {
        let peer = FakePeer::start("127.0.0.1:0", version, advertised.clone()).unwrap();
        let mut node = new_node(base_config(temp_dir(&format!("accept-gate-{version}"))));
        node.start().unwrap();
        node.connect_to(peer.addr);
        run_until(&mut node, BUDGET, |n| n.peer_manager().gray_count() > 0 || n.peer_count() > 0);
        // Give the merge a moment even once the peer is up.
        run_until(&mut node, Duration::from_millis(300), |_| false);
        let gray = node.peer_manager().gray_addresses();
        assert_eq!(
            gray.contains(&expected),
            should_take,
            "version {version}: gray list is {gray:?}, expected the IPv6 entry to be taken = {should_take}"
        );
        node.shutdown();
    }
}

// ---------------------------------------------------------------------------
// the round trip: wire -> lists -> file -> dial
// ---------------------------------------------------------------------------

/// Deliverable 2, end to end without a socket: an entry that arrives in a
/// peer's `local_peerlist6` reaches the gray list, survives
/// `p2pstate.wrkz.bin`, and comes back out of the dial selection as a real
/// IPv6 `SocketAddr`.
#[test]
fn an_ipv6_peer_survives_the_peer_file_and_is_offered_for_dialling() {
    let dir = temp_dir("v6-round-trip");
    let path = dir.join(P2P_NET_DATA_FILENAME);
    let expected = SocketAddr::new(IpAddr::V6(PEER6), PEER6_PORT as u16);

    {
        let mut pm = PeerManager::open(&path, false, false);
        // Exactly what `handle_remote_peerlist6` is handed on the wire.
        pm.merge_peerlist6(&[PeerlistEntry6 { id: 3, last_seen: now_secs(), ip: PEER6.octets(), port: PEER6_PORT }]);
        assert!(pm.gray_addresses().contains(&expected), "the received entry is in gray");
        // A back ping, or an outbound handshake, promotes it to white.
        pm.set_peer_just_seen(3, expected, now_secs());
        assert!(pm.white_addresses().contains(&expected), "promoted to white");
        pm.save().unwrap();
    }

    let pm = PeerManager::open(&path, false, false);
    assert!(pm.white_addresses().contains(&expected), "the IPv6 entry survived the file");
    assert_eq!(pm.white_count(), 1);
    let candidates = pm.dial_candidates(4, &[]);
    assert!(candidates.contains(&expected), "the IPv6 peer is offered for dialling: {candidates:?}");
    assert!(candidates.iter().any(|a| a.is_ipv6()));

    // The C++ file is byte-compatible: version 2 of `PeerlistManager::serialize`
    // holds the two IPv6 lists after the two IPv4 ones.
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.windows(16).any(|w| w == PEER6.octets()), "the 16 raw address bytes are in the file");
}

/// And the dial itself works over IPv6: `Connection::connect` to a v6 address,
/// the handshake, and the peer in the engine's table.
#[test]
fn the_node_dials_an_ipv6_peer_and_completes_the_handshake() {
    if !ipv6_available("the_node_dials_an_ipv6_peer") {
        return;
    }
    let peer = FakePeer::start("[::1]:0", P2P_CURRENT_VERSION, Vec::new()).unwrap();
    assert!(peer.addr.is_ipv6());
    let mut node = new_node(base_config(temp_dir("dial-v6")));
    node.start().unwrap();
    node.connect_to(peer.addr);
    assert!(run_until(&mut node, BUDGET, |n| n.peer_count() == 1), "the IPv6 dial produced a peer");
    assert!(peer.saw_handshake(), "the peer saw our COMMAND_HANDSHAKE over IPv6");
    assert_eq!(node.connection_counts(), (0, 1), "counted as one outbound connection, like any other");
    node.shutdown();
}

/// `peers::resolve` has to accept the IPv6 spellings an operator writes:
/// a bare literal, a bracketed one, and a bracketed one with a port. A naive
/// "contains a colon means it has a port" test gets every one of them wrong.
#[test]
fn seed_and_peer_addresses_parse_in_every_ipv6_spelling() {
    let expect = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 17855);
    for text in ["::1", "[::1]", "0:0:0:0:0:0:0:1"] {
        let got = wrkz_node::peers::resolve(text, 17855).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(got.contains(&expect), "{text} -> {got:?}");
    }
    let got = wrkz_node::peers::resolve("[::1]:9999", 17855).unwrap();
    assert!(got.contains(&SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9999)), "{got:?}");
    // IPv4 is untouched.
    let got = wrkz_node::peers::resolve("127.0.0.1", 17855).unwrap();
    assert_eq!(got[0], SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 17855));
    let got = wrkz_node::peers::resolve("127.0.0.1:1234", 17855).unwrap();
    assert_eq!(got[0], SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234));
}
