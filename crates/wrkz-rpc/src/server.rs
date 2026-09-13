// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The listener, the worker pool and the middleware — `RpcServer::setupRoutes`
//! (`RpcServer.cpp:173`) and `RpcServer::middleware` (`RpcServer.cpp:507`).
//!
//! No async runtime and no web framework: a `TcpListener` on an acceptor
//! thread, a bounded queue, and a fixed set of worker threads. The properties
//! that matter for something listening on a socket:
//!
//! - **Bounded workers.** [`ServerConfig::workers`] threads, no more, whatever
//!   arrives. The C++ pool is `max(32, cores·2)` growing to eight times that
//!   (`RpcServer.cpp:41`); ours does not grow, because a fixed pool plus a
//!   bounded queue is the property we want to be able to state.
//! - **Bounded queue.** [`ServerConfig::queue_capacity`] connections may wait.
//!   Past that the acceptor answers `503` and closes, rather than growing a
//!   backlog an attacker chooses the size of.
//! - **Bounded per client.** One address may hold at most
//!   [`ServerConfig::max_connections_per_ip`] connections, queued or served;
//!   the next is answered `429` by the acceptor before any worker sees it.
//! - **Timeouts on every socket operation, and deadlines on every request.**
//!   Read and write timeouts are set before the first byte, so a client that
//!   opens a connection and stalls costs one worker for one timeout; and the
//!   head and the body each have a deadline for the whole of them
//!   ([`http::read_request_timed`]), so one that trickles a byte at a time
//!   costs no more.
//! - **Bounded request.** The request line, the headers and the body all have
//!   caps ([`crate::http::HttpLimits`]), and nothing is allocated from a
//!   length the client declared.
//! - **Keep-alive is optional and bounded** ([`ServerConfig::keep_alive`]):
//!   at most [`ServerConfig::keep_alive_max`] requests and an idle deadline,
//!   so an idle connection cannot hold a worker indefinitely.
//!
//! Everything the middleware does, it does in the C++ order: CORS header,
//! `Content-Type`, body size, access token, rate limit, body parse, route
//! permission, sync gate, handler.

use crate::api::NodeApi;
use crate::console::ConsoleSlot;
use crate::handlers;
use crate::http::{self, DeadlineStream, HttpError, HttpLimits, ReadTimeout, Request, Response};
use crate::jsonrpc;
use crate::sync_cache::SyncCache;
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, SocketAddrV6, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `RpcMode` (`RpcServer.h:36`). `Explorer` is a superset of `Standard`, and a
/// route asking for more than the daemon runs with gets the 403 of
/// `RpcServer.cpp:600`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum RpcMode {
    #[default]
    Standard,
    Explorer,
}

/// How the server behaves. Defaults are the C++ daemon's defaults.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// `--rpc-bind-ip` / `--rpc-bind-port`.
    pub bind: String,
    /// `--rpc-bind-ipv6-address`, already joined with the RPC port as
    /// `[addr]:port`. Empty means no IPv6 listener, which is the C++ default:
    /// `m_ipv6Host` is empty unless an address was given
    /// (`RpcServer.cpp:97`), and only then is `listenIpv6` started
    /// (`RpcServer.cpp:374`).
    ///
    /// The second listener serves the **same** routes off the **same** worker
    /// pool and the same [`Context`], so every limit in this struct — workers,
    /// queue capacity, body caps, the access token, the rate limit — counts
    /// both families together.
    pub bind_ipv6: String,
    /// Worker threads. Bounded and fixed.
    pub workers: usize,
    /// Connections that may wait for a worker before the acceptor sheds load.
    pub queue_capacity: usize,
    /// Connections one client address may hold open at once, waiting or being
    /// served. Past it the acceptor answers `429` and closes, before a worker
    /// is involved, so no single address can take the whole pool. Loopback is
    /// exempt, and so is everyone when [`Self::trust_proxy`] is set, because
    /// then every connection arrives from the proxy. `0` disables the cap.
    /// `cpp-httplib` has no such limit; 8 is a pool's or an explorer
    /// back end's handful of parallel requests with room to spare, and half
    /// the default worker count.
    pub max_connections_per_ip: usize,
    /// Caps on the request itself.
    pub limits: HttpLimits,
    /// Deepest JSON nesting a body may use.
    pub max_json_depth: usize,
    /// `--rpc-read-timeout`, at least 1 second (`RpcServer.cpp:99`).
    pub read_timeout: Duration,
    /// `--rpc-write-timeout`.
    pub write_timeout: Duration,
    /// Whether to honour `Connection: keep-alive`.
    pub keep_alive: bool,
    /// `set_keep_alive_timeout(3)` (`RpcServer.cpp:55`).
    pub keep_alive_timeout: Duration,
    /// `set_keep_alive_max_count(1000)`.
    pub keep_alive_max: u32,
    /// `--rpc-max-requests-per-minute`, 240 by default; 0 disables the limit.
    pub max_requests_per_minute: u32,
    /// `--rpc-access-token`. Empty means no token is required.
    pub access_token: String,
    /// `--enable-cors <origin>`. Empty means no CORS headers at all.
    pub cors_header: String,
    /// `--daemon-mode`.
    pub mode: RpcMode,
    /// `--rpc-max-block-count`, at least 1 (`RpcServer.cpp:104`).
    pub max_block_count: u64,
    /// `--rpc-max-global-index-range`, at least 100 (`RpcServer.cpp:103`).
    pub max_global_index_range: u64,
    /// `--rpc-trust-proxy`: read the client IP from `X-Forwarded-For`.
    pub trust_proxy: bool,
    /// What `/info` reports as `compression`, and whether responses are
    /// compressed. `"gzip"`, the default, is what a C++ node built with zlib
    /// reports (`RpcServer.cpp:66`) and compresses JSON bodies for clients
    /// that accept it ([`http::gzip_if_accepted`]); `"none"` never compresses.
    pub compression: String,
    /// `--rpc-sync-cache-size`, in bytes: finished `/getwalletsyncdata` bodies
    /// kept for the next wallet asking for the same range
    /// ([`crate::sync_cache`]). `0` disables the cache.
    pub sync_cache_bytes: usize,
    /// `--rpc-stream-threshold`, in bytes: a compressible body at least this
    /// large goes out compressed straight into the socket, framed with
    /// `Transfer-Encoding: chunked`, instead of being compressed into a second
    /// buffer first ([`http::write_response_streamed`]). It saves a worker the
    /// compressed copy of a large answer; it costs byte-identical framing with
    /// the C++ daemon, which always sends `Content-Length`. `0`, the default,
    /// turns it off, and then every response is framed as the C++ frames it.
    pub stream_threshold_bytes: usize,
    /// `--enable-metrics`: serve `GET /metrics` ([`crate::metrics`]). Off by
    /// default, and then that path is the 404 of any unrouted one.
    pub metrics: bool,
    /// `--enable-health`: serve `GET /health` ([`crate::metrics::health`]),
    /// 200 when synced and 503 before. Off by default, like `metrics`.
    pub health: bool,
    /// `--rpc-ipc-path`: also serve the RPC on a local socket at this path,
    /// or `@name` in Linux's abstract namespace ([`crate::ipc`]). Empty means
    /// no IPC listener, the C++ default.
    pub ipc_path: String,
    /// `--rpc-ipc-mode`: the socket file's permissions, owner only by default.
    pub ipc_mode: u32,
    /// `--rpc-ipc-group`: the group to own the socket file. Empty keeps the
    /// daemon user's primary group.
    pub ipc_group: String,
    /// `--rpc-ipc-require-token`: demand `access_token` from IPC callers too.
    pub ipc_require_token: bool,
    /// What `/info` reports as `version`.
    pub version: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:17856".into(),
            bind_ipv6: String::new(),
            workers: 16,
            queue_capacity: 128,
            max_connections_per_ip: 8,
            limits: HttpLimits::default(),
            max_json_depth: crate::json::DEFAULT_MAX_DEPTH,
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            keep_alive: true,
            keep_alive_timeout: Duration::from_secs(3),
            keep_alive_max: 1000,
            max_requests_per_minute: 240,
            access_token: String::new(),
            cors_header: String::new(),
            mode: RpcMode::Standard,
            max_block_count: 1000,
            max_global_index_range: 5000,
            trust_proxy: false,
            compression: "gzip".into(),
            sync_cache_bytes: crate::sync_cache::DEFAULT_SYNC_CACHE_BYTES,
            // Off: the C++ frames every response with `Content-Length`.
            stream_threshold_bytes: 0,
            metrics: false,
            health: false,
            ipc_path: String::new(),
            ipc_mode: crate::ipc::DEFAULT_MODE,
            ipc_group: String::new(),
            ipc_require_token: false,
            version: crate::DAEMON_VERSION.into(),
        }
    }
}

