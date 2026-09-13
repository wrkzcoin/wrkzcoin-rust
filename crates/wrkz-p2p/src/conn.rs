// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A Levin connection over a `TcpStream` (spec/08 "Transport", "Timeouts and
//! limits").
//!
//! One conversation per socket, messages processed in order, every socket
//! operation under a timeout. The C++ node runs each connection as a
//! coroutine on one dispatcher; this is std-only, so a node runs one reader
//! thread per connection and pushes frames to its engine over a bounded
//! channel. Nothing here allocates from a peer-declared length: the frame
//! reader grows its buffer with the bytes that actually arrive
//! ([`levin::read_frame_limited`]).
//!
//! Two ways to use it:
//!
//! - [`Connection::invoke`] for the two commands the C++ genuinely invokes,
//!   `COMMAND_HANDSHAKE` and `COMMAND_PING` (`NetNode.cpp:860`, `:2160`): send
//!   a request and read frames until the matching response arrives, keeping
//!   anything else that came in the meantime.
//! - [`Connection::read_frame`] plus [`Connection::send`] for the steady state,
//!   where every CryptoNote notification is fire-and-forget.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::levin::{self, Header};

/// `P2P_DEFAULT_CONNECTION_TIMEOUT` (5 s).
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(5_000);
/// The handshake budget, 3x the connection timeout (spec/08 "Timeouts and limits").
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(15_000);
/// The back-ping budget, 2x the connection timeout (`NetNode.cpp:2189`).
pub const PING_TIMEOUT: Duration = Duration::from_millis(10_000);
/// `P2P_DEFAULT_INVOKE_TIMEOUT`: a write that has not completed in this long
/// closes the connection.
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(120_000);
/// How long a connection may produce no frame at all before it is considered
/// dead. The C++ `timeoutLoop` closes a connection whose pending invoke has
/// expired; a notification-only peer is kept alive by the 60 s timed sync, so
/// anything past a few missed intervals is stalled.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(200);

/// How many frames [`Connection::invoke`] will read while waiting for its
/// response. A peer that streams notifications forever never trips the socket
/// read timeout, because every individual read succeeds.
const INVOKE_MAX_FRAMES: usize = 256;

/// One Levin conversation.
pub struct Connection {
    stream: TcpStream,
    peer: SocketAddr,
    max_payload: u64,
    /// Frames read while waiting for an invoke response, in arrival order.
    /// Bounded by [`INVOKE_MAX_FRAMES`] and drained by [`Connection::take_pending`].
    pending: Vec<(Header, Vec<u8>)>,
}

