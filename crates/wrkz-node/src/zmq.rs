// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The ZMQ publisher (`src/daemon/ZmqPublisher.cpp`): `--zmq-pub ADDRESS`,
//! on by default at `tcp://127.0.0.1:17857`, and `--no-zmq`.
//!
//! # Topics
//!
//! Every message is two frames, the topic and a compact JSON body, byte for
//! byte what the C++ sends (`ZmqPublisher.cpp:168-226`). `height` is the
//! block's index.
//!
//! | Topic | Body | Sent when |
//! | --- | --- | --- |
//! | `hashblock` | `{"height":N,"hash":"…"}` | a block joins the main chain |
//! | `chain_main` | `{"height":N,"hash":"…","transaction_hashes":["…",…]}` | straight after; the coinbase first |
//! | `hashblock_alt` | `{"height":N,"hash":"…"}` | a block is kept on an alternative chain |
//! | `chainswitch` | `{"common_root_height":R,"hashes":["…",…]}` | a reorganisation; the common root first |
//! | `txpool_add` | `{"hashes":["…"]}` | a transaction enters the pool |
//! | `txpool_del` | `{"hashes":[…],"reason":"InBlock"}` (or `Outdated`, `NotActual`) | transactions leave it |
//!
//! Blocks a reorganisation brings in are announced by `chainswitch` alone,
//! never by `hashblock`, as in the C++.
//!
//! Where this differs, on purpose:
//!
//! - `txpool_del` with `InBlock` lists the transactions the block mined; the
//!   C++'s list is always empty (see [`wrkz_rpc::events::block_events`]), and
//!   an empty `txpool_del` is not sent at all.
//! - An IPv6 address binds. The C++ never sets `ZMQ_IPV6`, so libzmq refuses
//!   one there.
//! - An `ipc://` socket is created owner-only, as the RPC's is; libzmq leaves
//!   it to the umask.
//!
//! # The wire
//!
//! Not libzmq, but the protocol it speaks: a PUB socket over ZMTP 3.1
//! (rfc.zeromq.org/spec/37) with the NULL mechanism, which is all the C++'s
//! socket offers — it is built without CURVE and sets no ZAP handler
//! (`external/CMakeLists.txt:75-81`). That keeps a C++ library and its build
//! off every platform this daemon cross-compiles to, and a publisher is the
//! small end of the protocol: exchange greetings and READY commands, then
//! write frames, and answer the three things a subscriber ever says —
//! SUBSCRIBE, CANCEL (or their ZMTP 3.0 form, a one-frame message whose first
//! byte is 1 or 0) and PING.
//!
//! A subscriber receives the topics it subscribed to, matched by prefix as
//! libzmq matches them, so subscribing to `hashblock` brings `hashblock_alt`
//! too, and the empty prefix brings everything. One that stops reading loses
//! messages once [`QUEUE_MESSAGES`] are waiting for it — libzmq's PUB socket
//! drops at its send high-water mark in the same way — and never holds up the
//! node or the other subscribers.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_primitives::Hash;
use wrkz_rpc::events::{ChainEvent, EventListener};

use crate::{log_debug, log_info, log_warn};

/// `ZMQ_PUB_DEFAULT_ENDPOINT` (`CryptoNoteConfig.h:532`).
pub const DEFAULT_ENDPOINT: &str = "tcp://127.0.0.1:17857";
/// Messages waiting for one subscriber before it misses some: libzmq's default
/// `ZMQ_SNDHWM`.
pub const QUEUE_MESSAGES: usize = 1000;
/// Subscribers at once. libzmq has no such limit; each one here costs two
/// threads, so there is one.
pub const MAX_SUBSCRIBERS: usize = 64;
/// The largest frame a subscriber may send. It only ever sends topic prefixes
/// and a PING.
const MAX_INBOUND_FRAME: u64 = 64 * 1024;
const GREETING_BYTES: usize = 64;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A subscriber that does not take a frame within this is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const WRITER_POLL: Duration = Duration::from_millis(100);

const FLAG_MORE: u8 = 0x01;
const FLAG_LONG: u8 = 0x02;
const FLAG_COMMAND: u8 = 0x04;

/// A `--zmq-pub` address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `tcp://host:port`. A host of `*` is every IPv4 interface and a port of
    /// `*` or `0` an ephemeral one, as libzmq reads them; an IPv6 host is
    /// written in brackets.
    Tcp { host: String, port: u16 },
    /// `ipc://path`: a local socket (`/path`, or `@name` on Linux).
    Ipc(String),
}

