// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The HTTP listener `wrkz-wallet-api` and `wrkz-service` both run on: TCP,
//! an optional second listener on IPv6, and an optional local IPC socket, all
//! feeding one bounded queue and one fixed pool of workers.
//!
//! It is the shape of the daemon's [`wrkz_rpc::server`] — the same `Conn`
//! either-of-two-sockets, the same acceptor per listener onto one queue — with
//! the handler passed in rather than bound to the daemon's route table, which
//! is why it lives here and not in `wrkz-rpc`. HTTP/1.1 itself is
//! [`wrkz_rpc::http`]; nothing here parses a request.
//!
//! What every connection gets, whichever listener it came in on:
//!
//! - read and write timeouts before the first byte, a deadline on the whole
//!   request head and another on the body ([`http::read_request_timed`]), and
//!   caps on every part of a request;
//! - a bounded backlog that sheds with a bare `503` and `Connection: close`
//!   rather than growing — httplib's own answer;
//! - bounded keep-alive;
//! - gzip, when [`ListenConfig::gzip`] is on and the client sends
//!   `Accept-Encoding: gzip` for a JSON body of at least
//!   [`http::MIN_GZIP_BYTES`] — what `cpp-httplib` does when the C++ is built
//!   with zlib;
//! - one log line per request ([`crate::logging`]): the peer, the method, the
//!   path, the status and how long it took. Never the query string, a header
//!   or a body, which is where an API key, a password or a private key would
//!   be.
//!
//! # The three listeners
//!
//! - **TCP**, on [`ListenConfig::bind`]. Failing to bind it fails
//!   [`Listener::start`], as it ends both C++ programs.
//! - **IPv6**, on [`ListenConfig::bind_ipv6`], bound `IPV6_V6ONLY` through
//!   [`wrkz_p2p::bind::bind_ipv6_only`] so it can never take the IPv4 wildcard
//!   on the same port — what `set_ipv6_v6only(true)` does for the C++'s second
//!   `httplib::Server` (`ApiDispatcher.cpp:105`). A failure here is reported
//!   ([`Listener::ipv6_error`]) and the other listeners carry on, which is the
//!   C++'s policy too (`ApiDispatcher.cpp:453`).
//! - **IPC**, on [`ListenConfig::ipc`]: an `AF_UNIX` socket from
//!   `wrkz_rpc::ipc::bind` (unix only), owner-only from the first instant and widened to
//!   the mode only after the group is applied. It is bound before any thread
//!   of this listener exists, because the bind narrows the process umask for a
//!   moment (`ApiDispatcher.cpp:422`). A failure is reported
//!   ([`Listener::ipc_error`]) rather than fatal, unless it was the only
//!   listener asked for. Windows has no IPC socket, for the C++'s reason: no
//!   dependable permissions on the socket file.
//!
//! Who may call over IPC is not decided here: the handler is told the
//! [`Peer`] and applies its own program's rule.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, SocketAddrV6, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wrkz_rpc::http::{self, DeadlineStream, HttpError, HttpLimits, ReadTimeout, Request, Response};
use wrkz_rpc::log::Level;

////////////////////////
/* CONFIGURATION      */
////////////////////////

/// Where the local socket goes and who may open it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcConfig {
    /// An absolute path, or `@name` for Linux's abstract namespace.
    pub path: String,
    /// The permission bits of the socket file ([`wrkz_rpc::ipc::DEFAULT_MODE`]).
    pub mode: u32,
    /// The group to own it; empty keeps the process's.
    pub group: String,
}

/// How a [`Listener`] behaves. The defaults are the daemon's.
#[derive(Clone, Debug)]
pub struct ListenConfig {
    /// `ip:port` for the TCP listener; `None` for none at all, which only a
    /// program serving on IPC alone asks for.
    pub bind: Option<String>,
    /// `[address]:port` for the IPv6 listener, or `None`.
    pub bind_ipv6: Option<String>,
    /// The IPC socket, or `None`.
    pub ipc: Option<IpcConfig>,
    pub workers: usize,
    pub queue_capacity: usize,
    pub limits: HttpLimits,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub keep_alive: bool,
    pub keep_alive_timeout: Duration,
    pub keep_alive_max: u32,
    /// Compress a JSON answer for a client that accepts gzip.
    pub gzip: bool,
}

impl Default for ListenConfig {
    fn default() -> Self {
        ListenConfig {
            bind: None,
            bind_ipv6: None,
            ipc: None,
            workers: 8,
            queue_capacity: 64,
            limits: HttpLimits::default(),
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            keep_alive: true,
            keep_alive_timeout: Duration::from_secs(3),
            keep_alive_max: 1000,
            gzip: false,
        }
    }
}