impl Connection {
    /// Dial `addr` with the connect timeout and set the read/write timeouts.
    pub fn connect(addr: SocketAddr, connect_timeout: Duration, read_timeout: Duration) -> io::Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, connect_timeout)?;
        Self::from_stream(stream, read_timeout)
    }

    /// Wrap an accepted or already connected socket.
    pub fn from_stream(stream: TcpStream, read_timeout: Duration) -> io::Result<Self> {
        let peer = stream.peer_addr()?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        // Every message is written in one call, so Nagle only adds latency.
        stream.set_nodelay(true).ok();
        Ok(Self { stream, peer, max_payload: levin::DEFAULT_MAX_PAYLOAD, pending: Vec::new() })
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Lower the accepted payload size below [`levin::DEFAULT_MAX_PAYLOAD`].
    pub fn set_max_payload(&mut self, max: u64) {
        self.max_payload = max.min(levin::DEFAULT_MAX_PAYLOAD);
    }

    pub fn set_read_timeout(&self, t: Duration) -> io::Result<()> {
        self.stream.set_read_timeout(Some(t))
    }

    /// A second handle on the same socket, so a reader thread and the engine's
    /// writer can use it at once. Levin has no interleaving within a frame and
    /// each frame is written by a single `write_all`, so one writer plus one
    /// reader is safe; two writers are not, and the node keeps exactly one.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
            peer: self.peer,
            max_payload: self.max_payload,
            pending: Vec::new(),
        })
    }

    /// Shut both directions down, which unblocks a reader thread parked in
    /// `read_frame`. Errors are ignored: the peer may already be gone.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Read one frame under the socket's read timeout, refusing from the header
    /// alone a payload over its command's cap
    /// ([`crate::limits::max_inbound_payload`]) or over this connection's
    /// limit, whichever is lower.
    ///
    /// Any error leaves the stream at an unknown offset, so the caller MUST
    /// close the connection rather than read again.
    pub fn read_frame(&mut self) -> io::Result<(Header, Vec<u8>)> {
        let max = self.max_payload;
        levin::read_frame_by(&mut self.stream, |h| crate::limits::max_inbound_payload(h).min(max))
    }

    /// Write one frame.
    pub fn send(&mut self, header: &Header, payload: &[u8]) -> io::Result<()> {
        levin::write_frame(&mut self.stream, header, payload)
    }

    /// A notification (`post_notify`: `m_have_to_return_data = 0`).
    pub fn notify(&mut self, command: u32, payload: &[u8]) -> io::Result<()> {
        self.send(&Header::request(command, false), payload)
    }

    /// A reply to a request, with the same command id and the response flag.
    pub fn reply(&mut self, command: u32, return_code: i32, payload: &[u8]) -> io::Result<()> {
        self.send(&Header::response(command, return_code), payload)
    }

    /// The answer to a command we do not implement (`NetNode.cpp:2868`).
    pub fn reply_not_defined(&mut self, command: u32) -> io::Result<()> {
        self.reply(command, levin::ERROR_HANDLER_NOT_DEFINED, &[])
    }

    /// Send a request and read frames until the response to `command` arrives.
    ///
    /// Anything else read on the way is kept in the pending buffer
    /// ([`Connection::take_pending`]) so a handshake that races an incoming
    /// notification does not lose it. Gives up after `budget` or
    /// 256 frames, whichever comes first.
    pub fn invoke(&mut self, command: u32, payload: &[u8], budget: Duration) -> io::Result<(Header, Vec<u8>)> {
        self.send(&Header::request(command, true), payload)?;
        let deadline = Instant::now() + budget;
        self.set_read_timeout(budget)?;
        for _ in 0..INVOKE_MAX_FRAMES {
            let (h, body) = self.read_frame()?;
            if h.command == command && h.is_response() {
                return Ok((h, body));
            }
            if self.pending.len() < INVOKE_MAX_FRAMES {
                self.pending.push((h, body));
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, format!("no response to command {command}")))
    }

    /// The frames [`Connection::invoke`] read while waiting, in arrival order.
    pub fn take_pending(&mut self) -> Vec<(Header, Vec<u8>)> {
        std::mem::take(&mut self.pending)
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;
    use std::net::TcpListener;

    /// `invoke` matches the response by command and keeps the notification
    /// that arrived in between, exactly as the C++ handshake does while the
    /// peer is already relaying.
    #[test]
    fn invoke_matches_by_command_and_keeps_the_notifications() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut c = Connection::from_stream(s, Duration::from_secs(5)).unwrap();
            let (h, _) = c.read_frame().unwrap();
            assert_eq!(h.command, msg::COMMAND_PING);
            assert!(h.have_to_return_data);
            // a notification first, then a response to a different command,
            // then the one the caller is waiting for
            c.notify(msg::NOTIFY_NEW_TRANSACTIONS, &msg::new_transactions(&[b"tx".to_vec()])).unwrap();
            c.reply(msg::COMMAND_TIMED_SYNC, levin::RETCODE_SUCCESS, &[]).unwrap();
            c.reply(msg::COMMAND_PING, levin::RETCODE_SUCCESS, &msg::ping_response(0xabcd)).unwrap();
        });

        let mut c = Connection::connect(addr, CONNECT_TIMEOUT, Duration::from_secs(5)).unwrap();
        let (h, body) = c.invoke(msg::COMMAND_PING, &msg::ping_request(), PING_TIMEOUT).unwrap();
        assert_eq!(h.return_code, levin::RETCODE_SUCCESS);
        assert!(msg::parse_ping_response(&body).unwrap().is_ok_for(0xabcd));
        let pending = c.take_pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].0.command, msg::NOTIFY_NEW_TRANSACTIONS);
        assert_eq!(pending[1].0.command, msg::COMMAND_TIMED_SYNC);
        assert!(c.take_pending().is_empty());
        peer.join().unwrap();
    }

    /// A peer that accepts the connection and then says nothing must make the
    /// invoke time out rather than hang.
    #[test]
    fn invoke_times_out_on_a_silent_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(400));
            drop(s);
        });
        let mut c = Connection::connect(addr, CONNECT_TIMEOUT, Duration::from_secs(5)).unwrap();
        let e = c.invoke(msg::COMMAND_PING, &msg::ping_request(), Duration::from_millis(150)).unwrap_err();
        assert!(matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock), "{e:?}");
        peer.join().unwrap();
    }

    /// Garbage on the wire is an `InvalidData` error from the framing layer,
    /// never a panic and never an allocation from the declared length.
    #[test]
    fn a_bad_signature_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(&[0xffu8; 33]).unwrap();
        });
        let mut c = Connection::connect(addr, CONNECT_TIMEOUT, Duration::from_secs(5)).unwrap();
        assert_eq!(c.read_frame().unwrap_err().kind(), io::ErrorKind::InvalidData);
        peer.join().unwrap();
    }
}