impl Endpoint {
    pub fn parse(text: &str) -> Result<Endpoint, String> {
        if let Some(address) = text.strip_prefix("tcp://") {
            let (host, port) = match address.strip_prefix('[') {
                Some(rest) => {
                    let (host, after) =
                        rest.split_once(']').ok_or_else(|| format!("{text}: unclosed [ in the address"))?;
                    (host, after.strip_prefix(':').ok_or_else(|| format!("{text}: no port"))?)
                }
                None => address.rsplit_once(':').ok_or_else(|| format!("{text}: no port"))?,
            };
            if host.is_empty() {
                return Err(format!("{text}: no host"));
            }
            let port = match port {
                "*" => 0,
                p => p.parse::<u16>().map_err(|_| format!("{text}: {p} is not a port"))?,
            };
            Ok(Endpoint::Tcp { host: host.to_string(), port })
        } else if let Some(path) = text.strip_prefix("ipc://") {
            if path.is_empty() {
                return Err(format!("{text}: no socket path"));
            }
            Ok(Endpoint::Ipc(path.to_string()))
        } else {
            match text.split_once("://") {
                Some((scheme, _)) => Err(format!("{text}: the {scheme}:// transport is not supported here")),
                None => Err(format!("{text}: not a ZMQ address; use tcp://host:port or ipc://path")),
            }
        }
    }
}

/// `ZmqPublisher::isNonLoopbackTcpEndpoint` (`ZmqPublisher.cpp:293-330`),
/// test for test: only the host is looked at, and only the three spellings of
/// loopback count. `*` is therefore non-loopback, which it is.
pub fn is_non_loopback_tcp(endpoint: &str) -> bool {
    let Some(address) = endpoint.strip_prefix("tcp://") else { return false };
    if address.is_empty() {
        return false;
    }
    let host = if let Some(rest) = address.strip_prefix('[') {
        let Some((host, _)) = rest.split_once(']') else { return false };
        host
    } else {
        address.rsplit_once(':').map_or(address, |(host, _)| host)
    };
    let host = host.to_ascii_lowercase();
    !(host == "127.0.0.1" || host == "localhost" || host == "::1")
}

/// The JSON bodies an event is published as, with their topics
/// (`ZmqPublisher::publishMessage`).
pub fn messages(event: &ChainEvent) -> Vec<(&'static str, String)> {
    match event {
        ChainEvent::BlockAdded { index, hash, transaction_hashes } => vec![
            ("hashblock", format!("{{\"height\":{index},\"hash\":\"{}\"}}", hex::encode(hash))),
            (
                "chain_main",
                format!(
                    "{{\"height\":{index},\"hash\":\"{}\",\"transaction_hashes\":{}}}",
                    hex::encode(hash),
                    hash_array(transaction_hashes)
                ),
            ),
        ],
        ChainEvent::AlternativeBlockAdded { index, hash } => {
            vec![("hashblock_alt", format!("{{\"height\":{index},\"hash\":\"{}\"}}", hex::encode(hash)))]
        }
        ChainEvent::ChainSwitched { common_root_index, hashes } => vec![(
            "chainswitch",
            format!("{{\"common_root_height\":{common_root_index},\"hashes\":{}}}", hash_array(hashes)),
        )],
        ChainEvent::PoolAdded { hash } => {
            vec![("txpool_add", format!("{{\"hashes\":[\"{}\"]}}", hex::encode(hash)))]
        }
        ChainEvent::PoolRemoved { hashes, reason } => {
            vec![("txpool_del", format!("{{\"hashes\":{},\"reason\":\"{}\"}}", hash_array(hashes), reason.as_str()))]
        }
    }
}

fn hash_array(hashes: &[Hash]) -> String {
    let quoted: Vec<String> = hashes.iter().map(|h| format!("\"{}\"", hex::encode(h))).collect();
    format!("[{}]", quoted.join(","))
}

// -- ZMTP -------------------------------------------------------------------

/// Our greeting (RFC 37, "The greeting"): the signature, version 3.1, the NULL
/// mechanism, and as-server 0, which NULL does not use. Byte 8 is 1 as libzmq
/// writes it, the length an older peer would read there.
pub fn greeting() -> [u8; GREETING_BYTES] {
    let mut g = [0u8; GREETING_BYTES];
    g[0] = 0xff;
    g[8] = 0x01;
    g[9] = 0x7f;
    g[10] = 3;
    g[11] = 1;
    g[12..16].copy_from_slice(b"NULL");
    g
}

