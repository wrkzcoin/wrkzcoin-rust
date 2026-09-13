// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! WrkzCoin P2P wire protocol (spec/08-p2p-protocol.md).
//!
//! - [`levin`]: the 33-byte frame header and blocking read/write helpers.
//! - [`msg`]: the KV-binary shapes of the 1000-series P2P commands and the
//!   2000-series CryptoNote notifications, with encoders and decoders for both
//!   directions.
//! - [`conn`]: one Levin conversation over a `TcpStream`, with the spec/08
//!   timeouts, the payload limit, and a request/response invoke that matches
//!   the reply by command id.
//! - [`bind`]: binding a listener to one address family, which is what lets a
//!   node run an IPv4 and an IPv6 listener on the same port.
//! - [`limits`]: the per-command inbound payload caps [`conn::Connection`]
//!   applies from the Levin header, before a body is read.
//!
//! Transport is plain TCP; this crate deliberately uses `std::net` so the wire
//! layer has no runtime dependency. The connection manager, peer lists and the
//! sync state machine live in `wrkz-node` on top of it.

pub mod bind;
pub mod conn;
pub mod levin;
pub mod limits;
pub mod msg;