/// A per-IP request counter over a one-minute window
/// (`RpcServer::isRateLimited`, `RpcServer.cpp:805`).
#[derive(Default)]
struct RateLimiter {
    window_start: u64,
    by_ip: HashMap<String, u32>,
}

impl RateLimiter {
    /// True when this request is over the limit. The whole map is dropped when
    /// the window rolls over, so it cannot grow without bound.
    fn check(&mut self, ip: &str, limit: u32, now: u64) -> bool {
        if limit == 0 {
            return false;
        }
        let window_start = now - (now % 60);
        if window_start != self.window_start {
            self.by_ip.clear();
            self.window_start = window_start;
        }
        let count = self.by_ip.entry(ip.to_string()).or_insert(0);
        if *count >= limit {
            return true;
        }
        *count += 1;
        false
    }
}

/// Shared, immutable-after-start state every worker reads.
pub struct Context {
    pub api: Arc<dyn NodeApi>,
    pub config: ServerConfig,
    limiter: Mutex<RateLimiter>,
    /// Finished `/getwalletsyncdata` bodies, shared by every worker.
    pub sync_cache: SyncCache,
    /// What the workers have answered, for `/metrics`.
    pub counters: crate::metrics::Counters,
    /// The daemon's console, once it has installed one, for `POST /console`
    /// on the IPC socket ([`crate::console`]).
    pub console: ConsoleSlot,
}

impl Context {
    pub fn new(api: Arc<dyn NodeApi>, config: ServerConfig) -> Self {
        let sync_cache = SyncCache::new(config.sync_cache_bytes);
        Self {
            api,
            config,
            limiter: Mutex::new(RateLimiter::default()),
            sync_cache,
            counters: crate::metrics::Counters::default(),
            console: ConsoleSlot::default(),
        }
    }