/// Whether a peer's greeting is one this publisher can go on with: ZMTP 3 or
/// later, with the NULL mechanism.
pub fn check_greeting(g: &[u8; GREETING_BYTES]) -> Result<(), String> {
    if g[0] != 0xff || g[9] & 0x01 == 0 {
        return Err("not a ZMTP greeting".to_string());
    }
    if g[10] < 3 {
        return Err(format!("ZMTP {}.{} is too old; this publisher needs 3.0 or later", g[10], g[11]));
    }
    let mechanism = &g[12..32];
    let name_len = mechanism.iter().position(|&b| b == 0).unwrap_or(mechanism.len());
    if &mechanism[..name_len] != b"NULL" || mechanism[name_len..].iter().any(|&b| b != 0) {
        let name = String::from_utf8_lossy(&mechanism[..name_len]);
        return Err(format!("the {name} security mechanism is not offered; only NULL is"));
    }
    Ok(())
}

/// One frame, flags and size before its body (RFC 37, "Framing").
pub fn put_frame(out: &mut Vec<u8>, flags: u8, body: &[u8]) {
    match u8::try_from(body.len()) {
        Ok(short) => {
            out.push(flags);
            out.push(short);
        }
        Err(_) => {
            out.push(flags | FLAG_LONG);
            out.extend_from_slice(&(body.len() as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(body);
}

/// A command frame: its name's length, the name, then its data.
pub fn command(name: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + name.len() + data.len());
    body.push(name.len() as u8);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(data);
    let mut out = Vec::with_capacity(body.len() + 9);
    put_frame(&mut out, FLAG_COMMAND, &body);
    out
}

/// A metadata property as READY carries it: a one-byte name length, the name,
/// a four-byte big-endian value length and the value.
fn property(name: &str, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + name.len() + 4 + value.len());
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// Our READY: the one property a PUB socket announces.
pub fn ready() -> Vec<u8> {
    command("READY", &property("Socket-Type", b"PUB"))
}

/// The ERROR command, whose reason a peer logs before it goes.
pub fn error_command(reason: &str) -> Vec<u8> {
    let reason = &reason.as_bytes()[..reason.len().min(255)];
    let mut data = Vec::with_capacity(1 + reason.len());
    data.push(reason.len() as u8);
    data.extend_from_slice(reason);
    command("ERROR", &data)
}

/// A published message: the topic frame, then the body frame.
pub fn encode_message(topic: &str, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(topic.len() + body.len() + 11);
    put_frame(&mut out, FLAG_MORE, topic.as_bytes());
    put_frame(&mut out, 0, body.as_bytes());
    out
}

struct Frame {
    flags: u8,
    body: Vec<u8>,
}

impl Frame {
    fn is_command(&self) -> bool {
        self.flags & FLAG_COMMAND != 0
    }

    fn has_more(&self) -> bool {
        self.flags & FLAG_MORE != 0
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut flags = [0u8; 1];
    r.read_exact(&mut flags)?;
    let flags = flags[0];
    if flags & !(FLAG_MORE | FLAG_LONG | FLAG_COMMAND) != 0 {
        return Err(invalid(format!("frame flags {flags:#04x} set reserved bits")));
    }
    let size = if flags & FLAG_LONG != 0 {
        let mut size = [0u8; 8];
        r.read_exact(&mut size)?;
        u64::from_be_bytes(size)
    } else {
        let mut size = [0u8; 1];
        r.read_exact(&mut size)?;
        u64::from(size[0])
    };
    if size > MAX_INBOUND_FRAME {
        return Err(invalid(format!("a {size}-byte frame from a subscriber")));
    }
    let mut body = vec![0u8; size as usize];
    r.read_exact(&mut body)?;
    Ok(Frame { flags, body })
}

/// A command's name and data.
fn split_command(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&len, rest) = body.split_first()?;
    let name = rest.get(..usize::from(len))?;
    Some((name, &rest[usize::from(len)..]))
}

/// The value of one metadata property in a READY's data, by name, which RFC 37
/// makes case-insensitive. `None` for a missing property or malformed data.
fn metadata<'a>(mut data: &'a [u8], wanted: &str) -> Option<&'a [u8]> {
    while !data.is_empty() {
        let (&name_len, rest) = data.split_first()?;
        let name = rest.get(..usize::from(name_len))?;
        let rest = &rest[usize::from(name_len)..];
        let value_len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let value = rest.get(4..4 + value_len)?;
        if name.eq_ignore_ascii_case(wanted.as_bytes()) {
            return Some(value);
        }
        data = &rest[4 + value_len..];
    }
    None
}

enum HandshakeError {
    Io(io::Error),
    /// The connection is closed without a word: there is no agreed framing to
    /// put one in, or the peer has already said why.
    Quiet(String),
    /// Answered with an ERROR command, then closed.
    Refused(String),
}

/// Read the subscriber's greeting and READY. Ours were queued before this was
/// called, so a peer waiting to see our signature before it sends the rest of
/// its greeting — libzmq does — is not left waiting.
fn handshake<R: Read>(stream: &mut R) -> Result<(), HandshakeError> {
    let mut peer = [0u8; GREETING_BYTES];
    stream.read_exact(&mut peer).map_err(HandshakeError::Io)?;
    check_greeting(&peer).map_err(HandshakeError::Quiet)?;
    let frame = read_frame(stream).map_err(HandshakeError::Io)?;
    if !frame.is_command() {
        return Err(HandshakeError::Quiet("a message arrived before READY".to_string()));
    }
    let Some((name, data)) = split_command(&frame.body) else {
        return Err(HandshakeError::Quiet("a malformed command".to_string()));
    };
    match name {
        b"READY" => {}
        b"ERROR" => {
            let reason = data.split_first().map(|(_, r)| String::from_utf8_lossy(r).into_owned()).unwrap_or_default();
            return Err(HandshakeError::Quiet(format!("the subscriber sent ERROR: {reason}")));
        }
        other => {
            return Err(HandshakeError::Quiet(format!("expected READY, got {}", String::from_utf8_lossy(other))));
        }
    }
    match metadata(data, "Socket-Type") {
        Some(t) if t.eq_ignore_ascii_case(b"SUB") || t.eq_ignore_ascii_case(b"XSUB") => Ok(()),
        Some(t) => {
            Err(HandshakeError::Refused(format!("a PUB socket does not talk to {}", String::from_utf8_lossy(t))))
        }
        None => Err(HandshakeError::Refused("READY carried no Socket-Type".to_string())),
    }
}

// -- sockets ------------------------------------------------------------------

enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Stream {
    fn try_clone(&self) -> io::Result<Stream> {
        match self {
            Stream::Tcp(s) => s.try_clone().map(Stream::Tcp),
            #[cfg(unix)]
            Stream::Unix(s) => s.try_clone().map(Stream::Unix),
        }
    }

    fn prepare(&self) {
        // An accepted socket inherits the listener's non-blocking mode on
        // Windows, and not on Linux; make it blocking on both.
        match self {
            Stream::Tcp(s) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_nodelay(true);
                let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
                let _ = s.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
            }
            #[cfg(unix)]
            Stream::Unix(s) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
                let _ = s.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
            }
        }
    }

    /// No read timeout: a subscriber that is happy with its subscriptions has
    /// nothing more to say.
    fn wait_indefinitely(&self) {
        match self {
            Stream::Tcp(s) => {
                let _ = s.set_read_timeout(None);
            }
            #[cfg(unix)]
            Stream::Unix(s) => {
                let _ = s.set_read_timeout(None);
            }
        }
    }

    fn shutdown(&self) {
        match self {
            Stream::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            #[cfg(unix)]
            Stream::Unix(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
        }
    }
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Ipc(std::os::unix::net::UnixListener, String),
}

