// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The built-in stratum server (`src/daemon/StratumServer.cpp`), so a stock
//! miner can point straight at this node: `--stratum-bind-port`.
//!
//! Our blocks are Forknote lineage: the outer header is major, minor and
//! `prev_id`, and the real timestamp and nonce live inside a merge-mining
//! parent block after it. A miner that expects Monero's flat header cannot
//! read that, which is why xmrig's own `--daemon` mode rejects our templates —
//! against the C++ daemon and against this one alike.
//!
//! None of that matters over stratum. What a miner hashes is the parent
//! block's hashing serialization, an ordinary CryptoNote blob with the nonce
//! four bytes wide at offset 39, exactly where every stratum miner writes it.
//! So the node assembles the block and hands out plain blobs, and the miner
//! never sees a block at all.
//!
//! The protocol is the C++'s, message for message: `login`, `getjob`,
//! `submit`, `keepalived`, and a `job` notification whenever the tip moves.
//! Each connection gets its own extra nonce, so two rigs never grind the same
//! nonce space, and keeps its last [`JOB_HISTORY`] jobs, so a share found as a
//! block lands is still checked against the job it was found on.
//!
//! A block found here goes through [`NodeApi::submit_block`], the same call as
//! `submitblock`, and so reaches the network the same way: the daemon loop
//! announces it the moment it is added.
//!
//! Threads, not the C++'s fibres: one accepting, one watching the tip, and a
//! reader and a writer per connection. The writer drains a bounded queue, so a
//! miner that stops reading is dropped at [`MAX_QUEUED_MESSAGES`] rather than
//! stalling the job broadcast to everyone else.
//!
//! Two of those threads hand a connection jobs: its reader, answering a login
//! or re-jobbing the rig that just found a block, and the tip watcher. The C++
//! does both on one dispatcher, so each message there describes the job its
//! caller built. Here, building a job and queuing the message that carries it
//! happen under one per-connection lock, for the same two guarantees: a message
//! never describes a job someone else built — xmrig takes two identical jobs in
//! a row for a broken pool and reconnects (`Client::parseJob`, "duplicate job
//! received") — and jobs reach the miner in the order they were built, so the
//! last one it holds is the newest.

use std::collections::{HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_primitives::block::{BlockTemplate, ParentBlock, BLOCK_MAJOR_VERSION_2};
use wrkz_primitives::tx::{append_merge_mining_tag, BaseTransaction, MergeMiningTag, TransactionPrefix};
use wrkz_primitives::Hash;
use wrkz_rpc::api::SubmitOutcome;
use wrkz_rpc::json::{self, Json, Obj, ParseLimits};
use wrkz_rpc::NodeApi;

use crate::{log_debug, log_info, log_warn};

/// How many past jobs a connection keeps (`JOB_HISTORY`).
pub const JOB_HISTORY: usize = 4;
/// A stratum line is a small JSON object; past this the connection is closed
/// rather than the buffer grown (`MAX_LINE_BYTES`).
pub const MAX_LINE_BYTES: usize = 8192;
const READ_CHUNK_BYTES: usize = 4096;
/// Messages queued to a connection that has stopped reading before it is
/// treated as gone (`MAX_QUEUED_MESSAGES`).
pub const MAX_QUEUED_MESSAGES: usize = 64;
/// The per-connection extra nonce put in the coinbase (`EXTRA_NONCE_BYTES`).
pub const EXTRA_NONCE_BYTES: usize = 8;
/// `stratumMaxConnections` (`DaemonConfiguration.h:105`).
pub const DEFAULT_MAX_CONNECTIONS: usize = 32;

/// How often the tip is compared with the one the jobs were built on. The C++
/// is told by the core's message queue; this port has no such queue, and a
/// read of the top index and its hash is cheap.
const TIP_POLL: Duration = Duration::from_millis(200);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// How long the writer waits for the queue before checking whether the
/// connection is closing.
const WRITER_POLL: Duration = Duration::from_millis(100);
/// A miner that does not take a line within this is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// `--stratum-bind-ip`, `--stratum-bind-port`, `--stratum-share-difficulty`,
/// `--stratum-max-connections`, with the C++ defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StratumConfig {
    /// Loopback by default, as the C++ binds it.
    pub bind_ip: IpAddr,
    /// 0 leaves the server off.
    pub port: u16,
    /// 0 means shares are only reported when they are blocks, which is what
    /// solo mining wants. Anything else lowers the miner's target so it
    /// reports progress, while a block still needs the real one.
    pub share_difficulty: u64,
    /// Miners allowed at once; 0 is taken as 1, as the C++ takes it.
    pub max_connections: usize,
}

