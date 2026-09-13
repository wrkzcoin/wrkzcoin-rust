// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The node engine: the connection manager of `src/p2p/NetNode.cpp` and the
//! sync state machine of `src/cryptonoteprotocol/CryptoNoteProtocolHandler.cpp`
//! (spec/08).
//!
//! Everything runs on one thread. Sockets are owned by the reader and writer
//! threads of [`crate::net`], and dialling, the handshake and the back ping
//! happen on short-lived threads, so no peer can block the engine and no
//! handler needs a lock. That is the same shape as the C++ node, whose
//! connections are coroutines on a single dispatcher: handlers there are
//! likewise never concurrent with each other.
//!
//! # How the states map to the C++
//!
//! | C++ (`ConnectionContext.h:38`) | here |
//! | --- | --- |
//! | `state_befor_handshake` | [`PeerState::BeforeHandshake`] |
//! | `state_sync_required` -> `state_synchronizing` in `connectionHandler` | the engine's `pump_state`, run after every frame |
//! | `state_pool_sync_required` -> `state_normal` + `NOTIFY_REQUEST_TX_POOL` | the same `pump_state` |
//! | `state_synchronizing` | chain entry / get-objects in flight |
//! | `state_idle` | the peer offered a block we already had |
//! | `state_normal` | relay duty; blocks and transactions accepted |
//! | `state_shutdown` | the engine's `drop_peer`: the sink is closed and the peer forgotten |

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, TcpListener};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use wrkz_chain::{AddOutcome, AddStatus, ChainError, ChainState, PowHint, Rule, Timings};
use wrkz_p2p::conn;
use wrkz_p2p::levin::{self, Header};
use wrkz_p2p::msg::{self, BasicNodeData, CoreSyncData, LiteBlock, MissingTxs, NewBlock, RawBlockLegacy};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::{
    CRYPTONOTE_NETWORK, P2P_CURRENT_VERSION, P2P_DEFAULT_CONNECTIONS_COUNT, P2P_DEFAULT_PEERS_IN_HANDSHAKE,
    P2P_DEFAULT_PORT, P2P_IPV6_CAPABILITY_VERSION, P2P_LITE_BLOCKS_PROPOGATION_VERSION, P2P_MINIMUM_VERSION,
    P2P_NET_DATA_FILENAME, SEED_NODES,
};
use wrkz_primitives::Hash;
use wrkz_rpc::events::{AppliedBlock, Events};
use wrkz_storage::KvStore;

use crate::net::{self, ConnIds, Event, FrameFault, InboundGate, InboundLimits, Sink};
use crate::peers::{self, BanList, Offence, PeerManager, MAX_ANCHORS, MISBEHAVIOUR_BAN_SECONDS};
use crate::pool::{TxPool, TxVerdict};
use crate::sync::{
    majority_not_ahead, objects_timeout, prune_capability_fork_active, recalculate_observed_height, ConnId, PeerCtx,
    PeerState, SyncTuning, BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT, CHAIN_REQUEST_TIMEOUT, OBJECTS_TIMEOUT_MAX,
    OBJECTS_TIMEOUT_MIN, SYNC_BLOCK_BUDGET_MAX_BYTES, SYNC_ORPHAN_RETRY_LIMIT,
};
use crate::{log_debug, log_error, log_info, log_trace, log_warn};

/// How often the engine wakes up for the timed sync, the connection maker and
/// the bookkeeping.
const TICK: Duration = Duration::from_millis(500);
/// How often the peer state file is rewritten (`m_peerlist_store_interval`).
const PEERLIST_STORE_INTERVAL: Duration = Duration::from_secs(60);
/// How often the progress line is printed.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
/// The most block bytes one `NOTIFY_RESPONSE_GET_OBJECTS` answer carries.
/// The C++ closes a connection whose write queue passes 32 MiB with the new
/// message counted (`NetNode.cpp:181`), so an answer at the 48 MB sync budget
/// could never be sent at all; this leaves 4 MiB of the write budget for the
/// relay traffic queued beside it.
const SERVE_OBJECTS_MAX_BYTES: usize = net::WRITE_BUFFER_BYTES - 4 * 1024 * 1024;
const _: () = assert!(SERVE_OBJECTS_MAX_BYTES as u64 <= SYNC_BLOCK_BUDGET_MAX_BYTES);
/// Transactions go out in `NOTIFY_NEW_TRANSACTIONS` messages of at most this
/// many blob bytes — the pool dump answering `NOTIFY_REQUEST_TX_POOL`
/// included, which the C++ sends as one message of any size — so nothing we
/// send passes a receiver's relay cap (`wrkz_p2p::limits::RELAY_MAX_PAYLOAD`).
const TX_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// Extra time a request deadline gets while frames from the peer are waiting
/// for the engine: then it may be the engine that is behind, not the peer.
/// Bounded, so a peer cannot keep a stalled request alive by streaming frames.
const REQUEST_DEADLINE_GRACE: Duration = Duration::from_secs(10);

/// Everything the operator can set.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    /// The port we listen on, and advertise as `my_port` unless
    /// [`NodeConfig::external_port`] says otherwise.
    pub p2p_port: u16,
    /// `--p2p-external-port`: the port advertised as `my_port` in place of the
    /// listening one, for a node behind a NAT that forwards another port to it
    /// (`NetNode.cpp:2058`). 0, the default, advertises the listening port.
    /// `--hide-my-port` wins over it, and so does `--no-listen`. It has no part
    /// in the UPnP mapping, which is always of the listening port.
    pub external_port: u16,
    pub bind: IpAddr,
    /// `--p2p-bind-ipv6-address`: the address of a **second**, IPv6-only
    /// listener. `None` (the C++ empty string) means no IPv6 listener at all —
    /// `m_enableIPv6 = !m_bind_ipv6.empty()`, `NetNode.cpp:489`.
    pub bind_ipv6: Option<Ipv6Addr>,
    /// `--p2p-bind-port-ipv6`, "0 = same as --p2p-bind-port"
    /// (`DaemonConfiguration.cpp:337`, applied at `NetNodeConfig.cpp:104`).
    pub p2p_port_ipv6: u16,
    /// Bind a listener at all. A node that does not listen never passes a back
    /// ping and so never reaches anyone's white list.
    pub listen: bool,
    /// Extra `host:port` seeds. When empty the compiled-in [`SEED_NODES`] and
    /// the DNS seeds are used.
    pub seeds: Vec<String>,
    pub use_default_seeds: bool,
    /// `--add-exclusive-node`: when any is given, these are the only nodes
    /// this node dials — no seeds, no anchors, no `--add-peer`, nothing from
    /// the peer lists — and the seeds are not even resolved
    /// (`connections_maker`, `NetNode.cpp:1622-1630`). Inbound connections are
    /// still accepted within `--in-peers`, and peer lists are still exchanged.
    pub exclusive_nodes: Vec<PinnedNode>,
    /// `--add-priority-node`: dialled every round until connected, before the
    /// peer lists fill the rest of `--out-peers` (`NetNode.cpp:1645`).
    pub priority_nodes: Vec<PinnedNode>,
    pub max_outgoing: usize,
    pub max_incoming: usize,
    pub tuning: SyncTuning,
    /// `--allow-local-ip`: accept private and loopback peers in the lists.
    pub allow_local_ip: bool,
    /// `--hide-my-port`: advertise `my_port = 0`, which suppresses back pings.
    pub hide_my_port: bool,
    /// `--p2p-reset-peerstate`.
    pub reset_peer_state: bool,
    /// Stop once the chain reaches this height (block index).
    pub sync_to: Option<u32>,
    /// Stop once a peer reports we are at its top.
    pub exit_when_synced: bool,
    /// Advertise `NODE_CAPABILITY_FLAG_PRUNED` with this depth.
    pub pruned_depth: Option<u32>,
    /// Advertise `NODE_CAPABILITY_FLAG_LITE` from this height.
    pub lite_start_height: Option<u32>,
    /// The `--lite-height` of an explicit `--lite` node, for the lite-height
    /// depth check ([`crate::daemon::LiteDepthCheck`]). Deliberately not
    /// [`NodeConfig::lite_start_height`], which a body-less import also sets to
    /// advertise its floor: that node never chose a lite height and must not be
    /// stopped for one.
    pub lite_height_check: Option<u32>,
    /// Write the chain state with the storage engine's write-ahead log off
    /// until the node first synchronizes, as the import does. See
    /// `Node::commit_chain`.
    pub unlogged_initial_sync: bool,
    /// Inbound connections from one address, refused in the accept thread
    /// before any thread is spawned for them. 0 is no limit; loopback is
    /// exempt. The C++ has no such limit.
    pub max_inbound_per_ip: usize,
    /// Inbound connections from one network group (an IPv4 /16, an IPv6 /32),
    /// likewise.
    pub max_inbound_per_subnet: usize,
    /// Score and ban loopback peers like any other. Off by default: every test
    /// runs its peers on 127.0.0.1, and a process on this host is not the
    /// adversary the scores are for.
    pub ban_loopback: bool,

    // -- timers. The defaults are the C++ values; a test lowers them so the
    // timeout paths are reachable without waiting minutes.
    /// How long a peer has to answer `NOTIFY_REQUEST_CHAIN` before it is taken
    /// off sync ([`CHAIN_REQUEST_TIMEOUT`]; the C++ has no deadline).
    pub chain_request_timeout: Duration,
    /// The floor and ceiling of a get-objects deadline, which scales with the
    /// batch between them ([`crate::sync::objects_timeout`]).
    pub objects_timeout_min: Duration,
    pub objects_timeout_max: Duration,
    /// `P2P_DEFAULT_HANDSHAKE_INTERVAL`: how often `COMMAND_TIMED_SYNC` goes
    /// out to every connection in state normal or idle.
    pub timed_sync_interval: Duration,
    /// How long an inbound connection may take to send `COMMAND_HANDSHAKE`.
    pub handshake_timeout: Duration,
    /// How long a connection may produce no frame at all before it is dropped.
    pub idle_timeout: Duration,
    /// The engine's heartbeat: timed sync, the connection maker, bookkeeping.
    pub tick_interval: Duration,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("."),
            p2p_port: P2P_DEFAULT_PORT,
            external_port: 0,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bind_ipv6: None,
            p2p_port_ipv6: 0,
            listen: true,
            seeds: Vec::new(),
            use_default_seeds: true,
            exclusive_nodes: Vec::new(),
            priority_nodes: Vec::new(),
            max_outgoing: P2P_DEFAULT_CONNECTIONS_COUNT,
            max_incoming: P2P_DEFAULT_CONNECTIONS_COUNT,
            tuning: SyncTuning::default(),
            allow_local_ip: false,
            hide_my_port: false,
            reset_peer_state: false,
            sync_to: None,
            exit_when_synced: false,
            pruned_depth: None,
            lite_start_height: None,
            lite_height_check: None,
            unlogged_initial_sync: false,
            max_inbound_per_ip: 3,
            max_inbound_per_subnet: 8,
            ban_loopback: false,
            chain_request_timeout: CHAIN_REQUEST_TIMEOUT,
            objects_timeout_min: OBJECTS_TIMEOUT_MIN,
            objects_timeout_max: OBJECTS_TIMEOUT_MAX,
            timed_sync_interval: Duration::from_secs(60),
            handshake_timeout: conn::HANDSHAKE_TIMEOUT,
            idle_timeout: conn::IDLE_TIMEOUT,
            tick_interval: TICK,
        }
    }
}

/// The chain state, shared with whoever else holds it.
///
/// The engine is single-threaded and owns the only *writer*; the RPC server
/// (`wrkz-rpc`) holds the same handle for its reads. An `RwLock` and not a
/// `Mutex` for the reason `Core` uses a `std::shared_mutex`: reads are the
/// common case and must not queue behind each other, while `addBlock` needs
/// exclusive access for exactly the length of one write batch.
pub type SharedChain<S> = Arc<RwLock<ChainState<S>>>;

/// How often an exclusive or priority node that is not connected is dialled:
/// `m_connections_maker_interval(1)` (`NetNode.cpp:283`) runs one round a
/// second, and each round dials every such node (`connect_to_peerlist`,
/// `:2851`).
const PINNED_DIAL_INTERVAL: Duration = Duration::from_secs(1);
/// The longest a priority node that keeps failing waits between dials. The C++
/// dials it every round for as long as it runs; this doubles the wait after
/// each failed dial up to here, and starts again from one second once a dial
/// succeeds. An exclusive node is never backed off: it is all the node has.
const PRIORITY_RETRY_MAX: Duration = Duration::from_secs(60);

/// The wait before the next dial of a priority node that has failed
/// `failures` times in a row.
fn priority_retry(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (PINNED_DIAL_INTERVAL * (1u32 << doublings)).min(PRIORITY_RETRY_MAX)
}

/// One `--add-exclusive-node` or `--add-priority-node` entry, resolved.
///
/// The C++ takes `a.b.c.d:port` and nothing else (`parseIpAddressAndPort`,
/// `StringTools.cpp:431`); this takes whatever [`peers::resolve`] does — a
/// hostname, an IPv6 literal, a bare host on the default port. A hostname may
/// resolve to several addresses, and they are still **one** node: it counts as
/// connected when any of them is, and a failed dial moves on to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedNode {
    /// As the operator wrote it, for the log.
    pub name: String,
    /// Where it may be reached, in the order they are tried. Never empty.
    pub addrs: Vec<SocketAddr>,
}

impl PinnedNode {
    /// Resolve `target`, on [`P2P_DEFAULT_PORT`] when it names no port.
    pub fn resolve(target: &str) -> io::Result<Self> {
        let mut addrs: Vec<SocketAddr> = Vec::new();
        for addr in peers::resolve(target, P2P_DEFAULT_PORT)? {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        if addrs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{target} resolves to no address")));
        }
        Ok(Self { name: target.to_string(), addrs })
    }

    /// A node at one known address.
    pub fn at(addr: SocketAddr) -> Self {
        Self { name: addr.to_string(), addrs: vec![addr] }
    }
}

/// Which list a [`PinnedNode`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinKind {
    Exclusive,
    Priority,
}

impl PinKind {
    fn name(self) -> &'static str {
        match self {
            PinKind::Exclusive => "exclusive",
            PinKind::Priority => "priority",
        }
    }
}

/// The engine's record of one pinned node.
struct Pinned {
    node: PinnedNode,
    kind: PinKind,
    /// Which of `node.addrs` the next dial tries.
    next: usize,
    /// No dial before this.
    retry_at: Instant,
    /// Dials that failed in a row, for a priority node's backoff.
    failures: u32,
}

struct Peer {
    ctx: PeerCtx,
    sink: Sink,
}

/// One live connection, as [`Node::connection_rows`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionRow {
    pub addr: SocketAddr,
    /// `m_is_income`.
    pub incoming: bool,
    /// 0 until the handshake associates one.
    pub peer_id: u64,
    pub state: PeerState,
    /// The peer's `P2P_CURRENT_VERSION`.
    pub version: u8,
    /// Since the connection was established (`m_started`).
    pub uptime: Duration,
    /// `m_remote_blockchain_height`.
    pub remote_height: u32,
    pub remote_is_pruned: bool,
    pub remote_is_lite: bool,
    /// `m_sync_batch_size` and `m_sync_failures`.
    pub sync_batch_size: u32,
    pub sync_failures: u32,
    /// Blocks this connection has contributed since it came up.
    pub blocks_added: u64,
}

