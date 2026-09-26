// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `GET /ws`: the chain and pool events as a WebSocket stream, on the RPC
//! port. This port's own; the C++ daemon has no WebSocket. Off unless the
//! daemon is started with `--enable-websocket`, and then a path like any
//! other: the same access token and the same rate limit as every route.
//!
//! # What a subscriber receives
//!
//! Text messages, each one JSON object `{"topic":"…","data":{…}}`. `data` is
//! byte for byte the body the ZMQ socket publishes under the same topic
//! ([`crate::events::topic_messages`]):
//!
//! | Topic | `data` | Sent when |
//! | --- | --- | --- |
//! | `hello` | `{"height":N,"hash":"…","topics":[…]}` | first, once: the tip, and the topics this stream carries |
//! | `hashblock` | `{"height":N,"hash":"…"}` | a block joins the main chain |
//! | `chain_main` | `{"height":N,"hash":"…","transaction_hashes":[…]}` | straight after; the coinbase first |
//! | `hashblock_alt` | `{"height":N,"hash":"…"}` | a block is kept on an alternative chain |
//! | `chainswitch` | `{"common_root_height":R,"hashes":[…]}` | a reorganisation; the common root first |
//! | `txpool_add` | `{"hashes":["…"]}` | a transaction enters the pool |
//! | `txpool_del` | `{"hashes":[…],"reason":"InBlock"}` | transactions leave it |
//! | `heartbeat` | `{}` | every [`WsConfig::ping_interval`] |
//!
//! `height` is a block index, as everywhere on the ZMQ socket. `?topics=` picks
//! topics by prefix, comma-separated, as a ZMQ subscription does
//! (`?topics=hashblock` brings `hashblock_alt` too); without it every topic is
//! sent. `hello` and `heartbeat` are always sent.
//!
//! A client should treat the stream as a *notification*: the RPC routes stay
//! the source of truth. On (re)connecting it reads `hello` and catches up over
//! HTTP; it never assumes it saw every event.
//!
//! # Limits
//!
//! - [`WsConfig::max_clients`] subscribers at once, and at most
//!   [`WsConfig::max_clients_per_ip`] from one address (loopback and the IPC
//!   socket exempt). An upgraded connection also keeps the RPC connection slot
//!   of its address for as long as it is open.
//! - Each subscriber costs two threads — a writer on its queue, a reader for
//!   pongs and the close — and a queue of [`QUEUE_MESSAGES`] frames.
//! - One that falls that far behind is **disconnected**, not skipped: a
//!   stream that silently lost messages would look complete when it is not.
//! - The server pings every [`WsConfig::ping_interval`]; a subscriber that
//!   sends nothing at all (a pong answers the ping) for
//!   [`WsConfig::idle_timeout`] is gone. The `heartbeat` message beside the
//!   ping is for browsers, whose scripts never see ping frames.
//! - What a subscriber may send is a pong, a ping, a close, or a message of at
//!   most [`MAX_INBOUND_MESSAGE`] bytes, which is read and ignored.
//!
//! # Browsers
//!
//! A page's script cannot set headers on a WebSocket, so it cannot present
//! the access token: a daemon with `--rpc-access-token` only streams to
//! programs. And a page that could not call the RPC cannot subscribe either:
//! a request with an `Origin` is refused unless `--enable-cors` allows that
//! origin (or `*`) — the same pages that may read the RPC may read the stream.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_ws::frame::{self, close, encode_frame, Message, MessageReader, Opcode, Role};

use crate::events::{topic_messages, ChainEvent, EventListener};

