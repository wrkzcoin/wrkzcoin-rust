// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Lite node base snapshots: the C++ `.litesnap` file (`src/daemon/LiteSnapshot.h`,
//! `LITESNAPSHOT.md`), read and written so that a file moves between this port
//! and `Wrkzd` in both directions.
//!
//! A snapshot is the index-only region `[0, H)` of a lite node — block info,
//! spent key images and key outputs, as the C++ database's own KV records — so
//! a node can start from `H` instead of rebuilding that region from the chain.
//! It is a pure function of the chain and `H`: two nodes at different tips
//! produce the same payload digest, which is what lets a digest be compiled in
//! and checked by someone who did not make the file.
//!
//! - [`container`] — the header, the zstd frames, the chained digest, the
//!   compiled-in digest table and `--snapshot-info`.
//! - [`records`] — the three C++ tables, filed and decoded strictly, and
//!   encoded the way the C++ export encodes them.
//! - [`import`] — `--import-lite-snapshot`: the C++ importer's refusals and
//!   verifying pass, then the records transcoded into this crate's namespace.
//! - [`export`] — `snapshot_export`'s walk: the region below a height, out of
//!   this crate's state, in the C++ database's key order and encodings.
//!
//! This module needs the `lite-snapshot` feature, which pulls in the zstd C
//! library; the daemon enables it and nothing a wallet builds does.

pub mod container;
pub mod export;
pub mod import;
pub mod records;

pub use container::{compiled_in_digests, BlessedDigest, Header, SnapshotResult};
pub use records::{SnapshotRecord, Table};