    fn now(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

/// What a route needs before its handler runs (`RpcServer.cpp:176`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Route {
    kind: RouteKind,
    permissions: RpcMode,
    body_required: bool,
    sync_required: bool,
    /// Registered on the IPC server only (`RpcServer.cpp:323`): anyone else
    /// gets the 404 of a path that is not routed.
    ipc_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteKind {
    Info,
    Height,
    Peers,
    JsonRpc,
    SendTransaction,
    GetRandomOuts,
    GetWalletSyncData,
    GetGlobalIndexes,
    QueryBlocksLite,
    GetTransactionsStatus,
    GetPoolChanges,
    QueryBlocksDetailed,
    GetGlobalIndexesDeprecated,
    GetRawBlocks,
    Console,
}

/// `srv.Get(...)` / `srv.Post(...)` (`RpcServer.cpp:289-316`), verbatim and in
/// order. Anything not here is a 404, exactly as the C++ leaves it — including
/// the endpoints other CryptoNote daemons serve (`/fee`, `/getpeers`,
/// `/getblocks`, `/gettransactions`, `/queryblocks`, `/get_pool_changes`), which
/// this daemon does not route. See the crate docs, "Routes the C++ does not
/// have". `POST /console` is in the table for the IPC socket alone
/// (`RpcServer.cpp:323-326`).
fn route(method: &str, path: &str) -> Option<Route> {
    use RouteKind::*;
    const STD: RpcMode = RpcMode::Standard;
    const EXP: RpcMode = RpcMode::Explorer;
    let r = |kind, permissions, body_required, sync_required| {
        Some(Route { kind, permissions, body_required, sync_required, ipc_only: false })
    };
    match (method, path) {
        ("GET", "/json_rpc") => r(JsonRpc, STD, true, false),
        ("GET", "/info") => r(Info, STD, false, false),
        ("GET", "/height") => r(Height, STD, false, false),
        ("GET", "/peers") => r(Peers, STD, false, false),
        // Monero-lineage solo miners poll these two spellings
        // (`RpcServer.cpp:294`). `/getheight` must not carry a `hash` member:
        // xmrig reads its absence as "this is a CryptoNote daemon".
        ("GET", "/getinfo") => r(Info, STD, false, false),
        ("GET", "/getheight") => r(Height, STD, false, false),
        ("POST", "/json_rpc") => r(JsonRpc, STD, true, false),
        ("POST", "/sendrawtransaction") => r(SendTransaction, STD, true, true),
        ("POST", "/getrandom_outs") => r(GetRandomOuts, STD, true, false),
        ("POST", "/getwalletsyncdata") => r(GetWalletSyncData, STD, true, false),
        ("POST", "/get_global_indexes_for_range") => r(GetGlobalIndexes, STD, true, false),
        ("POST", "/queryblockslite") => r(QueryBlocksLite, STD, true, false),
        ("POST", "/get_transactions_status") => r(GetTransactionsStatus, STD, true, false),
        ("POST", "/get_pool_changes_lite") => r(GetPoolChanges, STD, true, false),
        ("POST", "/queryblocksdetailed") => r(QueryBlocksDetailed, EXP, true, false),
        ("POST", "/get_o_indexes") => r(GetGlobalIndexesDeprecated, STD, true, false),
        ("POST", "/getrawblocks") => r(GetRawBlocks, STD, true, false),
        ("POST", "/console") => {
            Some(Route { kind: Console, permissions: STD, body_required: true, sync_required: false, ipc_only: true })
        }
        _ => None,
    }
}

/// `RpcServer::handleOptions` (`RpcServer.cpp:864`): `OPTIONS` on any path,
/// deliberately *not* through the middleware.
fn handle_options(ctx: &Context, req: &Request) -> Response {
    let supported = if ctx.config.cors_header.is_empty() { "" } else { "OPTIONS, GET, POST" };
    let mut res = Response::new(200);
    if req.header("Access-Control-Request-Method").is_some() {
        res.set_header("Access-Control-Allow-Methods", supported);
    } else {
        res.set_header("Allow", supported);
    }
    if !ctx.config.cors_header.is_empty() {
        res.set_header("Access-Control-Allow-Origin", &ctx.config.cors_header);
        res.set_header("Access-Control-Allow-Headers", "Origin, X-Requested-With, Content-Type, Accept");
    }
    res
}

/// `RpcServer::getClientIp` (`RpcServer.cpp:670`).
///
/// The first element of `X-Forwarded-For` is trimmed: the header is written
/// `client, proxy1, proxy2` with spaces, and an untrimmed value would miss both
/// the rate-limit exemption below and any later comparison.
fn client_ip(ctx: &Context, req: &Request, peer: &str) -> String {
    if !ctx.config.trust_proxy {
        return peer.to_string();
    }
    match req.header("X-Forwarded-For") {
        None => peer.to_string(),
        Some(forwarded) => match forwarded.split_once(',') {
            Some((first, _)) => first.trim().to_string(),
            None => forwarded.trim().to_string(),
        },
    }
}

/// Whether an address *string* is the loopback of either family.
///
/// The C++ compares against `"127.0.0.1"` and `"::1"` as strings
/// (`RpcServer.cpp:570`), which is exactly right for the two spellings a socket
/// hands back and wrong for everything else a client or a proxy can write. This
/// parses instead, so `::1`, the equivalent `0:0:0:0:0:0:0:1`, the whole of
/// `127.0.0.0/8`, and an IPv4-mapped `::ffff:127.0.0.1` (which is what a
/// dual-stack socket would report, and what a proxy may put in
/// `X-Forwarded-For` even though our own listeners are single-family) are all
/// recognised.
pub fn is_loopback_ip(ip: &str) -> bool {
    // A bracketed literal, or one carrying a zone or a port, still has to be
    // recognised: `[::1]`, `::1%lo0`.
    let text = ip.trim();
    let text = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
    let text = text.split('%').next().unwrap_or(text);
    text.parse::<IpAddr>().is_ok_and(is_loopback_addr)
}

/// [`is_loopback_ip`] for an address already parsed.
fn is_loopback_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback(),
        IpAddr::V6(a) => a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// Whether two byte strings are equal, looking at every byte whatever the
/// first difference: no early exit for a timing side channel to measure. The
/// lengths are compared openly; [`token_matches`] hashes first so that they
/// are always equal.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// The access-token comparison. The C++ compares with `!=`
/// (`RpcServer.cpp:560`), which stops at the first differing byte and so tells
/// a patient caller, by timing, how much of a guess was right. Both sides are
/// hashed to 32 bytes first and the digests compared in constant time, which
/// also keeps the token's length out of the timing.
pub fn token_matches(provided: &str, expected: &str) -> bool {
    constant_time_eq(&wrkz_pow::cn_fast_hash(provided.as_bytes()), &wrkz_pow::cn_fast_hash(expected.as_bytes()))
}

/// `RpcServer::middleware` plus the route table: one request in, one response
/// out, with no socket involved. This is what the offline tests drive.
pub fn dispatch(ctx: &Context, req: &Request, peer: &str) -> Response {
    let mut res = if req.method == "OPTIONS" { handle_options(ctx, req) } else { dispatch_routed(ctx, req, peer) };
    // `res.set_header("Access-Control-Allow-Origin", …)` happens before
    // anything can fail (`RpcServer.cpp:529`), so even a 401 carries it.
    if !ctx.config.cors_header.is_empty() && res.header("Access-Control-Allow-Origin").is_none() {
        res.set_header("Access-Control-Allow-Origin", &ctx.config.cors_header);
    }
    res
}

/// The first two steps of the middleware: the body cap and the access token
/// (`RpcServer.cpp:536-560`). `/json_rpc` runs these before it parses its
/// body; everything else runs them as part of [`gate_transport`].
pub(crate) fn gate_auth(ctx: &Context, req: &Request, peer: &str) -> Option<Response> {
    // A caller on the IPC socket already passed the socket file's
    // permissions, which the kernel enforced (`RpcServer.cpp:543-573`).
    let ipc = peer == crate::ipc::IPC_PEER;

    // `RpcServer.cpp:536`: the body cap is checked before anything else.
    if req.body.len() > ctx.config.limits.max_body {
        return Some(handlers::fail_request(413, "RPC request body too large"));
    }

    // `RpcServer.cpp:548`: `X-API-Key`, else `Authorization: Bearer` — not on
    // the IPC socket unless `--rpc-ipc-require-token` asks for it.
    if !ctx.config.access_token.is_empty() && (!ipc || ctx.config.ipc_require_token) {
        let provided = match req.header("X-API-Key") {
            Some(t) if !t.is_empty() => t,
            _ => match req.header("Authorization") {
                Some(a) if a.starts_with("Bearer ") => &a["Bearer ".len()..],
                _ => "",
            },
        };
        if !token_matches(provided, &ctx.config.access_token) {
            return Some(handlers::fail_request(401, "Unauthorized RPC request"));
        }
    }
    None
}

/// The first half of the middleware: the body cap, the access token and the
/// rate limit (`RpcServer.cpp:536-580`), in that order.
pub(crate) fn gate_transport(ctx: &Context, req: &Request, peer: &str) -> Option<Response> {
    if let Some(early) = gate_auth(ctx, req, peer) {
        return Some(early);
    }
    let ipc = peer == crate::ipc::IPC_PEER;
    let ip = if ipc { String::new() } else { client_ip(ctx, req, peer) };

    // `RpcServer.cpp:570`: loopback is exempt from the rate limit — either
    // family, so a client on `::1` is treated exactly as one on `127.0.0.1`.
    if !ip.is_empty() && !is_loopback_ip(&ip) {
        let now = ctx.now();
        let limited = match ctx.limiter.lock() {
            Ok(mut l) => l.check(&ip, ctx.config.max_requests_per_minute, now),
            // A poisoned limiter must not open the gate.
            Err(_) => true,
        };
        if limited {
            return Some(handlers::fail_request(429, "Too many RPC requests, please retry later"));
        }
    }
    None
}

/// The second half: the route's permissions and the sync gate
/// (`RpcServer.cpp:600` and `:576`), which the C++ applies *after* the body
/// has been parsed.
pub(crate) fn gate_route(ctx: &Context, permissions: RpcMode, sync_required: bool) -> Option<Response> {
    if permissions > ctx.config.mode {
        return Some(handlers::fail_request(
            403,
            "You do not have permission to access this method. Please relaunch your daemon with \
             --daemon-mode explorer to access explorer RPC methods.",
        ));
    }
    if sync_required && !ctx.api.is_synced() {
        return Some(handlers::fail_request(
            503,
            "Daemon must be synced to process this RPC method call, please retry when synced",
        ));
    }
    None
}

/// Both halves, for a `/json_rpc` method whose body was parsed before the
/// middleware ever ran.
pub(crate) fn gate(
    ctx: &Context,
    req: &Request,
    peer: &str,
    permissions: RpcMode,
    sync_required: bool,
) -> Option<Response> {
    gate_transport(ctx, req, peer).or_else(|| gate_route(ctx, permissions, sync_required))
}

fn dispatch_routed(ctx: &Context, req: &Request, peer: &str) -> Response {
    // This port's own route, and only when asked for: off, it is the 404 of
    // any path the C++ table does not have. The same transport gate as every
    // route — token and rate limit — applies.
    if ctx.config.metrics && req.method == "GET" && req.path == "/metrics" {
        if let Some(early) = gate_transport(ctx, req, peer) {
            return early;
        }
        return crate::metrics::render(ctx.api.as_ref(), &ctx.counters, &ctx.sync_cache);
    }
    if ctx.config.health && req.method == "GET" && req.path == "/health" {
        if let Some(early) = gate_transport(ctx, req, peer) {
            return early;
        }
        return crate::metrics::health(ctx.api.as_ref());
    }
    let Some(route) = route(&req.method, &req.path) else {
        // `cpp-httplib` answers 404 for an unrouted path, with an empty body.
        return Response::new(404);
    };
    // The transport's own peer name, never `X-Forwarded-For`: only a
    // connection accepted on the IPC socket carries it.
    if route.ipc_only && peer != crate::ipc::IPC_PEER {
        return Response::new(404);
    }

    // `/json_rpc` parses its own body and picks its own route *before* the
    // middleware runs (`RpcServer.cpp:194`), so it owns the whole sequence.
    if route.kind == RouteKind::JsonRpc {
        return jsonrpc::handle(ctx, req, peer);
    }

    if let Some(early) = gate_transport(ctx, req, peer) {
        return early;
    }

    // `RpcServer::getJsonBody` (`RpcServer.cpp:733`): a route that needs no
    // body gets an empty object, and a body that is not JSON is a 400 with the
    // C++'s two-part message.
    let body = if route.body_required {
        match crate::json::parse(
            &req.body,
            crate::json::ParseLimits { max_bytes: ctx.config.limits.max_body, max_depth: ctx.config.max_json_depth },
        ) {
            Ok(v) => v,
            Err(_) => return handlers::fail_json_body(&req.body),
        }
    } else {
        crate::json::Json::Object(Vec::new())
    };

    if let Some(early) = gate_route(ctx, route.permissions, route.sync_required) {
        return early;
    }

    let api = ctx.api.as_ref();
    match route.kind {
        RouteKind::Info => handlers::info(api, &ctx.config),
        RouteKind::Height => handlers::height(api),
        RouteKind::Peers => handlers::peers(api),
        // Handled above.
        RouteKind::JsonRpc => Response::new(404),
        RouteKind::SendTransaction => handlers::send_transaction(api, &body),
        RouteKind::GetRandomOuts => handlers::get_random_outs(api, &body),
        RouteKind::GetWalletSyncData => handlers::get_wallet_sync_data(api, &ctx.config, &body, Some(&ctx.sync_cache)),
        RouteKind::GetGlobalIndexes => handlers::get_global_indexes(api, &ctx.config, &body),
        RouteKind::QueryBlocksLite => handlers::query_blocks_lite(api, &body),
        RouteKind::GetTransactionsStatus => handlers::get_transactions_status(api, &body),
        RouteKind::GetPoolChanges => handlers::get_pool_changes(api, &body),
        RouteKind::QueryBlocksDetailed => handlers::query_blocks_detailed(api, &body),
        RouteKind::GetGlobalIndexesDeprecated => handlers::get_global_indexes_deprecated(api, &body),
        RouteKind::GetRawBlocks => handlers::get_raw_blocks(api, &ctx.config, &body),
        RouteKind::Console => crate::console::handle(&ctx.console, &body),
    }
}

// ---------------------------------------------------------------------------
// the listener
// ---------------------------------------------------------------------------

/// One accepted connection: TCP (either family), or the IPC socket.
enum Conn {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Conn {
    /// The address the middleware sees: the client IP, or
    /// [`crate::ipc::IPC_PEER`].
    fn peer(&self) -> String {
        match self {
            Conn::Tcp(s) => s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default(),
            #[cfg(unix)]
            Conn::Unix(_) => crate::ipc::IPC_PEER.to_string(),
        }
    }

    fn set_nodelay(&self) {
        match self {
            Conn::Tcp(s) => {
                let _ = s.set_nodelay(true);
            }
            #[cfg(unix)]
            Conn::Unix(_) => {}
        }
    }

    /// Turn the connection away with a pre-rendered answer, from the acceptor
    /// ([`http::shed_tcp`]).
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

/// Open connections per client address, from accept to close.
///
/// An entry exists only while its address has a connection open, and every
/// open connection is queued or held by a worker, so the map never holds more
/// than `workers + queue_capacity` entries.
#[derive(Default)]
struct ConnectionCounts {
    by_ip: Mutex<HashMap<IpAddr, usize>>,
}

impl ConnectionCounts {
    /// One more connection for `ip`, or `None` when it already has `cap`.
    fn acquire(self: &Arc<Self>, ip: IpAddr, cap: usize) -> Option<ConnectionSlot> {
        let mut by_ip = self.by_ip.lock().unwrap_or_else(|p| p.into_inner());
        let open = by_ip.entry(ip).or_insert(0);
        if *open >= cap {
            return None;
        }
        *open += 1;
        Some(ConnectionSlot { counts: Arc::clone(self), ip })
    }

    #[cfg(test)]
    fn open(&self, ip: IpAddr) -> usize {
        self.by_ip.lock().unwrap().get(&ip).copied().unwrap_or(0)
    }
}

/// One counted connection; dropping it — when the connection closes, or is
/// shed from a full queue — gives the slot back.
struct ConnectionSlot {
    counts: Arc<ConnectionCounts>,
    ip: IpAddr,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut by_ip = self.counts.by_ip.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(open) = by_ip.get_mut(&self.ip) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                by_ip.remove(&self.ip);
            }
        }
    }
}

/// What the acceptors need to turn a connection away before a worker sees
/// it: the per-address cap, and the two answers, rendered once at start.
struct Admission {
    /// `0` when the cap is off.
    per_ip: usize,
    counts: Arc<ConnectionCounts>,
    /// The `503` for a full queue.
    busy: Vec<u8>,
    /// The `429` for an address over its connection cap.
    crowded: Vec<u8>,
}

impl Admission {
    fn new(config: &ServerConfig) -> Self {
        let answer = |status, message| {
            let mut res = handlers::fail_request(status, message);
            if !config.cors_header.is_empty() {
                res.set_header("Access-Control-Allow-Origin", &config.cors_header);
            }
            http::render_closing(&res)
        };
        Self {
            // Behind a trusted proxy every connection has the proxy's address.
            per_ip: if config.trust_proxy { 0 } else { config.max_connections_per_ip },
            counts: Arc::new(ConnectionCounts::default()),
            busy: answer(503, "RPC server is busy, please retry shortly"),
            crowded: answer(429, "Too many concurrent RPC connections from this address"),
        }
    }