/// The topics [`crate::events::topic_messages`] publishes, in the order the
/// `hello` lists them.
pub const TOPICS: &[&str] = &["hashblock", "chain_main", "hashblock_alt", "chainswitch", "txpool_add", "txpool_del"];
/// Frames waiting for one subscriber before it is disconnected: the ZMQ
/// socket's high-water mark.
pub const QUEUE_MESSAGES: usize = 1000;
/// The largest message a subscriber may send. It has nothing to say.
pub const MAX_INBOUND_MESSAGE: usize = 4096;
/// Most prefixes one `?topics=` may name.
pub const MAX_TOPIC_PREFIXES: usize = 16;
/// A subscriber that does not take a frame within this is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a writer waits on its queue before looking at whether its
/// subscriber is gone, or a ping is due.
const WRITER_POLL: Duration = Duration::from_millis(100);
/// After our close frame, how long the peer has to close its side, and how
/// much it may still send meanwhile.
const LINGER: Duration = Duration::from_secs(1);
const LINGER_BYTES: usize = 64 * 1024;

/// `--enable-websocket` and its limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsConfig {
    /// `--ws-max-clients`.
    pub max_clients: usize,
    /// `--ws-max-clients-per-ip`. `0` disables the per-address cap.
    pub max_clients_per_ip: usize,
    /// How often the server pings, and sends `heartbeat`.
    pub ping_interval: Duration,
    /// How long a subscriber may send nothing before it is dropped. At least
    /// twice [`Self::ping_interval`], so one late pong is not fatal.
    pub idle_timeout: Duration,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            max_clients: 128,
            max_clients_per_ip: 4,
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(90),
        }
    }
}

/// What the hub needs of an upgraded connection: TCP, or the IPC socket.
///
/// Read and written through `&self`, as `std` allows for both socket types,
/// so the reader and the writer thread share one handle and nothing has to be
/// duplicated (`try_clone`) or kept in step.
pub trait WsStream: Send + Sync + 'static {
    fn read_shared(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_shared(&self, buf: &[u8]) -> io::Result<usize>;
    /// Close both directions.
    fn shutdown_stream(&self);
    /// Close the sending direction only: our FIN after our close frame.
    fn shutdown_write(&self);
    fn set_read_timeout_stream(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout_stream(&self, timeout: Option<Duration>) -> io::Result<()>;
}

impl WsStream for TcpStream {
    fn read_shared(&self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }

    fn write_shared(&self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn shutdown_stream(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }

    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }

    fn set_read_timeout_stream(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }

    fn set_write_timeout_stream(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(timeout)
    }
}

/// One thread's view of the shared socket.
struct Half(Arc<dyn WsStream>);

impl Read for Half {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_shared(buf)
    }
}

