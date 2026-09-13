// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Binding a TCP listener to **one** address family.
//!
//! The C++ daemon runs two listeners when IPv6 is configured — a `TcpListener`
//! on the IPv4 address and a second one on the IPv6 address (`NetNode.cpp:489`,
//! and `RpcServer.cpp:138` for the RPC) — and it pins the IPv6 one to IPv6 only
//! (`m_ipv6Server->set_ipv6_v6only(true)`, `RpcServer.cpp:139`). This module is
//! the equivalent for `std::net`, which has no way to set a socket option
//! before `bind(2)`.
//!
//! # Why `IPV6_V6ONLY` is set, and set to 1
//!
//! A socket bound to `::` with `IPV6_V6ONLY` **off** is a dual-stack socket: it
//! also owns the IPv4 wildcard on that port, as IPv4-mapped addresses. That is
//! the Linux default (`net.ipv6.bindv6only = 0`), and it breaks the two-listener
//! design in whichever order the binds happen:
//!
//! - IPv6 first, then IPv4 — the IPv4 `bind(2)` fails with `EADDRINUSE`, and the
//!   daemon refuses to start on a port that looked free;
//! - IPv4 first, then IPv6 — the IPv6 `bind(2)` fails for the same reason.
//!
//! Even where it happened to work, every IPv4 peer would arrive on the IPv6
//! listener with an `::ffff:a.b.c.d` peer address, which the peer lists, the ban
//! table and `PeerlistEntry` would then have to un-map by hand.
//!
//! So the IPv6 socket is always created with `IPV6_V6ONLY = 1`: the IPv4
//! listener owns IPv4, the IPv6 listener owns IPv6, each accepted socket has an
//! address of the family it arrived on, and the two ports are independent.
//! Windows and the BSDs already default to 1; Linux does not, so it is set
//! explicitly before `bind(2)` on every unix and verified afterwards.

use std::io;
use std::net::{SocketAddrV6, TcpListener};

/// Bind a listener to `addr` with `IPV6_V6ONLY` set, so it never takes over the
/// IPv4 wildcard on the same port.
///
/// On unix the socket is created by hand, because the option has to be set
/// between `socket(2)` and `bind(2)`. On Windows the option already defaults to
/// on and cannot be changed after `bind`, so the ordinary `std` bind is used and
/// the result is verified.
pub fn bind_ipv6_only(addr: SocketAddrV6) -> io::Result<TcpListener> {
    let listener = sys::bind_ipv6_only(addr)?;
    // Cheap, and it turns "a kernel we guessed wrong about" into a start-up
    // error naming the address rather than a peer list full of `::ffff:` entries.
    if !ipv6_only(&listener)? {
        return Err(io::Error::other(format!(
            "the IPv6 listener on [{}]:{} came up dual-stack (IPV6_V6ONLY is off), and would take over \
             the IPv4 wildcard on that port",
            addr.ip(),
            addr.port()
        )));
    }
    Ok(listener)
}

/// Read `IPV6_V6ONLY` back off a listener. Only meaningful for an IPv6 socket.
pub fn ipv6_only(listener: &TcpListener) -> io::Result<bool> {
    sys::ipv6_only(listener)
}

/// Whether this host has a usable IPv6 loopback.
///
/// A CI container is often built without one, and a test that needs `::1` has
/// to skip rather than fail there.
pub fn ipv6_loopback_available() -> bool {
    TcpListener::bind("[::1]:0").is_ok()
}