impl Listener {
    fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        match endpoint {
            Endpoint::Tcp { host, port } => {
                let addr = if host == "*" {
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), *port)
                } else if let Ok(ip) = host.parse::<IpAddr>() {
                    SocketAddr::new(ip, *port)
                } else {
                    (host.as_str(), *port)
                        .to_socket_addrs()?
                        .next()
                        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("{host} has no address")))?
                };
                let listener = TcpListener::bind(addr)?;
                listener.set_nonblocking(true)?;
                Ok(Listener::Tcp(listener))
            }
            #[cfg(unix)]
            Endpoint::Ipc(path) => {
                let listener = wrkz_rpc::ipc::bind(path, wrkz_rpc::ipc::DEFAULT_MODE, "")?;
                listener.set_nonblocking(true)?;
                Ok(Listener::Ipc(listener, path.clone()))
            }
            #[cfg(not(unix))]
            Endpoint::Ipc(_) => Err(io::Error::new(ErrorKind::Unsupported, wrkz_rpc::ipc::UNSUPPORTED)),
        }
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        match self {
            Listener::Tcp(l) => l.local_addr().ok(),
            #[cfg(unix)]
            Listener::Ipc(..) => None,
        }
    }

    fn accept(&self) -> io::Result<(Stream, String)> {
        match self {
            Listener::Tcp(l) => l.accept().map(|(s, addr)| (Stream::Tcp(s), addr.to_string())),
            #[cfg(unix)]
            Listener::Ipc(l, path) => l.accept().map(|(s, _)| (Stream::Unix(s), wrkz_rpc::ipc::describe(path))),
        }
    }

    fn close(self) {
        #[cfg(unix)]
        if let Listener::Ipc(_, path) = &self {
            wrkz_rpc::ipc::cleanup(path);
        }
    }
}