impl Write for Half {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_shared(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Which topics a subscriber asked for: every one, or those matching any of
/// a few prefixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topics(Option<Vec<String>>);

impl Topics {
    /// Every topic.
    pub fn all() -> Self {
        Topics(None)
    }

    /// `?topics=` from a query string: absent or empty is every topic. Each
    /// prefix must match at least one topic, so a misspelt one is an error
    /// rather than a stream that stays silent.
    pub fn from_query(query: &str) -> Result<Self, String> {
        let Some(value) = query.split('&').find_map(|pair| pair.strip_prefix("topics=")) else {
            return Ok(Topics::all());
        };
        let prefixes: Vec<String> =
            value.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect();
        if prefixes.is_empty() {
            return Ok(Topics::all());
        }
        if prefixes.len() > MAX_TOPIC_PREFIXES {
            return Err(format!("at most {MAX_TOPIC_PREFIXES} topics"));
        }
        if let Some(unknown) = prefixes.iter().find(|p| !TOPICS.iter().any(|t| t.starts_with(p.as_str()))) {
            return Err(format!("no topic starts with '{unknown}'; the topics are {}", TOPICS.join(", ")));
        }
        Ok(Topics(Some(prefixes)))
    }

    pub fn wants(&self, topic: &str) -> bool {
        match &self.0 {
            None => true,
            Some(prefixes) => prefixes.iter().any(|p| topic.starts_with(p.as_str())),
        }
    }

    /// The topics this subscription carries, for its `hello`.
    pub fn carried(&self) -> Vec<&'static str> {
        TOPICS.iter().copied().filter(|t| self.wants(t)).collect()
    }
}

/// One `{"topic":…,"data":…}` text frame, encoded once for every subscriber.
fn message_frame(topic: &str, data: &str) -> Arc<Vec<u8>> {
    Arc::new(encode_frame(Opcode::Text, format!("{{\"topic\":\"{topic}\",\"data\":{data}}}").as_bytes(), None))
}

/// The first message: the tip, and the topics.
pub fn hello(height: u64, hash: &[u8; 32], topics: &Topics) -> String {
    let carried: Vec<String> = topics.carried().iter().map(|t| format!("\"{t}\"")).collect();
    format!("{{\"height\":{height},\"hash\":\"{}\",\"topics\":[{}]}}", hex::encode(hash), carried.join(","))
}

struct Client {
    ip: String,
    exempt: bool,
    topics: Topics,
    queue: SyncSender<Arc<Vec<u8>>>,
    /// The one handle both threads use, and what shuts it from any thread.
    socket: Arc<dyn WsStream>,
    gone: AtomicBool,
}

impl Client {
    /// Close the socket; both of its threads then finish on their own.
    fn drop_connection(&self) {
        if !self.gone.swap(true, Ordering::SeqCst) {
            self.socket.shutdown_stream();
        }
    }
}

struct Shared {
    config: WsConfig,
    clients: Mutex<Vec<Arc<Client>>>,
    /// Places promised to upgrades whose `101` is being written.
    reserved: Mutex<Vec<(String, bool)>>,
    running: AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
    published: AtomicU64,
    disconnected_slow: AtomicU64,
}

impl Shared {
    fn clients(&self) -> MutexGuard<'_, Vec<Arc<Client>>> {
        self.clients.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn reserved(&self) -> MutexGuard<'_, Vec<(String, bool)>> {
        self.reserved.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn remove(&self, client: &Arc<Client>) {
        client.drop_connection();
        self.clients().retain(|c| !Arc::ptr_eq(c, client));
    }

    fn send_to_all(&self, topic: &str, frame: &Arc<Vec<u8>>) {
        let clients = self.clients().clone();
        for client in clients {
            if !client.topics.wants(topic) {
                continue;
            }
            if let Err(TrySendError::Full(_)) = client.queue.try_send(Arc::clone(frame)) {
                self.disconnected_slow.fetch_add(1, Ordering::Relaxed);
                self.remove(&client);
            }
        }
    }
}

impl EventListener for Shared {
    fn on_event(&self, event: &ChainEvent) {
        if !self.running.load(Ordering::SeqCst) {
            return;
        }
        for (topic, body) in topic_messages(event) {
            self.published.fetch_add(1, Ordering::Relaxed);
            self.send_to_all(topic, &message_frame(topic, &body));
        }
    }
}

/// A place held for one upgrade while its `101` is written. Dropping it
/// without [`WsHub::admit`] gives the place back.
pub struct Reservation {
    shared: Arc<Shared>,
    ip: String,
    exempt: bool,
    taken: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.taken {
            let mut reserved = self.shared.reserved();
            if let Some(i) = reserved.iter().position(|(ip, _)| *ip == self.ip) {
                reserved.remove(i);
            }
        }
    }
}

/// Why an upgrade was turned away before its `101`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Every place is taken: `503`.
    Full,
    /// This address holds its share: `429`.
    TooManyFromAddress,
    /// The hub is stopping: `503`.
    Stopping,
}

/// The subscribers, and what publishes to them. Cheap to clone.
#[derive(Clone)]
pub struct WsHub {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for WsHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsHub").field("clients", &self.clients()).finish()
    }
}

