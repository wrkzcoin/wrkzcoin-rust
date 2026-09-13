// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The listener and the sync thread.
//!
//! The socket is [`wrkz_wallet::listen`], the listener `wrkz-wallet-api` runs
//! on, with HTTP/1.1 from [`wrkz_rpc::http`]: a TCP port, or — with
//! `--bind-ipc-path` — a local socket *instead of* the port, which is the
//! choice `PaymentGateService::runWalletService` makes between `startIpc` and
//! `start` (`walletservice/PaymentGateService.cpp:203-217`).
//!
//! The properties are the daemon's and `wrkz-wallet-api`'s: a fixed worker
//! pool, a bounded backlog that sheds with a `503` rather than growing, read
//! and write timeouts on every socket before the first byte, a deadline on the
//! whole head and another on the whole body, and bounded keep-alive.
//!
//! Two things differ from `wrkz-wallet-api`, both because the C++ service's
//! `HttpServer` is not httplib: nothing is gzipped, and there is no IPv6
//! listener. On either transport the request still carries its `password`
//! member: `PaymentServiceJsonRpcServer::processJsonRpcRequest` checks it in
//! the JSON-RPC layer, which does not know which socket a request came over
//! (`PaymentServiceJsonRpcServer.cpp:186`).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_rpc::http::{self, HttpLimits, Request, Response};
use wrkz_wallet::api::SyncLog;
use wrkz_wallet::listen::{Handler, IpcConfig, ListenConfig, Listener, Peer};

use crate::ServiceState;

/// How the listener behaves. The defaults match `wrkz-wallet-api`'s.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// `--bind-address:--bind-port`; empty for no TCP port at all, which is
    /// what `--bind-ipc-path` asks for.
    pub bind: String,
    /// `--bind-ipc-path`, `--bind-ipc-mode` and `--bind-ipc-group`, or `None`.
    pub ipc: Option<IpcConfig>,
    pub workers: usize,
    pub queue_capacity: usize,
    pub limits: HttpLimits,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub keep_alive: bool,
    pub keep_alive_timeout: Duration,
    pub keep_alive_max: u32,
}

impl Default for ServeConfig {
    fn default() -> Self {
        let listen = ListenConfig::default();
        ServeConfig {
            bind: format!("127.0.0.1:{}", crate::DEFAULT_PORT),
            ipc: None,
            workers: listen.workers,
            queue_capacity: listen.queue_capacity,
            limits: listen.limits,
            read_timeout: listen.read_timeout,
            write_timeout: listen.write_timeout,
            keep_alive: listen.keep_alive,
            keep_alive_timeout: listen.keep_alive_timeout,
            keep_alive_max: listen.keep_alive_max,
        }
    }
}

/// A running service. Dropping it stops the listener, the workers and the sync
/// thread, and joins them all.
pub struct RunningService {
    listener: Listener,
    state: Arc<ServiceState>,
    sync: Option<JoinHandle<()>>,
}

impl RunningService {
    /// The TCP address actually bound — `None` when serving on a local socket
    /// alone — which is how a test finds the port after asking for
    /// `127.0.0.1:0`.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr()
    }

    /// The local socket, once it is actually bound.
    pub fn ipc_path(&self) -> Option<&str> {
        self.listener.ipc_path()
    }

    /// Stop accepting, drain, join, and remove the socket file. `Drop` calls
    /// this too.
    pub fn stop(&mut self) {
        self.state.stopping.store(true, Ordering::SeqCst);
        self.listener.stop();
        if let Some(sync) = self.sync.take() {
            let _ = sync.join();
        }
    }
}

impl Drop for RunningService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bind and start serving. Returns as soon as the listener is up, and fails
/// when it cannot come up at all — a port in use, or a socket that cannot be
/// bound, which is fatal in the C++ too (`HttpServer::startIpc` throws).
pub fn start(state: Arc<ServiceState>, config: ServeConfig) -> std::io::Result<RunningService> {
    let listen = ListenConfig {
        bind: (!config.bind.is_empty()).then_some(config.bind),
        bind_ipv6: None,
        ipc: config.ipc,
        workers: config.workers,
        queue_capacity: config.queue_capacity,
        limits: config.limits,
        read_timeout: config.read_timeout,
        write_timeout: config.write_timeout,
        keep_alive: config.keep_alive,
        keep_alive_timeout: config.keep_alive_timeout,
        keep_alive_max: config.keep_alive_max,
        gzip: false,
    };

    let serving = Arc::clone(&state);
    let handler: Handler = Arc::new(move |request: &Request, _peer: &Peer| {
        let mut response = answer(&serving, request);
        if !serving.config.cors_header.is_empty() {
            response.set_header("Access-Control-Allow-Origin", &serving.config.cors_header);
        }
        response
    });
    let listener = Listener::start(listen, handler)?;
    if let Some(e) = listener.ipc_error() {
        return Err(std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, e.to_string()));
    }

    let sync = {
        let state = Arc::clone(&state);
        std::thread::spawn(move || sync_loop(&state))
    };

    Ok(RunningService { listener, state, sync: Some(sync) })
}

/// One round of syncing per pass, then whatever wait the step asked for. The
/// lock is taken and released per step, so a request never waits on more than
/// one daemon round trip.
fn sync_loop(state: &Arc<ServiceState>) {
    let mut ticks_since_info = 0u32;
    let mut log = SyncLog::default();
    while !state.stopping.load(Ordering::SeqCst) {
        let wait = {
            let mut open = state.write();
            // `Nigel`'s ten-second `/info` cadence.
            ticks_since_info += 1;
            if ticks_since_info >= 40 {
                ticks_since_info = 0;
                open.refresh_info();
            }
            let round = open.sync_round();
            log.record(&round, &open);
            state.notify.sync_step(&open);
            round.wait
        };
        sleep_unless_stopping(wait.max(Duration::from_millis(10)), &state.stopping);
    }
}

/// Sleep for `wait`, waking early when the service is told to stop.
fn sleep_unless_stopping(wait: Duration, stopping: &AtomicBool) {
    let deadline = std::time::Instant::now() + wait;
    while !stopping.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// `JsonRpcServer::processRequest` (`jsonrpcserver/JsonRpcServer.cpp:57`):
/// `/json_rpc` and nothing else, and a body that does not parse is still a
/// **200** carrying a JSON-RPC parse error.
pub fn answer(state: &Arc<ServiceState>, request: &http::Request) -> Response {
    if request.path != "/json_rpc" {
        return Response::new(404);
    }
    let body = crate::dispatch(state, &request.body);
    Response::json(200, body.to_string())
}
