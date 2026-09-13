// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Connection plumbing: one reader thread and one writer thread per peer, and
//! the event channel they share with the engine.
//!
//! The C++ node runs every connection as a coroutine on one `System::Dispatcher`
//! (`NetNode::connectionHandler`, `NetNode.cpp:2829`) with a per-connection
//! write queue capped at `P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE` (32 MiB) whose
//! overflow closes the connection. This is the std-only equivalent:
//!
//! - the **reader** thread blocks in [`Connection::read_frame`] under the
//!   socket read timeout — which refuses a frame over its command's size cap
//!   (`wrkz_p2p::limits`) from the header alone — and pushes each frame onto
//!   the bounded channel to the engine. Each peer may have at most
//!   [`INBOUND_FRAMES_PER_PEER`] frames or [`INBOUND_BYTES_PER_PEER`] bytes
//!   waiting there; past that its reader stops reading, so the peer is slowed
//!   by TCP instead of filling the shared queue for everyone;
//! - the **writer** thread owns the send half and drains a queue whose size is
//!   counted in **bytes**: the engine offers frames without blocking and closes
//!   the peer once more than [`WRITE_BUFFER_BYTES`] are waiting, which is the
//!   C++ `pushMessage` rule (`NetNode.cpp:181`) exactly;
//! - the **accept** thread applies the ban list, the inbound cap and the
//!   per-address limits ([`InboundGate`]) before a socket gets any thread at
//!   all, so a refused peer costs one accept and one close;
//! - the **engine** never touches a socket, so no peer can block it. Dialling,
//!   the handshake and the back ping all run on their own short-lived threads
//!   and report back as events.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use wrkz_p2p::conn::{self, Connection};
use wrkz_p2p::levin::{self, Header, OversizedFrame};
use wrkz_p2p::msg::{self, BasicNodeData, CoreSyncData, HandshakeResponse};
use wrkz_primitives::constants::{CRYPTONOTE_NETWORK, P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE, P2P_MINIMUM_VERSION};

use crate::peers::{net_group, BanList, NetGroup};
use crate::sync::ConnId;
use crate::{log_debug, log_trace};

/// Bytes queued for one peer's writer before the peer is dropped:
/// `P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE`, counted the way `pushMessage`
/// counts it — the frame being offered included (`NetNode.cpp:181-187`).
pub const WRITE_BUFFER_BYTES: usize = P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE;
/// Frames queued for one peer's writer. The byte budget is the real limit;
/// this only bounds the channel for a flood of tiny frames.
pub const WRITE_QUEUE_FRAMES: usize = 1024;
/// Frames in flight from every reader to the engine. Bounded, so a flood
/// stalls the readers rather than the allocator.
pub const EVENT_QUEUE: usize = 1024;
/// Frames one peer may have waiting for the engine. With the default 15 + 15
/// connections this keeps every peer's share of [`EVENT_QUEUE`] below its
/// size, so one busy peer cannot make the others wait for queue space.
pub const INBOUND_FRAMES_PER_PEER: usize = 32;
/// Bytes one peer may have waiting for the engine. A single frame larger than
/// this — a whole get-objects batch — is still let through when nothing else
/// from the peer is waiting.
pub const INBOUND_BYTES_PER_PEER: usize = 8 * 1024 * 1024;

/// What a writer thread accepts.
enum Out {
    /// A frame, and whether it is a `NOTIFY_RESPONSE_GET_OBJECTS` answer.
    Frame(Header, Vec<u8>, bool),
    Close,
}

/// The bytes and get-objects answers a writer has yet to send.
#[derive(Debug, Default)]
struct WriteBudget {
    bytes: AtomicUsize,
    objects: AtomicUsize,
}

