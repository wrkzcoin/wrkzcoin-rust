// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Following the daemon's event stream (`GET /ws`), so that a synced wallet
//! hears of a block the moment its daemon has it.
//!
//! Without it a synced wallet asks `/getwalletsyncdata` every two seconds
//! ([`SYNCED_POLL`]) to find out whether anything happened, which is most of
//! the load an idle wallet puts on a public node, and still up to two seconds
//! late. With it the wallet waits for the daemon to say `hashblock`,
//! `chainswitch` or a pool change, then syncs straight away, and polls only
//! every [`LIVE_SYNCED_POLL`] in case a message was lost.
//!
//! **The stream only wakes; it never decides.** What the wallet has is still
//! whatever the HTTP sync gave it, so nothing here can make a balance wrong —
//! at worst a wallet learns of a block at the next poll, as it always did.
//!
//! - A daemon that does not serve `/ws` — a C++ node, or ours without
//!   `--enable-websocket` — answers the upgrade with something other than
//!   `101`. The watch then leaves that daemon alone for [`UNSUPPORTED_RETRY`]
//!   and the wallet polls as before; nothing is logged louder than debug.
//! - A dropped connection is retried with a backoff from one second to a
//!   minute.
//! - The link counts as live only while something arrives: the daemon sends a
//!   heartbeat every thirty seconds, so [`LIVE_WINDOW`] without a frame means
//!   the stream is gone, whatever the socket says, and the wallet goes back to
//!   polling every two seconds until it reconnects.
//! - An IPC daemon (`/path`, `@name`) is not followed: there is nothing to
//!   gain on a local socket.
//! - `https://` daemons are followed over `wss://` when the build has the
//!   `https` feature, with the same root certificates as the HTTP client.
//!
//! Reads are cut into one-second slices, and bytes are collected into a
//! buffer that is only parsed once a whole frame is in it
//! ([`wrkz_ws::parse_frame`]), so a timeout never leaves the reader inside a
//! frame, and a change of daemon or a stop is noticed within a second.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wrkz_ws::frame::{close, close_payload, encode_frame, parse_frame, Assembler, Message, Opcode, Role};
use wrkz_ws::handshake;

use crate::sync::SyncStep;

/// How often a synced wallet asks its daemon for new blocks when nothing
/// tells it: `OpenWallet::sync_round`'s wait after [`SyncStep::Synced`].
pub const SYNCED_POLL: Duration = Duration::from_secs(2);
/// How often it asks while the stream is live, in case a message was lost.
pub const LIVE_SYNCED_POLL: Duration = Duration::from_secs(30);
/// No frame for this long and the stream is not live: two and a half
/// heartbeats.
pub const LIVE_WINDOW: Duration = Duration::from_secs(75);
/// How long a daemon that does not serve `/ws` is left alone.
pub const UNSUPPORTED_RETRY: Duration = Duration::from_secs(600);
/// The first and the longest wait before reconnecting.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// One read's wait: how quickly a stop or a change of daemon is noticed.
const READ_SLICE: Duration = Duration::from_secs(1);
/// The longest a drop waits for the thread to finish.
pub const STOP_WAIT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The daemon's messages are a few hundred bytes; a `chainswitch` lists the
/// hashes of a reorganisation, at most 180 blocks deep.
const MAX_MESSAGE: usize = 64 * 1024;
/// The largest answer head read during the handshake.
const MAX_HEAD: usize = 8 * 1024;
/// The topics that mean "sync now". `hello` does too: whatever happened while
/// the stream was down is only caught up by syncing.
const WAKE_TOPICS: &[&str] = &["hello", "hashblock", "chain_main", "chainswitch", "txpool_add", "txpool_del"];

/// Where the stream is: a daemon's base URL, turned into a host, a port, TLS
/// or not, and the path under which the daemon is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    host: String,
    port: u16,
    tls: bool,
    /// Whatever path the base URL had (a reverse proxy's prefix), `/ws` after it.
    path: String,
}

