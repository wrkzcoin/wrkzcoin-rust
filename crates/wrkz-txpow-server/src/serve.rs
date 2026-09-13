// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The listeners and the connection workers, on `wrkz_rpc::http` — the same
//! shape as `wrkz_wallet::api::serve`: a bounded backlog that sheds with a
//! `503` instead of growing, a fixed worker pool, and read and write
//! deadlines on every socket. Unlike it, a handler is told who is asking,
//! which the per-address rate limit needs.
//!
//! Long polls hold a worker each for up to `--max-wait-ms`, so the pool is
//! sized for the jobs that can be waiting rather than for the CPU count, as
//! the C++ sizes httplib's.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV6, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_rpc::http::{self, DeadlineStream, HttpError, HttpLimits, Response};

use crate::api::{Api, MAX_BODY_BYTES};
use crate::log::{Level, Logger};

/// How to listen.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    pub bind_ip: String,
    pub bind_port: u16,
    /// A second, IPv6-only listener on the same port. Empty is none.
    pub bind_ipv6_address: String,
    pub workers: usize,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub keep_alive_timeout: Duration,
    pub keep_alive_max: u32,
}

impl Default for ServeConfig {
    fn default() -> Self {
        ServeConfig {
            bind_ip: "127.0.0.1".into(),
            bind_port: 17870,
            bind_ipv6_address: String::new(),
            workers: 16,
            // httplib's settings in the C++ (`HttpApi::configure`).
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            keep_alive_timeout: Duration::from_secs(5),
            keep_alive_max: 64,
        }
    }
}

impl ServeConfig {
    /// Workers for `max_queue` waiting jobs: at least eight, and room for every
    /// queued job's long poll plus as many again for everything else.
    pub fn workers_for(max_queue: usize) -> usize {
        max_queue.max(8) * 2
    }
}

/// Running listeners. Dropping them stops the acceptors and the workers.
pub struct Running {
    addrs: Vec<SocketAddr>,
    stopping: Arc<AtomicBool>,
    queue: Arc<Queue>,
    threads: Vec<JoinHandle<()>>,
}

impl Running {
    /// The first listener's address — how a test finds the port it asked the
    /// system to choose.
    pub fn local_addr(&self) -> SocketAddr {
        self.addrs[0]
    }

    /// Every address listened on.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Stop accepting, drop what is queued, join the workers. A worker holding
    /// a long poll finishes it first; stop the service before this, so every
    /// held request has its answer at once.
    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        for addr in &self.addrs {
            // Unblock the acceptor parked in `accept()`. An unspecified address
            // cannot be connected to, its loopback can.
            let wake = match addr.ip() {
                IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port()),
                IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), addr.port()),
                _ => *addr,
            };
            if let Ok(s) = TcpStream::connect_timeout(&wake, Duration::from_secs(1)) {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        self.queue.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bind and start serving `api`. Fails when the main listener cannot bind; a
/// second IPv6 listener that cannot bind is logged and skipped, as the C++.
pub fn start(api: Arc<Api>, config: ServeConfig, logger: Arc<Logger>) -> std::io::Result<Running> {
    let mut listeners = vec![TcpListener::bind((config.bind_ip.as_str(), config.bind_port))?];
    if !config.bind_ipv6_address.is_empty() {
        let bound = config
            .bind_ipv6_address
            .parse::<Ipv6Addr>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
            .and_then(|ip| wrkz_p2p::bind::bind_ipv6_only(SocketAddrV6::new(ip, config.bind_port, 0, 0)));
        match bound {
            Ok(l) => listeners.push(l),
            Err(e) => logger.log(
                Level::Warning,
                format!(
                    "Could not bind the IPv6 listener to [{}]:{} ({e}), continuing without it",
                    config.bind_ipv6_address, config.bind_port
                ),
            ),
        }
    }

    let addrs = listeners.iter().map(TcpListener::local_addr).collect::<std::io::Result<Vec<_>>>()?;
    let workers = config.workers.max(1);
    let queue = Arc::new(Queue::new(workers * 4));
    let stopping = Arc::new(AtomicBool::new(false));
    let config = Arc::new(config);
    let mut threads = Vec::with_capacity(workers + listeners.len());

    for _ in 0..workers {
        let (queue, api, config) = (Arc::clone(&queue), Arc::clone(&api), Arc::clone(&config));
        threads.push(std::thread::spawn(move || {
            while let Some((stream, peer)) = queue.pop() {
                serve_connection(&api, &config, stream, peer);
            }
        }));
    }

    let busy = http::render_closing(&Response::new(503));
    for listener in listeners {
        let (queue, stopping, busy) = (Arc::clone(&queue), Arc::clone(&stopping), busy.clone());
        threads.push(std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let Ok(peer) = stream.peer_addr() else { continue };
                if let Err((rejected, _)) = queue.push((stream, peer)) {
                    http::shed_tcp(&rejected, &busy);
                }
            }
            queue.close();
        }));
    }

    Ok(Running { addrs, stopping, queue, threads })
}

fn serve_connection(api: &Api, config: &ServeConfig, stream: TcpStream, peer: SocketAddr) {
    let limits = HttpLimits { max_body: MAX_BODY_BYTES, ..HttpLimits::default() };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(config.write_timeout));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(DeadlineStream::new(stream, config.read_timeout));
    let hint = format!("timeout={}, max={}", config.keep_alive_timeout.as_secs(), config.keep_alive_max);

    let mut served: u32 = 0;
    loop {
        let head = if served == 0 {
            reader.get_mut().set_per_read(config.read_timeout);
            config.read_timeout
        } else {
            reader.get_mut().set_per_read(config.keep_alive_timeout);
            config.keep_alive_timeout + config.read_timeout
        };
        let request = match http::read_request_timed(&mut reader, &limits, head, config.read_timeout) {
            Ok(r) => r,
            Err(HttpError::Closed) => return,
            Err(e) => {
                let status = match e {
                    HttpError::BodyTooLarge => 413,
                    HttpError::HeadersTooLarge => 431,
                    HttpError::Unsupported(_) => 501,
                    _ => 400,
                };
                let _ = http::write_response(&mut writer, &Response::new(status), false, &hint);
                return;
            }
        };

        let res = api.handle(&request, peer.ip());
        served += 1;
        let keep = request.wants_keep_alive() && served < config.keep_alive_max;
        if http::write_response(&mut writer, &res, keep, &hint).is_err() || !keep {
            return;
        }
    }
}

type Item = (TcpStream, SocketAddr);

struct Queue {
    inner: Mutex<Option<VecDeque<Item>>>,
    ready: Condvar,
    capacity: usize,
}

impl Queue {
    fn new(capacity: usize) -> Self {
        Queue { inner: Mutex::new(Some(VecDeque::new())), ready: Condvar::new(), capacity }
    }

    fn push(&self, item: Item) -> Result<(), Item> {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(q) if q.len() < self.capacity => {
                q.push_back(item);
                drop(guard);
                self.ready.notify_one();
                Ok(())
            }
            _ => Err(item),
        }
    }

    fn pop(&self) -> Option<Item> {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let queue = guard.as_mut()?;
            if let Some(item) = queue.pop_front() {
                return Some(item);
            }
            guard = self.ready.wait(guard).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(q) = guard.take() {
            for (s, _) in q {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        drop(guard);
        self.ready.notify_all();
    }
}