impl WriteBudget {
    fn release(&self, bytes: usize, objects: bool) {
        self.bytes.fetch_sub(bytes, Ordering::Relaxed);
        if objects {
            self.objects.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// What one peer has waiting in the engine's queue, so its reader can stop
/// reading when it is ahead ([`INBOUND_FRAMES_PER_PEER`],
/// [`INBOUND_BYTES_PER_PEER`]). Every frame carries a [`Ticket`] that gives its
/// share back when the engine is done with it.
#[derive(Debug, Default)]
pub struct Inflow {
    /// `(frames, bytes)` waiting.
    waiting: Mutex<(usize, usize)>,
    room: Condvar,
}

impl Inflow {
    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, usize)> {
        self.waiting.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Block until the peer may have another frame read, or `closed` is set.
    /// Returns false for the latter. The wait is in slices so a peer closed by
    /// the engine is noticed even if no ticket is ever returned.
    fn wait_for_room(&self, closed: &AtomicBool) -> bool {
        let mut w = self.lock();
        loop {
            if closed.load(Ordering::Relaxed) {
                return false;
            }
            let (frames, bytes) = *w;
            if frames == 0 || (frames < INBOUND_FRAMES_PER_PEER && bytes < INBOUND_BYTES_PER_PEER) {
                return true;
            }
            w = self
                .room
                .wait_timeout(w, Duration::from_millis(500))
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }

    fn take(self: &Arc<Self>, bytes: usize) -> Ticket {
        let mut w = self.lock();
        w.0 += 1;
        w.1 += bytes;
        Ticket { inflow: Some(Arc::clone(self)), bytes }
    }

    /// Frames from this peer the engine has not finished with.
    pub fn frames_waiting(&self) -> usize {
        self.lock().0
    }
}

/// One frame's share of its peer's [`Inflow`], given back on drop.
#[derive(Debug)]
pub struct Ticket {
    inflow: Option<Arc<Inflow>>,
    bytes: usize,
}

impl Ticket {
    /// A ticket that accounts for nothing, for a frame made up in a test.
    pub fn none() -> Self {
        Self { inflow: None, bytes: 0 }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(inflow) = self.inflow.take() {
            let mut w = inflow.lock();
            w.0 = w.0.saturating_sub(1);
            w.1 = w.1.saturating_sub(self.bytes);
            inflow.room.notify_all();
        }
    }
}

/// The engine's handle on one peer's writer.
#[derive(Clone)]
pub struct Sink {
    tx: SyncSender<Out>,
    /// Set once the peer has been closed, so repeated sends are cheap no-ops.
    closed: Arc<AtomicBool>,
    budget: Arc<WriteBudget>,
    inflow: Arc<Inflow>,
}

impl Sink {
    fn new(tx: SyncSender<Out>) -> Self {
        Self {
            tx,
            closed: Arc::new(AtomicBool::new(false)),
            budget: Arc::new(WriteBudget::default()),
            inflow: Arc::new(Inflow::default()),
        }
    }

    /// Queue a frame. `false` means the queue would pass [`WRITE_BUFFER_BYTES`]
    /// or the writer is gone: the caller MUST drop the peer, which is what the
    /// C++ write-buffer overflow does.
    #[must_use]
    fn offer(&self, header: Header, payload: Vec<u8>, objects: bool) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        let size = levin::HEADER_LEN + payload.len();
        let before = self.budget.bytes.fetch_add(size, Ordering::Relaxed);
        if before + size > WRITE_BUFFER_BYTES {
            self.budget.bytes.fetch_sub(size, Ordering::Relaxed);
            self.closed.store(true, Ordering::Relaxed);
            return false;
        }
        if objects {
            self.budget.objects.fetch_add(1, Ordering::Relaxed);
        }
        match self.tx.try_send(Out::Frame(header, payload, objects)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.budget.release(size, objects);
                self.closed.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    /// Queue a frame; see [`Sink::notify`] and friends for the usual shapes.
    #[must_use]
    pub fn send(&self, header: Header, payload: Vec<u8>) -> bool {
        self.offer(header, payload, false)
    }

    /// `post_notify`: a notification, never expecting a reply.
    #[must_use]
    pub fn notify(&self, command: u32, payload: Vec<u8>) -> bool {
        self.send(Header::request(command, false), payload)
    }

    /// `NOTIFY_RESPONSE_GET_OBJECTS`, counted until the writer has sent it so
    /// the engine can serve one batch per peer at a time
    /// ([`Sink::objects_in_flight`]).
    #[must_use]
    pub fn notify_objects(&self, payload: Vec<u8>) -> bool {
        self.offer(Header::request(msg::NOTIFY_RESPONSE_GET_OBJECTS, false), payload, true)
    }

    /// Whether a get-objects answer to this peer is still queued or being
    /// written.
    pub fn objects_in_flight(&self) -> bool {
        self.budget.objects.load(Ordering::Relaxed) != 0
    }

    /// Bytes waiting for this peer's writer.
    pub fn queued_bytes(&self) -> usize {
        self.budget.bytes.load(Ordering::Relaxed)
    }

    /// Frames from this peer that are waiting for the engine, or being handled.
    pub fn frames_waiting(&self) -> usize {
        self.inflow.frames_waiting()
    }

    /// A request that expects a response (`COMMAND_TIMED_SYNC` only, in the
    /// steady state: its reply is handled asynchronously, `NetNode.cpp:323`).
    #[must_use]
    pub fn request(&self, command: u32, payload: Vec<u8>) -> bool {
        self.send(Header::request(command, true), payload)
    }

    /// A reply carrying the same command id and the response flag.
    #[must_use]
    pub fn reply(&self, command: u32, return_code: i32, payload: Vec<u8>) -> bool {
        self.send(Header::response(command, return_code), payload)
    }

    /// Close the connection; the writer shuts the socket down, which unblocks
    /// the reader thread. Frames already queued are written first.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(Out::Close);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// Why the reader stopped on a frame it would not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFault {
    /// Not a Levin frame: a wrong signature or impossible flags.
    Malformed,
    /// A frame over its command's size cap.
    Oversized,
}

/// What the engine loop consumes.
pub enum Event {
    /// A connection is usable. `handshake` is present for an outbound
    /// connection (we invoked `COMMAND_HANDSHAKE` and it passed the network id
    /// and minimum version checks); an inbound one arrives before its
    /// handshake, which the engine answers itself.
    Established { id: ConnId, addr: SocketAddr, incoming: bool, sink: Sink, handshake: Option<Box<HandshakeResponse>> },
    /// One Levin frame from a peer. The ticket holds the frame's share of the
    /// peer's inbound allowance until the engine drops it.
    Frame { id: ConnId, header: Header, payload: Vec<u8>, ticket: Ticket },
    /// The reader refused a frame; a [`Event::Closed`] follows.
    BadFrame { id: ConnId, fault: FrameFault, reason: String },
    /// The connection ended, for any reason.
    Closed { id: ConnId, reason: String },
    /// An outbound dial or its handshake failed.
    DialFailed { addr: SocketAddr, reason: String },
    /// The result of a back ping started for an inbound handshake
    /// (`try_ping`, `NetNode.cpp:2160`).
    BackPing { id: ConnId, ip: IpAddr, port: u32, peer_id: u64, ok: bool },
    /// The periodic wake-up: timed sync, the connection maker, bookkeeping.
    Tick,
}

/// Hands out connection ids.
#[derive(Default)]
pub struct ConnIds(AtomicU64);

impl ConnIds {
    pub fn next(&self) -> ConnId {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Spawn the writer, tell the engine about the connection through `announce`,
/// then spawn the reader, and return the engine's sink. `slot`, for an
/// inbound connection, is held by the reader and so released exactly when the
/// connection is gone.
///
/// The order matters: a frame for a connection id the engine has not been
/// told about is dropped, so the reader must not deliver one before
/// `Established` is queued. It used to be spawned first, and a peer that sent
/// its handshake the moment it connected could lose the race and wait out the
/// handshake timeout. `announce` returns false when the engine is gone.
fn attach(
    conn: Connection,
    id: ConnId,
    events: SyncSender<Event>,
    slot: Option<InboundSlot>,
    announce: impl FnOnce(&Sink) -> bool,
) -> std::io::Result<Sink> {
    let write_half = conn.try_clone()?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Out>(WRITE_QUEUE_FRAMES);
    let sink = Sink::new(tx);
    let budget = Arc::clone(&sink.budget);
    let closed = Arc::clone(&sink.closed);
    let inflow = Arc::clone(&sink.inflow);

    std::thread::Builder::new().name(format!("wrkz-p2p-write-{id}")).spawn(move || writer(write_half, rx, &budget))?;
    if !announce(&sink) {
        sink.close();
        return Ok(sink);
    }
    let on_spawn_failure = events.clone();
    let spawned = std::thread::Builder::new().name(format!("wrkz-p2p-read-{id}")).spawn(move || {
        let _slot = slot;
        let reason = reader(conn, id, &events, &inflow, &closed);
        closed.store(true, Ordering::Relaxed);
        let _ = events.send(Event::Closed { id, reason });
    });
    if let Err(e) = spawned {
        // The engine already has the connection; tell it the connection is
        // over rather than leave it to the idle timeout.
        sink.close();
        let _ = on_spawn_failure.send(Event::Closed { id, reason: format!("spawn reader: {e}") });
    }
    Ok(sink)
}

fn writer(mut conn: Connection, rx: Receiver<Out>, budget: &WriteBudget) {
    while let Ok(out) = rx.recv() {
        match out {
            Out::Frame(header, payload, objects) => {
                let sent = conn.send(&header, &payload);
                budget.release(levin::HEADER_LEN + payload.len(), objects);
                if let Err(e) = sent {
                    log_debug!("write to {} failed: {e}", conn.peer_addr());
                    break;
                }
            }
            Out::Close => break,
        }
    }
    // Shutting the socket down is what releases the reader thread.
    conn.shutdown();
}

/// Pump frames to the engine until the socket errors, the peer is closed or
/// the engine is gone. Returns why it stopped.
fn reader(
    mut conn: Connection,
    id: ConnId,
    events: &SyncSender<Event>,
    inflow: &Arc<Inflow>,
    closed: &AtomicBool,
) -> String {
    loop {
        // Back-pressure per peer: while this peer has its share of the queue,
        // read nothing more, and let TCP slow it down.
        if !inflow.wait_for_room(closed) {
            return "closed".to_string();
        }
        match conn.read_frame() {
            Ok((header, payload)) => {
                log_trace!("<- {} command {} ({} bytes)", conn.peer_addr(), header.command, payload.len());
                let ticket = inflow.take(payload.len());
                // A blocking send: the queue is bounded, so a peer that
                // outruns the engine is slowed by TCP rather than buffered.
                if events.send(Event::Frame { id, header, payload, ticket }).is_err() {
                    return "engine stopped".to_string();
                }
            }
            Err(e) => {
                // A timeout or a closed socket is nobody's fault; a frame that
                // is not Levin, or is over its cap, is the peer's.
                if e.kind() == std::io::ErrorKind::InvalidData {
                    let fault =
                        if OversizedFrame::of(&e).is_some() { FrameFault::Oversized } else { FrameFault::Malformed };
                    let _ = events.send(Event::BadFrame { id, fault, reason: e.to_string() });
                }
                return e.to_string();
            }
        }
    }
}

/// Dial `addr`, invoke `COMMAND_HANDSHAKE` and hand the connection to the
/// engine (`NodeServer::handshake`, `NetNode.cpp:860`).
///
/// The two checks that happen here and not in the engine are the ones that
/// decide whether the connection exists at all: a wrong network id and a peer
/// below `P2P_MINIMUM_VERSION` are closed without ever becoming a peer.
pub fn spawn_outbound(
    id: ConnId,
    addr: SocketAddr,
    node_data: BasicNodeData,
    sync_data: CoreSyncData,
    events: SyncSender<Event>,
) {
    let name = format!("wrkz-p2p-dial-{id}");
    let on_spawn_failure = events.clone();
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let fail = |events: &SyncSender<Event>, reason: String| {
            let _ = events.send(Event::DialFailed { addr, reason });
        };
        let mut conn = match Connection::connect(addr, conn::CONNECT_TIMEOUT, conn::HANDSHAKE_TIMEOUT) {
            Ok(c) => c,
            Err(e) => return fail(&events, format!("connect: {e}")),
        };
        let payload = msg::handshake_request(&node_data, &sync_data);
        let (header, body) = match conn.invoke(msg::COMMAND_HANDSHAKE, &payload, conn::HANDSHAKE_TIMEOUT) {
            Ok(v) => v,
            Err(e) => return fail(&events, format!("handshake: {e}")),
        };
        if header.return_code != levin::RETCODE_SUCCESS {
            return fail(&events, format!("handshake return code {}", header.return_code));
        }
        let hs = match msg::parse_handshake_response(&body) {
            Ok(v) => v,
            Err(e) => return fail(&events, format!("handshake response: {e}")),
        };
        if hs.node_data.network_id != CRYPTONOTE_NETWORK {
            return fail(&events, "wrong network id".to_string());
        }
        if hs.node_data.version < P2P_MINIMUM_VERSION {
            return fail(&events, format!("peer version {} below the minimum", hs.node_data.version));
        }
        // Past the handshake the peer only sends notifications and the timed
        // sync; the generous handshake budget would let a dead peer sit for
        // 15 s per read, so fall back to the idle timeout.
        let _ = conn.set_read_timeout(conn::IDLE_TIMEOUT);
        let pending = conn.take_pending();
        let announce = |sink: &Sink| {
            let inflow = Arc::clone(&sink.inflow);
            let established =
                Event::Established { id, addr, incoming: false, sink: sink.clone(), handshake: Some(Box::new(hs)) };
            if events.send(established).is_err() {
                return false;
            }
            // Anything that arrived while we waited for the handshake response
            // is real traffic and is delivered in arrival order, before the
            // reader delivers anything newer. It is at most the invoke's 256
            // frames, each under its command's cap, and counts against the
            // peer's allowance like any other frame.
            for (header, payload) in pending {
                let ticket = inflow.take(payload.len());
                if events.send(Event::Frame { id, header, payload, ticket }).is_err() {
                    return false;
                }
            }
            true
        };
        if let Err(e) = attach(conn, id, events.clone(), None, announce) {
            fail(&events, format!("attach: {e}"));
        }
    });
    if let Err(e) = spawned {
        let _ = on_spawn_failure.send(Event::DialFailed { addr, reason: format!("spawn: {e}") });
    }
}

/// Attach an accepted connection. Its `COMMAND_HANDSHAKE` arrives as an
/// ordinary frame and the engine answers it (`handle_handshake`).
pub fn attach_inbound(
    id: ConnId,
    stream: TcpStream,
    events: SyncSender<Event>,
    slot: InboundSlot,
) -> std::io::Result<()> {
    let conn = Connection::from_stream(stream, conn::IDLE_TIMEOUT)?;
    let addr = conn.peer_addr();
    let announce = |sink: &Sink| {
        events.send(Event::Established { id, addr, incoming: true, sink: sink.clone(), handshake: None }).is_ok()
    };
    attach(conn, id, events.clone(), Some(slot), announce)?;
    Ok(())
}

/// `try_ping` (`NetNode.cpp:2160`): open a *new* connection to the address the
/// peer says it listens on, invoke `COMMAND_PING`, and require `OK` with the
/// same peer id. Only a peer that passes is added to the white list.
pub fn spawn_back_ping(id: ConnId, ip: IpAddr, port: u32, peer_id: u64, events: SyncSender<Event>) {
    let on_spawn_failure = events.clone();
    let spawned = std::thread::Builder::new().name(format!("wrkz-p2p-ping-{id}")).spawn(move || {
        let ok = back_ping(ip, port, peer_id);
        let _ = events.send(Event::BackPing { id, ip, port, peer_id, ok });
    });
    if spawned.is_err() {
        let _ = on_spawn_failure.send(Event::BackPing { id, ip, port, peer_id, ok: false });
    }
}

fn back_ping(ip: IpAddr, port: u32, peer_id: u64) -> bool {
    let Ok(port) = u16::try_from(port) else { return false };
    let addr = SocketAddr::new(ip, port);
    let Ok(mut conn) = Connection::connect(addr, conn::CONNECT_TIMEOUT, conn::PING_TIMEOUT) else {
        return false;
    };
    match conn.invoke(msg::COMMAND_PING, &msg::ping_request(), conn::PING_TIMEOUT) {
        Ok((header, body)) if header.return_code == levin::RETCODE_SUCCESS => {
            msg::parse_ping_response(&body).map(|r| r.is_ok_for(peer_id)).unwrap_or(false)
        }
        _ => false,
    }
}

/// How many inbound connections the accept threads let in. A per-address or
/// per-group limit of 0 is no limit; a total of 0 admits nobody, as
/// `--in-peers 0` always has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundLimits {
    /// `--in-peers`: all inbound connections, before their handshake included.
    pub total: usize,
    /// From one address. Not applied to loopback.
    pub per_ip: usize,
    /// From one network group ([`net_group`]). Not applied to loopback.
    pub per_group: usize,
}

/// The accept threads' admission check: the ban list, the inbound cap and the
/// per-address and per-group limits, applied **before** a socket gets its two
/// threads. The C++ checks none of this until the connection exists, and
/// neither did this node — the engine refused a peer only after its reader and
/// writer were running.
///
/// The counts are of sockets alive, released when a connection's reader thread
/// ends, which is after the engine has dropped it.
pub struct InboundGate {
    limits: InboundLimits,
    bans: BanList,
    counts: Mutex<GateCounts>,
}

#[derive(Default)]
struct GateCounts {
    total: usize,
    by_ip: HashMap<IpAddr, usize>,
    by_group: HashMap<NetGroup, usize>,
}

/// One admitted inbound connection's place in the [`InboundGate`] counts.
pub struct InboundSlot {
    gate: Arc<InboundGate>,
    ip: IpAddr,
}

impl InboundGate {
    pub fn new(limits: InboundLimits, bans: BanList) -> Arc<Self> {
        Arc::new(Self { limits, bans, counts: Mutex::new(GateCounts::default()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateCounts> {
        self.counts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Admit a connection from `ip`, or say why not.
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Result<InboundSlot, String> {
        let ip = ip.to_canonical();
        if self.bans.is_banned(ip) {
            return Err("banned".to_string());
        }
        let group = net_group(ip);
        let mut c = self.lock();
        let over = |limit: usize, n: usize| limit != 0 && n >= limit;
        if c.total >= self.limits.total {
            return Err(format!("inbound connection limit {} reached", self.limits.total));
        }
        // Everything on this host shares one address; the host is not the
        // adversary these limits are for.
        if !ip.is_loopback() {
            if over(self.limits.per_ip, c.by_ip.get(&ip).copied().unwrap_or(0)) {
                return Err(format!("{} connections from this address already", self.limits.per_ip));
            }
            if over(self.limits.per_group, c.by_group.get(&group).copied().unwrap_or(0)) {
                return Err(format!("{} connections from this network group already", self.limits.per_group));
            }
        }
        c.total += 1;
        *c.by_ip.entry(ip).or_insert(0) += 1;
        *c.by_group.entry(group).or_insert(0) += 1;
        Ok(InboundSlot { gate: Arc::clone(self), ip })
    }

    /// Inbound connections currently admitted.
    pub fn open_count(&self) -> usize {
        self.lock().total
    }
}

impl Drop for InboundSlot {
    fn drop(&mut self) {
        let group = net_group(self.ip);
        let mut c = self.gate.lock();
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.by_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                c.by_ip.remove(&self.ip);
            }
        }
        if let Some(n) = c.by_group.get_mut(&group) {
            *n -= 1;
            if *n == 0 {
                c.by_group.remove(&group);
            }
        }
    }
}

/// Accept loop. Each accepted socket passes the [`InboundGate`] or is closed
/// on the spot, before any thread is spawned for it; the engine still applies
/// the cap and the ban list when the `Established` event arrives, for a ban
/// added in between.
///
/// `family` is `"v4"` or `"v6"` and names the thread only: both loops hand
/// their sockets to the **same** `events` channel, the same [`ConnIds`] and
/// the same gate, so the engine sees one stream of connections and applies one
/// inbound cap, one peer manager and one ban table to both. That is
/// `NodeServer::acceptLoop` and `NodeServer::acceptLoopIPv6`
/// (`NetNode.cpp:2588`, `:2656`), which likewise differ only in which listener
/// they call `accept()` on.
pub fn spawn_listener(
    listener: TcpListener,
    family: &str,
    ids: Arc<ConnIds>,
    events: SyncSender<Event>,
    gate: Arc<InboundGate>,
) {
    let _ = std::thread::Builder::new().name(format!("wrkz-p2p-accept-{family}")).spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let admitted = s.peer_addr().map_err(|e| e.to_string()).and_then(|a| gate.admit(a.ip()));
                    let slot = match admitted {
                        Ok(slot) => slot,
                        Err(reason) => {
                            log_debug!(
                                "refusing inbound {}: {reason}",
                                s.peer_addr().map_or("?".into(), |a| a.to_string())
                            );
                            continue;
                        }
                    };
                    let id = ids.next();
                    if let Err(e) = attach_inbound(id, s, events.clone(), slot) {
                        log_debug!("inbound attach failed: {e}");
                    }
                }
                Err(e) => {
                    log_debug!("accept failed: {e}");
                    // A listener that keeps erroring would spin; stopping is
                    // safe because the node still has its outbound peers.
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        return;
                    }
                }
            }
        }
    });
}

/// The engine's heartbeat.
pub fn spawn_ticker(interval: Duration, events: SyncSender<Event>) {
    let _ = std::thread::Builder::new().name("wrkz-p2p-tick".to_string()).spawn(move || loop {
        std::thread::sleep(interval);
        // A full queue means the engine is busy and will get here anyway; a
        // dropped tick is better than a backlog of them.
        match events.try_send(Event::Tick) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => return,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    /// A sink whose writer has gone reports the failure instead of blocking,
    /// which is what makes "drop the peer when its queue is full" reachable.
    #[test]
    fn a_full_sink_reports_failure() {
        let (tx, rx) = sync_channel::<Out>(1);
        let sink = Sink::new(tx);
        assert!(sink.notify(msg::NOTIFY_NEW_TRANSACTIONS, vec![1, 2, 3]));
        assert!(!sink.notify(msg::NOTIFY_NEW_TRANSACTIONS, vec![4]), "queue of 1 is full");
        assert!(sink.is_closed());
        drop(rx);
    }

    /// The write queue is counted in bytes, the offered frame included, as
    /// `pushMessage` counts it: a frame that fits exactly is taken, one byte
    /// more closes the peer. Bytes the writer has sent are given back.
    #[test]
    fn the_write_queue_is_counted_in_bytes() {
        let (tx, rx) = sync_channel::<Out>(16);
        let sink = Sink::new(tx);
        let half = WRITE_BUFFER_BYTES / 2 - levin::HEADER_LEN;
        assert!(sink.notify(msg::NOTIFY_NEW_BLOCK, vec![0; half]));
        assert!(sink.notify_objects(vec![0; half]));
        assert_eq!(sink.queued_bytes(), WRITE_BUFFER_BYTES);
        assert!(sink.objects_in_flight());
        // what the writer does after each send
        let Ok(Out::Frame(_, p, objects)) = rx.recv() else { panic!() };
        sink.budget.release(levin::HEADER_LEN + p.len(), objects);
        assert!(sink.objects_in_flight(), "the get-objects answer is still queued");
        let Ok(Out::Frame(_, p, objects)) = rx.recv() else { panic!() };
        sink.budget.release(levin::HEADER_LEN + p.len(), objects);
        assert!(!sink.objects_in_flight());
        assert_eq!(sink.queued_bytes(), 0);
        // one frame on its own can never exceed the budget
        assert!(!sink.notify(msg::NOTIFY_NEW_BLOCK, vec![0; WRITE_BUFFER_BYTES]));
        assert!(sink.is_closed());
        assert_eq!(sink.queued_bytes(), 0, "a refused frame is not counted");
    }

    /// A peer with its share of the engine's queue waiting is not read from
    /// until a frame is handled; closing the peer releases the wait too.
    #[test]
    fn a_reader_waits_for_its_share_of_the_queue() {
        let inflow = Arc::new(Inflow::default());
        let closed = Arc::new(AtomicBool::new(false));
        let mut tickets: Vec<Ticket> = (0..INBOUND_FRAMES_PER_PEER).map(|_| inflow.take(10)).collect();
        assert_eq!(inflow.frames_waiting(), INBOUND_FRAMES_PER_PEER);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (i, c) = (Arc::clone(&inflow), Arc::clone(&closed));
        std::thread::spawn(move || {
            let _ = done_tx.send(i.wait_for_room(&c));
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err(), "the reader must wait");
        tickets.pop();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(true), "one frame handled frees a slot");

        // one frame over the byte allowance is let through when nothing else
        // waits, and holds the next one back
        drop(tickets);
        let big = inflow.take(INBOUND_BYTES_PER_PEER + 1);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (i, c) = (Arc::clone(&inflow), Arc::clone(&closed));
        std::thread::spawn(move || {
            let _ = done_tx.send(i.wait_for_room(&c));
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        closed.store(true, Ordering::Relaxed);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(false), "a closed peer stops waiting");
        drop(big);
        assert_eq!(inflow.frames_waiting(), 0);
        drop(Ticket::none());
    }

    /// The accept gate: bans, the total, per address and per group, with the
    /// counts given back when a connection ends; loopback is exempt from the
    /// per-address limits but not from the total.
    #[test]
    fn the_inbound_gate() {
        let bans = BanList::new();
        let gate = InboundGate::new(InboundLimits { total: 6, per_ip: 2, per_group: 3 }, bans.clone());
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let a1 = gate.admit(ip("45.10.0.1")).unwrap();
        let _a2 = gate.admit(ip("45.10.0.1")).unwrap();
        assert!(gate.admit(ip("45.10.0.1")).is_err(), "per address");
        let _b = gate.admit(ip("45.10.0.2")).unwrap();
        assert!(gate.admit(ip("45.10.0.3")).is_err(), "per /16");
        assert!(gate.admit(ip("::ffff:45.10.0.4")).is_err(), "a mapped address is its IPv4 address");
        drop(a1);
        let _a3 = gate.admit(ip("45.10.0.3")).unwrap();
        bans.ban(ip("46.1.0.1"), 60);
        assert!(gate.admit(ip("46.1.0.1")).is_err(), "banned");
        let _l: Vec<InboundSlot> = (0..3).map(|_| gate.admit(ip("127.0.0.1")).unwrap()).collect();
        assert_eq!(gate.open_count(), 6);
        assert!(gate.admit(ip("127.0.0.1")).is_err(), "the total applies to loopback too");
    }

    #[test]
    fn conn_ids_are_unique_and_nonzero() {
        let ids = ConnIds::default();
        assert_eq!(ids.next(), 1);
        assert_eq!(ids.next(), 2);
    }
}
