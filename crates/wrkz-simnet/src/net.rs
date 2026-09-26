// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The simnet itself: nodes, the links between them, and the waits a test
//! needs.
//!
//! Every node is a whole daemon minus the parts that only matter to an
//! operator: a simnet [`ChainState`] on [`MemStore`], the transaction pool,
//! the P2P engine ([`Node`]) on a thread of its own, [`ChainNode`] for the RPC,
//! and — unless asked otherwise — the RPC server with its `GET /ws` stream,
//! each on a free loopback port. They are put together the way `wrkz-node`
//! puts them together, so a test drives the same code a user runs.
//!
//! Nodes talk only through [`Link`]s: a node dials the links it is the dialler
//! of, as exclusive nodes, and dials nothing else — a node with no link to
//! dial has no outbound connections at all — so an address learned from a
//! peer list can never go round a cut link.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_mempool::TransactionPool;
use wrkz_node::mempool::SharedMempool;
use wrkz_node::node::PinnedNode;
use wrkz_node::{Node, NodeConfig};
use wrkz_primitives::constants::SIMNET_NETWORK;
use wrkz_primitives::Hash;
use wrkz_rpc::api::{NodeApi, SubmitOutcome};
use wrkz_rpc::events::{EventListener, Events};
use wrkz_rpc::node::{ChainNode, P2pSnapshot};
use wrkz_rpc::server::{self, RunningServer, ServerConfig};
use wrkz_rpc::ws::{WsConfig, WsHub};
use wrkz_storage::MemStore;

use crate::link::Link;

/// How long one engine step may wait for something to happen.
const TICK: Duration = Duration::from_millis(20);
/// How often a node compares heights with its peers (`COMMAND_TIMED_SYNC`).
const TIMED_SYNC: Duration = Duration::from_secs(2);
static NEXT_NETWORK: AtomicU64 = AtomicU64::new(0);

/// What one node runs besides its chain and its P2P engine.
#[derive(Clone, Debug)]
pub struct NodeOptions {
    /// Serve the RPC on loopback.
    pub rpc: bool,
    /// The port for it; `0` is any free one.
    pub rpc_port: u16,
    /// Serve `GET /ws` on the RPC (`--enable-websocket`).
    pub websocket: bool,
    /// The RPC's `--enable-cors`, for a browser wallet pointed at the node.
    pub cors: String,
    /// The P2P port; `0` is any free one.
    pub p2p_port: u16,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self { rpc: true, rpc_port: 0, websocket: true, cors: String::new(), p2p_port: 0 }
    }
}

/// One node of a simnet. Dropping it stops it.
pub struct SimNode {
    index: usize,
    api: Arc<ChainNode<MemStore>>,
    status: Arc<Mutex<P2pSnapshot>>,
    p2p_addr: SocketAddr,
    hub: Option<WsHub>,
    // Declared before `engine`, so the RPC stops first.
    rpc: Option<RunningServer>,
    stop: Arc<AtomicBool>,
    engine: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SimNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimNode").field("index", &self.index).field("p2p", &self.p2p_addr).finish()
    }
}

