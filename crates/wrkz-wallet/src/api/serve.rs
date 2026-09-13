// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The listeners and the sync thread — `ApiDispatcher::start`
//! (`ApiDispatcher.cpp:420`) and the `WalletSynchronizer` thread
//! `WalletBackend` starts on open.
//!
//! The sockets are [`crate::listen`]: the TCP listener, the IPv6 one when
//! [`ServeConfig::bind_ipv6`] is set and the IPC socket when
//! [`ServeConfig::ipc`] is, all serving [`dispatch`] from one fixed pool of
//! workers behind one bounded queue. The properties are the daemon's: a
//! backlog that sheds with a `503` rather than grows, read and write timeouts
//! on every socket before the first byte, deadlines on the request head and
//! body, and caps on every part of a request.
//!
//! Every listener runs the same middleware, so an IPC caller needs the same
//! `X-API-KEY` a TCP one does (see [`crate::api`], "Transport").
//!
//! There is no per-address connection cap here, unlike the daemon's: the API
//! binds loopback by default, and every caller must hold the API key.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_rpc::http::{HttpLimits, Request};

use super::view::Fingerprint;
use super::{dispatch, ApiState, SyncLog};
use crate::listen::{Handler, IpcConfig, ListenConfig, Listener, Peer};

/// How the listeners behave. The defaults match the daemon's.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// `--rpc-bind-ip` and `--port`, as `ip:port`.
    pub bind: String,
    /// `[address]:port` for the IPv6 listener ([`crate::listen::ipv6_bind`]),
    /// or empty for none.
    pub bind_ipv6: String,
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
    /// gzip for a client that accepts it; on, as in a C++ build with zlib.
    pub gzip: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        let listen = ListenConfig::default();
        ServeConfig {
            bind: format!("127.0.0.1:{}", super::SERVICE_DEFAULT_PORT),
            bind_ipv6: String::new(),
            ipc: None,
            workers: listen.workers,
            queue_capacity: listen.queue_capacity,
            limits: listen.limits,
            read_timeout: listen.read_timeout,
            write_timeout: listen.write_timeout,
            keep_alive: listen.keep_alive,
            keep_alive_timeout: listen.keep_alive_timeout,
            keep_alive_max: listen.keep_alive_max,
            gzip: true,
        }
    }
}

/// A running API server. Dropping it stops the listeners, the workers and the
/// sync thread, and joins them all.
pub struct RunningApi {
    listener: Listener,
    addr: SocketAddr,
    stopping: Arc<AtomicBool>,
    sync: Option<JoinHandle<()>>,
}

impl RunningApi {
    /// The address actually bound, which is how a test finds the port after
    /// asking for `127.0.0.1:0`.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The IPv6 listener's address, when one came up.
    pub fn local_addr6(&self) -> Option<SocketAddr> {
        self.listener.local_addr6()
    }

    /// The IPC socket, once it is actually bound (`ApiDispatcher::getIpcPath`).
    pub fn ipc_path(&self) -> Option<&str> {
        self.listener.ipc_path()
    }

    /// Why the IPv6 listener asked for did not come up.
    pub fn ipv6_error(&self) -> Option<&str> {
        self.listener.ipv6_error()
    }

    /// Why the IPC socket asked for did not come up.
    pub fn ipc_error(&self) -> Option<&str> {
        self.listener.ipc_error()
    }

    /// Stop accepting, drain, join. `Drop` calls this too.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.listener.stop();
        if let Some(sync) = self.sync.take() {
            let _ = sync.join();
        }
    }
}

impl Drop for RunningApi {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bind and start serving. Returns as soon as the listeners are up; fails only
/// when the IPv4 listener cannot bind.
pub fn start(state: Arc<ApiState>, config: ServeConfig) -> std::io::Result<RunningApi> {
    let listen = ListenConfig {
        bind: Some(config.bind),
        bind_ipv6: (!config.bind_ipv6.is_empty()).then_some(config.bind_ipv6),
        ipc: config.ipc,
        workers: config.workers,
        queue_capacity: config.queue_capacity,
        limits: config.limits,
        read_timeout: config.read_timeout,
        write_timeout: config.write_timeout,
        keep_alive: config.keep_alive,
        keep_alive_timeout: config.keep_alive_timeout,
        keep_alive_max: config.keep_alive_max,
        gzip: config.gzip,
    };

    let serving = Arc::clone(&state);
    // Every transport gets the same middleware, `X-API-KEY` included.
    let handler: Handler = Arc::new(move |request: &Request, _peer: &Peer| dispatch(&serving, request));
    let listener = Listener::start(listen, handler)?;
    let addr = listener
        .local_addr()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no IPv4 listener"))?;

    // The sync thread: the C++ starts one per open wallet; one for the process
    // is the same thing, because only one wallet is ever open.
    let stopping = Arc::new(AtomicBool::new(false));
    let sync = {
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || sync_loop(&state, &stopping))
    };

    Ok(RunningApi { listener, addr, stopping, sync: Some(sync) })
}

/// One round of syncing per pass, then whatever wait the step asked for.
///
/// The working wallet is held for the whole step, round trip included, so a
/// step is applied all at once; a change a route makes waits for at most that
/// one step. What the step changed is published before the wallet is let go,
/// so the read-only routes, which never take it, see it straight away.
fn sync_loop(state: &Arc<ApiState>, stopping: &AtomicBool) {
    let mut ticks_since_info = 0u32;
    let mut log = SyncLog::default();
    // What this loop last published; `None` publishes on the next step.
    let mut published: Option<Fingerprint> = None;

    while !stopping.load(Ordering::SeqCst) {
        let wait = {
            let mut guard = match state.wallet.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match guard.as_mut() {
                None => {
                    log = SyncLog::default();
                    published = None;
                    Duration::from_millis(250)
                }
                Some(open) => {
                    // `Nigel`'s ten-second `/info` cadence.
                    ticks_since_info += 1;
                    if ticks_since_info >= 40 {
                        ticks_since_info = 0;
                        open.refresh_info();
                    }
                    let round = open.sync_round();
                    log.record(&round, open);
                    state.announce_sync_step(open);

                    // A synced wallet idling at the tip changes nothing, and
                    // copies nothing.
                    let now = Fingerprint::of(open);
                    if published != Some(now) {
                        state.publish(Some(open));
                        published = Some(now);
                    }
                    round.wait
                }
            }
        };

        // Never spin: a zero wait still yields, so a request can take the lock.
        sleep_unless_stopping(wait.max(Duration::from_millis(10)), stopping);
    }
}

/// Sleep for `wait`, waking early when the server is told to stop, so a
/// twenty-second back-off after a `429` does not hold up a shutdown.
pub(crate) fn sleep_unless_stopping(wait: Duration, stopping: &AtomicBool) {
    let slice = Duration::from_millis(100);
    let deadline = std::time::Instant::now() + wait;
    loop {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(slice));
    }
}
