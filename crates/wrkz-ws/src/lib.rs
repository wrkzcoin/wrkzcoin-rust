// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! WebSocket, RFC 6455, as much of it as a publisher and its subscribers need.
//!
//! The daemon's `GET /ws` (`wrkz_rpc::ws`) pushes the chain and pool events
//! its ZMQ socket publishes, and the wallets (`wrkz_wallet::tip_watch`) listen
//! so that a synced wallet hears of a block the moment the daemon has it,
//! instead of asking every two seconds. Both ends are here: the server reads
//! masked frames and writes plain ones, the client the other way round.
//!
//! - [`frame`] — the frame format (§5): read one frame from a stream within a
//!   payload cap, write one, and reassemble fragments into a message.
//! - [`handshake`] — the opening handshake (§4): check an upgrade request and
//!   answer it, or build one and check the answer.
//!
//! What is deliberately missing: extensions (no `permessage-deflate`, so no
//! reserved bits are ever valid), subprotocols, and fragmenting on write —
//! every frame this crate writes is a whole message, and the messages here
//! are a few hundred bytes of JSON.
//!
//! No async runtime and no allocation sized by the peer: a length is checked
//! against the caller's cap before anything is allocated for it.

#![forbid(unsafe_code)]

pub mod frame;
pub mod handshake;

pub use frame::{
    encode_frame, parse_frame, read_frame, Assembler, Frame, FrameError, Message, MessageReader, Opcode, Role,
};
pub use handshake::HandshakeError;
