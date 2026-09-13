// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! WrkzCoin consensus primitives above the hashing layer.
//!
//! Everything here is derived from the specification in `spec/` and pinned by
//! the vectors in `spec/vectors/`. Module map (spec document in brackets):
//!
//! - [`constants`] — every network parameter and the derived tables (spec 01)
//! - [`varint`] — unsigned LEB128 (spec 04)
//! - [`base58`] — CryptoNote block base58 and address encoding (spec 05)
//! - [`ser`] — the binary serializer and deserializer (spec 04)
//! - [`tx`] — transaction structures, hashes, extra parsing (spec 04, 06)
//! - [`block`] — block structures, hashing blobs, block id, PoW input (spec 04, 07)
//! - [`difficulty`] — LWMA-2 and the legacy algorithm (spec 07)
//! - [`fees`], [`mixins`] — fee ladder and mixin tiers (spec 06)
//! - [`kv`] — the KV binary "portable storage" format used by P2P and the database (spec 04)

pub mod base58;
pub mod block;
pub mod constants;
pub mod difficulty;
pub mod fees;
pub mod kv;
pub mod mixins;
pub mod mnemonic;
pub mod ser;
pub mod tx;
pub mod varint;
pub mod wordlist;

pub use wrkz_pow::Hash;

/// Error type shared by the parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Input ended before a complete structure was read.
    Truncated,
    /// Bytes remained after a structure that must consume the whole buffer.
    TrailingBytes(usize),
    /// A varint overflowed its target width or was non-canonical.
    BadVarint,
    /// An unknown variant tag, version or otherwise malformed field.
    Malformed(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => write!(f, "input truncated"),
            Error::TrailingBytes(n) => write!(f, "{n} trailing bytes"),
            Error::BadVarint => write!(f, "bad varint"),
            Error::Malformed(what) => write!(f, "malformed: {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