/// What `processObjects` reports back, mirroring the C++ `int` return: 0 keeps
/// going, anything else means the batch is finished for this peer.
enum Processed {
    Continue,
    Stop,
}

/// Why a handler wants its connection closed, and whether the address is
/// charged for it ([`Offence`]). Most reasons are not offences — a peer on
/// another network, a chain we cannot extend, our own storage failing — and
/// convert from a plain string.
struct PeerError {
    reason: String,
    offence: Option<Offence>,
}

impl PeerError {
    fn offence(offence: Offence, reason: impl Into<String>) -> Self {
        Self { reason: reason.into(), offence: Some(offence) }
    }
}

impl From<String> for PeerError {
    fn from(reason: String) -> Self {
        Self { reason, offence: None }
    }
}

impl From<&str> for PeerError {
    fn from(reason: &str) -> Self {
        Self { reason: reason.to_string(), offence: None }
    }
}

type Handled = Result<(), PeerError>;

/// A message that did not decode is the sender's fault, whatever it was.
fn malformed<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> PeerError {
    move |e| PeerError::offence(Offence::Malformed, format!("{what}: {e}"))
}

/// Which block rule failures the sender answers for.
///
/// Proof of work, and everything checked from the block and its parent alone —
/// its encoding, its coinbase, its transaction list, its reward, and the
/// transactions' own shape and signatures — bans: no honest node relays or
/// serves such a block, because it checked the same rules first. A rule that
/// reads our clock, a fork's version schedule, or state the sender need not
/// share — the timestamp limits, the block version, the size median, spent
/// key images, global indexes, the fee and mixin ladders, our own ability to
/// reorganise — only drops the connection, as it always did: around a fork an
/// honest peer that has not upgraded fails exactly those.
fn block_offence(rule: &Rule) -> Option<Offence> {
    match rule {
        Rule::ProofOfWorkTooWeak { .. } => Some(Offence::BadProofOfWork),
        Rule::CheckpointBlockHashMismatch { .. } => Some(Offence::CheckpointMismatch),
        Rule::DeserializationFailed(_)
        | Rule::ParentBlockWrongVersion
        | Rule::ParentBlockSizeTooBig
        | Rule::CoinbaseInputWrongCount(_)
        | Rule::CoinbaseInputUnexpectedType
        | Rule::BaseInputWrongBlockIndex { .. }
        | Rule::CoinbaseHasSignatures
        | Rule::CoinbaseOutputZeroAmount
        | Rule::CoinbaseOutputInvalidKey
        | Rule::CoinbaseOutputsAmountOverflow
        | Rule::TransactionDuplicates
        | Rule::TransactionInconsistency
        | Rule::BlockRewardMismatch { .. } => Some(Offence::InvalidBlock),
        Rule::Transaction { rule, .. } if crate::mempool::tx_rule_is_intrinsic(rule) => Some(Offence::InvalidBlock),
        _ => None,
    }
}

fn block_error(rule: &Rule, context: &str) -> PeerError {
    PeerError { reason: format!("{context}: {rule}"), offence: block_offence(rule) }
}

/// Split transaction blobs into runs of at most [`TX_BATCH_BYTES`], one blob
/// larger than that on its own.
fn tx_batches(blobs: &[Vec<u8>]) -> Vec<&[Vec<u8>]> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (i, blob) in blobs.iter().enumerate() {
        if i > start && bytes + blob.len() > TX_BATCH_BYTES {
            out.push(&blobs[start..i]);
            (start, bytes) = (i, 0);
        }
        bytes += blob.len();
    }
    if start < blobs.len() {
        out.push(&blobs[start..]);
    }
    out
}

/// The per-block cost of each `add_block` phase over the blocks added since
/// `before`, for the progress line; empty when none were.
fn phase_line(before: &Timings, now: &Timings) -> String {
    let blocks = now.blocks.saturating_sub(before.blocks);
    if blocks == 0 {
        return String::new();
    }
    let us = |a: Duration, b: Duration| a.saturating_sub(b).as_secs_f64() * 1e6 / blocks as f64;
    format!(
        " | per block: decode {:.0}us validate {:.0}us commit {:.0}us total {:.0}us",
        us(now.decode, before.decode),
        us(now.validate, before.validate),
        us(now.commit, before.commit),
        us(now.total, before.total)
    )
}

/// The node.
pub struct Node<S: KvStore, P: TxPool> {
    chain: SharedChain<S>,
    pool: P,
    pm: PeerManager,
    cfg: NodeConfig,
    peers: HashMap<ConnId, Peer>,
    events_tx: SyncSender<Event>,
    events_rx: Receiver<Event>,
    ids: Arc<ConnIds>,
    /// Outbound dials in flight, so the connection maker does not double-dial.
    dialing: HashSet<SocketAddr>,
    /// The exclusive nodes, then the priority nodes.
    pinned: Vec<Pinned>,
    listen_addr: Option<SocketAddr>,
    /// The IPv6 listener's address, when one was configured. A second socket,
    /// never a second connection pool: both accept loops feed one engine.
    listen_addr6: Option<SocketAddr>,
    /// `m_observedHeight`: the top block index the network is believed at —
    /// the median of the handshaken peers' claims
    /// ([`recalculate_observed_height`]), not the tallest.
    observed_height: u32,
    /// `m_synchronized`: set once a majority of handshaken peers are not ahead
    /// of us ([`Node::check_synchronized`]), and never cleared.
    synchronized: bool,
    /// Some peer has said we hold its top, or a sync peer ran out of blocks:
    /// what prompts [`Node::check_synchronized`], which from then on runs on
    /// every tick until it passes.
    sync_claimed: bool,
    /// Anchors from the last run, dialled before anything else.
    anchors: Vec<SocketAddr>,
    /// The chain's phase timings at the last progress line.
    last_timings: Timings,
    started: bool,
    stop: bool,
    last_tick: Instant,
    last_peerlist_store: Instant,
    last_progress: Instant,
    progress_from_height: u32,
    progress_since: Instant,
    /// The lite-height depth check, fed by every peer's sync data.
    lite_depth: crate::daemon::LiteDepthCheck,
    /// Why the node stopped, when it stopped because it must not run on.
    fatal: Option<String>,
    /// Whether the store is writing its write-ahead log; see `commit_chain`.
    logging: bool,
    /// Where the blocks this engine applies are published.
    events: Events,
}

impl<S: KvStore, P: TxPool> Node<S, P> {
    /// Build a node over an open chain state and a pool.
    ///
    /// The peer state file is read here; the listener is bound by
    /// [`Node::start`] so a caller can inspect the configuration first.
    pub fn new(chain: ChainState<S>, pool: P, cfg: NodeConfig) -> Self {
        Self::with_shared_chain(Arc::new(RwLock::new(chain)), pool, cfg)
    }

    /// Over a chain state someone else holds a handle to as well — the RPC
    /// server, in the daemon binary.
    ///
    /// The engine takes the **write** guard only inside `add_block`, and the
    /// read guard for the handful of lookups that serve a peer, so a reader on
    /// another thread never waits longer than one block application and a block
    /// never waits longer than the reads already in flight. This is the same
    /// arrangement as the C++ `Core::m_chainMutex`, which is a
    /// `std::shared_mutex` taken shared by every read path and unique by
    /// `addBlock`.
    pub fn with_shared_chain(chain: SharedChain<S>, pool: P, cfg: NodeConfig) -> Self {
        let state_path = cfg.data_dir.join(P2P_NET_DATA_FILENAME);
        let mut pm = PeerManager::open(&state_path, cfg.allow_local_ip, cfg.reset_peer_state);
        // A node with exclusive nodes never dials a seed, so it does not look
        // one up either.
        if cfg.exclusive_nodes.is_empty() {
            pm.set_seeds(resolve_seeds(&cfg));
        }
        let anchors = pm.take_anchors();
        let last_timings = chain.read().unwrap_or_else(|p| p.into_inner()).timings();
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(net::EVENT_QUEUE);
        let now = Instant::now();
        let exclusive = cfg.exclusive_nodes.iter().map(|n| (n, PinKind::Exclusive));
        let priority = cfg.priority_nodes.iter().map(|n| (n, PinKind::Priority));
        let pinned = exclusive
            .chain(priority)
            .filter(|(node, _)| !node.addrs.is_empty())
            .map(|(node, kind)| Pinned { node: node.clone(), kind, next: 0, retry_at: now, failures: 0 })
            .collect();
        Self {
            chain,
            pool,
            pm,
            cfg,
            peers: HashMap::new(),
            events_tx,
            events_rx,
            ids: Arc::new(ConnIds::default()),
            dialing: HashSet::new(),
            pinned,
            listen_addr: None,
            listen_addr6: None,
            observed_height: 0,
            synchronized: false,
            sync_claimed: false,
            anchors,
            last_timings,
            started: false,
            stop: false,
            last_tick: now,
            last_peerlist_store: now,
            last_progress: now,
            progress_from_height: 0,
            progress_since: now,
            lite_depth: Default::default(),
            fatal: None,
            logging: true,
            events: Events::default(),
        }
    }

    /// Publish the blocks this engine applies to `events`' listeners
    /// ([`wrkz_rpc::events`]). What the pool drops or takes back is published
    /// by the pool itself ([`crate::mempool::SharedMempool::with_events`]).
    pub fn set_events(&mut self, events: Events) {
        self.events = events;
    }

    // -- accessors -----------------------------------------------------------