impl WsHub {
    pub fn new(mut config: WsConfig) -> Self {
        config.max_clients = config.max_clients.max(1);
        config.ping_interval = config.ping_interval.max(Duration::from_secs(1));
        config.idle_timeout = config.idle_timeout.max(config.ping_interval * 2);
        Self {
            shared: Arc::new(Shared {
                config,
                clients: Mutex::new(Vec::new()),
                reserved: Mutex::new(Vec::new()),
                running: AtomicBool::new(true),
                threads: Mutex::new(Vec::new()),
                published: AtomicU64::new(0),
                disconnected_slow: AtomicU64::new(0),
            }),
        }
    }

    pub fn config(&self) -> &WsConfig {
        &self.shared.config
    }

    /// What the chain and the pool publish to, for the daemon's `Events`.
    pub fn listener(&self) -> Arc<dyn EventListener> {
        Arc::clone(&self.shared) as Arc<dyn EventListener>
    }

    /// Subscribers connected now.
    pub fn clients(&self) -> usize {
        self.shared.clients().len()
    }

    /// Messages published since start, counted once however many received them.
    pub fn published(&self) -> u64 {
        self.shared.published.load(Ordering::Relaxed)
    }

    /// Subscribers disconnected for falling [`QUEUE_MESSAGES`] behind.
    pub fn disconnected_slow(&self) -> u64 {
        self.shared.disconnected_slow.load(Ordering::Relaxed)
    }

    /// Hold a place for `ip`, before the `101` is written. `exempt` — loopback
    /// or the IPC socket — skips the per-address cap but not the total.
    pub fn reserve(&self, ip: &str, exempt: bool) -> Result<Reservation, Refusal> {
        if !self.shared.running.load(Ordering::SeqCst) {
            return Err(Refusal::Stopping);
        }
        let clients = self.shared.clients();
        let mut reserved = self.shared.reserved();
        if clients.len() + reserved.len() >= self.shared.config.max_clients {
            return Err(Refusal::Full);
        }
        let cap = self.shared.config.max_clients_per_ip;
        if !exempt && cap != 0 {
            let held = clients.iter().filter(|c| !c.exempt && c.ip == ip).count()
                + reserved.iter().filter(|(r, e)| !*e && r == ip).count();
            if held >= cap {
                return Err(Refusal::TooManyFromAddress);
            }
        }
        reserved.push((ip.to_string(), exempt));
        Ok(Reservation { shared: Arc::clone(&self.shared), ip: ip.to_string(), exempt, taken: false })
    }

    /// Take over a connection whose `101` has been written: send `hello`, then
    /// everything published, until either side closes. `guard` is held for as
    /// long as the connection is open — the RPC's per-address connection slot.
    pub fn admit(
        &self,
        mut reservation: Reservation,
        stream: Box<dyn WsStream>,
        topics: Topics,
        hello_data: &str,
        guard: Box<dyn Send>,
    ) {
        let shared = &self.shared;
        let stream: Arc<dyn WsStream> = Arc::from(stream);
        let (reader, writer) = (Half(Arc::clone(&stream)), Half(Arc::clone(&stream)));
        let _ = stream.set_write_timeout_stream(Some(WRITE_TIMEOUT));
        let _ = stream.set_read_timeout_stream(Some(shared.config.idle_timeout));
        let (queue, queued) = sync_channel::<Arc<Vec<u8>>>(QUEUE_MESSAGES);
        // The hello goes first; the queue is empty, so this cannot fail.
        let _ = queue.try_send(message_frame("hello", hello_data));
        let client = Arc::new(Client {
            ip: reservation.ip.clone(),
            exempt: reservation.exempt,
            topics,
            queue,
            socket: stream,
            gone: AtomicBool::new(false),
        });
        {
            let mut clients = shared.clients();
            let mut reserved = shared.reserved();
            if let Some(i) = reserved.iter().position(|(ip, _)| *ip == reservation.ip) {
                reserved.remove(i);
            }
            reservation.taken = true;
            clients.push(Arc::clone(&client));
        }

        // Whichever thread learns first that the connection is over ends it —
        // except that a reader which queued a close frame leaves that to the
        // writer, which must send the frame before the socket is shut.
        let ping_interval = shared.config.ping_interval;
        let writing = Arc::clone(&client);
        let hub = Arc::clone(shared);
        let write = std::thread::Builder::new().name("wrkz-ws-write".into()).spawn(move || {
            write_loop(&writing, writer, queued, ping_interval);
            hub.remove(&writing);
        });
        let reading = Arc::clone(&client);
        let hub = Arc::clone(shared);
        let read = std::thread::Builder::new().name("wrkz-ws-read".into()).spawn(move || {
            let _guard = guard;
            if read_loop(&reading, reader) == ReadEnd::Gone {
                hub.remove(&reading);
            }
        });
        // Only writers are joined on stop. A reader parked in `recv` is not
        // woken by `shutdown` everywhere (Windows keeps it until the peer
        // closes or the idle timeout passes), and it holds nothing a stop
        // needs back — so, like the ZMQ publisher's, it is left to finish.
        let mut threads = shared.threads.lock().unwrap_or_else(|p| p.into_inner());
        threads.retain(|t| !t.is_finished());
        if write.is_err() || read.is_err() {
            shared.remove(&client);
        }
        threads.extend(write);
    }