/// Who sent a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Peer {
    /// A TCP client, on either family.
    Tcp(SocketAddr),
    /// A process on this machine that could open the IPC socket file.
    Ipc,
}

impl Peer {
    /// Whether the request came over the local socket.
    pub fn is_ipc(&self) -> bool {
        matches!(self, Peer::Ipc)
    }
}

impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Peer::Tcp(addr) => write!(f, "{addr}"),
            Peer::Ipc => f.write_str(wrkz_rpc::ipc::IPC_PEER),
        }
    }
}

/// One request in, one complete response out.
pub type Handler = Arc<dyn Fn(&Request, &Peer) -> Response + Send + Sync>;

/// `[address]:port` for an IPv6 listener: the address with any brackets the
/// operator already wrote taken off, so `::1` and `[::1]` both work.
pub fn ipv6_bind(address: &str, port: u16) -> String {
    let bare = address.trim().trim_start_matches('[').trim_end_matches(']');
    format!("[{bare}]:{port}")
}

////////////////////////
/* THE LISTENER       */
////////////////////////

/// A running listener. Dropping it stops every acceptor and worker and joins
/// them, and removes the IPC socket file.
pub struct Listener {
    addr: Option<SocketAddr>,
    addr6: Option<SocketAddr>,
    ipc: Option<String>,
    ipv6_error: Option<String>,
    ipc_error: Option<String>,
    stopping: Arc<AtomicBool>,
    queue: Arc<Queue>,
    threads: Vec<JoinHandle<()>>,
}

impl Listener {
    /// Bind everything `config` asks for and start serving `handler`. Returns
    /// once the listeners are up.
    ///
    /// Fails when the TCP listener cannot bind, or when nothing at all came up.
    pub fn start(config: ListenConfig, handler: Handler) -> std::io::Result<Listener> {
        // The IPC socket first, on this thread, before anything below spawns:
        // its bind narrows the process umask for an instant.
        #[cfg(unix)]
        let (ipc_listener, ipc_error) = match &config.ipc {
            None => (None, None),
            Some(ipc) => match wrkz_rpc::ipc::bind(&ipc.path, ipc.mode, &ipc.group) {
                Ok(listener) => (Some((listener, ipc.path.clone())), None),
                Err(e) => (None, Some(format!("{}: {e}", wrkz_rpc::ipc::describe(&ipc.path)))),
            },
        };
        #[cfg(not(unix))]
        let ipc_error = config.ipc.as_ref().map(|_| wrkz_rpc::ipc::UNSUPPORTED.to_string());

        let listener = match &config.bind {
            Some(bind) => Some(TcpListener::bind(bind)?),
            None => None,
        };
        let addr = listener.as_ref().map(TcpListener::local_addr).transpose()?;

        let (listener6, ipv6_error) = match &config.bind_ipv6 {
            None => (None, None),
            Some(text) => match parse_ipv6_bind(text).and_then(wrkz_p2p::bind::bind_ipv6_only) {
                Ok(l) => (Some(l), None),
                Err(e) => (None, Some(format!("{text}: {e}"))),
            },
        };
        let addr6 = listener6.as_ref().map(TcpListener::local_addr).transpose()?;

        #[cfg(unix)]
        let nothing_else = ipc_listener.is_none();
        #[cfg(not(unix))]
        let nothing_else = true;
        if listener.is_none() && listener6.is_none() && nothing_else {
            let why = ipc_error.clone().or_else(|| ipv6_error.clone()).unwrap_or_else(|| "no listener".to_string());
            return Err(std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, why));
        }