    /// `Ok(None)` for a connection that is not counted, `Ok(Some(slot))` for
    /// one that is, and `Err(())` for one over its address's cap.
    fn admit(&self, stream: &TcpStream) -> Result<Option<ConnectionSlot>, ()> {
        if self.per_ip == 0 {
            return Ok(None);
        }
        let Ok(addr) = stream.peer_addr() else { return Ok(None) };
        if is_loopback_addr(addr.ip()) {
            return Ok(None);
        }
        self.counts.acquire(addr.ip(), self.per_ip).map(Some).ok_or(())
    }
}

/// An accepted connection and, when its address is counted, its slot, which
/// lives exactly as long as the connection does.
struct Accepted {
    conn: Conn,
    slot: Option<ConnectionSlot>,
}

/// A bounded queue of accepted connections, drained by the worker threads.
struct Queue {
    inner: Mutex<Option<std::collections::VecDeque<Accepted>>>,
    ready: Condvar,
    capacity: usize,
}

impl Queue {
    fn new(capacity: usize) -> Self {
        Self { inner: Mutex::new(Some(std::collections::VecDeque::new())), ready: Condvar::new(), capacity }
    }

    /// The connection back when the queue is full (or closed) and the caller
    /// must shed it.
    fn push(&self, accepted: Accepted) -> Result<(), Accepted> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
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