impl Target {
    /// `http://host:port[/prefix]` or `https://…`; `None` for anything else,
    /// an IPC address included.
    pub fn from_base_url(url: &str) -> Option<Target> {
        let url = url.trim().trim_end_matches('/');
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else {
            (false, url.strip_prefix("http://")?)
        };
        let (authority, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, after) = bracketed.split_once(']')?;
            match after.strip_prefix(':') {
                Some(p) => (host, p.parse().ok()?),
                None if after.is_empty() => (host, default_port),
                None => return None,
            }
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().ok()?),
                None => (authority, default_port),
            }
        };
        if host.is_empty() {
            return None;
        }
        Some(Target { host: host.to_string(), port, tls, path: format!("{prefix}/ws") })
    }

    /// The `Host` header: the port only when it is not the scheme's default.
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

struct State {
    /// What to follow, and a counter bumped whenever that changes, so the
    /// thread drops a connection to the old daemon.
    target: Option<Target>,
    generation: u64,
    /// Events heard since start; a waiter compares against its own count.
    events: u64,
    /// When the last frame arrived.
    last_frame: Option<Instant>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Called on every wake-worthy message, from the watch's thread. For a
    /// front end that waits on something else — Pluton waits on its command
    /// channel.
    notify: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The watch: a thread following one daemon's stream at a time. Dropping it
/// stops the thread, waiting at most [`STOP_WAIT`] for it (see the `Drop`).
pub struct TipWatch {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for TipWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TipWatch").field("live", &self.is_live()).finish()
    }
}

impl Default for TipWatch {
    fn default() -> Self {
        Self::start()
    }
}