impl SimNode {
    fn start(index: usize, options: &NodeOptions, dial: Vec<SocketAddr>, data_dir: PathBuf) -> io::Result<SimNode> {
        std::fs::create_dir_all(&data_dir)?;
        let chain_cfg = Config { simnet: true, ..Config::default() };
        let chain = ChainState::open_or_genesis(MemStore::default(), chain_cfg, Checkpoints::none())
            .map_err(|e| io::Error::other(format!("simnet chain: {e}")))?;
        let chain = Arc::new(RwLock::new(chain));
        let pool = Arc::new(Mutex::new(TransactionPool::new(Default::default())));
        let status = Arc::new(Mutex::new(P2pSnapshot::standalone()));
        let hub = options.websocket.then(|| WsHub::new(WsConfig::default()));
        let listeners: Vec<Arc<dyn EventListener>> = hub.iter().map(WsHub::listener).collect();
        let events = Events::new(listeners);

        // A node that dials nobody gets no outbound slots at all, so it cannot
        // go round a link to an address a peer told it about.
        let cfg = NodeConfig {
            data_dir,
            p2p_port: options.p2p_port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            use_default_seeds: false,
            max_outgoing: if dial.is_empty() { 0 } else { dial.len() },
            exclusive_nodes: dial.into_iter().map(PinnedNode::at).collect(),
            allow_local_ip: true,
            network_id: SIMNET_NETWORK,
            tick_interval: TICK,
            // A block mined while a peer is still catching up is not relayed to
            // it; the timed sync is what finds the gap, every minute on
            // mainnet. A test network can afford to look every two seconds.
            timed_sync_interval: TIMED_SYNC,
            ..NodeConfig::default()
        };

        // The engine is built on its own thread, which it never leaves; what
        // the rest needs of it comes back over a channel.
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = sync_channel(1);
        let (api_tx, api_rx) = channel::<Arc<ChainNode<MemStore>>>();
        let engine = {
            let (chain, pool, events, status, stop) =
                (Arc::clone(&chain), Arc::clone(&pool), events.clone(), Arc::clone(&status), Arc::clone(&stop));
            std::thread::Builder::new().name(format!("wrkz-simnet-node-{index}")).spawn(move || {
                let mempool = SharedMempool::new(pool, Arc::clone(&chain)).with_events(events.clone());
                let mut node = Node::with_shared_chain(chain, mempool, cfg);
                node.set_events(events);
                let started = node.start().map(|()| (node.waker(), node.listen_addr()));
                let failed = started.is_err();
                let _ = ready_tx.send(started);
                if failed {
                    return;
                }
                let Ok(api) = api_rx.recv() else { return };
                run_engine(&mut node, &api, &status, &stop);
                node.shutdown();
            })?
        };
        let (waker, listen) = ready_rx.recv().map_err(|_| io::Error::other("the engine thread died"))??;
        let p2p_addr = listen.ok_or_else(|| io::Error::other("the P2P listener did not come up"))?;

        let snapshot = {
            let status = Arc::clone(&status);
            Box::new(move || status.lock().unwrap_or_else(|p| p.into_inner()).clone())
        };
        let api = Arc::new(
            ChainNode::shared(chain, pool, snapshot)
                .with_mined_block_hook(Box::new(move || waker.wake()))
                .with_events(events),
        );
        api_tx.send(Arc::clone(&api)).map_err(|_| io::Error::other("the engine thread died"))?;

        let rpc = if options.rpc {
            let config = ServerConfig {
                bind: format!("127.0.0.1:{}", options.rpc_port),
                cors_header: options.cors.clone(),
                // Tests hammer one node from one address.
                max_requests_per_minute: 0,
                ..ServerConfig::default()
            };
            Some(server::start_with(Arc::clone(&api) as Arc<dyn NodeApi>, config, hub.clone())?)
        } else {
            None
        };

        Ok(SimNode { index, api, status, p2p_addr, hub, rpc, stop, engine: Some(engine) })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// The top block's index.
    pub fn height(&self) -> u64 {
        self.api.top_index()
    }

    /// The top block's hash.
    pub fn top_hash(&self) -> Hash {
        self.api.block_hash_by_index(self.height()).ok().flatten().unwrap_or([0; 32])
    }

    /// Connections open now, either direction.
    pub fn connections(&self) -> u64 {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).total_connections
    }

    pub fn p2p_addr(&self) -> SocketAddr {
        self.p2p_addr
    }

    /// `http://127.0.0.1:PORT`, when the node serves the RPC.
    pub fn rpc_url(&self) -> Option<String> {
        self.rpc.as_ref().map(|r| format!("http://{}", r.local_addr()))
    }

    /// `ws://127.0.0.1:PORT/ws`, when the node serves the stream.
    pub fn ws_url(&self) -> Option<String> {
        match (&self.rpc, &self.hub) {
            (Some(rpc), Some(_)) => Some(format!("ws://{}/ws", rpc.local_addr())),
            _ => None,
        }
    }