    /// A read guard on the chain state. Held only for as long as the caller
    /// keeps it, so a caller that wants several reads of one chain should take
    /// it once.
    pub fn chain(&self) -> RwLockReadGuard<'_, ChainState<S>> {
        self.chain_read()
    }

    /// The shared handle, for a caller that wires the same state into an RPC
    /// server.
    pub fn chain_handle(&self) -> SharedChain<S> {
        Arc::clone(&self.chain)
    }

    /// Apply one block and do the pool bookkeeping `Core::addBlock` does
    /// afterwards (`Core.cpp:1690-1730`): drop what the block mined, offer back
    /// what a chain switch unwound, and age the pool at the new height.
    ///
    /// The exclusive chain guard is held for the `add_block` call and **not**
    /// for the pool work, so a reader on another thread waits for one write
    /// batch and nothing more.
    ///
    /// `hint` is the block's proof-of-work hash when [`Node::pow_hints`]
    /// computed it ahead of time; `None` hashes inline, as before.
    fn apply_block(
        &mut self,
        block_blob: &[u8],
        tx_blobs: &[Vec<u8>],
        hint: Option<PowHint>,
    ) -> wrkz_chain::Result<AddOutcome> {
        let report = {
            let mut chain = self.chain_write();
            let report = chain.add_block_detailed_with_pow(block_blob, tx_blobs, hint)?;
            // A whole block is applied, so this is a height a batched store
            // may commit at if its limits are reached. The usual commit is
            // `commit_chain`, once per event.
            chain.block_boundary()?;
            report
        };
        if matches!(report.outcome.status, AddStatus::Main | AddStatus::AlternativeAndSwitched) {
            // Everything the new main chain now holds leaves the pool. An empty
            // pool — every block of an initial sync — has nothing to shed, so
            // the block's transactions are not parsed and hashed a second
            // time. Nothing can slip in unchecked: admission holds the chain
            // read guard through its insert (`mempool.rs`, "Locking"), so a
            // transaction validated against the chain before this block is
            // already in the pool by the time the write guard above was taken.
            if !self.pool.is_empty() {
                let hashes: Vec<Hash> = tx_blobs
                    .iter()
                    .filter_map(|b| wrkz_primitives::tx::Transaction::from_bytes(b).ok())
                    .filter_map(|t| t.hash().ok())
                    .collect();
                let images = crate::mempool::spent_key_images(tx_blobs);
                self.pool.on_block_added(report.outcome.index, &hashes, &images);
            }
            // A switch: every block of the new branch, not just this one, may
            // spend what the pool holds; then the old chain's transactions go
            // back up for admission (the order `add_block_with_pool` keeps).
            if report.outcome.status == AddStatus::AlternativeAndSwitched {
                self.pool.on_chain_switched();
            }
            if !report.unwound.is_empty() {
                let returned: Vec<Vec<u8>> =
                    report.unwound.iter().flat_map(|b| b.transactions.iter().cloned()).collect();
                self.pool.on_blocks_unwound(&returned);
            }
            self.pool.clean(report.outcome.index as u64 + 1);
        }
        if self.events.is_listening() {
            let applied = AppliedBlock {
                outcome: &report.outcome,
                block_blob,
                lowest_unwound: report.unwound.last().map(|left| left.index),
                pool_removed: &[],
                pool_restored: &[],
            };
            self.events.block_applied(&applied, |index| self.chain_hash(index).ok());
        }
        Ok(report.outcome)
    }

    fn chain_read(&self) -> RwLockReadGuard<'_, ChainState<S>> {
        self.chain.read().unwrap_or_else(|p| p.into_inner())
    }

    fn chain_write(&self) -> RwLockWriteGuard<'_, ChainState<S>> {
        self.chain.write().unwrap_or_else(|p| p.into_inner())
    }

    pub fn pool(&self) -> &P {
        &self.pool
    }

    /// Why the node stopped, when it stopped because it must not run on — the
    /// lite-height depth check. The daemon exits non-zero with it.
    pub fn fatal_error(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    pub fn peer_manager(&self) -> &PeerManager {
        &self.pm
    }

    /// `get_current_blockchain_height`: the top block index plus one.
    pub fn height(&self) -> u32 {
        self.chain_read().tip_index().map_or(0, |t| t + 1)
    }

    pub fn top_hash(&self) -> Hash {
        self.chain_read().tip_info().map(|i| i.block_hash).unwrap_or_default()
    }

    pub fn observed_height(&self) -> u32 {
        self.observed_height
    }

    pub fn is_synchronized(&self) -> bool {
        self.synchronized
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// The address the listener actually bound, once [`Node::start`] has run.
    /// Port 0 in the configuration means "any", so a test reads the real port
    /// from here.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen_addr
    }

    /// The address the IPv6 listener bound, when `--p2p-bind-ipv6-address` was
    /// given. `None` means this node has no IPv6 listener, which is the
    /// default in the C++ as well.
    pub fn listen_addr6(&self) -> Option<SocketAddr> {
        self.listen_addr6
    }

    /// The peer states, for tests and for the progress log.
    pub fn peer_states(&self) -> Vec<(SocketAddr, PeerState)> {
        self.peers.values().map(|p| (p.ctx.addr, p.ctx.state)).collect()
    }

    /// One row per live connection, in the shape the C++
    /// `connections_to_string` prints (`CryptoNoteProtocolHandler.cpp:315`).
    ///
    /// The daemon publishes this once per tick for the console's `print_cn`;
    /// the engine owns the contexts, so nothing else can read them directly.
    pub fn connection_rows(&self) -> Vec<ConnectionRow> {
        let mut rows: Vec<ConnectionRow> = self
            .peers
            .values()
            .map(|p| ConnectionRow {
                addr: p.ctx.addr,
                incoming: p.ctx.incoming,
                peer_id: p.ctx.peer_id,
                state: p.ctx.state,
                version: p.ctx.version,
                uptime: p.ctx.connected_at.elapsed(),
                remote_height: p.ctx.remote_height,
                remote_is_pruned: p.ctx.remote_is_pruned,
                remote_is_lite: p.ctx.remote_is_lite,
                sync_batch_size: p.ctx.sync_batch_size,
                sync_failures: p.ctx.sync_failures,
                blocks_added: p.ctx.blocks_added,
            })
            .collect();
        // Stable output: an operator running `print_cn` twice in a row should
        // not see the rows shuffle because the map iterated differently.
        rows.sort_by_key(|r| (r.incoming, r.addr));
        rows
    }

    /// The shared ban table (`ban list`, `ban add`, `ban delete`). Cloned out
    /// before the loop starts, so the console thread can reach it.
    pub fn ban_list(&self) -> BanList {
        self.pm.bans()
    }

    /// `(incoming, outgoing)` live connections, which is what `/info` reports
    /// as `incoming_connections_count` and `outgoing_connections_count`.
    pub fn connection_counts(&self) -> (usize, usize) {
        let incoming = self.peers.values().filter(|p| p.ctx.incoming).count();
        (incoming, self.peers.len() - incoming)
    }

    /// The number of seed addresses this node was configured with
    /// (`/info`'s `seed_nodes_count`).
    pub fn seed_count(&self) -> usize {
        self.pm.seeds().len()
    }

    // -- lifecycle -----------------------------------------------------------

    /// Bind the listener and start the heartbeat.
    pub fn start(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        if self.cfg.listen {
            // One gate for both families, as there is one engine.
            let gate = InboundGate::new(
                InboundLimits {
                    total: self.cfg.max_incoming,
                    per_ip: self.cfg.max_inbound_per_ip,
                    per_group: self.cfg.max_inbound_per_subnet,
                },
                self.pm.bans(),
            );
            let listener = TcpListener::bind(SocketAddr::new(self.cfg.bind, self.cfg.p2p_port))?;
            self.listen_addr = Some(listener.local_addr()?);
            log_info!("listening on {}", listener.local_addr()?);
            net::spawn_listener(listener, "v4", self.ids.clone(), self.events_tx.clone(), Arc::clone(&gate));

            // `NetNode.cpp:489`: a second listener, only when an address was
            // given, and IPv6-only so it cannot take the IPv4 port with it.
            if let Some(ip) = self.cfg.bind_ipv6 {
                let port = self.ipv6_listen_port();
                let listener = wrkz_p2p::bind::bind_ipv6_only(SocketAddrV6::new(ip, port, 0, 0))?;
                let addr = listener.local_addr()?;
                self.listen_addr6 = Some(addr);
                // The C++ line, verbatim: "IPv6 P2P net service bound on [x]:p".
                log_info!("IPv6 P2P net service bound on [{ip}]:{}", addr.port());
                net::spawn_listener(listener, "v6", self.ids.clone(), self.events_tx.clone(), gate);
            }
        }
        if self.cfg.external_port != 0 {
            // `NetNode.cpp:775-778`, the C++'s line.
            log_info!("External port defined as {}", self.cfg.external_port);
            if self.cfg.hide_my_port {
                log_info!("--hide-my-port advertises port 0, so the external port is not advertised");
            }
        }
        net::spawn_ticker(self.cfg.tick_interval, self.events_tx.clone());
        self.progress_from_height = self.height();
        self.progress_since = Instant::now();
        self.started = true;
        log_info!(
            "node up: height {} peer_id {:016x} ({} white, {} gray peers known)",
            self.height(),
            self.pm.peer_id(),
            self.pm.white_count(),
            self.pm.gray_count()
        );
        let names = |nodes: &[PinnedNode]| nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>().join(", ");
        if self.exclusive_mode() {
            log_info!("exclusive nodes: {}; no other peer is dialled", names(&self.cfg.exclusive_nodes));
        }
        if !self.cfg.priority_nodes.is_empty() {
            log_info!("priority nodes: {}", names(&self.cfg.priority_nodes));
        }
        Ok(())
    }

    /// The port the IPv6 listener binds: `--p2p-bind-port-ipv6` when it is
    /// non-zero, otherwise `--p2p-bind-port`
    /// (`m_bindPortIpv6 = (p2pBindPortIpv6 > 0) ? p2pBindPortIpv6 : port`,
    /// `NetNodeConfig.cpp:104`). The *configured* IPv4 port, so a daemon asked
    /// for port 0 gets an ephemeral port on each family rather than trying to
    /// reuse the one the IPv4 listener happened to be given.
    fn ipv6_listen_port(&self) -> u16 {
        if self.cfg.p2p_port_ipv6 != 0 {
            self.cfg.p2p_port_ipv6
        } else {
            self.cfg.p2p_port
        }
    }

    /// Dial one address now, outside the connection maker's target counting.
    pub fn connect_to(&mut self, addr: SocketAddr) {
        if self.dialing.contains(&addr) || self.peers.values().any(|p| p.ctx.addr == addr) {
            return;
        }
        self.dialing.insert(addr);
        let id = self.ids.next();
        log_debug!("dialing {addr}");
        net::spawn_outbound(id, addr, self.node_data(), self.core_sync_data(), self.events_tx.clone());
    }

    /// `--add-peer`: onto the white list, as the C++ puts it there at start
    /// (`NetNode.cpp:742-745`), and dialled now rather than whenever the list
    /// selection happens to pick it — unless exclusive nodes are configured,
    /// which leaves it on the list and undialled, as the C++ leaves it.
    pub fn add_peer(&mut self, addr: SocketAddr) {
        self.pm.add_command_line_peer(addr);
        if !self.exclusive_mode() {
            self.connect_to(addr);
        }
    }

    /// Whether `--add-exclusive-node` was given: then nothing else is dialled.
    pub fn exclusive_mode(&self) -> bool {
        !self.cfg.exclusive_nodes.is_empty()
    }

    /// Whether `addr` is one of an exclusive or priority node's addresses.
    fn is_pinned(&self, addr: SocketAddr) -> bool {
        self.pinned.iter().any(|p| p.node.addrs.contains(&addr))
    }

    fn has_outbound(&self, addr: SocketAddr) -> bool {
        self.peers.values().any(|p| !p.ctx.incoming && p.ctx.addr == addr)
    }

    /// Handle one event, waiting at most `timeout`. Returns false on timeout or
    /// when the event channel is gone.
    ///
    /// The periodic work also runs from here when nothing arrives, so the timed
    /// sync, the connection maker and the timeouts do not depend on the ticker
    /// thread being scheduled — a node whose ticker never started would
    /// otherwise never drop a stalled peer or dial a replacement.
    pub fn step(&mut self, timeout: Duration) -> bool {
        let handled = match self.events_rx.recv_timeout(timeout) {
            Ok(event) => {
                self.handle_event(event);
                true
            }
            Err(RecvTimeoutError::Timeout) => {
                self.maybe_tick();
                false
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.stop = true;
                false
            }
        };
        self.commit_chain();
        handled
    }

    /// Hand whatever the chain state's store is holding back to the engine.
    ///
    /// Behind a [`wrkz_storage::batch::BatchStore`] the blocks of one
    /// downloaded batch accumulate in its overlay and reach RocksDB here as
    /// **one** write batch instead of one per block — the import's batching,
    /// brought to P2P sync. It runs once per event, so a relayed block at the
    /// tip is still committed on its own, as before. Every read, the RPC's
    /// included, consults the overlay first, so nothing sees a stale chain in
    /// between. On a store that holds nothing back this is one read-guarded
    /// check.
    ///
    /// A failed commit is ours, not a peer's: the node stops, and the state on
    /// disk is the previous commit, a whole number of blocks, because the
    /// resume height travels in the same atomic batch as the blocks it covers.
    fn commit_chain(&mut self) {
        if self.chain_read().pending_bytes() != 0 {
            let committed = self.chain_write().flush();
            if let Err(e) = committed {
                log_error!("chain state commit failed: {e}");
                self.stop = true;
                return;
            }
        }
        // Initial sync with the write-ahead log off, as the import runs. Safe
        // for the reason batching is: the resume height is in the same atomic
        // batch as its blocks and the engine flushes memtables in order, so a
        // crash costs the blocks since the last memtable flush and never a
        // consistent state. The log comes back on — which first puts every
        // unlogged write into an SST file — the moment a peer confirms we hold
        // its top block, and stays on.
        let want_log = !self.cfg.unlogged_initial_sync || self.synchronized;
        if want_log != self.logging {
            let switched = self.chain_write().set_write_ahead_log(want_log);
            match switched {
                Ok(()) => {
                    self.logging = want_log;
                    if want_log {
                        log_info!("synchronized: chain state writes go through the write-ahead log again");
                    } else {
                        log_info!("initial sync: chain state written without the write-ahead log until synchronized");
                    }
                }
                Err(e) => {
                    log_error!("could not switch the write-ahead log {}: {e}", if want_log { "on" } else { "off" });
                    self.stop = true;
                }
            }
        }
    }

    /// Run [`Node::on_tick`] if a tick interval has passed since the last one,
    /// whether the wake-up came from the ticker thread or from an idle `step`.
    fn maybe_tick(&mut self) {
        if self.last_tick.elapsed() >= self.cfg.tick_interval {
            self.last_tick = Instant::now();
            self.on_tick();
        }
    }

    /// Run until a stop condition fires or `budget` elapses. Returns whether it
    /// stopped for a reason rather than for the budget.
    pub fn run_for(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let step = self.cfg.tick_interval;
        while !self.should_stop() && Instant::now() < deadline {
            self.step(step);
        }
        let stopped = self.should_stop();
        if stopped {
            self.shutdown();
        }
        stopped
    }

    /// Run until a stop condition fires.
    pub fn run(&mut self) {
        let step = self.cfg.tick_interval;
        while !self.should_stop() {
            self.step(step);
        }
        self.shutdown();
    }

    /// `--sync-to` reached, `--exit-when-synced` satisfied, or a fatal error.
    pub fn should_stop(&self) -> bool {
        if self.stop {
            return true;
        }
        if let Some(target) = self.cfg.sync_to {
            if self.chain_read().tip_index().unwrap_or(0) >= target {
                return true;
            }
        }
        self.cfg.exit_when_synced && self.synchronized
    }

    /// Close every connection and write the peer state file, the ban list and
    /// the anchors.
    pub fn shutdown(&mut self) {
        if let Err(e) = self.pm.save_anchors(&self.anchor_candidates()) {
            log_warn!("could not write the anchor peers: {e}");
        }
        for peer in self.peers.values() {
            peer.sink.close();
        }
        self.peers.clear();
        self.commit_chain();
        if let Err(e) = self.pm.save() {
            log_warn!("could not write the peer state file: {e}");
        }
        log_info!(
            "node down: height {} (network {}), {} blocks/s over the run",
            self.height(),
            self.observed_height + 1,
            self.progress_rate()
        );
    }

    // -- our own sync data ---------------------------------------------------

    /// `get_local_node_data` (`NetNode.cpp:2049-2066`): `my_port` is 0 with
    /// `--hide-my-port`, else `--p2p-external-port` when one was given, else
    /// the port the listener bound.
    fn node_data(&self) -> BasicNodeData {
        let my_port = if self.cfg.hide_my_port || !self.cfg.listen {
            0
        } else if self.cfg.external_port != 0 {
            // The port a NAT forwards to us, which is where a peer's back ping
            // has to arrive.
            u32::from(self.cfg.external_port)
        } else {
            // One `my_port` is all the handshake carries, and the C++ sends
            // `m_listeningPort` — the IPv4 listener's — even when the IPv6 one
            // is up on a different port. A peer that reached us over IPv6 and
            // back-pings this port therefore has to be reachable on the same
            // number, which is why `--p2p-bind-port-ipv6` defaults to the IPv4
            // port. The fallback below only matters for a node whose IPv4
            // listener never came up.
            self.listen_addr.or(self.listen_addr6).map(|a| a.port()).unwrap_or(self.cfg.p2p_port) as u32
        };
        BasicNodeData::ours(self.pm.peer_id(), my_port)
    }

    /// `get_payload_sync_data` (`CryptoNoteProtocolHandler.cpp:614`).
    fn core_sync_data(&self) -> CoreSyncData {
        let height = self.height();
        let mut flags = 0;
        let mut pruned_node_height = 0;
        let mut lite_start_height = 0;
        if let Some(depth) = self.cfg.pruned_depth {
            flags |= msg::NODE_CAPABILITY_FLAG_PRUNED;
            pruned_node_height = height.saturating_sub(depth);
        }
        if let Some(start) = self.cfg.lite_start_height {
            flags |= msg::NODE_CAPABILITY_FLAG_LITE;
            lite_start_height = start;
        }
        CoreSyncData {
            current_height: height,
            top_id: self.top_hash(),
            capability_flags: flags,
            pruned_node_height,
            lite_start_height,
        }
    }

    // -- event dispatch ------------------------------------------------------

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Established { id, addr, incoming, sink, handshake } => {
                self.on_established(id, addr, incoming, sink, handshake.map(|b| *b));
            }
            Event::Frame { id, header, payload, ticket } => {
                self.on_frame(id, header, payload);
                self.pump_state(id);
                // Only now may the peer's reader take another frame's share.
                drop(ticket);
            }
            Event::BadFrame { id, fault, reason } => {
                let offence = match fault {
                    FrameFault::Malformed => Offence::Malformed,
                    FrameFault::Oversized => Offence::Oversized,
                };
                self.penalise(id, offence, &reason);
                self.drop_peer(id, &reason);
            }
            Event::Closed { id, reason } => {
                if let Some(peer) = self.peers.remove(&id) {
                    log_debug!("{} closed: {reason}", peer.ctx.label());
                    peer.sink.close();
                    self.recalculate_observed_height();
                }
            }
            Event::DialFailed { addr, reason } => {
                self.dialing.remove(&addr);
                log_debug!("dial {addr} failed: {reason}");
                self.pm.note_dial_failure(addr);
                self.on_pinned_dial_failed(addr);
            }
            Event::BackPing { id, ip, port, peer_id, ok } => self.on_back_ping(id, ip, port, peer_id, ok),
            Event::Tick => self.maybe_tick(),
        }
    }

    fn on_established(
        &mut self,
        id: ConnId,
        addr: SocketAddr,
        incoming: bool,
        sink: Sink,
        handshake: Option<msg::HandshakeResponse>,
    ) {
        self.dialing.remove(&addr);
        if self.pm.is_banned(addr.ip()) {
            log_debug!("refusing {addr}: banned");
            sink.close();
            return;
        }
        let cap = if incoming { self.cfg.max_incoming } else { self.cfg.max_outgoing };
        let same_direction = self.peers.values().filter(|p| p.ctx.incoming == incoming).count();
        // An exclusive or priority node is dialled whatever the outbound count
        // says, and its connection counts toward `--out-peers` without ever
        // being refused for it: `try_to_connect_and_handshake_with_new_peer`
        // checks no count (`NetNode.cpp:1121-1209`).
        let pinned = !incoming && self.is_pinned(addr);
        if pinned {
            for p in self.pinned.iter_mut().filter(|p| p.node.addrs.contains(&addr)) {
                p.failures = 0;
            }
        }
        if same_direction >= cap && !pinned {
            log_debug!(
                "refusing {addr}: {} connection limit {cap} reached",
                if incoming { "inbound" } else { "outbound" }
            );
            sink.close();
            return;
        }
        let ctx = PeerCtx::new(id, addr, incoming, &self.cfg.tuning);
        self.peers.insert(id, Peer { ctx, sink });

        let Some(hs) = handshake else {
            // Inbound: its COMMAND_HANDSHAKE arrives as a frame.
            log_debug!("inbound connection from {addr}");
            return;
        };

        // The outbound handshake path of `NodeServer::handshake`, in its order:
        // peer list first, then the sync data, then the peer id.
        let now = peers::now_secs();
        if !self.pm.merge_peerlist(&hs.local_peerlist, hs.node_data.local_time, now) {
            return self.drop_peer(id, "handshake peer list has a future last_seen");
        }
        if hs.node_data.version >= P2P_IPV6_CAPABILITY_VERSION && !hs.local_peerlist6.is_empty() {
            self.pm.merge_peerlist6(&hs.local_peerlist6);
        }
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.ctx.version = hs.node_data.version;
            peer.ctx.my_port = hs.node_data.my_port;
            peer.ctx.handshake_done = true;
        }
        if hs.node_data.peer_id == self.pm.peer_id() {
            return self.drop_peer(id, "connected to self");
        }
        if !self.process_payload_sync_data(id, &hs.payload_data, true) {
            return self.drop_peer(id, "handshake sync data rejected");
        }
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.ctx.peer_id = hs.node_data.peer_id;
        }
        // A successful outbound handshake is what makes a peer white.
        self.pm.set_peer_just_seen(hs.node_data.peer_id, addr, now);
        log_info!(
            "handshake with {addr} ok: version {} height {} (we are at {})",
            hs.node_data.version,
            hs.payload_data.current_height,
            self.height()
        );
        self.pump_state(id);
    }

    /// `try_ping` succeeded: the peer really listens where it said, so it goes
    /// on the white list (`handle_handshake`, `NetNode.cpp:2288`).
    fn on_back_ping(&mut self, id: ConnId, ip: IpAddr, port: u32, peer_id: u64, ok: bool) {
        if !ok {
            log_debug!("back ping to {ip}:{port} failed; not adding to the white list");
            return;
        }
        let now = peers::now_secs();
        match ip {
            IpAddr::V4(v4) => {
                self.pm.append_white(msg::PeerlistEntry { ip: v4.octets(), port, id: peer_id, last_seen: now })
            }
            IpAddr::V6(v6) => {
                self.pm.append_white6(msg::PeerlistEntry6 { id: peer_id, last_seen: now, ip: v6.octets(), port })
            }
        }
        log_debug!("back ping to {ip}:{port} ok, added to the white list (conn {id})");
    }

    fn on_tick(&mut self) {
        self.pm.expire();
        self.timed_sync();
        self.expire_connections();
        self.expire_sync_requests();
        self.serve_deferred_objects();
        self.check_synchronized();
        self.connections_maker();
        if self.last_peerlist_store.elapsed() >= PEERLIST_STORE_INTERVAL {
            self.last_peerlist_store = Instant::now();
            if let Err(e) = self.pm.save() {
                log_warn!("could not write the peer state file: {e}");
            }
            if let Err(e) = self.pm.save_anchors(&self.anchor_candidates()) {
                log_warn!("could not write the anchor peers: {e}");
            }
        }
        if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.last_progress = Instant::now();
            self.log_progress();
        }
    }

    fn progress_rate(&self) -> f64 {
        let elapsed = self.progress_since.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        (self.height().saturating_sub(self.progress_from_height)) as f64 / elapsed
    }

    /// The progress line, with the per-block cost of each `add_block` phase
    /// over the interval when blocks were added in it.
    fn log_progress(&mut self) {
        let height = self.height();
        let network = self.observed_height.saturating_add(1).max(height);
        let syncing = self.peers.values().filter(|p| p.ctx.state == PeerState::Synchronizing).count();
        let timings = self.chain_read().timings();
        let phases = phase_line(&self.last_timings, &timings);
        self.last_timings = timings;
        log_info!(
            "height {height}/{network} ({:.1}%) peers {} ({syncing} syncing) {:.1} blocks/s pool {}{phases}",
            if network == 0 { 100.0 } else { height as f64 * 100.0 / network as f64 },
            self.peers.len(),
            self.progress_rate(),
            self.pool.len()
        );
    }

    /// Close connections that stopped speaking, and inbound ones that never
    /// handshook (`timeoutLoop`, `NetNode.cpp:2760`).
    ///
    /// The handshake deadline runs from when the connection was accepted, not
    /// from its last frame: a ping is allowed before the handshake, and a peer
    /// pinging forever must not hold an inbound slot without ever handshaking.
    fn expire_connections(&mut self) {
        let handshake_timeout = self.cfg.handshake_timeout;
        let idle_timeout = self.cfg.idle_timeout;
        let stale: Vec<(ConnId, &'static str)> = self
            .peers
            .values()
            .filter_map(|p| {
                if !p.ctx.handshake_done && p.ctx.connected_at.elapsed() > handshake_timeout {
                    Some((p.ctx.id, "no handshake within the handshake timeout"))
                } else if p.ctx.last_frame.elapsed() > idle_timeout {
                    Some((p.ctx.id, "idle: no frame within the timeout"))
                } else {
                    None
                }
            })
            .collect();
        for (id, reason) in stale {
            self.drop_peer(id, reason);
        }
    }

    /// Take a sync peer off sync when its chain or get-objects request has
    /// passed its deadline, however many other frames it has sent meanwhile.
    ///
    /// The C++ has only the idle timer, which any frame resets, so a peer that
    /// answers pings and timed syncs but never the chain request holds a sync
    /// slot for as long as it likes; three of them hold all three.
    fn expire_sync_requests(&mut self) {
        let now = Instant::now();
        let expired: Vec<(ConnId, &'static str)> = self
            .peers
            .values()
            .filter_map(|p| {
                let grace = if p.sink.frames_waiting() > 0 { REQUEST_DEADLINE_GRACE } else { Duration::ZERO };
                let passed = |d: Option<Instant>| d.is_some_and(|d| now >= d + grace);
                if passed(p.ctx.chain_request_deadline) {
                    Some((p.ctx.id, "chain request"))
                } else if passed(p.ctx.objects_deadline) {
                    Some((p.ctx.id, "get-objects request"))
                } else {
                    None
                }
            })
            .collect();
        for (id, what) in expired {
            self.on_request_timeout(id, what);
        }
    }

    /// A request passed its deadline: count a sync failure (`onSyncChunkFailure`),
    /// forget the batch, arrange for the late answer to be dropped silently,
    /// and put the peer back on relay duty — or drop it at the failure
    /// threshold — and hand the sync to another peer.
    fn on_request_timeout(&mut self, id: ConnId, what: &str) {
        let tuning = self.cfg.tuning;
        let dropped = {
            let Some(peer) = self.peers.get_mut(&id) else { return };
            if peer.ctx.chain_request_deadline.take().is_some() {
                peer.ctx.discard_next_chain_entry = true;
            }
            if peer.ctx.objects_deadline.take().is_some() {
                peer.ctx.discard_next_objects_response = true;
            }
            peer.ctx.clear_sync_lists();
            peer.ctx.pipelined_objects_outstanding = false;
            let dropped = peer.ctx.on_chunk_failure(&tuning);
            if !dropped {
                peer.ctx.state = PeerState::Normal;
            }
            log_info!(
                "{}: {what} not answered in time; off sync ({} of {} failures)",
                peer.ctx.label(),
                peer.ctx.sync_failures,
                tuning.peer_failure_threshold
            );
            dropped
        };
        if dropped {
            self.drop_peer(id, "too many sync failures");
        }
        self.hand_sync_to_another_peer(id);
    }

    /// Give a freed sync slot to a handshaken peer on relay duty that claims
    /// to be ahead of us, instead of waiting up to a minute for its next timed
    /// sync to do it. The peer with the fewest failures and the most blocks
    /// delivered goes first; the tallest claim is deliberately not the
    /// criterion.
    fn hand_sync_to_another_peer(&mut self, except: ConnId) {
        let height = self.height();
        let tuning = self.cfg.tuning;
        let syncing = self.peers.values().filter(|p| p.ctx.state.is_syncing()).count();
        if tuning.max_peers > 0 && syncing >= tuning.max_peers {
            return;
        }
        let next = self
            .peers
            .values()
            .filter(|p| {
                p.ctx.id != except
                    && p.ctx.handshake_done
                    && matches!(p.ctx.state, PeerState::Normal | PeerState::Idle)
                    && p.ctx.remote_height > height
                    && p.ctx.can_serve_our_chain(height)
            })
            .min_by_key(|p| (p.ctx.sync_failures, std::cmp::Reverse(p.ctx.blocks_added)))
            .map(|p| p.ctx.id);
        if let Some(next) = next {
            if let Some(peer) = self.peers.get_mut(&next) {
                peer.ctx.state = PeerState::SyncRequired;
            }
            self.pump_state(next);
        }
    }

    /// `timedSync` (`NetNode.cpp:934`): every 60 s, to every handshaken
    /// connection in state normal or idle — and, unlike the C++, not to one
    /// whose previous timed sync is still unanswered, so that exactly one
    /// response is ever expected.
    fn timed_sync(&mut self) {
        let interval = self.cfg.timed_sync_interval;
        let payload = msg::timed_sync_request(&self.core_sync_data());
        let due: Vec<ConnId> = self
            .peers
            .values()
            .filter(|p| {
                p.ctx.handshake_done
                    && !p.ctx.timed_sync_outstanding
                    && p.ctx.state.takes_timed_sync()
                    && p.ctx.last_timed_sync.elapsed() >= interval
            })
            .map(|p| p.ctx.id)
            .collect();
        for id in due {
            if let Some(peer) = self.peers.get_mut(&id) {
                peer.ctx.last_timed_sync = Instant::now();
                peer.ctx.timed_sync_outstanding = true;
            }
            self.post_request(id, msg::COMMAND_TIMED_SYNC, payload.clone());
        }
    }

    /// `connections_maker` (`NetNode.cpp:1618`), once per tick:
    ///
    /// 1. every exclusive node without an outbound connection is dialled, and
    ///    when any is configured that is all — no seeds, no anchors, no peer
    ///    list (`:1622-1630`);
    /// 2. every priority node likewise (`:1645`);
    /// 3. the anchors of the last run, once;
    /// 4. up to the outbound target, 70% of the candidates from the white
    ///    list, one per network group ([`PeerManager::dial_candidates_for`]),
    ///    the seeds as the fallback.
    ///
    /// The C++ contacts the seeds before the priority nodes when its white
    /// list is empty; here the seeds are the candidates' fallback, after them.
    fn connections_maker(&mut self) {
        self.dial_pinned(PinKind::Exclusive);
        if self.exclusive_mode() {
            return;
        }
        self.dial_pinned(PinKind::Priority);
        let outgoing = self.peers.values().filter(|p| !p.ctx.incoming).count() + self.dialing.len();
        let mut want = self.cfg.max_outgoing.saturating_sub(outgoing);
        if want == 0 {
            return;
        }
        if !self.anchors.is_empty() {
            for addr in std::mem::take(&mut self.anchors) {
                if want > 0 && !self.is_pinned(addr) && self.pm.may_dial(addr) {
                    log_debug!("dialing anchor {addr}");
                    self.connect_to(addr);
                    want -= 1;
                }
            }
        }
        let mut busy: Vec<SocketAddr> = self.peers.values().map(|p| p.ctx.addr).collect();
        busy.extend(self.dialing.iter().copied());
        // A priority node is dialled by its own schedule, never a second time
        // by the list selection, even while it waits out a failed dial.
        busy.extend(self.pinned.iter().flat_map(|p| p.node.addrs.iter().copied()));
        let outbound: Vec<SocketAddr> = self
            .peers
            .values()
            .filter(|p| !p.ctx.incoming)
            .map(|p| p.ctx.addr)
            .chain(self.dialing.iter().copied())
            .collect();
        for addr in self.pm.dial_candidates_for(want, &busy, &outbound) {
            self.connect_to(addr);
        }
    }

    /// `connect_to_peerlist` (`NetNode.cpp:2851-2862`) for one of the two
    /// lists: dial each node that has neither an outbound connection — an
    /// inbound one from the same address does not count (`is_addr_connected`,
    /// `:1082`) — nor a dial in flight. A banned host is skipped (`:1127`), a
    /// failed dial is not reported, and nothing ever gives up.
    fn dial_pinned(&mut self, kind: PinKind) {
        let now = Instant::now();
        let due: Vec<usize> = self
            .pinned
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == kind && now >= p.retry_at)
            .filter(|(_, p)| !p.node.addrs.iter().any(|a| self.dialing.contains(a) || self.has_outbound(*a)))
            .map(|(i, _)| i)
            .collect();
        for i in due {
            let addr = {
                let p = &mut self.pinned[i];
                p.retry_at = now + PINNED_DIAL_INTERVAL;
                p.node.addrs[p.next % p.node.addrs.len()]
            };
            if self.pm.is_banned(addr.ip()) {
                log_debug!("not dialling {} node {addr}: banned", kind.name());
                self.pinned[i].next = self.pinned[i].next.wrapping_add(1);
                continue;
            }
            log_debug!("dialling {} node {addr}", kind.name());
            self.connect_to(addr);
        }
    }

    /// A dial to a pinned node failed: its next dial tries its next address,
    /// and a priority node waits longer each time ([`priority_retry`]).
    fn on_pinned_dial_failed(&mut self, addr: SocketAddr) {
        let now = Instant::now();
        for p in self.pinned.iter_mut().filter(|p| p.node.addrs.contains(&addr)) {
            p.next = p.next.wrapping_add(1);
            if p.kind == PinKind::Priority {
                p.failures = p.failures.saturating_add(1);
                p.retry_at = p.retry_at.max(now + priority_retry(p.failures));
            }
        }
    }

    /// The outbound peers worth dialling first next time: handshaken, longest
    /// connected first, at most [`MAX_ANCHORS`].
    fn anchor_candidates(&self) -> Vec<SocketAddr> {
        let mut outbound: Vec<&Peer> = self.peers.values().filter(|p| !p.ctx.incoming && p.ctx.handshake_done).collect();
        outbound.sort_by_key(|p| p.ctx.connected_at);
        outbound.iter().take(MAX_ANCHORS).map(|p| p.ctx.addr).collect()
    }

    // -- sending -------------------------------------------------------------

    fn sink(&self, id: ConnId) -> Option<Sink> {
        self.peers.get(&id).map(|p| p.sink.clone())
    }

    /// `post_notify`: a notification. A queue that will not take it is the C++
    /// write-buffer overflow, and the connection goes.
    fn post(&mut self, id: ConnId, command: u32, payload: Vec<u8>) {
        let Some(sink) = self.sink(id) else { return };
        log_trace!("-> conn {id} notify {command} ({} bytes)", payload.len());
        if !sink.notify(command, payload) {
            self.drop_peer(id, "write queue full");
        }
    }

    fn post_request(&mut self, id: ConnId, command: u32, payload: Vec<u8>) {
        let Some(sink) = self.sink(id) else { return };
        if !sink.request(command, payload) {
            self.drop_peer(id, "write queue full");
        }
    }

    fn post_reply(&mut self, id: ConnId, command: u32, code: i32, payload: Vec<u8>) {
        let Some(sink) = self.sink(id) else { return };
        if !sink.reply(command, code, payload) {
            self.drop_peer(id, "write queue full");
        }
    }

    /// `context.m_state = state_shutdown` plus the close the connection handler
    /// performs when it sees it.
    fn drop_peer(&mut self, id: ConnId, reason: &str) {
        if let Some(peer) = self.peers.remove(&id) {
            log_debug!("dropping {}: {reason}", peer.ctx.label());
            peer.sink.close();
        }
        self.recalculate_observed_height();
    }

    /// `updateObservedHeight` (`CryptoNoteProtocolHandler.cpp:1656`), as the
    /// median of the handshaken peers' claims rather than the tallest: see
    /// [`recalculate_observed_height`]. Run whenever a claim changes.
    fn recalculate_observed_height(&mut self) {
        self.observed_height = recalculate_observed_height(self.peers.values().map(|p| &p.ctx));
    }

    /// Charge `offence` to the address of connection `id` (see
    /// [`Node::penalise_ip`]).
    fn penalise(&mut self, id: ConnId, offence: Offence, reason: &str) {
        if let Some(ip) = self.peers.get(&id).map(|p| p.ctx.addr.ip()) {
            self.penalise_ip(ip, offence, reason);
        }
    }

    /// Score an address; at [`peers::BAN_THRESHOLD`] it is banned for
    /// [`MISBEHAVIOUR_BAN_SECONDS`], the ban is written out at once, and every
    /// connection from it is closed. Loopback is only logged unless
    /// [`NodeConfig::ban_loopback`] is set.
    fn penalise_ip(&mut self, ip: IpAddr, offence: Offence, reason: &str) {
        let ip = ip.to_canonical();
        if ip.is_loopback() && !self.cfg.ban_loopback {
            log_debug!("{ip}: {offence:?} ({reason}); loopback is not scored");
            return;
        }
        if !self.pm.penalise(ip, offence.points()) {
            log_debug!("{ip}: {offence:?} ({reason}), misbehaviour score {}", self.pm.score(ip));
            return;
        }
        log_warn!("banning {ip} for {} h: {offence:?} ({reason})", MISBEHAVIOUR_BAN_SECONDS / 3600);
        if let Err(e) = self.pm.save_bans() {
            log_warn!("could not write the ban list: {e}");
        }
        let same: Vec<ConnId> =
            self.peers.values().filter(|p| p.ctx.addr.ip().to_canonical() == ip).map(|p| p.ctx.id).collect();
        for id in same {
            self.drop_peer(id, "banned");
        }
    }

    /// Close a connection for a handler's error, scoring it first when it is
    /// an offence.
    fn fail_peer(&mut self, id: ConnId, error: PeerError) {
        if let Some(offence) = error.offence {
            self.penalise(id, offence, &error.reason);
        }
        self.drop_peer(id, &error.reason);
    }

    // -- the connection loop transitions ------------------------------------

    /// `NetNode::connectionHandler` (`NetNode.cpp:2843`): `sync_required`
    /// becomes `synchronizing` with a chain request, and `pool_sync_required`
    /// becomes `normal` with a pool request. Run after every frame, which is
    /// where the C++ loop checks.
    fn pump_state(&mut self, id: ConnId) {
        self.serve_deferred_objects_for(id);
        let Some(state) = self.peers.get(&id).map(|p| p.ctx.state) else { return };
        match state {
            PeerState::SyncRequired => {
                self.request_chain_if_peer_can_serve(id, "sync required");
            }
            PeerState::PoolSyncRequired => {
                if let Some(peer) = self.peers.get_mut(&id) {
                    peer.ctx.state = PeerState::Normal;
                }
                self.request_missing_pool_transactions(id);
            }
            _ => {}
        }
    }

    /// `requestMissingPoolTransactions` (`:1640`).
    fn request_missing_pool_transactions(&mut self, id: ConnId) {
        let hashes = self.pool.transaction_hashes();
        self.post(id, msg::NOTIFY_REQUEST_TX_POOL, msg::request_tx_pool(&hashes));
    }

    /// `requestChainIfPeerCanServe` (`:1963`). A peer whose floor is above us
    /// is put back on relay duty rather than dropped.
    ///
    /// At most one chain request is in flight per peer: its answer is the only
    /// chain entry the peer may send, so a second request would make the
    /// second answer look unsolicited. One already in flight serves.
    fn request_chain_if_peer_can_serve(&mut self, id: ConnId, reason: &str) -> bool {
        let height = self.height();
        let timeout = self.cfg.chain_request_timeout;
        let Some(peer) = self.peers.get_mut(&id) else { return false };
        if peer.ctx.chain_request_deadline.is_some() {
            peer.ctx.state = PeerState::Synchronizing;
            return true;
        }
        if !peer.ctx.can_serve_our_chain(height) {
            log_debug!(
                "{}: {reason}, but it serves blocks only from {}; keeping it on relay duty",
                peer.ctx.label(),
                peer.ctx.serving_floor()
            );
            peer.ctx.state = PeerState::Normal;
            return false;
        }
        peer.ctx.state = PeerState::Synchronizing;
        let sparse = match self.build_sparse_chain() {
            Ok(s) => s,
            Err(e) => {
                log_error!("cannot build the sparse chain: {e}");
                self.stop = true;
                return false;
            }
        };
        log_debug!("conn {id}: {reason} -> NOTIFY_REQUEST_CHAIN ({} ids)", sparse.len());
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.ctx.chain_request_deadline = Some(Instant::now() + timeout);
        }
        self.post(id, msg::NOTIFY_REQUEST_CHAIN, msg::request_chain(&sparse));
        true
    }

    /// `Core::doBuildSparseChain` (`Core.cpp:4081`), resolved by index so the
    /// whole chain is never materialised.
    fn build_sparse_chain(&self) -> wrkz_chain::Result<Vec<Hash>> {
        let tip = self.chain_read().tip_index().unwrap_or(0);
        let mut out = Vec::new();
        for index in msg::sparse_chain_indices(tip) {
            match self.chain_read().block_info(index)? {
                Some(info) => out.push(info.block_hash),
                None => return Err(ChainError::Corrupt(format!("no block info at index {index}"))),
            }
        }
        Ok(out)
    }

    // -- process_payload_sync_data ------------------------------------------

    /// `process_payload_sync_data` (`CryptoNoteProtocolHandler.cpp:386`), the
    /// five-way decision of spec/08. Returns false when the connection must be
    /// dropped.
    fn process_payload_sync_data(&mut self, id: ConnId, sync: &CoreSyncData, is_initial: bool) -> bool {
        // Every read of the chain and of the other connections happens before
        // the peer is borrowed mutably.
        let current_height = self.height();
        if let Some(lite_height) = self.cfg.lite_height_check {
            let verdict =
                self.lite_depth.observe(lite_height, u64::from(current_height), u64::from(sync.current_height));
            if let Err(message) = verdict {
                log_error!("{message}");
                self.fatal = Some(message);
                self.stop = true;
                return false;
            }
        }
        let top_known = self.chain_read().has_block(&sync.top_id);
        let has_top = match top_known {
            Ok(v) => v,
            Err(e) => {
                log_error!("chain read failed: {e}");
                self.stop = true;
                return false;
            }
        };
        let active_sync_peers = self.peers.values().filter(|p| p.ctx.id != id && p.ctx.state.is_syncing()).count();
        let tuning = self.cfg.tuning;
        let we_are_pruned = self.cfg.pruned_depth.is_some();

        let mut declare_synchronized = false;
        {
            let Some(peer) = self.peers.get_mut(&id) else { return false };
            peer.ctx.record_capabilities(sync);
            if is_initial {
                peer.ctx.reset_sync_counters(&tuning);
            }
            // A timed sync that arrives before the handshake changes nothing.
            if peer.ctx.state == PeerState::BeforeHandshake && !is_initial {
                return true;
            }

            if peer.ctx.state == PeerState::Synchronizing {
                // A sync already in flight is left alone.
            } else if has_top {
                if is_initial {
                    declare_synchronized = true;
                    peer.ctx.state = PeerState::PoolSyncRequired;
                } else {
                    peer.ctx.state = PeerState::Normal;
                }
            } else {
                let fork_active = prune_capability_fork_active(current_height as u64, sync.current_height as u64);
                let full_node_must_use_full_peer = fork_active && !we_are_pruned && peer.ctx.remote_is_pruned;
                let peer_starts_above_us = !peer.ctx.can_serve_our_chain(current_height);
                let capped = tuning.max_peers > 0 && active_sync_peers >= tuning.max_peers;

                if full_node_must_use_full_peer || peer_starts_above_us || capped {
                    if peer_starts_above_us {
                        log_debug!(
                            "{}: serves blocks only from {}, above our height {current_height}; relay duty only",
                            peer.ctx.label(),
                            peer.ctx.serving_floor()
                        );
                    } else if full_node_must_use_full_peer {
                        log_debug!("{}: pruned peer after the prune fork; relay duty only", peer.ctx.label());
                    } else {
                        log_debug!("{}: sync peer cap {} reached; relay duty only", peer.ctx.label(), tuning.max_peers);
                    }
                    peer.ctx.state = if is_initial { PeerState::PoolSyncRequired } else { PeerState::Normal };
                } else {
                    peer.ctx.state = PeerState::SyncRequired;
                }
            }
            peer.ctx.remote_height = sync.current_height;
        }
        self.recalculate_observed_height();
        if declare_synchronized {
            self.on_connection_synchronized();
        }
        true
    }

    /// `on_connection_synchronized` (`:1413`): a peer tells us we hold its top
    /// block, or a sync peer has nothing more for us. The C++ sets
    /// `m_synchronized` right here, on that one peer's word; here the word only
    /// prompts [`Node::check_synchronized`].
    fn on_connection_synchronized(&mut self) {
        self.sync_claimed = true;
        self.check_synchronized();
    }

    /// Set `synchronized` — which turns the write-ahead log back on, satisfies
    /// `--exit-when-synced` and is what `/info` tells wallets — once a strict
    /// majority of handshaken peers are no more than a block ahead of us
    /// ([`majority_not_ahead`]). Rechecked on every tick after the first
    /// claim, so a node that caught up while the majority was still out is not
    /// left unsynchronized.
    fn check_synchronized(&mut self) {
        if self.synchronized || !self.sync_claimed {
            return;
        }
        let height = self.height();
        if !majority_not_ahead(self.peers.values().map(|p| &p.ctx), height) {
            log_trace!("at height {height}, but most peers claim more; not synchronized yet");
            return;
        }
        self.synchronized = true;
        log_info!("synchronized with the network at height {height}");
    }

    // -- frame dispatch ------------------------------------------------------

    fn on_frame(&mut self, id: ConnId, header: Header, payload: Vec<u8>) {
        let Some(peer) = self.peers.get_mut(&id) else { return };
        peer.ctx.last_frame = Instant::now();
        let handshaken = peer.ctx.handshake_done;
        let command = header.command;
        let is_response = header.is_response();
        // `state_befor_handshake`: an inbound connection may send its
        // handshake, and a ping — which is all another node's back ping ever
        // sends — and nothing else. The C++ dispatches everything regardless,
        // so a peer could relay blocks, pull the chain and set its height
        // without ever saying who it is.
        let allowed_early = matches!((command, is_response), (msg::COMMAND_HANDSHAKE, false) | (msg::COMMAND_PING, false));
        if !handshaken && !allowed_early {
            let error = PeerError::offence(Offence::Unsolicited, format!("command {command} before the handshake"));
            return self.fail_peer(id, error);
        }
        let result: Handled = match (command, is_response) {
            (msg::COMMAND_HANDSHAKE, false) => self.handle_handshake(id, &payload),
            (msg::COMMAND_TIMED_SYNC, false) => self.handle_timed_sync(id, &payload),
            (msg::COMMAND_TIMED_SYNC, true) => self.handle_timed_sync_response(id, &header, &payload),
            (msg::COMMAND_PING, false) => {
                self.post_reply(id, msg::COMMAND_PING, levin::RETCODE_SUCCESS, msg::ping_response(self.pm.peer_id()));
                Ok(())
            }
            (msg::NOTIFY_REQUEST_CHAIN, false) => self.handle_request_chain(id, &payload),
            (msg::NOTIFY_RESPONSE_CHAIN_ENTRY, false) => self.handle_response_chain_entry(id, &payload),
            (msg::NOTIFY_REQUEST_GET_OBJECTS, false) => self.handle_request_get_objects(id, &payload),
            (msg::NOTIFY_RESPONSE_GET_OBJECTS, false) => self.handle_response_get_objects(id, &payload),
            (msg::NOTIFY_REQUEST_TX_POOL, false) => self.handle_request_tx_pool(id, &payload),
            (msg::NOTIFY_NEW_BLOCK, false) => self.handle_notify_new_block(id, &payload),
            (msg::NOTIFY_NEW_LITE_BLOCK, false) => self.handle_notify_new_lite_block(id, &payload),
            (msg::NOTIFY_NEW_TRANSACTIONS, false) => self.handle_notify_new_transactions(id, &payload),
            (msg::NOTIFY_MISSING_TXS, false) => self.handle_notify_missing_txs(id, &payload),
            (other, false) => {
                // `ERROR_CONNECTION_HANDLER_NOT_DEFINED` with an empty body,
                // but only when the peer asked for an answer (`NetNode.cpp:2868`).
                // Then, unlike the C++, the connection goes: every command a
                // peer of this network sends has a handler above.
                if header.have_to_return_data {
                    self.post_reply(id, other, levin::ERROR_HANDLER_NOT_DEFINED, Vec::new());
                }
                Err(PeerError::offence(Offence::Unsolicited, format!("unknown command {other}")))
            }
            (other, true) => {
                Err(PeerError::offence(Offence::Unsolicited, format!("unexpected response to command {other}")))
            }
        };
        if let Err(e) = result {
            self.fail_peer(id, e);
        }
    }

    // -- the 1000-series -----------------------------------------------------

    /// `handle_handshake` (`NetNode.cpp:2240`).
    fn handle_handshake(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let (node, sync) = msg::parse_handshake_request(payload).map_err(malformed("bad handshake"))?;
        // A node of another network, or an old one, is lost rather than
        // misbehaving: closed, not scored.
        if node.network_id != CRYPTONOTE_NETWORK {
            return Err("wrong network id".into());
        }
        if node.version < P2P_MINIMUM_VERSION {
            return Err(format!("peer version {} below the minimum", node.version).into());
        }
        if node.version > P2P_CURRENT_VERSION {
            log_warn!(
                "peer speaks P2P version {}, we speak {P2P_CURRENT_VERSION}; we may be out of date",
                node.version
            );
        }
        {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            if !peer.ctx.incoming {
                return Err(PeerError::offence(Offence::Unsolicited, "COMMAND_HANDSHAKE on an outgoing connection"));
            }
            // Not `peer_id != 0`, the C++ test: the id is the peer's claim, and
            // one claiming 0 could handshake again and so reset its sync
            // failure counters.
            if peer.ctx.handshake_done {
                return Err(PeerError::offence(Offence::Unsolicited, "double COMMAND_HANDSHAKE"));
            }
            peer.ctx.version = node.version;
            peer.ctx.my_port = node.my_port;
            // Before the sync data, so this peer's height is counted in the
            // median and the majority the sync data is judged against.
            peer.ctx.handshake_done = true;
        }
        if !self.process_payload_sync_data(id, &sync, true) {
            return Err("handshake sync data rejected".into());
        }
        let addr = {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            peer.ctx.peer_id = node.peer_id;
            peer.ctx.addr
        };
        // The back ping runs on its own thread: only a peer that answers
        // `COMMAND_PING` on the port it advertised reaches the white list.
        if node.peer_id != self.pm.peer_id() && node.my_port != 0 {
            net::spawn_back_ping(id, addr.ip(), node.my_port, node.peer_id, self.events_tx.clone());
        }
        let peerlist = self.pm.peerlist_head(P2P_DEFAULT_PEERS_IN_HANDSHAKE);
        let peerlist6 = if node.version >= P2P_IPV6_CAPABILITY_VERSION {
            self.pm.peerlist6_head(P2P_DEFAULT_PEERS_IN_HANDSHAKE)
        } else {
            Vec::new()
        };
        let body = msg::handshake_response(&self.node_data(), &self.core_sync_data(), &peerlist, &peerlist6);
        self.post_reply(id, msg::COMMAND_HANDSHAKE, levin::RETCODE_SUCCESS, body);
        log_debug!("inbound handshake from {addr} (version {})", node.version);
        Ok(())
    }

    /// `handle_timed_sync` (`NetNode.cpp:2213`).
    fn handle_timed_sync(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let sync = msg::parse_timed_sync_request(payload).map_err(malformed("bad timed sync"))?;
        if !self.process_payload_sync_data(id, &sync, false) {
            return Err("timed sync data rejected".into());
        }
        let version = self.peers.get(&id).map(|p| p.ctx.version).unwrap_or(0);
        let peerlist = self.pm.peerlist_head(P2P_DEFAULT_PEERS_IN_HANDSHAKE);
        let peerlist6 = if version >= P2P_IPV6_CAPABILITY_VERSION {
            self.pm.peerlist6_head(P2P_DEFAULT_PEERS_IN_HANDSHAKE)
        } else {
            Vec::new()
        };
        let body = msg::timed_sync_response_from(peers::now_secs(), &self.core_sync_data(), &peerlist, &peerlist6);
        self.post_reply(id, msg::COMMAND_TIMED_SYNC, levin::RETCODE_SUCCESS, body);
        Ok(())
    }

    /// `handleTimedSyncResponse` (`NetNode.cpp:951`).
    ///
    /// Only as the answer to our own request: the C++ merges any response it
    /// is sent, and each carries 250 peer-list entries, so a peer could stream
    /// them into our gray list as fast as it liked.
    fn handle_timed_sync_response(&mut self, id: ConnId, header: &Header, payload: &[u8]) -> Handled {
        {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            if !peer.ctx.timed_sync_outstanding {
                return Err(PeerError::offence(Offence::Unsolicited, "COMMAND_TIMED_SYNC response without a request"));
            }
            peer.ctx.timed_sync_outstanding = false;
        }
        if header.return_code != levin::RETCODE_SUCCESS {
            return Err(format!("timed sync return code {}", header.return_code).into());
        }
        let rsp = msg::parse_timed_sync_response(payload).map_err(malformed("bad timed sync response"))?;
        if !self.pm.merge_peerlist(&rsp.local_peerlist, rsp.local_time, peers::now_secs()) {
            return Err("timed sync peer list has a future last_seen".into());
        }
        let version = self.peers.get(&id).map(|p| p.ctx.version).unwrap_or(0);
        if version >= P2P_IPV6_CAPABILITY_VERSION && !rsp.local_peerlist6.is_empty() {
            self.pm.merge_peerlist6(&rsp.local_peerlist6);
        }
        if !self.process_payload_sync_data(id, &rsp.payload_data, false) {
            return Err("timed sync data rejected".into());
        }
        Ok(())
    }

    // -- serving the chain ---------------------------------------------------

    /// `handle_request_chain` (`CryptoNoteProtocolHandler.cpp:1308`).
    fn handle_request_chain(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let ids = msg::parse_request_chain(payload).map_err(malformed("bad chain request"))?;
        let genesis = self.chain_hash(0).map_err(|e| e.to_string())?;
        if ids.last() != Some(&genesis) {
            return Err("chain request does not end with the genesis id".into());
        }
        // `findBlockchainSupplement`: the first hash we know.
        let mut start = None;
        for hash in &ids {
            if let Some(index) = self.chain_read().block_index_by_hash(hash).map_err(|e| e.to_string())? {
                start = Some(index);
                break;
            }
        }
        let start = start.ok_or("chain request lists no block we know")?;
        let tip = self.chain_read().tip_index().unwrap_or(0);
        let end = tip.min(start.saturating_add(BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT as u32 - 1));
        let mut hashes = Vec::new();
        for index in start..=end {
            hashes.push(self.chain_hash(index).map_err(|e| e.to_string())?);
        }
        let body = msg::chain_entry(start, tip + 1, &hashes);
        self.post(id, msg::NOTIFY_RESPONSE_CHAIN_ENTRY, body);
        Ok(())
    }

    fn chain_hash(&self, index: u32) -> wrkz_chain::Result<Hash> {
        self.chain_read()
            .block_info(index)?
            .map(|i| i.block_hash)
            .ok_or_else(|| ChainError::Corrupt(format!("no block info at index {index}")))
    }

    /// `handle_request_get_objects` (`:936`).
    ///
    /// One answer per peer at a time. A request that arrives while our answer
    /// to the previous one is still queued or being written is held, and
    /// served once that one is out: our own sync pipelines one request ahead,
    /// and its next request can overtake our writer by a hair. Another request
    /// while one is held is not a sync client, and the peer goes. Without this
    /// a peer could have us build answers faster than its socket drains them.
    fn handle_request_get_objects(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let wanted = msg::parse_request_get_objects(payload).map_err(malformed("bad get objects"))?;
        let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
        if peer.sink.objects_in_flight() {
            if peer.ctx.deferred_objects_request.is_some() {
                return Err(PeerError::offence(Offence::Unsolicited, "NOTIFY_REQUEST_GET_OBJECTS while two are unanswered"));
            }
            log_debug!("{}: get-objects request held until the previous answer is written", peer.ctx.label());
            peer.ctx.deferred_objects_request = Some(wanted);
            return Ok(());
        }
        self.serve_objects(id, &wanted)
    }

    /// Serve a held get-objects request once the answer before it is out.
    /// Checked after every frame from the peer and on every tick — the writer
    /// does not wake the engine — so a held request waits at most one tick.
    fn serve_deferred_objects_for(&mut self, id: ConnId) {
        let wanted = match self.peers.get_mut(&id) {
            Some(p) if !p.sink.objects_in_flight() => p.ctx.deferred_objects_request.take(),
            _ => None,
        };
        if let Some(wanted) = wanted {
            if let Err(e) = self.serve_objects(id, &wanted) {
                self.fail_peer(id, e);
            }
        }
    }

    fn serve_deferred_objects(&mut self) {
        let held: Vec<ConnId> =
            self.peers.values().filter(|p| p.ctx.deferred_objects_request.is_some()).map(|p| p.ctx.id).collect();
        for id in held {
            self.serve_deferred_objects_for(id);
        }
    }

    /// The answer to a get-objects request, bounded by a byte budget: a peer
    /// may ask for up to 10,000 hashes (the blob cap), and building the answer
    /// to that unconditionally would be a memory amplification of three orders
    /// of magnitude. The budget is [`SERVE_OBJECTS_MAX_BYTES`], so the answer
    /// always fits the peer's write budget; a C++ peer never asks for more than
    /// `--block-sync-size` blocks, so it is only ever reached by something
    /// that is not a C++ peer.
    fn serve_objects(&mut self, id: ConnId, wanted: &[Hash]) -> Handled {
        let mut blocks = Vec::new();
        let mut missed = Vec::new();
        let mut bytes = 0usize;
        for hash in wanted {
            let index = self.chain_read().block_index_by_hash(hash).map_err(|e| e.to_string())?;
            let raw = match index {
                Some(index) => self.chain_read().raw_block(index).map_err(|e| e.to_string())?,
                None => None,
            };
            match raw {
                Some((block, txs)) => {
                    let size = block.len() + txs.iter().map(|t| t.len()).sum::<usize>();
                    if !blocks.is_empty() && bytes + size > SERVE_OBJECTS_MAX_BYTES {
                        log_warn!("conn {id} asked for {} blocks; answering with the first {}", wanted.len(), blocks.len());
                        break;
                    }
                    bytes += size;
                    blocks.push(RawBlockLegacy { block, txs });
                }
                None => missed.push(*hash),
            }
        }
        let body = msg::get_objects_response(&blocks, &missed, self.height());
        let Some(sink) = self.sink(id) else { return Ok(()) };
        log_trace!("-> conn {id} get-objects answer: {} blocks, {} bytes", blocks.len(), body.len());
        if !sink.notify_objects(body) {
            return Err("write queue full".into());
        }
        Ok(())
    }

    /// `handleRequestTxPool` (`:1494`): send the transactions the peer lacks,
    /// in batches of at most [`TX_BATCH_BYTES`] where the C++ sends one message.
    fn handle_request_tx_pool(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let theirs = msg::parse_request_tx_pool(payload).map_err(malformed("bad pool request"))?;
        let theirs: HashSet<Hash> = theirs.into_iter().collect();
        let blobs: Vec<Vec<u8>> = self
            .pool
            .transaction_hashes()
            .into_iter()
            .filter(|h| !theirs.contains(h))
            .filter_map(|h| self.pool.transaction(&h))
            .collect();
        for batch in tx_batches(&blobs) {
            self.post(id, msg::NOTIFY_NEW_TRANSACTIONS, msg::new_transactions(batch));
        }
        Ok(())
    }

    // -- pulling the chain ---------------------------------------------------

    /// `handle_response_chain_entry` (`:1452`).
    ///
    /// Only as the answer to our own chain request — the C++ takes one at any
    /// time — and it **replaces** `needed_objects` where the C++ appends, so
    /// the list is one entry's worth, at most 10,000 ids, whatever the peer
    /// sends.
    fn handle_response_chain_entry(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            if peer.ctx.chain_request_deadline.take().is_none() {
                if std::mem::take(&mut peer.ctx.discard_next_chain_entry) {
                    log_debug!("{}: discarding the late answer to a chain request that timed out", peer.ctx.label());
                    return Ok(());
                }
                return Err(PeerError::offence(Offence::Unsolicited, "NOTIFY_RESPONSE_CHAIN_ENTRY without a request"));
            }
        }
        let entry = msg::parse_chain_entry(payload).map_err(malformed("bad chain entry"))?;
        let first_known = self.chain_read().has_block(&entry.block_ids[0]).map_err(|e| e.to_string())?;
        if !first_known {
            return Err(format!("chain entry starts at the unknown id {}", hex::encode(entry.block_ids[0])).into());
        }
        // `handle_response_chain_entry` stops testing after the first miss:
        // everything from there on is needed whether we hold it or not. Only
        // the prefix is probed, so a 10,000-id entry costs one lookup per block
        // we already have and none for the rest.
        let mut first_unknown = entry.block_ids.len();
        for (i, hash) in entry.block_ids.iter().enumerate() {
            if !self.chain_read().has_block(hash).map_err(|e| e.to_string())? {
                first_unknown = i;
                break;
            }
        }
        {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            peer.ctx.remote_height = entry.total_height;
            peer.ctx.last_response_height =
                entry.start_height.saturating_add(entry.block_ids.len() as u32).saturating_sub(1);
            if peer.ctx.last_response_height > peer.ctx.remote_height {
                return Err(format!(
                    "chain entry claims total_height {} but ends at {}",
                    entry.total_height, peer.ctx.last_response_height
                )
                .into());
            }
            peer.ctx.needed_objects = entry.block_ids[first_unknown..].iter().copied().collect();
            log_debug!(
                "{} chain entry: start {} total {} ids {} -> {} needed",
                peer.ctx.label(),
                entry.start_height,
                entry.total_height,
                entry.block_ids.len(),
                peer.ctx.needed_objects.len()
            );
        }
        // The entry's total height is the peer's claim too.
        self.recalculate_observed_height();
        self.request_missing_objects(id, false);
        Ok(())
    }

    /// `request_missing_objects` (`:1342`).
    fn request_missing_objects(&mut self, id: ConnId, check_having_blocks: bool) {
        let tuning = self.cfg.tuning;
        let mut request: Vec<Hash> = Vec::new();
        let mut want_chain = false;
        let mut synced = false;
        {
            // One read guard for the whole batch: the chain cannot move while
            // this runs anyway (the engine is the only writer), and taking it
            // per hash would be a lock acquisition per block of every batch.
            let chain = self.chain.read().unwrap_or_else(|p| p.into_inner());
            let Some(peer) = self.peers.get_mut(&id) else { return };
            if !peer.ctx.needed_objects.is_empty() {
                let batch = peer.ctx.batch_size(&tuning) as usize;
                while request.len() < batch {
                    let Some(hash) = peer.ctx.needed_objects.pop_front() else { break };
                    // `check_having_blocks`: skip what another peer delivered
                    // while this batch was in flight.
                    if check_having_blocks && chain.has_block(&hash).unwrap_or(false) {
                        continue;
                    }
                    peer.ctx.requested_objects.insert(hash);
                    request.push(hash);
                }
                peer.ctx.chunk_start = Instant::now();
            } else if peer.ctx.last_response_height < peer.ctx.remote_height.saturating_sub(1) {
                want_chain = true;
            } else {
                synced = true;
            }
        }
        if !request.is_empty() {
            log_debug!("conn {id}: NOTIFY_REQUEST_GET_OBJECTS for {} blocks", request.len());
            // The deadline scales with what the batch should weigh; with a
            // pipelined request it is this, the later one's.
            let (min, max) = (self.cfg.objects_timeout_min, self.cfg.objects_timeout_max);
            if let Some(peer) = self.peers.get_mut(&id) {
                let timeout = objects_timeout(request.len(), peer.ctx.avg_block_bytes, min, max);
                peer.ctx.objects_deadline = Some(Instant::now() + timeout);
            }
            self.post(id, msg::NOTIFY_REQUEST_GET_OBJECTS, msg::request_get_objects(&request));
            return;
        }
        if want_chain {
            self.request_chain_if_peer_can_serve(id, "more block ids needed");
            return;
        }
        if synced {
            // The peer has nothing more for us.
            self.request_missing_pool_transactions(id);
            if let Some(peer) = self.peers.get_mut(&id) {
                peer.ctx.state = PeerState::Normal;
            }
            log_info!("conn {id}: fully synchronized with this peer at height {}", self.height());
            self.on_connection_synchronized();
        }
    }

    /// `handle_response_get_objects` (`:830`).
    fn handle_response_get_objects(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            let requested = peer.ctx.objects_deadline.take().is_some();
            // A reply to a batch we abandoned — superseded by another peer, or
            // past its deadline: the peer did nothing wrong.
            if peer.ctx.discard_next_objects_response {
                peer.ctx.discard_next_objects_response = false;
                log_debug!("conn {id}: discarding a superseded block response");
                return Ok(());
            }
            // Nothing asked for. Not scored: a very late answer to an
            // abandoned batch can arrive after the next batch's answer, and any
            // block in an unrequested answer is an offence below anyway.
            if !requested {
                return Err("NOTIFY_RESPONSE_GET_OBJECTS without a request".into());
            }
        }
        let response = msg::parse_get_objects_response(payload).map_err(malformed("bad get objects response"))?;
        {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            if peer.ctx.last_response_height > response.current_blockchain_height {
                return Err(format!(
                    "peer height {} below our last response height {}",
                    response.current_blockchain_height, peer.ctx.last_response_height
                )
                .into());
            }
            // The peer's height as its latest message states it; it only moves
            // this peer's own vote in the median.
            peer.ctx.remote_height = peer.ctx.remote_height.max(response.current_blockchain_height);
        }
        self.recalculate_observed_height();

        // Parse and check every block before anything is applied, which is the
        // order `handle_response_get_objects` uses.
        let mut parsed: Vec<(Hash, BlockTemplate, RawBlockLegacy)> = Vec::new();
        for raw in response.blocks {
            let template =
                BlockTemplate::from_bytes(&raw.block).map_err(malformed("peer sent a block that does not parse"))?;
            let hash = template.hash().map_err(malformed("peer sent a block that does not hash"))?;
            {
                let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
                if !peer.ctx.requested_objects.remove(&hash) {
                    let reason = format!("block {} was not requested", hex::encode(hash));
                    return Err(PeerError::offence(Offence::UnrequestedBlock, reason));
                }
            }
            if template.transaction_hashes.len() != raw.txs.len() {
                let reason = format!(
                    "block {} lists {} transactions but carries {}",
                    hex::encode(hash),
                    template.transaction_hashes.len(),
                    raw.txs.len()
                );
                return Err(PeerError::offence(Offence::Malformed, reason));
            }
            parsed.push((hash, template, raw));
        }

        // Blocks the peer listed and then did not send.
        let outstanding: Vec<Hash> =
            self.peers.get(&id).map(|p| p.ctx.requested_objects.iter().copied().collect()).unwrap_or_default();
        if !outstanding.is_empty() {
            let missed: HashSet<Hash> = response.missed_ids.iter().copied().collect();
            let all_declared_missing = outstanding.iter().all(|h| missed.contains(h));
            let tuning = self.cfg.tuning;
            let dropped = {
                let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
                peer.ctx.clear_sync_lists();
                peer.ctx.on_chunk_failure(&tuning)
            };
            if !all_declared_missing {
                return Err(format!("peer did not return {} requested blocks", outstanding.len()).into());
            }
            if dropped {
                return Err("too many sync failures".into());
            }
            log_info!(
                "conn {id}: peer no longer holds {} of the blocks it listed (reorganised or pruned); re-requesting the chain",
                outstanding.len()
            );
            self.request_chain_if_peer_can_serve(id, "peer no longer holds the blocks it listed");
            return Ok(());
        }

        let block_count = parsed.len();
        let raw_bytes: usize =
            parsed.iter().map(|(_, _, r)| r.block.len() + r.txs.iter().map(|t| t.len()).sum::<usize>()).sum();
        let tuning = self.cfg.tuning;

        // Ask for the next batch before applying this one, so the peer is
        // transferring while we validate. Only when we already know what to ask
        // for: with `needed_objects` empty this would instead declare us synced.
        let pipelined = {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            peer.ctx.on_chunk_success(block_count, raw_bytes, &tuning);
            !peer.ctx.needed_objects.is_empty() && peer.ctx.state == PeerState::Synchronizing
        };
        if pipelined {
            self.request_missing_objects(id, true);
        }
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.ctx.pipelined_objects_outstanding = pipelined;
        }

        let outcome = self.process_objects(id, parsed);

        if let Some(peer) = self.peers.get_mut(&id) {
            peer.ctx.pipelined_objects_outstanding = false;
        }
        if let Processed::Stop = outcome {
            return Ok(());
        }
        // The batch applied cleanly, so the peer is behaving.
        let still_syncing = {
            let Some(peer) = self.peers.get_mut(&id) else { return Ok(()) };
            peer.ctx.sync_failures = 0;
            peer.ctx.orphan_retries = 0;
            peer.ctx.state == PeerState::Synchronizing
        };
        if !pipelined && still_syncing {
            self.request_missing_objects(id, true);
        }
        Ok(())
    }

    /// The proof-of-work hashes of a downloaded batch, computed across the
    /// validation threads before any of it is applied, so that step 10 of
    /// `addBlock` compares instead of hashing — otherwise 0.8 ms of CryptoNight
    /// per block on the engine's one thread. Only blocks above the checkpoint
    /// zone get one: inside it no proof of work is checked at all. The claimed
    /// height only chooses which blocks to pre-hash; a wrong claim costs a
    /// wasted hash or an inline one, never a different verdict.
    fn pow_hints(&self, blocks: &[(Hash, BlockTemplate, RawBlockLegacy)]) -> Vec<Option<PowHint>> {
        let (wanted, threads) = {
            let chain = self.chain_read();
            let checkpoints = chain.checkpoints();
            let wanted: Vec<Option<&BlockTemplate>> = blocks
                .iter()
                .map(|(_, t, _)| t.coinbase_height().filter(|h| !checkpoints.is_in_checkpoint_zone(*h)).map(|_| t))
                .collect();
            (wanted, chain.config().validate_threads)
        };
        PowHint::compute_many(&wanted, threads)
    }

    /// `processObjects` (`:1027`): every block through `ChainState::add_block`,
    /// with the C++ reaction to each outcome.
    fn process_objects(&mut self, id: ConnId, blocks: Vec<(Hash, BlockTemplate, RawBlockLegacy)>) -> Processed {
        let hints = self.pow_hints(&blocks);
        for ((hash, template, raw), hint) in blocks.into_iter().zip(hints) {
            let applied = self.apply_block(&raw.block, &raw.txs, hint);
            match applied {
                Ok(outcome) => {
                    if let Some(peer) = self.peers.get_mut(&id) {
                        peer.ctx.blocks_added += 1;
                    }
                    log_trace!("added block {} at index {} ({:?})", hex::encode(hash), outcome.index, outcome.status);
                }
                Err(ChainError::Rule(Rule::AlreadyExists)) => {
                    // Another peer got there first. Not misbehaviour: go idle
                    // and throw the pipelined reply away.
                    log_debug!("conn {id}: block {} already exists, going idle", hex::encode(hash));
                    if let Some(peer) = self.peers.get_mut(&id) {
                        peer.ctx.state = PeerState::Idle;
                        peer.ctx.clear_sync_lists();
                        peer.ctx.discard_next_objects_response = peer.ctx.pipelined_objects_outstanding;
                    }
                    return Processed::Stop;
                }
                Err(ChainError::Rule(Rule::RejectedAsOrphaned)) => {
                    let over_limit = {
                        let Some(peer) = self.peers.get_mut(&id) else { return Processed::Stop };
                        peer.ctx.orphan_retries += 1;
                        peer.ctx.clear_sync_lists();
                        peer.ctx.discard_next_objects_response = peer.ctx.pipelined_objects_outstanding;
                        peer.ctx.orphan_retries >= SYNC_ORPHAN_RETRY_LIMIT
                    };
                    if over_limit {
                        self.drop_peer(id, "sync orphan retry limit reached");
                        return Processed::Stop;
                    }
                    log_info!("conn {id}: block orphaned during sync, re-requesting the chain");
                    self.request_chain_if_peer_can_serve(id, "block received during sync was orphaned");
                    return Processed::Stop;
                }
                Err(ChainError::Rule(rule)) => {
                    // The C++ bans only for a checkpoint mismatch, for 900 s
                    // (`CryptoNoteProtocolHandler.cpp:1046`). Here that and
                    // every other rule the sender answers for is an offence
                    // ([`block_offence`]) and bans for a day; the rest only
                    // drop the connection, as before.
                    log_warn!(
                        "conn {id}: block {} at claimed index {:?} failed validation: {rule}",
                        hex::encode(hash),
                        template.coinbase_height()
                    );
                    self.fail_peer(id, block_error(&rule, "block verification failed"));
                    return Processed::Stop;
                }
                Err(e) => {
                    // Storage or state corruption is ours, not the peer's.
                    log_error!("chain write failed on block {}: {e}", hex::encode(hash));
                    self.stop = true;
                    return Processed::Stop;
                }
            }
        }
        Processed::Continue
    }

    // -- relay ---------------------------------------------------------------

    /// `handle_notify_new_block` (`:692`).
    fn handle_notify_new_block(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let mut nb = msg::parse_new_block(payload).map_err(malformed("bad new block"))?;
        let normal = {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            peer.ctx.remote_height = nb.current_blockchain_height;
            peer.ctx.state == PeerState::Normal
        };
        self.recalculate_observed_height();
        if !normal {
            return Ok(());
        }
        let applied = self.apply_block(&nb.block.block, &nb.block.txs, None);
        match applied {
            Ok(outcome) => match outcome.status {
                AddStatus::AlternativeAndSwitched => {
                    nb.hop += 1;
                    self.relay_block(&nb);
                    self.request_missing_pool_transactions(id);
                }
                AddStatus::Main => {
                    nb.hop += 1;
                    log_info!("relayed block accepted at index {}", outcome.index);
                    self.relay_block(&nb);
                }
                AddStatus::Alternative => {
                    log_info!(
                        "relayed block added as an alternative (peer height {}, ours {})",
                        nb.current_blockchain_height,
                        self.height()
                    );
                    if nb.current_blockchain_height > self.height() {
                        self.request_chain_if_peer_can_serve(id, "peer is ahead on an alternative chain");
                    }
                }
            },
            Err(ChainError::Rule(Rule::AlreadyExists)) => {}
            Err(ChainError::Rule(Rule::RejectedAsOrphaned)) => {
                self.request_chain_if_peer_can_serve(id, "relayed block does not attach to our chain");
            }
            Err(ChainError::Rule(rule)) => return Err(block_error(&rule, "relayed block failed validation")),
            Err(e) => {
                log_error!("chain write failed on a relayed block: {e}");
                self.stop = true;
            }
        }
        Ok(())
    }

    /// `handle_notify_new_lite_block` (`:1530`).
    fn handle_notify_new_lite_block(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let lb = msg::parse_lite_block(payload).map_err(malformed("bad lite block"))?;
        let normal = {
            let peer = self.peers.get_mut(&id).ok_or("connection gone")?;
            peer.ctx.remote_height = lb.current_blockchain_height;
            peer.ctx.state == PeerState::Normal
        };
        self.recalculate_observed_height();
        if !normal {
            return Ok(());
        }
        self.push_lite_block(id, lb, Vec::new())
    }

    /// `doPushLiteBlock` (`:1155`): fill the block's transactions from what we
    /// hold, ask the sender for the rest, and add it once it is complete.
    fn push_lite_block(&mut self, id: ConnId, lb: LiteBlock, provided: Vec<Vec<u8>>) -> Handled {
        let template =
            BlockTemplate::from_bytes(&lb.block_template).map_err(malformed("lite block template does not parse"))?;
        let block_hash = template.hash().map_err(malformed("lite block does not hash"))?;

        let mut provided_by_hash: HashMap<Hash, Vec<u8>> = HashMap::new();
        for blob in provided {
            match wrkz_primitives::tx::Transaction::from_bytes(&blob).ok().and_then(|t| t.hash().ok()) {
                Some(h) => {
                    provided_by_hash.insert(h, blob);
                }
                // A blob that does not parse cannot satisfy any hash; the
                // pending-lite-block check below then drops the peer, which is
                // what the C++ does when a requested transaction is not
                // supplied.
                None => continue,
            }
        }

        // A peer that answered our NOTIFY_MISSING_TXS must supply every hash
        // we asked for.
        if let Some(pending) = self.peers.get(&id).and_then(|p| p.ctx.pending_lite_block.as_ref()) {
            let unsatisfied = pending.missed_transactions.iter().any(|h| !provided_by_hash.contains_key(h));
            if unsatisfied {
                if let Some(peer) = self.peers.get_mut(&id) {
                    peer.ctx.pending_lite_block = None;
                }
                return Err("peer did not supply a transaction it was asked for".into());
            }
        }

        let mut have: Vec<Vec<u8>> = Vec::new();
        let mut need: Vec<Hash> = Vec::new();
        for hash in &template.transaction_hashes {
            if let Some(blob) = provided_by_hash.get(hash) {
                have.push(blob.clone());
            } else if let Some(blob) = self.pool.transaction(hash) {
                have.push(blob);
            } else {
                need.push(*hash);
            }
        }

        if !need.is_empty() {
            if self.peers.get(&id).is_some_and(|p| p.ctx.pending_lite_block.is_some()) {
                if let Some(peer) = self.peers.get_mut(&id) {
                    peer.ctx.pending_lite_block = None;
                }
                return Err("peer has a pending lite block but did not supply every transaction".into());
            }
            let request = MissingTxs {
                current_blockchain_height: lb.current_blockchain_height,
                block_hash,
                missing_txs: need.clone(),
            };
            if let Some(peer) = self.peers.get_mut(&id) {
                peer.ctx.pending_lite_block = Some(crate::sync::PendingLiteBlock {
                    request: lb,
                    missed_transactions: need.iter().copied().collect(),
                });
            }
            log_debug!("conn {id}: lite block {} needs {} transactions", hex::encode(block_hash), need.len());
            self.post(id, msg::NOTIFY_MISSING_TXS, msg::missing_txs(&request));
            return Ok(());
        }

        if let Some(peer) = self.peers.get_mut(&id) {
            peer.ctx.pending_lite_block = None;
        }
        let mut relay = LiteBlock { hop: lb.hop, ..lb.clone() };
        let applied = self.apply_block(&lb.block_template, &have, None);
        match applied {
            Ok(outcome) => match outcome.status {
                AddStatus::AlternativeAndSwitched => {
                    relay.hop += 1;
                    self.relay_lite_block(&relay, Some(id));
                    self.request_missing_pool_transactions(id);
                }
                AddStatus::Main => {
                    relay.hop += 1;
                    log_info!("lite block accepted at index {}", outcome.index);
                    self.relay_lite_block(&relay, Some(id));
                }
                AddStatus::Alternative => {
                    log_info!(
                        "lite block added as an alternative (peer height {}, ours {})",
                        lb.current_blockchain_height,
                        self.height()
                    );
                    if lb.current_blockchain_height > self.height() {
                        self.request_chain_if_peer_can_serve(id, "peer is ahead on an alternative chain");
                    }
                }
            },
            Err(ChainError::Rule(Rule::AlreadyExists)) => {}
            Err(ChainError::Rule(Rule::RejectedAsOrphaned)) => {
                self.request_chain_if_peer_can_serve(id, "relayed lite block does not attach to our chain");
            }
            Err(ChainError::Rule(rule)) => return Err(block_error(&rule, "lite block failed validation")),
            Err(e) => {
                log_error!("chain write failed on a lite block: {e}");
                self.stop = true;
            }
        }
        Ok(())
    }

    /// `handle_notify_new_transactions` (`:752`): a pending lite block turns
    /// this into the answer to our `NOTIFY_MISSING_TXS`; otherwise the
    /// transactions go through pool admission and the accepted ones are relayed.
    ///
    /// A transaction the pool refuses for a rule no honest relay passes on
    /// ([`TxVerdict::Invalid`]) costs the sender [`Offence::InvalidTransaction`]
    /// — once per message, however many it carried — but not the connection:
    /// the C++ does not drop for it, and neither does this.
    fn handle_notify_new_transactions(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let txs = msg::parse_new_transactions(payload).map_err(malformed("bad transactions"))?;
        if self.peers.get(&id).map(|p| p.ctx.state) != Some(PeerState::Normal) {
            return Ok(());
        }
        if let Some(pending) = self.peers.get_mut(&id).and_then(|p| p.ctx.pending_lite_block.clone()) {
            log_debug!("conn {id}: transactions answer a pending lite block");
            return self.push_lite_block(id, pending.request, txs);
        }
        let mut accepted: Vec<Vec<u8>> = Vec::new();
        let mut invalid: Option<String> = None;
        for blob in txs {
            match self.pool.add_relayed_transaction(&blob) {
                TxVerdict::Accepted => accepted.push(blob),
                TxVerdict::NotAccepted => {}
                TxVerdict::Invalid(why) => {
                    invalid.get_or_insert(why);
                }
            }
        }
        if let Some(why) = invalid {
            self.penalise(id, Offence::InvalidTransaction, &format!("relayed an invalid transaction: {why}"));
        }
        for batch in tx_batches(&accepted) {
            self.relay_to_all(msg::NOTIFY_NEW_TRANSACTIONS, msg::new_transactions(batch), Some(id));
        }
        Ok(())
    }

    /// `handle_notify_missing_txs` (`:1546`): a peer that cannot supply a
    /// requested transaction is dropped.
    fn handle_notify_missing_txs(&mut self, id: ConnId, payload: &[u8]) -> Handled {
        let request = msg::parse_missing_txs(payload).map_err(malformed("bad missing txs"))?;
        let mut blobs = Vec::new();
        for hash in &request.missing_txs {
            match self.pool.transaction(hash) {
                Some(blob) => blobs.push(blob),
                // The C++ also looks in the chain; this node has no
                // transaction-by-hash index yet (see the crate docs), and a
                // lite block only ever references pool transactions.
                None => return Err(format!("cannot supply the requested transaction {}", hex::encode(hash)).into()),
            }
        }
        self.post(id, msg::NOTIFY_NEW_TRANSACTIONS, msg::new_transactions(&blobs));
        Ok(())
    }

    /// `m_syncManager->relayTransactions({tx})` (`RpcServer.cpp:1140`): send
    /// transactions the RPC accepted on to every peer.
    ///
    /// The daemon loop drains `wrkz-rpc`'s relay queue into this each tick, so
    /// a transaction a wallet sent us reaches the network the same way one a
    /// peer sent us does.
    pub fn relay_transactions(&mut self, blobs: &[Vec<u8>]) {
        if blobs.is_empty() || self.peers.is_empty() {
            return;
        }
        log_debug!("relaying {} transactions to {} peers", blobs.len(), self.peers.len());
        for batch in tx_batches(blobs) {
            self.relay_to_all(msg::NOTIFY_NEW_TRANSACTIONS, msg::new_transactions(batch), None);
        }
    }

    /// A handle another thread can use to make [`Node::step`] return now
    /// instead of at the end of its timeout — so the daemon loop picks up a
    /// block the RPC or the stratum server just added without waiting a tick.
    pub fn waker(&self) -> Waker {
        Waker(self.events_tx.clone())
    }

    /// `m_syncManager->relayBlock(...)` (`RpcServer.cpp:1789`): a block a miner
    /// submitted through `submitblock` or stratum, announced to every peer.
    /// Returns how many peers it was sent to.
    pub fn relay_new_block(&mut self, block_blob: &[u8], tx_blobs: &[Vec<u8>]) -> usize {
        if self.peers.is_empty() {
            return 0;
        }
        let nb = NewBlock {
            block: RawBlockLegacy { block: block_blob.to_vec(), txs: tx_blobs.to_vec() },
            hop: 0,
            current_blockchain_height: self.height(),
        };
        self.relay_block(&nb)
    }

    /// `relayBlock` (`:1585`): the lite form to peers with
    /// `P2P_LITE_BLOCKS_PROPOGATION_VERSION` or above, the full form to the
    /// rest. Like the C++, the sender is **not** excluded here — it treats the
    /// echo as `ALREADY_EXISTS` and ignores it. Returns how many peers it was
    /// sent to.
    fn relay_block(&mut self, nb: &NewBlock) -> usize {
        let lite = LiteBlock {
            current_blockchain_height: nb.current_blockchain_height,
            hop: nb.hop,
            block_template: nb.block.block.clone(),
        };
        let lite_body = msg::lite_block(&lite);
        let full_body = msg::new_block(nb);
        let targets: Vec<(ConnId, bool)> =
            self.peers.values().map(|p| (p.ctx.id, p.ctx.version >= P2P_LITE_BLOCKS_PROPOGATION_VERSION)).collect();
        let sent = targets.len();
        for (id, wants_lite) in targets {
            if wants_lite {
                self.post(id, msg::NOTIFY_NEW_LITE_BLOCK, lite_body.clone());
            } else {
                self.post(id, msg::NOTIFY_NEW_BLOCK, full_body.clone());
            }
        }
        sent
    }

    /// `relay_post_notify<NOTIFY_NEW_LITE_BLOCK>` with the origin excluded,
    /// which is what `doPushLiteBlock` does.
    fn relay_lite_block(&mut self, lb: &LiteBlock, exclude: Option<ConnId>) {
        self.relay_to_all(msg::NOTIFY_NEW_LITE_BLOCK, msg::lite_block(lb), exclude);
    }

    /// `externalRelayNotifyToAll` with an excluded connection.
    fn relay_to_all(&mut self, command: u32, body: Vec<u8>, exclude: Option<ConnId>) {
        let targets: Vec<ConnId> = self.peers.values().map(|p| p.ctx.id).filter(|id| Some(*id) != exclude).collect();
        for id in targets {
            self.post(id, command, body.clone());
        }
    }
}