impl TipWatch {
    /// Start the thread, following nothing until [`TipWatch::follow`].
    pub fn start() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { target: None, generation: 0, events: 0, last_frame: None, stop: false }),
            changed: Condvar::new(),
            notify: Mutex::new(None),
        });
        let running = Arc::clone(&shared);
        let thread = std::thread::Builder::new().name("wrkz-tip-watch".into()).spawn(move || run(&running)).ok();
        TipWatch { shared, thread }
    }

    /// Call `notify` on every wake-worthy message, from the watch's thread.
    pub fn set_notify(&self, notify: Box<dyn Fn() + Send>) {
        *self.shared.notify.lock().unwrap_or_else(|p| p.into_inner()) = Some(notify);
    }

    /// Follow the daemon at `base_url` (`http://host:port`), or nothing. The
    /// same daemon again changes nothing; another one drops the connection to
    /// the old one.
    pub fn follow(&self, base_url: Option<&str>) {
        let target = base_url.and_then(Target::from_base_url);
        let mut state = self.shared.state();
        if state.target != target {
            state.target = target;
            state.generation += 1;
            state.last_frame = None;
            self.shared.changed.notify_all();
        }
    }

    /// Follow `host:port`, over TLS or not: the form `wrkz-wallet-api` and
    /// the command-line wallet keep their daemon in. An IPC address is not
    /// followed.
    pub fn follow_host(&self, host: &str, port: u16, ssl: bool) {
        // The IPC forms of `crate::ipc`: a local socket has no stream.
        if host.starts_with("ipc://") || host.starts_with('@') || host.starts_with('/') {
            self.follow(None);
            return;
        }
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
        let scheme = if ssl { "https" } else { "http" };
        self.follow(Some(&format!("{scheme}://{host}:{port}")));
    }

    /// Whether the stream is up and talking: a frame within [`LIVE_WINDOW`].
    pub fn is_live(&self) -> bool {
        self.shared.state().last_frame.is_some_and(|t| t.elapsed() < LIVE_WINDOW)
    }

    /// Events heard so far. Pass it to [`TipWatch::wait`] to wake only for
    /// ones after this point.
    pub fn events(&self) -> u64 {
        self.shared.state().events
    }

    /// Wait up to `timeout`, returning early — `true` — when an event newer
    /// than `seen` arrives, and early — `false` — when `stopping` is set. A
    /// caller that wants no stop flag passes one that is never set.
    pub fn wait(&self, timeout: Duration, seen: u64, stopping: &AtomicBool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state();
        loop {
            if state.events > seen {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || stopping.load(Ordering::SeqCst) || state.stop {
                return false;
            }
            // Sliced, so a stop flag nobody signals is still seen promptly.
            let slice = left.min(Duration::from_millis(100));
            state = self.shared.changed.wait_timeout(state, slice).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// The wait a sync loop should use after `step`: a synced wallet whose
    /// stream is live waits [`LIVE_SYNCED_POLL`] instead of `wait` — the
    /// stream wakes it for anything that happens meanwhile.
    pub fn pace(&self, step: &SyncStep, wait: Duration) -> Duration {
        match step {
            SyncStep::Synced { .. } if self.is_live() => wait.max(LIVE_SYNCED_POLL),
            _ => wait,
        }
    }
}

impl Drop for TipWatch {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state();
            state.stop = true;
            self.shared.changed.notify_all();
        }
        // The thread notices a stop within a read slice — except while the
        // system resolves a host name or a connect is under way, which nothing
        // can interrupt, and which a slow or unreachable DNS server can stretch
        // to many seconds. Closing a wallet must not wait on that: after
        // `STOP_WAIT` the thread is left to finish on its own. It holds only its
        // own shared state, and it ends as soon as the lookup does.
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + STOP_WAIT;
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// Why a connection ended.
enum Ended {
    /// The daemon answered the upgrade with this status: it has no stream.
    Unsupported(u16),
    /// Anything else: a failure to connect, a drop, a close.
    Lost(String),
    /// The target changed, or the watch is stopping.
    Superseded,
}

fn run(shared: &Shared) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let (target, generation) = {
            let mut state = shared.state();
            loop {
                if state.stop {
                    return;
                }
                if let Some(target) = state.target.clone() {
                    break (target, state.generation);
                }
                state = shared.changed.wait(state).unwrap_or_else(|p| p.into_inner());
            }
        };
        let started = Instant::now();
        let retry_in = match follow(shared, &target, generation) {
            Ended::Superseded => {
                backoff = BACKOFF_MIN;
                continue;
            }
            Ended::Unsupported(status) => {
                debug(format_args!("{} has no event stream (HTTP {status}); polling it instead", target.host_header()));
                UNSUPPORTED_RETRY
            }
            Ended::Lost(why) => {
                debug(format_args!("event stream from {} ended: {why}", target.host_header()));
                // A connection that lasted a while starts the backoff over.
                if started.elapsed() > BACKOFF_MAX {
                    backoff = BACKOFF_MIN;
                }
                let wait = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                wait
            }
        };
        shared.state().last_frame = None;
        // Wait out the retry, unless the target changes or the watch stops.
        let deadline = Instant::now() + retry_in;
        let mut state = shared.state();
        while !state.stop && state.generation == generation {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = shared.changed.wait_timeout(state, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }
}

/// A debug line in the programs' log; Pluton has no such log.
fn debug(args: std::fmt::Arguments<'_>) {
    #[cfg(feature = "frontends")]
    crate::logging::log(wrkz_rpc::log::Level::Debug, args);
    #[cfg(not(feature = "frontends"))]
    let _ = args;
}

/// Whether the watch has moved on from `generation`.
fn superseded(shared: &Shared, generation: u64) -> bool {
    let state = shared.state();
    state.stop || state.generation != generation
}

/// One connection to `target`, until it ends.
fn follow(shared: &Shared, target: &Target, generation: u64) -> Ended {
    let mut stream = match connect(target) {
        Ok(stream) => stream,
        Err(e) => return Ended::Lost(e),
    };
    let mut nonce = [0u8; 16];
    if getrandom::fill(&mut nonce).is_err() {
        return Ended::Lost("no randomness for the handshake key".into());
    }
    let key = handshake::client_key(nonce);
    let request = match handshake::client_request(&target.host_header(), &target.path, &key, &[]) {
        Ok(request) => request,
        Err(e) => return Ended::Lost(e.to_string()),
    };
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return Ended::Lost(e.to_string());
    }

    // The head, byte by byte into `buf` until the blank line: what follows it
    // is frames, and stays in `buf` for the frame parser.
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        if superseded(shared, generation) {
            return Ended::Superseded;
        }
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        if buf.len() > MAX_HEAD {
            return Ended::Lost("an answer head over the size limit".into());
        }
        match read_some(&mut stream, &mut buf) {
            Ok(true) => {}
            Ok(false) => return Ended::Lost("the daemon closed during the handshake".into()),
            Err(e) => return Ended::Lost(e.to_string()),
        }
    };
    let head = match handshake::read_response_head(&mut &buf[..head_end], MAX_HEAD) {
        Ok(head) => head,
        Err(e) => return Ended::Lost(e.to_string()),
    };
    if head.status != 101 {
        return Ended::Unsupported(head.status);
    }
    if let Err(e) = handshake::check_response(&head, &key) {
        return Ended::Lost(e.to_string());
    }
    buf.drain(..head_end);

    let mut assembler = Assembler::new(MAX_MESSAGE);
    loop {
        // Every whole frame collected so far.
        loop {
            let parsed = match parse_frame(&buf, Role::Client, assembler.frame_cap()) {
                Ok(parsed) => parsed,
                Err(e) => {
                    let code = e.close_code().unwrap_or(close::PROTOCOL_ERROR);
                    let _ = send(&mut stream, Opcode::Close, &close_payload(code, ""));
                    return Ended::Lost(e.to_string());
                }
            };
            let Some((frame, used)) = parsed else { break };
            buf.drain(..used);
            shared.state().last_frame = Some(Instant::now());
            match assembler.push(frame) {
                Ok(None) => {}
                Ok(Some(Message::Text(text))) => {
                    if is_wake(&text) {
                        wake(shared);
                    }
                }
                Ok(Some(Message::Binary(_))) | Ok(Some(Message::Pong(_))) => {}
                Ok(Some(Message::Ping(payload))) => {
                    if let Err(e) = send(&mut stream, Opcode::Pong, &payload) {
                        return Ended::Lost(e.to_string());
                    }
                }
                Ok(Some(Message::Close(code))) => {
                    let _ = send(&mut stream, Opcode::Close, &close_payload(close::NORMAL, ""));
                    return Ended::Lost(format!("closed by the daemon ({code:?})"));
                }
                Err(e) => {
                    let code = e.close_code().unwrap_or(close::PROTOCOL_ERROR);
                    let _ = send(&mut stream, Opcode::Close, &close_payload(code, ""));
                    return Ended::Lost(e.to_string());
                }
            }
        }
        if superseded(shared, generation) {
            let _ = send(&mut stream, Opcode::Close, &close_payload(close::NORMAL, ""));
            return Ended::Superseded;
        }
        if shared.state().last_frame.is_some_and(|t| t.elapsed() >= LIVE_WINDOW) {
            return Ended::Lost("nothing from the daemon for a whole live window".into());
        }
        match read_some(&mut stream, &mut buf) {
            Ok(true) => {}
            Ok(false) => return Ended::Lost("the daemon closed the connection".into()),
            Err(e) => return Ended::Lost(e.to_string()),
        }
    }
}