    /// `None` once the queue is closed and drained.
    fn pop(&self) -> Option<Accepted> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        loop {
            let queue = guard.as_mut()?;
            if let Some(stream) = queue.pop_front() {
                return Some(stream);
            }
            guard = match self.ready.wait(guard) {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
        }
    }

    fn close(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(q) = guard.take() {
            for s in q {
                s.conn.shutdown();
            }
        }
        drop(guard);
        self.ready.notify_all();
    }
}

/// A running server. Dropping it stops every listener and joins every thread.
pub struct RunningServer {
    addr: SocketAddr,
    /// The IPv6 listener's address, when `bind_ipv6` was set.
    addr6: Option<SocketAddr>,
    stopping: Arc<AtomicBool>,
    queue: Arc<Queue>,
    threads: Vec<JoinHandle<()>>,
    /// The IPC socket, once it is bound: removed again on stop.
    ipc: Option<String>,
    /// Why the IPC listener did not come up, when one was asked for.
    ipc_error: Option<String>,
    /// The workers' [`Context::console`].
    console: ConsoleSlot,
}

impl RunningServer {
    /// Where the daemon installs its console for `POST /console`
    /// (`setConsoleExecutor`, `RpcServer.cpp:462`). Until it does, and after it
    /// clears it, the route answers 503.
    pub fn console(&self) -> ConsoleSlot {
        self.console.clone()
    }