/// See [`Node::waker`].
#[derive(Clone)]
pub struct Waker(SyncSender<Event>);

impl Waker {
    /// Queue a tick. A full queue means the engine is already busy and will
    /// return from `step` anyway, so a failed send is not an error.
    pub fn wake(&self) {
        let _ = self.0.try_send(Event::Tick);
    }
}

/// The `--seed` list, or the compiled-in seeds and DNS seeds when it is empty.
fn resolve_seeds(cfg: &NodeConfig) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let mut push = |target: &str| match peers::resolve(target, P2P_DEFAULT_PORT) {
        Ok(addrs) => out.extend(addrs),
        Err(e) => log_warn!("cannot resolve seed {target}: {e}"),
    };
    for s in &cfg.seeds {
        push(s);
    }
    if cfg.use_default_seeds {
        for s in SEED_NODES {
            push(s);
        }
        for s in wrkz_primitives::constants::DNS_SEED_NODES {
            push(s);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_chain::TxRule;

    /// Transactions go out in messages of at most `TX_BATCH_BYTES` of blobs,
    /// in order, a blob larger than that on its own, nothing lost.
    #[test]
    fn transactions_go_out_in_batches() {
        let blobs: Vec<Vec<u8>> = vec![vec![1; 3 << 20], vec![2; 2 << 20], vec![3; 9 << 20], vec![4; 1], vec![5; 1]];
        let batches = tx_batches(&blobs);
        let sizes: Vec<usize> = batches.iter().map(|b| b.len()).collect();
        assert_eq!(sizes, vec![1, 1, 1, 2]);
        assert_eq!(batches.concat(), blobs);
        assert!(tx_batches(&[]).is_empty());
        // and the biggest batch still fits a receiver's relay cap
        assert!((TX_BATCH_BYTES as u64) < wrkz_p2p::limits::RELAY_MAX_PAYLOAD);
    }

    /// The per-phase costs over the interval, not since the start.
    #[test]
    fn the_progress_line_reports_the_interval() {
        let at = |blocks, us: u64| Timings {
            blocks,
            decode: Duration::from_micros(us),
            validate: Duration::from_micros(10 * us),
            commit: Duration::from_micros(2 * us),
            total: Duration::from_micros(13 * us),
            ..Default::default()
        };
        assert_eq!(phase_line(&at(5, 100), &at(5, 100)), "", "no blocks, no line");
        let line = phase_line(&at(10, 1_000), &at(20, 2_000));
        assert_eq!(line, " | per block: decode 100us validate 1000us commit 200us total 1300us");
        assert_eq!(phase_line(&at(20, 2_000), &Timings::default()), "", "a reset is not a negative interval");
    }

    /// Proof of work and the rules a sender checks itself are offences; a
    /// clock, a fork schedule or chain state it need not share are not.
    #[test]
    fn which_block_failures_are_offences() {
        assert_eq!(block_offence(&Rule::ProofOfWorkTooWeak { difficulty: 1 }), Some(Offence::BadProofOfWork));
        let mismatch = Rule::CheckpointBlockHashMismatch { expected: [0; 32], got: [1; 32] };
        assert_eq!(block_offence(&mismatch), Some(Offence::CheckpointMismatch));
        assert_eq!(block_offence(&Rule::CoinbaseHasSignatures), Some(Offence::InvalidBlock));
        assert_eq!(block_offence(&Rule::TimestampTooFarInFuture { timestamp: 2, limit: 1 }), None);
        assert_eq!(block_offence(&Rule::WrongVersion { expected: 8, got: 7 }), None);
        let signature = Rule::Transaction { hash: [0; 32], index: 0, rule: TxRule::InputInvalidSignatures { input: 0 } };
        assert_eq!(block_offence(&signature), Some(Offence::InvalidBlock));
        let spent = Rule::Transaction { hash: [0; 32], index: 0, rule: TxRule::InputKeyImageAlreadySpent { key_image: [0; 32] } };
        assert_eq!(block_offence(&spent), None);
        assert_eq!(block_offence(&Rule::RejectedAsOrphaned), None);
    }

    type Engine = Node<wrkz_storage::MemStore, crate::pool::BoundedTxSet>;

    /// An engine that listens nowhere and knows no seeds.
    fn offline_engine(name: &str, cfg: NodeConfig) -> Engine {
        let data_dir = std::env::temp_dir().join(format!("wrkz-node-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let cfg = NodeConfig { data_dir, listen: false, use_default_seeds: false, ..cfg };
        let store = wrkz_storage::MemStore::default();
        let chain = ChainState::open_or_genesis(store, wrkz_chain::Config::default(), wrkz_chain::Checkpoints::none())
            .expect("a genesis state");
        Node::new(chain, crate::pool::BoundedTxSet::new(16, 1 << 16), cfg)
    }

    fn white(ip: [u8; 4]) -> msg::PeerlistEntry {
        msg::PeerlistEntry { ip, port: 17855, id: u64::from(ip[3]), last_seen: 1 }
    }

    /// With an exclusive node configured it is the one dial: not the white
    /// list, not a seed, not an `--add-peer` (`NetNode.cpp:1622-1630`).
    #[test]
    fn with_an_exclusive_node_nothing_else_is_dialled() {
        // Nothing listens on port 1, so the dial fails on its own thread
        // without anything leaving this machine.
        let exclusive: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let cfg = NodeConfig {
            exclusive_nodes: vec![PinnedNode::at(exclusive)],
            seeds: vec!["192.0.2.2:17855".to_string()],
            ..Default::default()
        };
        let mut node = offline_engine("exclusive", cfg);
        assert!(node.pm.seeds().is_empty(), "the seeds are not even resolved");
        node.pm.append_white(white([192, 0, 2, 1]));
        let added: SocketAddr = "192.0.2.3:17855".parse().unwrap();
        node.add_peer(added);
        assert!(node.pm.white_addresses().contains(&added), "--add-peer is still listed, as in the C++");
        node.connections_maker();
        assert_eq!(node.dialing.iter().copied().collect::<Vec<_>>(), [exclusive]);

        // Not again while the dial is in flight, nor within the interval.
        node.connections_maker();
        node.dialing.clear();
        node.connections_maker();
        assert!(node.dialing.is_empty(), "{:?}", node.dialing);
    }

    /// A priority node is dialled on its own schedule, whatever `--out-peers`
    /// says, and the list selection never dials it a second time — not even
    /// while it waits out a failed dial.
    #[test]
    fn a_priority_node_is_dialled_by_its_schedule_and_never_by_the_lists() {
        // TEST-NET-1 is never routed: the dial times out on its own thread.
        let priority: SocketAddr = "192.0.2.10:17855".parse().unwrap();
        let cfg = NodeConfig { priority_nodes: vec![PinnedNode::at(priority)], max_outgoing: 4, ..Default::default() };
        let mut node = offline_engine("priority", cfg);
        node.pm.append_white(white([192, 0, 2, 10]));
        node.connections_maker();
        assert_eq!(node.dialing.iter().copied().collect::<Vec<_>>(), [priority]);

        node.dialing.clear();
        node.on_pinned_dial_failed(priority);
        node.on_pinned_dial_failed(priority);
        assert_eq!(node.pinned[0].failures, 2);
        assert!(node.pinned[0].retry_at > Instant::now() + Duration::from_millis(1500), "two failures wait 2 s");
        node.connections_maker();
        assert!(node.dialing.is_empty(), "{:?}", node.dialing);

        let zero = NodeConfig { priority_nodes: vec![PinnedNode::at(priority)], max_outgoing: 0, ..Default::default() };
        let mut node = offline_engine("priority-zero", zero);
        node.connections_maker();
        assert!(node.dialing.contains(&priority), "--out-peers 0 does not stop a priority node");
    }

    /// `get_local_node_data`: hidden, then the external port, then the port
    /// the node listens on.
    #[test]
    fn my_port_is_the_external_port_unless_it_is_hidden() {
        let mut node = offline_engine("my-port", NodeConfig { p2p_port: 17855, ..Default::default() });
        node.cfg.listen = true;
        assert_eq!(node.node_data().my_port, 17855, "the listening port");
        node.cfg.external_port = 27855;
        assert_eq!(node.node_data().my_port, 27855, "the port the NAT forwards");
        node.cfg.hide_my_port = true;
        assert_eq!(node.node_data().my_port, 0, "--hide-my-port wins");
        node.cfg.hide_my_port = false;
        node.cfg.listen = false;
        assert_eq!(node.node_data().my_port, 0, "a node that does not listen advertises no port");
    }

    #[test]
    fn a_failing_priority_node_backs_off_to_a_minute() {
        let waits: Vec<u64> = [0, 1, 2, 3, 4, 6, 7, 40].iter().map(|&f| priority_retry(f).as_secs()).collect();
        assert_eq!(waits, [1, 1, 2, 4, 8, 32, 60, 60]);
    }

    #[test]
    fn a_pinned_node_resolves_to_every_distinct_address() {
        let bare = PinnedNode::resolve("127.0.0.1").unwrap();
        assert_eq!(bare.addrs, ["127.0.0.1:17855".parse::<SocketAddr>().unwrap()], "the default port");
        assert_eq!(bare.name, "127.0.0.1");
        let v6 = PinnedNode::resolve("[::1]:9").unwrap();
        assert_eq!(v6.addrs, ["[::1]:9".parse::<SocketAddr>().unwrap()]);
        assert!(PinnedNode::resolve("1.2.3.4:99999").is_err());
    }
}