// -- the publisher ----------------------------------------------------------

struct Subscriber {
    peer: String,
    /// For `shutdown`: the reader and the writer each hold a clone.
    stream: Stream,
    outbox: SyncSender<Arc<Vec<u8>>>,
    /// Set once the handshake is done; nothing is published to it before.
    ready: AtomicBool,
    closing: AtomicBool,
    /// Topic prefixes, one entry per SUBSCRIBE: a CANCEL takes one away, as
    /// libzmq counts them.
    topics: Mutex<Vec<Vec<u8>>>,
}

impl Subscriber {
    fn topics(&self) -> MutexGuard<'_, Vec<Vec<u8>>> {
        self.topics.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wants(&self, topic: &[u8]) -> bool {
        self.ready.load(Ordering::SeqCst)
            && !self.closing.load(Ordering::SeqCst)
            && self.topics().iter().any(|prefix| topic.starts_with(prefix))
    }

    fn subscribe(&self, prefix: &[u8]) {
        self.topics().push(prefix.to_vec());
    }

    fn cancel(&self, prefix: &[u8]) {
        let mut topics = self.topics();
        if let Some(i) = topics.iter().position(|p| p == prefix) {
            topics.swap_remove(i);
        }
    }

    /// Queue bytes for the writer; `false` when there was no room.
    fn queue(&self, bytes: Arc<Vec<u8>>) -> bool {
        match self.outbox.try_send(bytes) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.close_now();
                false
            }
        }
    }

    /// Stop reading; the writer sends what is queued, then closes the socket.
    fn close_after_flush(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    fn close_now(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.stream.shutdown();
    }
}

struct Shared {
    running: AtomicBool,
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
    published: AtomicU64,
    dropped: AtomicU64,
    /// Set once the subscriber limit has been reported, so a client retrying in
    /// a loop does not fill the log.
    reported_limit: AtomicBool,
}