    /// The IPC socket the RPC is also served on — `None` unless the bind
    /// actually succeeded, so nothing is ever pointed at a socket that was
    /// never created (`RpcServer::getIpcPath`).
    pub fn ipc_path(&self) -> Option<&str> {
        self.ipc.as_deref()
    }

    /// Why the IPC listener did not start, when `ipc_path` was set. The C++
    /// warns and keeps serving on the listeners that did come up; so does
    /// this — [`start`] does not fail over it.
    pub fn ipc_error(&self) -> Option<&str> {
        self.ipc_error.as_deref()
    }

    /// The address actually bound, which is how a test finds the port after
    /// asking for `127.0.0.1:0`.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The IPv6 address actually bound, or `None` when no IPv6 listener was
    /// configured.
    pub fn local_addr6(&self) -> Option<SocketAddr> {
        self.addr6
    }

    /// Stop accepting, drain, and join. Called by `Drop` as well.
    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Unblock each acceptor, which is parked in `accept()`.
        for addr in [Some(self.addr), self.addr6].into_iter().flatten() {
            if let Ok(s) = TcpStream::connect(addr) {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        #[cfg(unix)]
        if let Some(path) = &self.ipc {
            let _ = crate::ipc::connect(path);
        }
        self.queue.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Nothing else removes the socket file, and a leftover one would stand
        // in the next start's way.
        #[cfg(unix)]
        if let Some(path) = self.ipc.take() {
            crate::ipc::cleanup(&path);
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Parse `[addr]:port` (or a bare IPv6 literal, or a host that resolves to
/// one) into an IPv6 socket address. Anything that is not IPv6 is an error, so
/// `--rpc-bind-ipv6-address 127.0.0.1` fails at start-up rather than quietly
/// binding a second IPv4 listener.
fn parse_ipv6_bind(text: &str) -> std::io::Result<SocketAddrV6> {
    use std::net::ToSocketAddrs;
    let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_string());
    let addr = text
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| bad(&format!("the RPC IPv6 bind address {text} resolved to nothing")))?;
    match addr {
        SocketAddr::V6(v6) => Ok(v6),
        SocketAddr::V4(_) => Err(bad(&format!("the RPC IPv6 bind address {text} is not an IPv6 address"))),
    }
}

/// The accept loop, one per listener. Every accepted socket goes onto the
/// **one** bounded queue the worker pool drains, so the two families share the
/// workers, the queue capacity, the per-address counts and the load shedding.
fn spawn_acceptor(
    listener: TcpListener,
    queue: Arc<Queue>,
    stopping: Arc<AtomicBool>,
    admission: Arc<Admission>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let slot = match admission.admit(&stream) {
                Ok(slot) => slot,
                Err(()) => {
                    http::shed_tcp(&stream, &admission.crowded);
                    continue;
                }
            };
            if let Err(rejected) = queue.push(Accepted { conn: Conn::Tcp(stream), slot }) {
                // Shed load rather than grow an unbounded backlog — with the
                // `503` the module docs promise, not a silent close. The
                // write cannot block the accept loop ([`http::shed_tcp`]).
                rejected.conn.shed(&admission.busy);
            }
        }
        queue.close();
    })
}

/// The IPC socket's accept loop, onto the same queue and workers. Every
/// caller shares one "address", so there is no per-address cap here; the
/// socket file's permissions already decided who may connect at all.
#[cfg(unix)]
fn spawn_ipc_acceptor(
    listener: UnixListener,
    queue: Arc<Queue>,
    stopping: Arc<AtomicBool>,
    admission: Arc<Admission>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            // Shed on a full queue, as for TCP.
            if let Err(rejected) = queue.push(Accepted { conn: Conn::Unix(stream), slot: None }) {
                rejected.conn.shed(&admission.busy);
            }
        }
        queue.close();
    })
}