        let workers = config.workers.max(1);
        let queue = Arc::new(Queue::new(config.queue_capacity.max(1)));
        let stopping = Arc::new(AtomicBool::new(false));
        let shed = Arc::new(Shed::new());
        let config = Arc::new(config);
        let mut threads = Vec::with_capacity(workers + 3);

        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let config = Arc::clone(&config);
            let handler = Arc::clone(&handler);
            threads.push(std::thread::spawn(move || {
                while let Some(accepted) = queue.pop() {
                    serve_connection(&config, &handler, accepted);
                }
            }));
        }

        for l in [listener, listener6].into_iter().flatten() {
            let (queue, stopping, shed) = (Arc::clone(&queue), Arc::clone(&stopping), Arc::clone(&shed));
            threads.push(std::thread::spawn(move || accept_tcp(l, &queue, &stopping, &shed)));
        }

        #[cfg(unix)]
        let ipc = match ipc_listener {
            Some((l, path)) => {
                let (queue, stopping, shed) = (Arc::clone(&queue), Arc::clone(&stopping), Arc::clone(&shed));
                threads.push(std::thread::spawn(move || accept_ipc(l, &queue, &stopping, &shed)));
                Some(path)
            }
            None => None,
        };
        #[cfg(not(unix))]
        let ipc = None;

        Ok(Listener { addr, addr6, ipc, ipv6_error, ipc_error, stopping, queue, threads })
    }

    /// The TCP address actually bound, which is how a test finds the port
    /// after asking for `127.0.0.1:0`.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// The IPv6 address actually bound.
    pub fn local_addr6(&self) -> Option<SocketAddr> {
        self.addr6
    }

    /// The IPC socket, only once its bind succeeded, so nothing announces a
    /// socket that was never created (`ApiDispatcher::getIpcPath`).
    pub fn ipc_path(&self) -> Option<&str> {
        self.ipc.as_deref()
    }

    /// Why the IPv6 listener asked for did not come up.
    pub fn ipv6_error(&self) -> Option<&str> {
        self.ipv6_error.as_deref()
    }

    /// Why the IPC socket asked for did not come up.
    pub fn ipc_error(&self) -> Option<&str> {
        self.ipc_error.as_deref()
    }

    /// Stop accepting, drain, join, and remove the socket file. `Drop` calls
    /// this too.
    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Each acceptor is parked in `accept()`; a connection wakes it. An
        // unspecified bind address is reached through loopback, because
        // connecting to `0.0.0.0` or `::` fails on Windows.
        for addr in [self.addr, self.addr6].into_iter().flatten() {
            let target = wake_address(addr);
            if let Ok(s) = TcpStream::connect_timeout(&target, Duration::from_secs(1)) {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        #[cfg(unix)]
        if let Some(path) = &self.ipc {
            let _ = wrkz_rpc::ipc::connect(path);
        }
        self.queue.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        #[cfg(unix)]
        if let Some(path) = self.ipc.take() {
            wrkz_rpc::ipc::cleanup(&path);
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where to connect to wake an acceptor bound to `addr`.
fn wake_address(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if v4.ip().is_unspecified() => SocketAddr::from(([127, 0, 0, 1], v4.port())),
        SocketAddr::V6(v6) if v6.ip().is_unspecified() => {
            SocketAddr::V6(SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, v6.port(), 0, 0))
        }
        other => other,
    }
}

/// `[addr]:port`, a bare IPv6 literal with a port, or a name that resolves to
/// an IPv6 address. Anything IPv4 is refused, so a mistyped
/// `--rpc-bind-ipv6-address 127.0.0.1` is an error rather than a second IPv4
/// listener.
fn parse_ipv6_bind(text: &str) -> std::io::Result<SocketAddrV6> {
    use std::net::ToSocketAddrs;
    let bad = |what: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, what);
    let addr = text.to_socket_addrs()?.next().ok_or_else(|| bad(format!("{text} resolved to nothing")))?;
    match addr {
        SocketAddr::V6(v6) => Ok(v6),
        SocketAddr::V4(_) => Err(bad(format!("{text} is not an IPv6 address"))),
    }
}

////////////////////////
/* ACCEPTING          */
////////////////////////

/// Rendered once: what a connection the queue has no room for is told.
struct Shed {
    busy: Vec<u8>,
    count: AtomicU64,
}

impl Shed {
    fn new() -> Shed {
        // httplib's own answer to a request it will not serve: the status and
        // nothing else.
        Shed { busy: http::render_closing(&Response::new(503)), count: AtomicU64::new(0) }
    }

    /// Log the first shed connection and every hundredth after it, so a flood
    /// is visible without becoming a flood of log lines.
    fn note(&self, peer: &Peer) {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(100) {
            crate::logging::log(
                Level::Warn,
                format_args!("request queue full: turned {peer} away with 503 ({n} so far)"),
            );
        }
    }
}

fn accept_tcp(listener: TcpListener, queue: &Queue, stopping: &AtomicBool, shed: &Shed) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        let stream = match stream {
            Ok(s) => s,
            Err(_) => {
                // Out of descriptors, most likely: do not spin on it.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let peer = match stream.peer_addr() {
            Ok(addr) => Peer::Tcp(addr),
            Err(_) => continue,
        };
        if let Err(rejected) = queue.push(Accepted { conn: Conn::Tcp(stream), peer }) {
            shed.note(&rejected.peer);
            rejected.conn.shed(&shed.busy);
        }
    }
    queue.close();
}