/// Whether a `{"topic":"…",…}` message is one that means "sync now". Only the
/// topic is read, and only as far as recognising it.
fn is_wake(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("{\"topic\":\"") else { return false };
    let Some((topic, _)) = rest.split_once('"') else { return false };
    WAKE_TOPICS.contains(&topic)
}

fn wake(shared: &Shared) {
    {
        let mut state = shared.state();
        state.events += 1;
        shared.changed.notify_all();
    }
    if let Some(notify) = shared.notify.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        notify();
    }
}

/// Append what the socket has to `buf`. `Ok(false)` is the end of the stream;
/// a slice that passes with nothing is `Ok(true)` with nothing added.
fn read_some(stream: &mut Stream, buf: &mut Vec<u8>) -> io::Result<bool> {
    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk) {
        Ok(0) => Ok(false),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(true),
        Err(e) => Err(e),
    }
}

/// A frame from this client: masked with a fresh key (RFC 6455 §5.3).
fn send(stream: &mut Stream, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
    let mut mask = [0u8; 4];
    getrandom::fill(&mut mask).map_err(|e| io::Error::other(e.to_string()))?;
    stream.write_all(&encode_frame(opcode, payload, Some(mask)))?;
    stream.flush()
}

/// The socket, plain or TLS.
enum Stream {
    Plain(TcpStream),
    #[cfg(feature = "https")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            #[cfg(feature = "https")]
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            #[cfg(feature = "https")]
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            #[cfg(feature = "https")]
            Stream::Tls(s) => s.flush(),
        }
    }
}