impl Default for StratumConfig {
    fn default() -> Self {
        Self {
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            share_difficulty: 0,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

/// Whether the node is far enough along to be worth mining on
/// (`StratumServer::chainReady`). A node still pulling the chain would hand
/// out templates on a block the network left behind long ago, so every one of
/// them would be orphaned.
pub type ChainReady = Box<dyn Fn() -> bool + Send + Sync>;

/// A running stratum server. Dropping it stops it.
pub struct StratumServer {
    shared: Arc<Shared>,
    local_addr: SocketAddr,
    threads: Vec<JoinHandle<()>>,
}

impl StratumServer {
    /// Bind and start serving. An `Err` is a listener that could not be bound;
    /// mining is optional, so the daemon logs it and runs on without it.
    pub fn start(api: Arc<dyn NodeApi>, cfg: &StratumConfig, ready: ChainReady) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(cfg.bind_ip, cfg.port))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            api,
            ready,
            share_difficulty: cfg.share_difficulty,
            max_connections: cfg.max_connections.max(1),
            running: AtomicBool::new(true),
            ids: AtomicU64::new(0),
            clients: Mutex::new(Vec::new()),
            reported_limit: AtomicBool::new(false),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("wrkz-stratum-accept".into())
                .spawn(move || shared.accept_loop(listener))?
        };
        let tip = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new().name("wrkz-stratum-tip".into()).spawn(move || shared.tip_loop())?
        };
        log_info!("stratum server listening on {local_addr}");
        if cfg.share_difficulty == 0 {
            log_info!(
                "stratum shares are set at the network difficulty: a miner only reports when it has actually found \
                 a block"
            );
        } else {
            log_info!(
                "stratum share difficulty fixed at {}. Shares below the network difficulty are counted, not \
                 submitted",
                cfg.share_difficulty
            );
        }
        Ok(Self { shared, local_addr, threads: vec![accept, tip] })
    }

    /// The address actually bound, which differs from the configured one when
    /// port 0 was asked for.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Miners connected right now.
    pub fn connections(&self) -> usize {
        self.shared.clients().len()
    }

    /// Close every connection and join the server's own threads.
    pub fn stop(&mut self) {
        if !self.shared.running.swap(false, Ordering::SeqCst) {
            return;
        }
        for client in self.shared.clients().drain(..) {
            client.close_now();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for StratumServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One job handed to one connection.
#[derive(Clone, Debug)]
struct Job {
    id: String,
    /// The block, merge-mining tag already written; only the nonce changes.
    block: BlockTemplate,
    /// The difficulty the chain wants. A share meeting this is a block.
    difficulty: u64,
    /// A **count**: the height of the block being mined.
    height: u64,
    blob_hex: String,
    /// Nonces already submitted against this job, so a resend is answered
    /// rather than counted twice.
    seen_nonces: HashSet<u32>,
}

#[derive(Default)]
struct ClientState {
    session_id: String,
    address: String,
    agent: String,
    logged_in: bool,
    /// Newest last.
    jobs: VecDeque<Job>,
    accepted_shares: u64,
    found_blocks: u64,
}

struct Client {
    peer: String,
    /// For `shutdown`: the reader and the writer each hold a clone.
    stream: TcpStream,
    outbox: SyncSender<String>,
    closing: AtomicBool,
    state: Mutex<ClientState>,
    /// Held from building a job to queuing the message that carries it (see
    /// the module notes). Always taken before `state`, never while holding it.
    issuing: Mutex<()>,
}

impl Client {
    fn state(&self) -> MutexGuard<'_, ClientState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn issuing(&self) -> MutexGuard<'_, ()> {
        self.issuing.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Stop reading; the writer sends what is queued, then closes the socket.
    fn close_after_flush(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    /// Close the socket now, which also wakes a reader blocked in `read`.
    fn close_now(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct Shared {
    api: Arc<dyn NodeApi>,
    ready: ChainReady,
    share_difficulty: u64,
    max_connections: usize,
    running: AtomicBool,
    /// Session ids, job ids and extra nonces, from one counter as in the C++.
    ids: AtomicU64,
    clients: Mutex<Vec<Arc<Client>>>,
    /// Set once the connection cap has been reported, so a rig retrying in a
    /// loop does not fill the log with it.
    reported_limit: AtomicBool,
}

impl Shared {
    fn clients(&self) -> MutexGuard<'_, Vec<Arc<Client>>> {
        self.clients.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        while self.running() {
            match listener.accept() {
                Ok((stream, addr)) => self.admit(stream, addr),
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
                Err(e) => {
                    log_warn!("stratum accept failed: {e}");
                    std::thread::sleep(ACCEPT_POLL);
                }
            }
        }
    }

    fn admit(self: &Arc<Self>, stream: TcpStream, addr: SocketAddr) {
        let mut clients = self.clients();
        if clients.len() >= self.max_connections {
            // Dropping the stream closes it. Said once until a slot frees up.
            if !self.reported_limit.swap(true, Ordering::SeqCst) {
                log_warn!("refused a stratum connection: already at the {} connection limit", self.max_connections);
            }
            return;
        }
        self.reported_limit.store(false, Ordering::SeqCst);

        // An accepted socket inherits the listener's non-blocking mode on
        // Windows, and not on Linux; make it blocking on both.
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_nodelay(true);
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let (Ok(reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        let (outbox, queued) = sync_channel(MAX_QUEUED_MESSAGES);
        let client = Arc::new(Client {
            peer: addr.ip().to_string(),
            stream,
            outbox,
            closing: AtomicBool::new(false),
            state: Mutex::new(ClientState::default()),
            issuing: Mutex::new(()),
        });
        clients.push(Arc::clone(&client));
        drop(clients);

        let for_writer = Arc::clone(&client);
        let spawned_writer = std::thread::Builder::new()
            .name("wrkz-stratum-write".into())
            .spawn(move || write_loop(&for_writer, writer, queued));
        let shared = Arc::clone(self);
        let for_reader = Arc::clone(&client);
        let spawned_reader = std::thread::Builder::new()
            .name("wrkz-stratum-read".into())
            .spawn(move || shared.read_loop(&for_reader, reader));
        if spawned_writer.is_err() || spawned_reader.is_err() {
            client.close_now();
            self.drop_client(&client);
        }
    }

    fn read_loop(&self, client: &Arc<Client>, mut reader: TcpStream) {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        'read: while self.running() && !client.is_closing() {
            let n = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    log_debug!("stratum client {} read ended: {e}", client.peer);
                    break;
                }
            };
            buffer.extend_from_slice(&chunk[..n]);
            while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=newline).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if !line.is_empty() {
                    self.handle_line(client, &line);
                }
                if client.is_closing() {
                    break 'read;
                }
            }
            if buffer.len() > MAX_LINE_BYTES {
                log_warn!("stratum client {} sent an oversized request; closing", client.peer);
                break;
            }
        }
        self.drop_client(client);
    }

    fn tip_loop(&self) {
        let mut last = self.tip();
        while self.running() {
            std::thread::sleep(TIP_POLL);
            let now = self.tip();
            if now != last {
                last = now;
                self.broadcast_jobs();
            }
        }
    }

    /// The top index and its hash: a reorganisation to a chain of the same
    /// height moves the second without the first.
    fn tip(&self) -> Option<(u64, Hash)> {
        let top = self.api.top_index();
        self.api.block_hash_by_index(top).ok().flatten().map(|hash| (top, hash))
    }

    fn handle_line(&self, client: &Arc<Client>, line: &[u8]) {
        let request = match json::parse(line, ParseLimits { max_bytes: MAX_LINE_BYTES, max_depth: 16 }) {
            Ok(r) if r.is_object() => r,
            _ => {
                log_debug!("stratum client {} sent malformed JSON", client.peer);
                client.close_after_flush();
                return;
            }
        };
        match request.get("method").and_then(Json::as_str).unwrap_or("") {
            "login" => self.handle_login(client, &request),
            "getjob" => self.handle_get_job(client, &request),
            "submit" => self.handle_submit(client, &request),
            "keepalived" | "keepalive" => {
                let mut result = Obj::new();
                result.set("status", "KEEPALIVED");
                self.reply_result(client, &request, result.build());
            }
            _ => self.reply_error(client, &request, "Unknown method"),
        }
    }

    fn handle_login(&self, client: &Arc<Client>, request: &Json) {
        let params = params_of(request);
        let address = string_of(params, "login");
        if let Err(message) = wrkz_rpc::jsonrpc::validate_address(&address) {
            log_warn!("stratum login from {} refused: {message}", client.peer);
            self.reply_error(client, request, &message);
            client.close_after_flush();
            return;
        }
        if !(self.ready)() {
            let h = self.api.height();
            let message = format!(
                "Node is still synchronizing ({} of {}); mining would only produce orphans. Retry once it has \
                 caught up.",
                h.height, h.network_height
            );
            log_info!("turned away a stratum miner from {}: still synchronizing", client.peer);
            self.reply_error(client, request, &message);
            return;
        }
        let agent = string_of(params, "agent");
        // Taken before the connection counts as logged in, so the tip watcher
        // cannot queue a job notification ahead of the login's own answer.
        let _issuing = client.issuing();
        let session_id = {
            let mut st = client.state();
            st.address = address.clone();
            st.agent = agent.clone();
            st.session_id = self.next_id().to_string();
            st.logged_in = true;
            st.jobs.clear();
            st.session_id.clone()
        };
        let job = match self.refresh_job(client) {
            Ok(job) => job,
            Err(e) => {
                self.reply_error(client, request, &e);
                return;
            }
        };
        let mut result = Obj::new();
        result.set("id", session_id).set("job", job).set("status", "OK").set("extensions", Json::Array(Vec::new()));
        self.reply_result(client, request, result.build());
        let agent = if agent.is_empty() { String::new() } else { format!(" ({agent})") };
        log_info!("stratum miner connected from {}{agent} mining to {address}", client.peer);
    }

    fn handle_get_job(&self, client: &Arc<Client>, request: &Json) {
        if !client.state().logged_in {
            self.reply_error(client, request, "Unauthenticated");
            return;
        }
        if !(self.ready)() {
            self.reply_error(client, request, "Node is still synchronizing");
            return;
        }
        // The job the miner has is still the right one until the tip moves,
        // and one job carries four billion nonces; building a template is not
        // cheap, so a miner polling getjob gets the same one back.
        let _issuing = client.issuing();
        let next_height = self.api.top_index() + 1;
        let current = {
            let st = client.state();
            st.jobs.back().filter(|j| j.height == next_height).map(|_| describe_job(&st, self.share_difficulty))
        };
        let job = match current {
            Some(job) => job,
            None => match self.refresh_job(client) {
                Ok(job) => job,
                Err(e) => {
                    self.reply_error(client, request, &e);
                    return;
                }
            },
        };
        self.reply_result(client, request, job);
    }

    fn handle_submit(&self, client: &Arc<Client>, request: &Json) {
        let params = params_of(request);
        // Everything needed from the job is copied out under the lock: the
        // hash below takes a while, and a block arriving meanwhile re-jobs this
        // connection and may push the job out of its history.
        let (candidate, job_difficulty, job_height) = {
            let mut st = client.state();
            if !st.logged_in {
                drop(st);
                self.reply_error(client, request, "Unauthenticated");
                return;
            }
            let job_id = string_of(params, "job_id");
            let Some(job) = st.jobs.iter_mut().find(|j| j.id == job_id) else {
                drop(st);
                self.reply_error(client, request, "Invalid job id");
                return;
            };
            let Some(nonce) = parse_nonce(&string_of(params, "nonce")) else {
                drop(st);
                self.reply_error(client, request, "Malformed nonce");
                return;
            };
            if !job.seen_nonces.insert(nonce) {
                drop(st);
                self.reply_error(client, request, "Duplicate share");
                return;
            }
            // The nonce is serialized inside the parent block, so setting it
            // is all it takes to rebuild both the hashing blob and the block.
            let mut candidate = job.block.clone();
            candidate.nonce = nonce;
            (candidate, job.difficulty, job.height)
        };

        let long_hash = match candidate.pow_hash() {
            Ok(h) => h,
            Err(e) => {
                log_warn!("stratum share from {} could not be hashed: {e}", client.peer);
                self.reply_error(client, request, "Could not hash share");
                return;
            }
        };
        // Miners send the hash they got. When it disagrees with ours the rig is
        // on the wrong algorithm, which is worth saying plainly.
        let claimed = string_of(params, "result");
        if !claimed.is_empty() && !claimed.eq_ignore_ascii_case(&hex::encode(long_hash)) {
            log_warn!(
                "stratum share from {} hashed to something else - check that the miner is running {}",
                client.peer,
                algorithm_name(candidate.major_version)
            );
            self.reply_error(client, request, "Invalid result");
            return;
        }
        let share_difficulty = if self.share_difficulty == 0 { job_difficulty } else { self.share_difficulty };
        if !wrkz_pow::check_hash(&long_hash, share_difficulty) {
            self.reply_error(client, request, "Low difficulty share");
            return;
        }
        client.state().accepted_shares += 1;

        if !wrkz_pow::check_hash(&long_hash, job_difficulty) {
            // Counted, not a block: only reachable below the network difficulty.
            self.reply_ok(client, request);
            return;
        }
        let Ok(blob) = candidate.to_bytes() else {
            self.reply_error(client, request, "Could not serialize block");
            return;
        };
        match self.api.submit_block(&blob) {
            Ok(SubmitOutcome::Added { .. }) => {}
            Ok(SubmitOutcome::NotAccepted) => {
                log_warn!("stratum block from {} at height {job_height} was rejected", client.peer);
                self.reply_error(client, request, "Block not accepted");
                return;
            }
            Err(e) => {
                log_warn!("stratum block from {} at height {job_height} was rejected: {e}", client.peer);
                self.reply_error(client, request, "Block not accepted");
                return;
            }
        }
        let address = {
            let mut st = client.state();
            st.found_blocks += 1;
            st.address.clone()
        };
        let id = candidate.hash().map(hex::encode).unwrap_or_default();
        log_info!(
            "stratum miner {} ({address}) found block at height {job_height}, difficulty {job_difficulty}, hash {id}",
            client.peer
        );
        self.reply_ok(client, request);
        // The tip watcher re-jobs everyone next, but the rig that found the
        // block should not spend that round trip on a dead template.
        self.send_job(client);
    }

    /// Build a fresh template for this connection, append it to its job history
    /// and return the `job` object for it (`StratumServer::refreshJob`). The
    /// object is taken under the same lock as the push, so it is this job's and
    /// not a newer one; a caller holds [`Client::issuing`] until the message
    /// carrying it is queued.
    fn refresh_job(&self, client: &Arc<Client>) -> Result<Json, String> {
        let address = client.state().address.clone();
        let extra_nonce: [u8; EXTRA_NONCE_BYTES] = self.next_id().to_le_bytes();
        let answer = match self.api.block_template(&address, &extra_nonce) {
            Ok(Ok(a)) => a,
            Ok(Err(reason)) => return Err(format!("Could not create a block template: {reason}")),
            Err(e) => return Err(format!("Could not create a block template: {e}")),
        };
        if answer.difficulty == 0 {
            return Err("The chain reported a zero difficulty".to_string());
        }
        let mut block =
            BlockTemplate::from_bytes(&answer.blob).map_err(|e| format!("Could not read the block template: {e}"))?;
        seal_merge_mining_tag(&mut block)?;
        let blob = block.pow_input().map_err(|e| format!("Could not build the hashing blob: {e}"))?;
        let job = Job {
            id: self.next_id().to_string(),
            block,
            difficulty: answer.difficulty,
            height: answer.height,
            blob_hex: hex::encode(blob),
            seen_nonces: HashSet::new(),
        };
        let mut st = client.state();
        st.jobs.push_back(job);
        while st.jobs.len() > JOB_HISTORY {
            st.jobs.pop_front();
        }
        Ok(describe_job(&st, self.share_difficulty))
    }

    fn send_job(&self, client: &Arc<Client>) {
        let _issuing = client.issuing();
        if !client.state().logged_in || client.is_closing() {
            return;
        }
        let job = match self.refresh_job(client) {
            Ok(job) => job,
            Err(e) => {
                log_warn!("could not hand {} a new job: {e}", client.peer);
                return;
            }
        };
        let mut notification = Obj::new();
        notification.set("jsonrpc", "2.0").set("method", "job").set("params", job);
        self.send(client, notification.build());
    }

    fn broadcast_jobs(&self) {
        if !(self.ready)() {
            return;
        }
        // A copy: refreshing reaches into the chain, and a connection may drop
        // part way through.
        let clients: Vec<Arc<Client>> = self.clients().clone();
        for client in &clients {
            self.send_job(client);
        }
    }

    fn send(&self, client: &Arc<Client>, payload: Json) {
        if client.is_closing() {
            return;
        }
        let mut line = payload.to_string();
        line.push('\n');
        match client.outbox.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                log_warn!("stratum client {} has stopped reading; closing it", client.peer);
                client.close_now();
            }
            Err(TrySendError::Disconnected(_)) => client.close_now(),
        }
    }

    fn reply_result(&self, client: &Arc<Client>, request: &Json, result: Json) {
        let mut response = Obj::new();
        response
            .set("id", request.get("id").cloned().unwrap_or(Json::Null))
            .set("jsonrpc", "2.0")
            .set("error", Json::Null)
            .set("result", result);
        self.send(client, response.build());
    }

    fn reply_ok(&self, client: &Arc<Client>, request: &Json) {
        let mut result = Obj::new();
        result.set("status", "OK");
        self.reply_result(client, request, result.build());
    }

    fn reply_error(&self, client: &Arc<Client>, request: &Json, message: &str) {
        let mut error = Obj::new();
        error.set("code", -1i64).set("message", message);
        let mut response = Obj::new();
        response
            .set("id", request.get("id").cloned().unwrap_or(Json::Null))
            .set("jsonrpc", "2.0")
            .set("error", error.build())
            .set("result", Json::Null);
        self.send(client, response.build());
    }

    fn drop_client(&self, client: &Arc<Client>) {
        client.close_after_flush();
        self.clients().retain(|c| !Arc::ptr_eq(c, client));
        let st = client.state();
        if st.logged_in {
            log_info!(
                "stratum miner {} disconnected after {} share(s) and {} block(s)",
                client.peer,
                st.accepted_shares,
                st.found_blocks
            );
        }
    }
}