#[cfg(unix)]
fn accept_ipc(listener: UnixListener, queue: &Queue, stopping: &AtomicBool, shed: &Shed) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if let Err(rejected) = queue.push(Accepted { conn: Conn::Unix(stream), peer: Peer::Ipc }) {
            shed.note(&rejected.peer);
            rejected.conn.shed(&shed.busy);
        }
    }
    queue.close();
}

////////////////////////
/* ONE CONNECTION     */
////////////////////////

/// One connection: read requests until the peer stops, the limits say no, or
/// keep-alive runs out.
fn serve_connection(config: &ListenConfig, handler: &Handler, accepted: Accepted) {
    let Accepted { conn, peer } = accepted;
    conn.set_nodelay();
    let _ = conn.set_write_timeout(Some(config.write_timeout));
    let Ok(mut writer) = conn.try_clone() else { return };
    let mut reader = BufReader::new(DeadlineStream::new(conn, config.read_timeout));
    let hint = format!("timeout={}, max={}", config.keep_alive_timeout.as_secs(), config.keep_alive_max);

    let mut served: u32 = 0;
    loop {
        // The daemon's rule (`wrkz_rpc::server`): the read timeout for the
        // whole head of the first request; on a kept-alive connection an idle
        // wait of at most the keep-alive timeout per read, with the same
        // allowance on top.
        let head = if served == 0 {
            reader.get_mut().set_per_read(config.read_timeout);
            config.read_timeout
        } else {
            reader.get_mut().set_per_read(config.keep_alive_timeout);
            config.keep_alive_timeout + config.read_timeout
        };
        let request = match http::read_request_timed(&mut reader, &config.limits, head, config.read_timeout) {
            Ok(r) => r,
            Err(HttpError::Closed) => return,
            Err(e) => {
                // The codes are httplib's, as they are for both C++ programs.
                let status = match e {
                    HttpError::BodyTooLarge => 413,
                    HttpError::HeadersTooLarge => 431,
                    HttpError::Unsupported(_) => 501,
                    _ => 400,
                };
                // An idle kept-alive connection timing out is not worth a line.
                if served == 0 || !matches!(e, HttpError::Io(_)) {
                    crate::logging::log(Level::Debug, format_args!("{peer} sent an unreadable request: {e}"));
                }
                let _ = http::write_response(&mut writer, &Response::new(status), false, &hint);
                return;
            }
        };

        let started = Instant::now();
        let mut response = handler(&request, &peer);
        // After the handler, as `cpp-httplib` does it: compression belongs to
        // the transport, and the handler's answer stays comparable.
        let coding = if config.gzip { http::compress_if_accepted(&request, &mut response) } else { None };
        log_request(&peer, &request, response.status, started.elapsed(), coding.is_some());

        served += 1;
        let keep = config.keep_alive && request.wants_keep_alive() && served < config.keep_alive_max;
        if http::write_response(&mut writer, &response, keep, &hint).is_err() || !keep {
            return;
        }
    }
}

/// One line per request. `401` at info — the C++ prints every rejected key —
/// a server error at warning, and the rest at debug, so an info log is
/// readable on a busy service. The path is logged without its query string,
/// and nothing from the headers or the body ever is.
fn log_request(peer: &Peer, request: &Request, status: u16, took: Duration, gzip: bool) {
    let level = match status {
        500.. => Level::Warn,
        401 => Level::Info,
        _ => Level::Debug,
    };
    if !crate::logging::enabled(level) {
        return;
    }
    let coding = if gzip { ", gzip" } else { "" };
    crate::logging::log(
        level,
        format_args!(
            "{peer} {} {} -> {status} in {:.1} ms{coding}",
            request.method,
            request.path,
            took.as_secs_f64() * 1000.0
        ),
    );
}

////////////////////////
/* CONNECTIONS        */
////////////////////////

/// One accepted connection, from whichever listener.
enum Conn {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

struct Accepted {
    conn: Conn,
    peer: Peer,
}

impl Conn {
    fn set_nodelay(&self) {
        match self {
            Conn::Tcp(s) => {
                let _ = s.set_nodelay(true);
            }
            #[cfg(unix)]
            Conn::Unix(_) => {}
        }
    }

    /// Answer from the acceptor without ever blocking it ([`http::shed_tcp`]).
    fn shed(&self, answer: &[u8]) {
        match self {
            Conn::Tcp(s) => http::shed_tcp(s, answer),
            #[cfg(unix)]
            Conn::Unix(s) => {
                let _ = s.set_nonblocking(true);
                let mut w = s;
                let _ = w.write_all(answer);
                let _ = s.shutdown(Shutdown::Write);
            }
        }
    }

    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        match self {
            Conn::Tcp(s) => s.set_write_timeout(d),
            #[cfg(unix)]
            Conn::Unix(s) => s.set_write_timeout(d),
        }
    }