/// Bind and start serving. Returns as soon as the listener (or both listeners)
/// are up.
///
/// With [`ServerConfig::bind_ipv6`] set there is a second listener, bound
/// IPv6-only through [`wrkz_p2p::bind::bind_ipv6_only`] so it cannot take the
/// IPv4 wildcard on the same port — the C++ pins its IPv6 `httplib::Server`
/// the same way (`set_ipv6_v6only(true)`, `RpcServer.cpp:139`). It serves the
/// same routes from the same worker pool.
pub fn start(api: Arc<dyn NodeApi>, config: ServerConfig) -> std::io::Result<RunningServer> {
    let listener = TcpListener::bind(&config.bind)?;
    let addr = listener.local_addr()?;
    let listener6 = match config.bind_ipv6.as_str() {
        "" => None,
        text => Some(wrkz_p2p::bind::bind_ipv6_only(parse_ipv6_bind(text)?)?),
    };
    let addr6 = listener6.as_ref().map(|l| l.local_addr()).transpose()?;
    // The IPC socket is bound here, on the calling thread and before any of
    // this server's threads exist, because the bind narrows the process umask
    // for an instant (`ipc::bind`), as `Common::Ipc::bindServer` does. A
    // failure is reported, not fatal: TCP keeps serving (`RpcServer.cpp:360`).
    #[cfg(unix)]
    let (ipc_listener, ipc_error) = match config.ipc_path.as_str() {
        "" => (None, None),
        path => match crate::ipc::bind(path, config.ipc_mode, &config.ipc_group) {
            Ok(listener) => (Some(listener), None),
            Err(e) => (None, Some(format!("{}: {e}", crate::ipc::describe(path)))),
        },
    };
    #[cfg(not(unix))]
    let ipc_error = (!config.ipc_path.is_empty()).then(|| crate::ipc::UNSUPPORTED.to_string());
    let workers = config.workers.max(1);
    let queue = Arc::new(Queue::new(config.queue_capacity.max(1)));
    let admission = Arc::new(Admission::new(&config));
    let ctx = Arc::new(Context::new(api, config));
    let console = ctx.console.clone();
    let stopping = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::with_capacity(workers + 2);

    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let ctx = Arc::clone(&ctx);
        threads.push(std::thread::spawn(move || {
            while let Some(stream) = queue.pop() {
                serve_connection(&ctx, stream);
            }
        }));
    }

    if let Some(listener6) = listener6 {
        threads.insert(
            0,
            spawn_acceptor(listener6, Arc::clone(&queue), Arc::clone(&stopping), Arc::clone(&admission)),
        );
    }
    threads.insert(0, spawn_acceptor(listener, Arc::clone(&queue), Arc::clone(&stopping), Arc::clone(&admission)));
    #[cfg(unix)]
    let ipc = match ipc_listener {
        Some(listener) => {
            threads.insert(
                0,
                spawn_ipc_acceptor(listener, Arc::clone(&queue), Arc::clone(&stopping), Arc::clone(&admission)),
            );
            Some(ctx.config.ipc_path.clone())
        }
        None => None,
    };
    #[cfg(not(unix))]
    let ipc = None;

    Ok(RunningServer { addr, addr6, stopping, queue, threads, ipc, ipc_error, console })
}