// ---------------------------------------------------------------------------
// unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod sys {
    use std::io;
    use std::net::{SocketAddrV6, TcpListener};
    use std::os::fd::{AsRawFd, FromRawFd};

    /// The listen backlog. `std::net::TcpListener::bind` uses 128, and so does
    /// the C++ dispatcher's listener.
    const BACKLOG: libc::c_int = 128;

    fn set_int(fd: i32, level: i32, name: i32, value: i32) -> io::Result<()> {
        // SAFETY: `fd` is a live socket, and the pointer and length describe an
        // `int` as `setsockopt` expects for every option used here.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                std::ptr::addr_of!(value).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn ipv6_only(listener: &TcpListener) -> io::Result<bool> {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as above; `getsockopt` writes at most `len` bytes into `value`.
        let rc = unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                std::ptr::addr_of_mut!(value).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value != 0)
    }

    pub(super) fn bind_ipv6_only(addr: SocketAddrV6) -> io::Result<TcpListener> {
        // SAFETY: a plain `socket(2)`; the returned descriptor is checked below.
        let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Owned from here on, so every `?` below closes the descriptor.
        // SAFETY: `fd` is a fresh socket nobody else owns.
        let listener = unsafe { TcpListener::from_raw_fd(fd) };

        // What `std` does for its own listeners, kept so a restart does not
        // trip over a socket in TIME_WAIT.
        set_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
        // The whole point of this function, and it has to happen here: on Linux
        // the option is read at `bind(2)` and cannot be changed afterwards.
        set_int(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1)?;
        // SAFETY: `FD_CLOEXEC` on a descriptor we own.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        sa.sin6_port = addr.port().to_be();
        sa.sin6_flowinfo = addr.flowinfo();
        sa.sin6_scope_id = addr.scope_id();
        sa.sin6_addr = libc::in6_addr { s6_addr: addr.ip().octets() };

        // SAFETY: `sa` is a fully initialised `sockaddr_in6` and the length
        // matches it exactly.
        let rc = unsafe {
            libc::bind(fd, std::ptr::addr_of!(sa).cast(), std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a bound stream socket.
        if unsafe { libc::listen(fd, BACKLOG) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(listener)
    }
}

// ---------------------------------------------------------------------------
// windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use std::io;
    use std::net::{SocketAddr, SocketAddrV6, TcpListener};
    use std::os::windows::io::AsRawSocket;

    const IPPROTO_IPV6: i32 = 41;
    /// `IPV6_V6ONLY` is 27 in `ws2ipdef.h` — **not** the 26 of Linux.
    const IPV6_V6ONLY: i32 = 27;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn getsockopt(s: u64, level: i32, optname: i32, optval: *mut u8, optlen: *mut i32) -> i32;
    }

    pub(super) fn ipv6_only(listener: &TcpListener) -> io::Result<bool> {
        let mut value: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as i32;
        // SAFETY: Winsock is already initialised (this listener came from
        // `std`), the socket is live, and `getsockopt` writes at most `len`
        // bytes into `value`.
        let rc = unsafe {
            getsockopt(
                listener.as_raw_socket(),
                IPPROTO_IPV6,
                IPV6_V6ONLY,
                std::ptr::addr_of_mut!(value).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value != 0)
    }

    /// Windows defaults `IPV6_V6ONLY` to on and refuses to change it after
    /// `bind`, so the ordinary bind is already what we want; `bind_ipv6_only`
    /// verifies it.
    pub(super) fn bind_ipv6_only(addr: SocketAddrV6) -> io::Result<TcpListener> {
        TcpListener::bind(SocketAddr::V6(addr))
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use std::io;
    use std::net::{SocketAddr, SocketAddrV6, TcpListener};

    pub(super) fn ipv6_only(_listener: &TcpListener) -> io::Result<bool> {
        // Nothing to read the option with; claim what the caller needs so the
        // bind is not refused outright on an exotic target.
        Ok(true)
    }

    pub(super) fn bind_ipv6_only(addr: SocketAddrV6) -> io::Result<TcpListener> {
        TcpListener::bind(SocketAddr::V6(addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// The property the whole module exists for: an IPv6 listener on the
    /// wildcard leaves the IPv4 wildcard on the same port free.
    #[test]
    fn a_wildcard_ipv6_listener_does_not_take_the_ipv4_port() {
        if !ipv6_loopback_available() {
            eprintln!("skipping: this host has no IPv6 loopback");
            return;
        }
        let Ok(v6) = bind_ipv6_only(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)) else {
            eprintln!("skipping: this host cannot bind the IPv6 wildcard");
            return;
        };
        assert!(ipv6_only(&v6).unwrap(), "IPV6_V6ONLY is on");
        let port = v6.local_addr().unwrap().port();
        // Dual-stack, this is EADDRINUSE.
        let v4 =
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).expect("the IPv4 wildcard on the same port is still free");
        assert_eq!(v4.local_addr().unwrap().port(), port);
    }

    #[test]
    fn a_loopback_ipv6_listener_reports_its_port() {
        if !ipv6_loopback_available() {
            eprintln!("skipping: this host has no IPv6 loopback");
            return;
        }
        let l = bind_ipv6_only(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)).unwrap();
        let addr = l.local_addr().unwrap();
        assert!(addr.is_ipv6());
        assert_ne!(addr.port(), 0);
    }
}