    fn try_clone(&self) -> std::io::Result<Conn> {
        match self {
            Conn::Tcp(s) => s.try_clone().map(Conn::Tcp),
            #[cfg(unix)]
            Conn::Unix(s) => s.try_clone().map(Conn::Unix),
        }
    }

    fn shutdown(&self) {
        match self {
            Conn::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            #[cfg(unix)]
            Conn::Unix(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

impl ReadTimeout for Conn {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        match self {
            Conn::Tcp(s) => s.set_read_timeout(d),
            #[cfg(unix)]
            Conn::Unix(s) => s.set_read_timeout(d),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Conn::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Conn::Unix(s) => s.flush(),
        }
    }
}

////////////////////////
/* THE BOUNDED QUEUE  */
////////////////////////

struct Queue {
    inner: Mutex<Option<VecDeque<Accepted>>>,
    ready: Condvar,
    capacity: usize,
}

impl Queue {
    fn new(capacity: usize) -> Queue {
        Queue { inner: Mutex::new(Some(VecDeque::new())), ready: Condvar::new(), capacity }
    }

    /// The connection back when the queue is full, or closed, and the caller
    /// must shed it.
    fn push(&self, accepted: Accepted) -> Result<(), Accepted> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_mut() {
            Some(q) if q.len() < self.capacity => {
                q.push_back(accepted);
                drop(guard);
                self.ready.notify_one();
                Ok(())
            }
            _ => Err(accepted),
        }
    }

    /// `None` once the queue is closed.
    fn pop(&self) -> Option<Accepted> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let queue = guard.as_mut()?;
            if let Some(accepted) = queue.pop_front() {
                return Some(accepted);
            }
            guard = self.ready.wait(guard).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn close(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(q) = guard.take() {
            for accepted in q {
                accepted.conn.shutdown();
            }
        }
        drop(guard);
        self.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo() -> Handler {
        Arc::new(|req: &Request, peer: &Peer| {
            let body =
                format!("{{\"path\":\"{}\",\"ipc\":{},\"pad\":\"{}\"}}", req.path, peer.is_ipc(), "x".repeat(2048));
            Response::json(200, body)
        })
    }

    #[test]
    fn an_ipv6_bind_takes_the_address_with_or_without_brackets() {
        assert_eq!(ipv6_bind("::1", 7856), "[::1]:7856");
        assert_eq!(ipv6_bind("[::1]", 7856), "[::1]:7856");
        assert_eq!(ipv6_bind(" fe80::1 ", 1), "[fe80::1]:1");
        assert!(parse_ipv6_bind("[::1]:7856").is_ok());
        assert!(parse_ipv6_bind("127.0.0.1:7856").is_err(), "an IPv4 address is refused, not bound twice");
    }

    #[test]
    fn an_unspecified_bind_is_woken_through_loopback() {
        assert_eq!(wake_address("0.0.0.0:80".parse().unwrap()), "127.0.0.1:80".parse::<SocketAddr>().unwrap());
        assert_eq!(wake_address("[::]:80".parse().unwrap()), "[::1]:80".parse::<SocketAddr>().unwrap());
        assert_eq!(wake_address("10.0.0.1:80".parse().unwrap()), "10.0.0.1:80".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn nothing_to_listen_on_is_an_error() {
        let config = ListenConfig { bind: None, ..Default::default() };
        assert!(Listener::start(config, echo()).is_err());
    }

    #[cfg(not(unix))]
    #[test]
    fn an_ipc_socket_is_refused_with_the_cpp_reason_where_there_is_none() {
        let config = ListenConfig {
            bind: Some("127.0.0.1:0".into()),
            ipc: Some(IpcConfig { path: "/run/x.sock".into(), mode: 0o600, group: String::new() }),
            ..Default::default()
        };
        let listener = Listener::start(config, echo()).expect("TCP still serves");
        assert_eq!(listener.ipc_error(), Some(wrkz_rpc::ipc::UNSUPPORTED));
        assert!(listener.ipc_path().is_none());

        // And alone, with nothing else to serve on, it is fatal.
        let config = ListenConfig {
            ipc: Some(IpcConfig { path: "/run/x.sock".into(), mode: 0o600, group: String::new() }),
            ..Default::default()
        };
        assert!(Listener::start(config, echo()).is_err());
    }
}
