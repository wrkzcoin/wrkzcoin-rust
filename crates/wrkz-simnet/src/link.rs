// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A link between two simnet nodes: a TCP relay on loopback that one node
//! dials in place of the other, and that a test can cut and heal.
//!
//! The engine has no way to be told "drop that peer and stay away", and a ban
//! would not do — every simnet node is on 127.0.0.1, which bans exempt. So the
//! partition happens in the wire instead: cutting a link closes every
//! connection through it and refuses new ones until it is healed. The node
//! that dials it keeps redialling (it is an exclusive node there), so healing
//! is all it takes for the two to meet again.
//!
//! Bytes are copied as they come; the relay understands nothing of what it
//! carries.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the accept loop and the copying threads look at whether they
/// should stop.
const POLL: Duration = Duration::from_millis(20);
const READ_SLICE: Duration = Duration::from_millis(200);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

struct Inner {
    target: Mutex<Option<SocketAddr>>,
    open: AtomicBool,
    stop: AtomicBool,
    /// Bumped on every cut; a copying thread from before it ends.
    generation: AtomicU64,
    /// Both ends of every connection, to shut on a cut.
    streams: Mutex<Vec<TcpStream>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// One relay. Dropping it closes everything through it.
pub struct Link {
    addr: SocketAddr,
    inner: Arc<Inner>,
    accept: Option<JoinHandle<()>>,
}

impl Link {
    /// Listen on a free loopback port, forwarding nowhere until
    /// [`Link::set_target`].
    pub fn bind() -> io::Result<Link> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner {
            target: Mutex::new(None),
            open: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            streams: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
        });
        let accepting = Arc::clone(&inner);
        let accept = std::thread::Builder::new()
            .name("wrkz-simnet-link".into())
            .spawn(move || accept_loop(&accepting, listener))?;
        Ok(Link { addr, inner, accept: Some(accept) })
    }

    /// The address to dial instead of the target.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Where connections are forwarded.
    pub fn set_target(&self, target: SocketAddr) {
        *self.inner.target.lock().unwrap_or_else(|p| p.into_inner()) = Some(target);
    }

    /// Close every connection through the link, and refuse new ones.
    pub fn cut(&self) {
        self.inner.open.store(false, Ordering::SeqCst);
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        for stream in self.inner.streams.lock().unwrap_or_else(|p| p.into_inner()).drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Accept connections again.
    pub fn heal(&self) {
        self.inner.open.store(true, Ordering::SeqCst);
    }

    pub fn is_open(&self) -> bool {
        self.inner.open.load(Ordering::SeqCst)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.cut();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let threads: Vec<JoinHandle<()>> =
            self.inner.threads.lock().unwrap_or_else(|p| p.into_inner()).drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

fn accept_loop(inner: &Arc<Inner>, listener: TcpListener) {
    while !inner.stop.load(Ordering::SeqCst) {
        let client = match listener.accept() {
            Ok((client, _)) => client,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL);
                continue;
            }
            Err(_) => {
                std::thread::sleep(POLL);
                continue;
            }
        };
        let target = *inner.target.lock().unwrap_or_else(|p| p.into_inner());
        let (true, Some(target)) = (inner.open.load(Ordering::SeqCst), target) else {
            // Cut, or not yet pointed anywhere: the dialler sees the
            // connection close, as it would a dead host.
            let _ = client.shutdown(Shutdown::Both);
            continue;
        };
        let Ok(server) = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) else {
            let _ = client.shutdown(Shutdown::Both);
            continue;
        };
        if let Err(()) = relay(inner, client, server) {
            continue;
        }
    }
}

/// Copy both ways between `client` and `server` until either side closes or
/// the link is cut.
fn relay(inner: &Arc<Inner>, client: TcpStream, server: TcpStream) -> Result<(), ()> {
    let _ = client.set_nonblocking(false);
    let generation = inner.generation.load(Ordering::SeqCst);
    let clones = (client.try_clone(), client.try_clone(), server.try_clone(), server.try_clone());
    let (Ok(client_read), Ok(client_keep), Ok(server_read), Ok(server_keep)) = clones else {
        return Err(());
    };
    inner.streams.lock().unwrap_or_else(|p| p.into_inner()).extend([client_keep, server_keep]);
    let mut threads = inner.threads.lock().unwrap_or_else(|p| p.into_inner());
    threads.retain(|t| !t.is_finished());
    for (from, to) in [(client_read, server), (server_read, client)] {
        let pumping = Arc::clone(inner);
        if let Ok(t) = std::thread::Builder::new()
            .name("wrkz-simnet-pump".into())
            .spawn(move || pump(&pumping, generation, from, to))
        {
            threads.push(t);
        }
    }
    Ok(())
}

fn pump(inner: &Inner, generation: u64, mut from: TcpStream, mut to: TcpStream) {
    let _ = from.set_read_timeout(Some(READ_SLICE));
    let mut buf = [0u8; 16 * 1024];
    loop {
        if inner.stop.load(Ordering::SeqCst) || inner.generation.load(Ordering::SeqCst) != generation {
            break;
        }
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 || s.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn round_trip(addr: SocketAddr) -> io::Result<Vec<u8>> {
        let mut s = TcpStream::connect(addr)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(b"ping")?;
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf)?;
        Ok(buf.to_vec())
    }

    #[test]
    fn bytes_pass_until_the_link_is_cut_and_again_once_healed() {
        let link = Link::bind().unwrap();
        link.set_target(echo_server());
        assert_eq!(round_trip(link.addr()).unwrap(), b"ping");

        let mut held = TcpStream::connect(link.addr()).unwrap();
        held.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        held.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        held.read_exact(&mut buf).unwrap();

        link.cut();
        // The open connection is closed, and a new one gets nothing through.
        assert!(matches!(held.read(&mut buf), Ok(0) | Err(_)));
        assert!(round_trip(link.addr()).is_err());

        link.heal();
        assert_eq!(round_trip(link.addr()).unwrap(), b"ping");
    }
}