fn connect(target: &Target) -> Result<Stream, String> {
    let addrs: Vec<_> =
        (target.host.as_str(), target.port).to_socket_addrs().map_err(|e| format!("{}: {e}", target.host))?.collect();
    let mut last = format!("{} resolved to nothing", target.host);
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(tcp) => {
                let _ = tcp.set_nodelay(true);
                tcp.set_read_timeout(Some(READ_SLICE)).map_err(|e| e.to_string())?;
                tcp.set_write_timeout(Some(WRITE_TIMEOUT)).map_err(|e| e.to_string())?;
                return wrap(target, tcp);
            }
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    Err(last)
}

#[cfg(feature = "https")]
fn wrap(target: &Target, tcp: TcpStream) -> Result<Stream, String> {
    if !target.tls {
        return Ok(Stream::Plain(tcp));
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(target.host.clone()).map_err(|e| e.to_string())?;
    let connection = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(connection, tcp))))
}

#[cfg(not(feature = "https"))]
fn wrap(target: &Target, tcp: TcpStream) -> Result<Stream, String> {
    if target.tls {
        return Err("this build has no TLS for a wss:// stream".into());
    }
    Ok(Stream::Plain(tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_becomes_a_target() {
        let t = Target::from_base_url("http://node-fin.wrkz.work:17856").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.tls, t.path.as_str()), ("node-fin.wrkz.work", 17856, false, "/ws"));
        assert_eq!(t.host_header(), "node-fin.wrkz.work:17856");

        let t = Target::from_base_url("https://example.com/daemon/").unwrap();
        assert_eq!((t.port, t.tls, t.path.as_str()), (443, true, "/daemon/ws"));
        assert_eq!(t.host_header(), "example.com");

        let t = Target::from_base_url("http://[::1]:17856").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 17856));
        assert_eq!(t.host_header(), "[::1]:17856");

        for bad in ["", "ftp://x", "http://", "http://:1", "http://user@host:1", "http://host:port", "/tmp/sock"] {
            assert_eq!(Target::from_base_url(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_the_topics_that_mean_sync_now_wake() {
        assert!(is_wake("{\"topic\":\"hashblock\",\"data\":{}}"));
        assert!(is_wake("{\"topic\":\"hello\",\"data\":{}}"));
        assert!(is_wake("{\"topic\":\"txpool_del\",\"data\":{}}"));
        assert!(!is_wake("{\"topic\":\"heartbeat\",\"data\":{}}"));
        assert!(!is_wake("{\"topic\":\"hashblock_alt\",\"data\":{}}"));
        assert!(!is_wake("not json"));
    }

    #[test]
    fn nothing_followed_is_never_live_and_waits_out_its_timeout() {
        let watch = TipWatch::start();
        assert!(!watch.is_live());
        let never = AtomicBool::new(false);
        let started = Instant::now();
        assert!(!watch.wait(Duration::from_millis(150), watch.events(), &never));
        assert!(started.elapsed() >= Duration::from_millis(150));
        let stop = AtomicBool::new(true);
        assert!(!watch.wait(Duration::from_secs(60), 0, &stop), "a set stop flag returns at once");
        let synced = SyncStep::Synced { height: 1 };
        assert_eq!(watch.pace(&synced, SYNCED_POLL), SYNCED_POLL, "not live: the usual poll");
    }
}