impl Shared {
    fn subscribers(&self) -> MutexGuard<'_, Vec<Arc<Subscriber>>> {
        self.subscribers.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// `ZmqPublisher::sendMultipart`: one message to every subscriber that
    /// asked for its topic.
    fn publish(&self, topic: &str, body: &str) {
        if !self.running() {
            return;
        }
        let message = Arc::new(encode_message(topic, body));
        self.published.fetch_add(1, Ordering::Relaxed);
        let subscribers = self.subscribers().clone();
        for subscriber in subscribers.iter().filter(|s| s.wants(topic.as_bytes())) {
            if !subscriber.queue(Arc::clone(&message)) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn accept_loop(self: Arc<Self>, listener: Listener) {
        while self.running() {
            match listener.accept() {
                Ok((stream, peer)) => self.admit(stream, peer),
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
                Err(e) => {
                    log_debug!("ZMQ accept failed: {e}");
                    std::thread::sleep(ACCEPT_POLL);
                }
            }
        }
        listener.close();
    }

    fn admit(self: &Arc<Self>, stream: Stream, peer: String) {
        let mut subscribers = self.subscribers();
        if subscribers.len() >= MAX_SUBSCRIBERS {
            if !self.reported_limit.swap(true, Ordering::SeqCst) {
                log_warn!("refused a ZMQ subscriber from {peer}: already at the {MAX_SUBSCRIBERS} subscriber limit");
            }
            return;
        }
        self.reported_limit.store(false, Ordering::SeqCst);
        stream.prepare();
        let (Ok(reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        let (outbox, queued) = sync_channel(QUEUE_MESSAGES + 2);
        let subscriber = Arc::new(Subscriber {
            peer,
            stream,
            outbox,
            ready: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            topics: Mutex::new(Vec::new()),
        });
        // Our greeting and READY go out first, before the peer has said
        // anything and before anything can be published to it.
        let mut hello = greeting().to_vec();
        hello.extend_from_slice(&ready());
        subscriber.queue(Arc::new(hello));
        subscribers.push(Arc::clone(&subscriber));
        drop(subscribers);

        let for_writer = Arc::clone(&subscriber);
        let spawned_writer = std::thread::Builder::new()
            .name("wrkz-zmq-write".into())
            .spawn(move || write_loop(&for_writer, writer, queued));
        let shared = Arc::clone(self);
        let for_reader = Arc::clone(&subscriber);
        let spawned_reader = std::thread::Builder::new()
            .name("wrkz-zmq-read".into())
            .spawn(move || shared.read_loop(&for_reader, reader));
        if spawned_writer.is_err() || spawned_reader.is_err() {
            subscriber.close_now();
            self.drop_subscriber(&subscriber);
        }
    }

    fn read_loop(&self, subscriber: &Arc<Subscriber>, mut reader: Stream) {
        match handshake(&mut reader) {
            Ok(()) => {
                reader.wait_indefinitely();
                subscriber.ready.store(true, Ordering::SeqCst);
                log_debug!("ZMQ subscriber connected from {}", subscriber.peer);
                if let Err(e) = serve_subscriber(subscriber, &mut reader) {
                    log_debug!("ZMQ subscriber {} gone: {e}", subscriber.peer);
                }
                subscriber.close_now();
            }
            Err(HandshakeError::Refused(reason)) => {
                log_debug!("ZMQ connection from {} refused: {reason}", subscriber.peer);
                subscriber.queue(Arc::new(error_command(&reason)));
                subscriber.close_after_flush();
            }
            Err(HandshakeError::Quiet(reason)) => {
                log_debug!("ZMQ connection from {} closed: {reason}", subscriber.peer);
                subscriber.close_now();
            }
            Err(HandshakeError::Io(e)) => {
                log_debug!("ZMQ connection from {} ended during the handshake: {e}", subscriber.peer);
                subscriber.close_now();
            }
        }
        self.drop_subscriber(subscriber);
    }

    fn drop_subscriber(&self, subscriber: &Arc<Subscriber>) {
        self.subscribers().retain(|s| !Arc::ptr_eq(s, subscriber));
    }
}

impl EventListener for Shared {
    fn on_event(&self, event: &ChainEvent) {
        for (topic, body) in messages(event) {
            self.publish(topic, &body);
        }
    }
}

/// What a subscriber says once it is connected: which topics it wants, and
/// PING.
fn serve_subscriber(subscriber: &Subscriber, reader: &mut Stream) -> io::Result<()> {
    // Inside a multi-frame data message, whose frames are skipped: a PUB
    // socket has no use for messages, only for the one-frame subscriptions of
    // ZMTP 3.0.
    let mut in_message = false;
    loop {
        let frame = read_frame(reader)?;
        if frame.is_command() {
            let Some((name, data)) = split_command(&frame.body) else {
                return Err(invalid("a malformed command".to_string()));
            };
            match name {
                b"SUBSCRIBE" => subscriber.subscribe(data),
                b"CANCEL" => subscriber.cancel(data),
                b"PING" => {
                    // A two-byte TTL, then up to 16 bytes of context to echo.
                    let context = data.get(2..).unwrap_or_default();
                    subscriber.queue(Arc::new(command("PONG", &context[..context.len().min(16)])));
                }
                b"ERROR" => return Err(invalid("the subscriber sent ERROR".to_string())),
                _ => {}
            }
        } else if in_message {
            in_message = frame.has_more();
        } else if frame.has_more() {
            in_message = true;
        } else {
            match frame.body.split_first() {
                Some((&1, prefix)) => subscriber.subscribe(prefix),
                Some((&0, prefix)) => subscriber.cancel(prefix),
                _ => {}
            }
        }
    }
}

/// Send what is queued, in order; once the subscriber is closing and the queue
/// is empty, close the socket.
fn write_loop(subscriber: &Subscriber, mut writer: Stream, queued: Receiver<Arc<Vec<u8>>>) {
    loop {
        match queued.recv_timeout(WRITER_POLL) {
            Ok(bytes) => {
                if let Err(e) = writer.write_all(&bytes) {
                    log_debug!("ZMQ subscriber {} write ended: {e}", subscriber.peer);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if subscriber.closing.load(Ordering::SeqCst) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    subscriber.close_now();
}

/// A running publisher. Dropping it stops it.
pub struct ZmqPublisher {
    shared: Arc<Shared>,
    endpoint: String,
    local_addr: Option<SocketAddr>,
    accept: Option<JoinHandle<()>>,
}

impl ZmqPublisher {
    /// Bind `endpoint` and start accepting subscribers
    /// (`ZmqPublisher::start`, `ZmqPublisher.cpp:55-112`). A failure is logged
    /// with the C++'s wording and returned; the daemon runs on without ZMQ.
    pub fn start(endpoint: &str) -> Result<ZmqPublisher, String> {
        let bound = Endpoint::parse(endpoint).and_then(|e| Listener::bind(&e).map_err(|e| e.to_string()));
        let listener = match bound {
            Ok(listener) => listener,
            Err(e) => {
                log_warn!("Failed to bind ZMQ endpoint {endpoint}: {e}");
                return Err(e);
            }
        };
        let local_addr = listener.local_addr();
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            subscribers: Mutex::new(Vec::new()),
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            reported_limit: AtomicBool::new(false),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("wrkz-zmq-accept".into())
                .spawn(move || shared.accept_loop(listener))
                .map_err(|e| e.to_string())?
        };
        if is_non_loopback_tcp(endpoint) {
            log_warn!(
                "ZMQ PUB endpoint is non-loopback: {endpoint}. Ensure network-level access controls are in place."
            );
        }
        log_info!("ZMQ publisher started on {endpoint}");
        Ok(ZmqPublisher { shared, endpoint: endpoint.to_string(), local_addr, accept: Some(accept) })
    }

    /// What to hand [`wrkz_rpc::events::Events`], so chain and pool changes
    /// reach the subscribers.
    pub fn listener(&self) -> Arc<dyn EventListener> {
        Arc::clone(&self.shared) as Arc<dyn EventListener>
    }

    /// The TCP address actually bound, which differs from the configured one
    /// when port 0 was asked for; `None` for an `ipc://` socket.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Subscribers connected, handshaken or not.
    pub fn subscribers(&self) -> usize {
        self.shared.subscribers().len()
    }

    /// Subscribers past the handshake with at least one topic: the ones a
    /// message published now would reach, if its topic matched.
    pub fn ready_subscribers(&self) -> usize {
        self.shared.subscribers().iter().filter(|s| s.ready.load(Ordering::SeqCst) && !s.topics().is_empty()).count()
    }

    /// Messages published, whether or not anyone wanted them.
    pub fn published(&self) -> u64 {
        self.shared.published.load(Ordering::Relaxed)
    }

    /// Messages a subscriber missed because its queue was full.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Close every subscriber and stop accepting (`ZmqPublisher::stop`).
    pub fn stop(&mut self) {
        if !self.shared.running.swap(false, Ordering::SeqCst) {
            return;
        }
        for subscriber in self.shared.subscribers().drain(..) {
            subscriber.close_now();
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        log_info!("ZMQ publisher stopped. Published={}, dropped={}", self.published(), self.dropped());
    }
}

impl Drop for ZmqPublisher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_rpc::events::PoolRemoval;

    #[test]
    fn the_greeting_is_zmtp_3_1_with_the_null_mechanism() {
        let g = greeting();
        assert_eq!(&g[..10], &[0xff, 0, 0, 0, 0, 0, 0, 0, 1, 0x7f]);
        assert_eq!(&g[10..12], &[3, 1]);
        assert_eq!(&g[12..16], b"NULL");
        assert!(g[16..].iter().all(|&b| b == 0), "the rest of the mechanism, as-server and the filler");
        assert_eq!(check_greeting(&g), Ok(()));
    }

    #[test]
    fn a_greeting_this_publisher_cannot_serve_is_refused() {
        let mut old = greeting();
        old[10] = 2;
        assert!(check_greeting(&old).unwrap_err().contains("too old"));
        let mut curve = greeting();
        curve[12..17].copy_from_slice(b"CURVE");
        assert!(check_greeting(&curve).unwrap_err().contains("CURVE"));
        let mut http = greeting();
        http[..4].copy_from_slice(b"GET ");
        assert!(check_greeting(&http).is_err());
    }

    #[test]
    fn frames_and_commands_are_laid_out_as_rfc_37_says() {
        let mut short = Vec::new();
        put_frame(&mut short, FLAG_MORE, b"hashblock");
        assert_eq!(short, [&[0x01, 9][..], b"hashblock"].concat());

        let body = vec![7u8; 300];
        let mut long = Vec::new();
        put_frame(&mut long, 0, &body);
        assert_eq!(&long[..9], &[0x02, 0, 0, 0, 0, 0, 0, 1, 44]);
        assert_eq!(long.len(), 9 + 300);

        let r = ready();
        assert_eq!(&r[..2], &[0x04, 25]);
        assert_eq!(&r[2..8], b"\x05READY");
        assert_eq!(&r[8..20], b"\x0bSocket-Type");
        assert_eq!(&r[20..27], b"\x00\x00\x00\x03PUB");

        let (name, data) = split_command(&r[2..]).unwrap();
        assert_eq!(name, b"READY");
        assert_eq!(metadata(data, "socket-type"), Some(&b"PUB"[..]), "names match case-insensitively");
        assert_eq!(metadata(data, "Identity"), None);
        assert_eq!(metadata(&data[..data.len() - 1], "Socket-Type"), None, "a truncated value is no value");
    }

    #[test]
    fn events_are_published_with_the_cpps_topics_and_bodies() {
        let hash = [0xab; 32];
        let hex = "ab".repeat(32);
        let block = ChainEvent::BlockAdded { index: 42, hash, transaction_hashes: vec![[1; 32], [2; 32]] };
        assert_eq!(
            messages(&block),
            vec![
                ("hashblock", format!("{{\"height\":42,\"hash\":\"{hex}\"}}")),
                (
                    "chain_main",
                    format!(
                        "{{\"height\":42,\"hash\":\"{hex}\",\"transaction_hashes\":[\"{}\",\"{}\"]}}",
                        "01".repeat(32),
                        "02".repeat(32)
                    )
                ),
            ]
        );
        assert_eq!(
            messages(&ChainEvent::AlternativeBlockAdded { index: 7, hash }),
            vec![("hashblock_alt", format!("{{\"height\":7,\"hash\":\"{hex}\"}}"))]
        );
        assert_eq!(
            messages(&ChainEvent::ChainSwitched { common_root_index: 9, hashes: vec![hash] }),
            vec![("chainswitch", format!("{{\"common_root_height\":9,\"hashes\":[\"{hex}\"]}}"))]
        );
        assert_eq!(
            messages(&ChainEvent::PoolAdded { hash }),
            vec![("txpool_add", format!("{{\"hashes\":[\"{hex}\"]}}"))]
        );
        assert_eq!(
            messages(&ChainEvent::PoolRemoved { hashes: vec![hash], reason: PoolRemoval::Outdated }),
            vec![("txpool_del", format!("{{\"hashes\":[\"{hex}\"],\"reason\":\"Outdated\"}}"))]
        );
    }

    #[test]
    fn endpoints_read_as_libzmq_reads_them() {
        let tcp = |host: &str, port| Ok(Endpoint::Tcp { host: host.to_string(), port });
        assert_eq!(Endpoint::parse("tcp://127.0.0.1:17857"), tcp("127.0.0.1", 17857));
        assert_eq!(Endpoint::parse("tcp://*:17857"), tcp("*", 17857));
        assert_eq!(Endpoint::parse("tcp://[::1]:17857"), tcp("::1", 17857));
        assert_eq!(Endpoint::parse("tcp://localhost:*"), tcp("localhost", 0));
        assert_eq!(Endpoint::parse("ipc:///run/wrkzd.zmq"), Ok(Endpoint::Ipc("/run/wrkzd.zmq".to_string())));
        for bad in ["tcp://127.0.0.1", "tcp://:17857", "tcp://[::1:17857", "tcp://h:99999", "ipc://", "17857"] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
        assert!(Endpoint::parse("inproc://x").unwrap_err().contains("inproc://"));
    }

    #[test]
    fn the_loopback_warning_follows_the_cpp_test() {
        for loopback in ["tcp://127.0.0.1:17857", "tcp://LOCALHOST:1", "tcp://[::1]:1", "ipc:///x", "tcp://"] {
            assert!(!is_non_loopback_tcp(loopback), "{loopback}");
        }
        for open in ["tcp://*:17857", "tcp://0.0.0.0:1", "tcp://[::]:1", "tcp://192.168.1.2:1"] {
            assert!(is_non_loopback_tcp(open), "{open}");
        }
    }
}