    /// The node as the RPC handlers see it.
    pub fn api(&self) -> &Arc<ChainNode<MemStore>> {
        &self.api
    }

    /// Its WebSocket hub, when it has one.
    pub fn ws_hub(&self) -> Option<&WsHub> {
        self.hub.as_ref()
    }

    /// Mine one block paying `address`, through `getblocktemplate` and
    /// `submitblock` as a pool would: a simnet block needs no nonce. It
    /// carries whatever the pool has, and is announced to the node's peers.
    pub fn mine(&self, address: &str) -> Result<Hash, String> {
        let template = self
            .api
            .block_template(address, &[])
            .map_err(|e| format!("getblocktemplate: {e:?}"))?
            .map_err(|e| format!("getblocktemplate: {e}"))?;
        let hash = wrkz_primitives::block::BlockTemplate::from_bytes(&template.blob)
            .and_then(|b| b.hash())
            .map_err(|e| format!("the template does not parse: {e:?}"))?;
        match self.api.submit_block(&template.blob).map_err(|e| format!("submitblock: {e:?}"))? {
            SubmitOutcome::Added { .. } => Ok(hash),
            SubmitOutcome::NotAccepted => Err(format!("node {} did not accept its own template", self.index)),
        }
    }

    /// [`SimNode::mine`], `count` times.
    pub fn mine_many(&self, address: &str, count: usize) -> Result<Vec<Hash>, String> {
        (0..count).map(|_| self.mine(address)).collect()
    }
}

impl Drop for SimNode {
    fn drop(&mut self) {
        if let Some(mut rpc) = self.rpc.take() {
            rpc.stop();
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(engine) = self.engine.take() {
            let _ = engine.join();
        }
    }
}

/// The daemon's main loop (`wrkz-node`, `serve`), less the operator's parts:
/// step the engine, publish what the RPC reports, and hand the engine what the
/// RPC accepted.
fn run_engine(
    node: &mut Node<MemStore, SharedMempool<MemStore>>,
    api: &ChainNode<MemStore>,
    status: &Mutex<P2pSnapshot>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) && !node.should_stop() {
        node.step(TICK);
        let (incoming, outgoing) = node.connection_counts();
        let height = node.height();
        let observed = node.observed_height();
        {
            let mut s = status.lock().unwrap_or_else(|p| p.into_inner());
            s.total_connections = (incoming + outgoing) as u64;
            s.outgoing_connections = outgoing as u64;
            s.observed_height = u64::from(observed);
            s.blockchain_height = u64::from(height.max(observed));
            // A simnet node on its own is the whole network, so it counts as
            // synchronized; with peers, the engine decides.
            s.synchronized = node.is_synchronized() || node.peer_count() == 0;
        }
        for mined in api.take_block_relay_queue() {
            node.relay_new_block(&mined.block, &mined.transactions);
        }
        let relay = api.take_relay_queue();
        if !relay.is_empty() {
            node.relay_transactions(&relay);
        }
    }
}

/// How the nodes of a simnet are joined.
#[derive(Clone, Debug, Default)]
pub struct SimnetBuilder {
    nodes: Vec<NodeOptions>,
    /// `(dialler, listener)` pairs.
    links: Vec<(usize, usize)>,
}

impl SimnetBuilder {
    /// Add a node.
    pub fn node(mut self, options: NodeOptions) -> Self {
        self.nodes.push(options);
        self
    }

    /// Add `count` nodes with the default options.
    pub fn nodes(mut self, count: usize) -> Self {
        self.nodes.extend(std::iter::repeat_n(NodeOptions::default(), count));
        self
    }

    /// Join `dialler` to `listener`: the first dials the second through a
    /// link that can be cut.
    pub fn link(mut self, dialler: usize, listener: usize) -> Self {
        self.links.push((dialler, listener));
        self
    }

    /// Each node dials the next: 0 → 1 → 2 → …
    pub fn line(mut self) -> Self {
        for i in 1..self.nodes.len() {
            self.links.push((i - 1, i));
        }
        self
    }