/// One connection: read requests until the peer stops, the limits say no, or
/// keep-alive runs out.
fn serve_connection(ctx: &Arc<Context>, accepted: Accepted) {
    // `_held` keeps the address's connection slot until this returns.
    let Accepted { conn: stream, slot: _held } = accepted;
    let cfg = &ctx.config;
    let peer = stream.peer();
    stream.set_nodelay();
    let _ = stream.set_write_timeout(Some(cfg.write_timeout));
    let Ok(write_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(DeadlineStream::new(stream, cfg.read_timeout));
    let mut writer = write_half;
    let keep_alive_hint = format!("timeout={}, max={}", cfg.keep_alive_timeout.as_secs(), cfg.keep_alive_max);

    let mut served: u32 = 0;
    loop {
        // The first request has `--rpc-read-timeout` for its whole head. A
        // later one on a kept-alive connection may first sit idle — each read
        // waiting no longer than the keep-alive timeout, so an idle connection
        // releases its worker promptly — and has the same allowance on top.
        let head = if served == 0 {
            reader.get_mut().set_per_read(cfg.read_timeout);
            cfg.read_timeout
        } else {
            reader.get_mut().set_per_read(cfg.keep_alive_timeout);
            cfg.keep_alive_timeout + cfg.read_timeout
        };
        let request = match http::read_request_timed(&mut reader, &cfg.limits, head, cfg.read_timeout) {
            Ok(r) => r,
            Err(HttpError::Closed) => return,
            Err(e) => {
                let res = match e {
                    HttpError::BodyTooLarge => handlers::fail_request(413, "RPC request body too large"),
                    HttpError::HeadersTooLarge => handlers::fail_request(431, "RPC request headers too large"),
                    HttpError::Unsupported(w) => {
                        handlers::fail_request(501, &format!("{w} is not supported by this RPC server"))
                    }
                    _ => handlers::fail_request(400, "Failed to parse request"),
                };
                let _ = write_one(&mut writer, &res, false, &keep_alive_hint);
                return;
            }
        };

        let mut res = dispatch(ctx, &request, &peer);
        // After the handler, as `cpp-httplib` does it: compression is a
        // property of the transport, and `dispatch` stays byte-comparable.
        let on = cfg.compression == "gzip";
        // Large enough to be worth streaming, and the operator asked for it?
        // Then the coding is decided here and applied by the writer, so the
        // compressed copy is never held.
        let stream = if on && cfg.stream_threshold_bytes != 0 && res.body.len() >= cfg.stream_threshold_bytes {
            http::negotiated_coding(&request, &res)
        } else {
            None
        };
        let coding = match stream {
            Some(c) => Some(c),
            None if on => http::compress_if_accepted(&request, &mut res),
            None => None,
        };
        ctx.counters.record(res.status, coding.is_some());
        served += 1;
        let keep = cfg.keep_alive && request.wants_keep_alive() && served < cfg.keep_alive_max;
        let written = match stream {
            Some(c) => http::write_response_streamed(&mut writer, &res, c, keep, &keep_alive_hint),
            None => write_one(&mut writer, &res, keep, &keep_alive_hint),
        };
        if written.is_err() || !keep {
            return;
        }
    }
}

fn write_one<W: Write>(w: &mut W, res: &Response, keep: bool, hint: &str) -> std::io::Result<()> {
    http::write_response(w, res, keep, hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_table_is_the_cpp_route_table() {
        assert!(route("GET", "/info").is_some());
        assert!(route("GET", "/getinfo").is_some());
        assert!(route("GET", "/height").is_some());
        assert!(route("GET", "/getheight").is_some());
        assert!(route("GET", "/peers").is_some());
        assert!(route("GET", "/json_rpc").is_some());
        assert!(route("POST", "/json_rpc").is_some());
        assert!(route("POST", "/sendrawtransaction").unwrap().sync_required);
        assert!(!route("POST", "/getwalletsyncdata").unwrap().sync_required);
        assert_eq!(route("POST", "/queryblocksdetailed").unwrap().permissions, RpcMode::Explorer);
        // Registered on the IPC server alone (`RpcServer.cpp:323-326`).
        let console = route("POST", "/console").expect("the IPC socket's console route");
        assert!(console.ipc_only && console.body_required && !console.sync_required);
        assert_eq!(console.permissions, RpcMode::Standard);
        assert!(route("GET", "/console").is_none());
        assert!(!route("GET", "/info").unwrap().ipc_only && !route("POST", "/json_rpc").unwrap().ipc_only);
        // `GET` on a `POST` route, and the endpoints other CryptoNote daemons
        // serve that this one does not.
        for (m, p) in [
            ("GET", "/getwalletsyncdata"),
            ("POST", "/info"),
            ("GET", "/fee"),
            ("GET", "/getpeers"),
            ("POST", "/getblocks"),
            ("POST", "/gettransactions"),
            ("POST", "/queryblocks"),
            ("POST", "/get_pool_changes"),
            ("GET", "/"),
        ] {
            assert!(route(m, p).is_none(), "{m} {p} is not a route of RpcServer.cpp");
        }
    }

    #[test]
    fn the_rate_limiter_resets_with_the_window() {
        let mut l = RateLimiter::default();
        for _ in 0..3 {
            assert!(!l.check("1.2.3.4", 3, 100));
        }
        assert!(l.check("1.2.3.4", 3, 100), "the fourth in the window is refused");
        assert!(!l.check("5.6.7.8", 3, 100), "another address has its own budget");
        // A new window clears the map entirely, so it cannot grow without bound.
        assert!(!l.check("1.2.3.4", 3, 160));
        assert_eq!(l.by_ip.len(), 1);
        // A limit of zero disables it.
        let mut off = RateLimiter::default();
        for _ in 0..1000 {
            assert!(!off.check("1.2.3.4", 0, 100));
        }
    }

    #[test]
    fn explorer_is_above_standard() {
        assert!(RpcMode::Explorer > RpcMode::Standard);
    }

    #[test]
    fn connection_slots_are_counted_per_address_and_given_back() {
        let counts = Arc::new(ConnectionCounts::default());
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "2001:db8::1".parse().unwrap();
        let first = counts.acquire(a, 2).expect("under the cap");
        let second = counts.acquire(a, 2).expect("at the cap");
        assert!(counts.acquire(a, 2).is_none(), "a third is over it");
        assert!(counts.acquire(b, 2).is_some(), "another address has its own count");
        drop(first);
        assert_eq!(counts.open(a), 1);
        let third = counts.acquire(a, 2).expect("a closed connection frees its slot");
        drop((second, third));
        assert!(counts.by_ip.lock().unwrap().is_empty(), "nothing is kept for an address with nothing open");
    }

    #[test]
    fn admission_is_off_behind_a_trusted_proxy_and_answers_with_json() {
        let on = Admission::new(&ServerConfig::default());
        assert_eq!(on.per_ip, 8);
        let proxied = Admission::new(&ServerConfig { trust_proxy: true, ..Default::default() });
        assert_eq!(proxied.per_ip, 0, "every connection would be the proxy's");
        let busy = String::from_utf8(on.busy.clone()).unwrap();
        assert!(busy.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{busy}");
        assert!(busy.contains("Connection: close\r\n"), "{busy}");
        assert!(busy.ends_with(r#"{"error":"RPC server is busy, please retry shortly","status":"Failed"}"#));
        assert!(String::from_utf8(on.crowded.clone()).unwrap().starts_with("HTTP/1.1 429 Too Many Requests\r\n"));
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cres", "s3cret"));
        assert!(!token_matches("s3cre", "s3cret"));
        assert!(!token_matches("", "s3cret"));
        assert!(!token_matches("s3cret\0", "s3cret"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