/// Send what is queued, in order; once the connection is closing and the
/// queue is empty, close the socket.
fn write_loop(client: &Arc<Client>, mut writer: TcpStream, queued: Receiver<String>) {
    loop {
        match queued.recv_timeout(WRITER_POLL) {
            Ok(line) => {
                if let Err(e) = writer.write_all(line.as_bytes()) {
                    log_debug!("stratum client {} write ended: {e}", client.peer);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if client.is_closing() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    client.close_now();
}

/// Write the merge-mining tag the daemon template leaves as a placeholder
/// (`StratumServer.cpp:775-814`; what the bundled miner's
/// `adjustMergeMiningTag` does).
///
/// Skipped, the proof-of-work check recomputes the auxiliary root, finds it
/// does not match the zeroed tag, and throws the block out as "Proof of work
/// is too weak" — a thoroughly misleading way to say the commitment was never
/// written. It has to happen before the hashing blob is built: the tag lives in
/// the parent coinbase, whose hash is part of what is hashed. The auxiliary
/// hash covers only the outer header, the miner transaction and the transaction
/// hashes, so writing the tag cannot change what it commits to.
pub fn seal_merge_mining_tag(block: &mut BlockTemplate) -> Result<(), String> {
    if block.major_version < BLOCK_MAJOR_VERSION_2 {
        return Ok(());
    }
    let aux = block.auxiliary_header_hash().map_err(|e| format!("Could not write the merge mining tag: {e}"))?;
    let pb = block.parent_block.as_ref().ok_or("Could not write the merge mining tag: no parent block")?;
    let mut extra = Vec::new();
    append_merge_mining_tag(&mut extra, &MergeMiningTag { depth: 0, merkle_root: aux });
    let prefix = &pb.base_transaction().prefix;
    let coinbase = BaseTransaction {
        prefix: TransactionPrefix {
            version: prefix.version,
            unlock_time: prefix.unlock_time,
            inputs: prefix.inputs.clone(),
            outputs: prefix.outputs.clone(),
            extra,
        },
    };
    block.parent_block = Some(ParentBlock::new(
        pb.major_version,
        pb.minor_version,
        pb.previous_block_hash,
        pb.transaction_count,
        pb.base_transaction_branch.clone(),
        coinbase,
        pb.blockchain_branch.clone(),
    ));
    Ok(())
}

/// `HASHING_ALGORITHMS_BY_BLOCK_VERSION` in the spelling miners use
/// (`algorithmName`). Only the current fork is reachable in practice.
pub fn algorithm_name(major_version: u8) -> &'static str {
    match major_version {
        4 => "cn-lite/1",
        5 => "cn-pico/trtl",
        6 => "argon2/chukwa",
        7 => "cn/upx2",
        _ => "cn/0",
    }
}

/// The target a miner compares the top eight bytes of its hash against, read
/// little endian (`encodeTarget`). `check_hash` still decides a share; the
/// target only sets how often the miner bothers to send one.
pub fn encode_target(difficulty: u64) -> String {
    let target = if difficulty <= 1 { u64::MAX } else { u64::MAX / difficulty };
    hex::encode(target.to_le_bytes())
}

/// The `job` object: what the miner hashes and how hard.
fn describe_job(st: &ClientState, share_difficulty: u64) -> Json {
    let Some(job) = st.jobs.back() else { return Json::Object(Vec::new()) };
    let mut o = Obj::new();
    o.set("blob", job.blob_hex.as_str())
        .set("job_id", job.id.as_str())
        .set("target", encode_target(if share_difficulty == 0 { job.difficulty } else { share_difficulty }))
        .set("height", job.height)
        .set("algo", algorithm_name(job.block.major_version));
    o.build()
}

/// `params`, when it is an object.
fn params_of(request: &Json) -> &Json {
    const EMPTY: &Json = &Json::Null;
    request.get("params").filter(|p| p.is_object()).unwrap_or(EMPTY)
}

/// A string member, or empty when it is missing or not a string.
fn string_of(object: &Json, key: &str) -> String {
    object.get(key).and_then(Json::as_str).unwrap_or_default().to_string()
}

/// Eight hex digits, the nonce's four bytes as the miner wrote them into the
/// blob, read little endian as the block stores it.
fn parse_nonce(hex_nonce: &str) -> Option<u32> {
    let bytes: [u8; 4] = hex::decode(hex_nonce).ok()?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_primitives::tx::{Input, Transaction};

    /// A v7 block in the daemon template's shape: the placeholder tag zeroed.
    fn daemon_template() -> BlockTemplate {
        BlockTemplate {
            major_version: 7,
            minor_version: 0,
            timestamp: 1_800_000_000,
            previous_block_hash: [7; 32],
            nonce: 0,
            parent_block: Some(wrkz_mempool::template_builder::daemon_template_parent_block()),
            base_transaction: Transaction {
                prefix: TransactionPrefix {
                    version: 1,
                    unlock_time: 4_400_040,
                    inputs: vec![Input::Base { block_index: 4_400_000 }],
                    outputs: Vec::new(),
                    extra: vec![0x01; 33],
                },
                signatures: Vec::new(),
            },
            transaction_hashes: vec![[9; 32]],
        }
    }

    #[test]
    fn an_unsealed_template_fails_the_commitment_and_a_sealed_one_passes_it() {
        // A zero hash clears any difficulty, so only the commitment decides.
        let unsealed = daemon_template();
        assert!(!unsealed.check_proof_of_work_with(&[0; 32], 1).unwrap(), "the zeroed tag commits to nothing");
        let mut sealed = daemon_template();
        seal_merge_mining_tag(&mut sealed).unwrap();
        assert!(sealed.check_proof_of_work_with(&[0; 32], 1).unwrap());
        // The block itself is unchanged: same id inputs, same header.
        assert_eq!(sealed.auxiliary_header_hash().unwrap(), unsealed.auxiliary_header_hash().unwrap());
        // And it survives the trip through a block blob.
        let again = BlockTemplate::from_bytes(&sealed.to_bytes().unwrap()).unwrap();
        assert!(again.check_proof_of_work_with(&[0; 32], 1).unwrap());
    }

    #[test]
    fn the_nonce_sits_at_offset_39_of_the_hashing_blob() {
        // Where every stratum miner writes it: the parent block's varint major
        // and minor, a five-byte varint timestamp, the 32-byte previous hash.
        let mut block = daemon_template();
        seal_merge_mining_tag(&mut block).unwrap();
        block.nonce = 0x1234_5678;
        let blob = block.pow_input().unwrap();
        assert_eq!(&blob[39..43], &0x1234_5678u32.to_le_bytes());
        assert_eq!(parse_nonce(&hex::encode(&blob[39..43])), Some(0x1234_5678));
    }

    #[test]
    fn a_v1_template_needs_no_tag_and_keeps_its_nonce_at_39_too() {
        let mut block = daemon_template();
        block.major_version = 1;
        block.parent_block = None;
        let before = block.clone();
        seal_merge_mining_tag(&mut block).unwrap();
        assert_eq!(block, before);
        block.nonce = 0xdead_beef;
        assert_eq!(&block.pow_input().unwrap()[39..43], &0xdead_beefu32.to_le_bytes());
    }

    #[test]
    fn targets_and_algorithms_are_the_cpps() {
        assert_eq!(encode_target(0), "ffffffffffffffff");
        assert_eq!(encode_target(1), "ffffffffffffffff");
        assert_eq!(encode_target(2), "ffffffffffffff7f");
        assert_eq!(encode_target(u64::MAX), "0100000000000000");
        assert_eq!(algorithm_name(7), "cn/upx2");
        assert_eq!(algorithm_name(6), "argon2/chukwa");
        assert_eq!(algorithm_name(1), "cn/0");
    }

    #[test]
    fn nonces_are_four_bytes_of_hex() {
        assert_eq!(parse_nonce("01000000"), Some(1));
        assert_eq!(parse_nonce("0100000"), None);
        assert_eq!(parse_nonce("0100000000"), None);
        assert_eq!(parse_nonce("zz000000"), None);
    }
}