    /// A line, closed: the last node dials the first.
    pub fn ring(self) -> Self {
        let n = self.nodes.len();
        let mut built = self.line();
        if n > 2 {
            built.links.push((n - 1, 0));
        }
        built
    }

    /// Every pair joined, the lower index dialling.
    pub fn mesh(mut self) -> Self {
        let n = self.nodes.len();
        for a in 0..n {
            for b in a + 1..n {
                self.links.push((a, b));
            }
        }
        self
    }

    /// Start every node and every link.
    pub fn build(self) -> io::Result<Simnet> {
        let n = self.nodes.len();
        if let Some(&(a, b)) = self.links.iter().find(|(a, b)| *a >= n || *b >= n || a == b) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("no link from node {a} to node {b}")));
        }
        let root = std::env::temp_dir().join(format!(
            "wrkz-simnet-{}-{}",
            std::process::id(),
            NEXT_NETWORK.fetch_add(1, Ordering::Relaxed)
        ));
        let mut links = Vec::with_capacity(self.links.len());
        for &(dialler, listener) in &self.links {
            links.push((dialler, listener, Link::bind()?));
        }
        let mut nodes = Vec::with_capacity(n);
        for (i, options) in self.nodes.iter().enumerate() {
            let dial = links.iter().filter(|(d, _, _)| *d == i).map(|(_, _, l)| l.addr()).collect();
            nodes.push(SimNode::start(i, options, dial, root.join(format!("node-{i}")))?);
        }
        for (_, listener, link) in &links {
            link.set_target(nodes[*listener].p2p_addr());
        }
        Ok(Simnet { nodes, links, root })
    }
}

/// A running simnet. Dropping it stops every node and removes its files.
pub struct Simnet {
    nodes: Vec<SimNode>,
    links: Vec<(usize, usize, Link)>,
    root: PathBuf,
}

impl Simnet {
    pub fn builder() -> SimnetBuilder {
        SimnetBuilder::default()
    }

    pub fn node(&self, index: usize) -> &SimNode {
        &self.nodes[index]
    }

    pub fn nodes(&self) -> &[SimNode] {
        &self.nodes
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Where the nodes keep their peer state.
    pub fn data_dir(&self) -> &Path {
        &self.root
    }

    fn links_between(&self, a: usize, b: usize) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(move |(d, l, _)| (*d == a && *l == b) || (*d == b && *l == a)).map(|(_, _, link)| link)
    }

    /// Cut every link between `a` and `b`.
    pub fn cut(&self, a: usize, b: usize) {
        for link in self.links_between(a, b) {
            link.cut();
        }
    }

    /// Heal every link between `a` and `b`.
    pub fn heal(&self, a: usize, b: usize) {
        for link in self.links_between(a, b) {
            link.heal();
        }
    }

    /// Wait up to `timeout` for `done`, checking every few milliseconds.
    pub fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&Simnet) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until every one of `nodes` has the same top block, and return it.
    pub fn wait_for_agreement(&self, nodes: &[usize], timeout: Duration) -> Result<(u64, Hash), String> {
        let tips = |s: &Simnet| nodes.iter().map(|&i| (s.node(i).height(), s.node(i).top_hash())).collect::<Vec<_>>();
        if self.wait_until(timeout, |s| tips(s).windows(2).all(|w| w[0] == w[1])) {
            let first = tips(self)[0];
            return Ok(first);
        }
        let described: Vec<String> = tips(self)
            .iter()
            .zip(nodes)
            .map(|((h, t), i)| format!("node {i} at {h} ({})", hex::encode(&t[..4])))
            .collect();
        Err(format!("no agreement within {timeout:?}: {}", described.join(", ")))
    }

    /// Wait until every node has at least `connections` connections open.
    pub fn wait_for_connections(&self, connections: u64, timeout: Duration) -> bool {
        self.wait_until(timeout, |s| s.nodes.iter().all(|n| n.connections() >= connections))
    }
}

impl Drop for Simnet {
    fn drop(&mut self) {
        // Nodes first, while their links still carry their goodbyes.
        self.nodes.clear();
        self.links.clear();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