    /// Say goodbye to every subscriber (close 1001), and wait for their
    /// threads. Nothing is admitted afterwards.
    pub fn stop(&self) {
        if !self.shared.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let goodbye = Arc::new(encode_frame(Opcode::Close, &frame::close_payload(close::GOING_AWAY, "stopping"), None));
        let clients = self.shared.clients().clone();
        for client in &clients {
            // A full queue is dropped straight away instead.
            if client.queue.try_send(Arc::clone(&goodbye)).is_err() {
                self.shared.remove(client);
            }
        }
        // The writers send the close and shut their sockets; give them a
        // moment, then shut whatever is left.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !self.shared.clients().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Bound first: a guard held by the `for` would still be held inside
        // `remove`, which takes the same lock.
        let left = self.shared.clients().clone();
        for client in left {
            self.shared.remove(&client);
        }
        let threads: Vec<JoinHandle<()>> =
            self.shared.threads.lock().unwrap_or_else(|p| p.into_inner()).drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

/// Everything queued for one subscriber, a ping and a heartbeat whenever the
/// queue has been quiet for a ping interval, and a close frame ends it.
fn write_loop(client: &Client, mut writer: Half, queued: Receiver<Arc<Vec<u8>>>, ping_interval: Duration) {
    let ping = encode_frame(Opcode::Ping, b"", None);
    let heartbeat = message_frame("heartbeat", "{}");
    let mut last_ping = std::time::Instant::now();
    // The client holds the sending end of `queued` for as long as anything
    // holds the client, this thread included, so the queue never reports
    // itself closed: the wait is sliced, and `gone` is what ends it.
    while !client.gone.load(Ordering::SeqCst) {
        if last_ping.elapsed() >= ping_interval {
            last_ping = std::time::Instant::now();
            if writer.write_all(&ping).and_then(|()| writer.write_all(&heartbeat)).is_err() {
                break;
            }
        }
        let bytes = match queued.recv_timeout(WRITER_POLL) {
            Ok(bytes) => bytes,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if writer.write_all(&bytes).is_err() {
            break;
        }
        // A close frame is the last thing sent.
        if bytes.first() == Some(&(0x80 | 0x8)) {
            linger(&writer.0);
            break;
        }
    }
    client.drop_connection();
}

/// The closing half of RFC 6455 §7.1.1: after our close frame, send FIN and
/// wait a moment for the peer's, reading and discarding whatever it still
/// sends. Shutting the socket with bytes unread in it would answer with a
/// reset instead, and a reset can overtake the close frame just written — the
/// peer would never learn why it was closed.
fn linger(stream: &Arc<dyn WsStream>) {
    stream.shutdown_write();
    let _ = stream.set_read_timeout_stream(Some(LINGER));
    let deadline = std::time::Instant::now() + LINGER;
    let mut buf = [0u8; 4096];
    let mut read = 0;
    while read < LINGER_BYTES && std::time::Instant::now() < deadline {
        match stream.read_shared(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => read += n,
        }
    }
}

/// How a reader finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadEnd {
    /// The connection is over: the reader ends it.
    Gone,
    /// A close frame is queued; the writer sends it, then ends the connection.
    Closing,
}

/// Queue the close frame that ends this connection, if there is room.
fn queue_close(client: &Client, code: u16) -> ReadEnd {
    let frame = Arc::new(encode_frame(Opcode::Close, &frame::close_payload(code, ""), None));
    match client.queue.try_send(frame) {
        Ok(()) => ReadEnd::Closing,
        Err(_) => ReadEnd::Gone,
    }
}

/// What a subscriber sends: pongs, pings to answer, and the close.
fn read_loop(client: &Client, mut reader: Half) -> ReadEnd {
    let mut messages = MessageReader::new(Role::Server, MAX_INBOUND_MESSAGE);
    loop {
        if client.gone.load(Ordering::SeqCst) {
            return ReadEnd::Gone;
        }
        match messages.read(&mut reader) {
            Ok(Message::Ping(payload)) => {
                if client.queue.try_send(Arc::new(encode_frame(Opcode::Pong, &payload, None))).is_err() {
                    return ReadEnd::Gone;
                }
            }
            Ok(Message::Pong(_)) | Ok(Message::Text(_)) | Ok(Message::Binary(_)) => {}
            // Echo the close (§5.5.1).
            Ok(Message::Close(_)) => return queue_close(client, close::NORMAL),
            Err(e) => {
                return match e.close_code() {
                    Some(code) => queue_close(client, code),
                    None => ReadEnd::Gone,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_prefixes_and_a_misspelling_is_an_error() {
        assert_eq!(Topics::from_query(""), Ok(Topics::all()));
        assert_eq!(Topics::from_query("other=1&topics="), Ok(Topics::all()));
        let blocks = Topics::from_query("topics=hashblock,chainswitch").unwrap();
        assert!(blocks.wants("hashblock") && blocks.wants("hashblock_alt") && blocks.wants("chainswitch"));
        assert!(!blocks.wants("txpool_add") && !blocks.wants("chain_main"));
        assert_eq!(blocks.carried(), vec!["hashblock", "hashblock_alt", "chainswitch"]);
        assert!(Topics::from_query("topics=hashblok").is_err());
        let many = format!("topics={}", vec!["txpool"; MAX_TOPIC_PREFIXES + 1].join(","));
        assert!(Topics::from_query(&many).is_err());
    }

    #[test]
    fn the_hello_names_the_tip_and_the_topics() {
        let body = hello(12, &[0xab; 32], &Topics::from_query("topics=txpool").unwrap());
        assert_eq!(
            body,
            format!("{{\"height\":12,\"hash\":\"{}\",\"topics\":[\"txpool_add\",\"txpool_del\"]}}", "ab".repeat(32))
        );
    }

    #[test]
    fn places_are_counted_per_address_and_given_back() {
        let hub = WsHub::new(WsConfig { max_clients: 3, max_clients_per_ip: 2, ..WsConfig::default() });
        let a = hub.reserve("1.2.3.4", false).unwrap();
        let _b = hub.reserve("1.2.3.4", false).unwrap();
        assert_eq!(hub.reserve("1.2.3.4", false).err(), Some(Refusal::TooManyFromAddress));
        // Loopback skips the per-address cap, not the total.
        let _c = hub.reserve("127.0.0.1", true).unwrap();
        assert_eq!(hub.reserve("127.0.0.1", true).err(), Some(Refusal::Full));
        drop(a);
        assert!(hub.reserve("5.6.7.8", false).is_ok());
        hub.stop();
        assert_eq!(hub.reserve("5.6.7.8", false).err(), Some(Refusal::Stopping));
    }
}
