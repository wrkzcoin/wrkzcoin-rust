// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! WrkzCoin wallet library (spec/09-rpc-and-wallet-sync.md, spec/10-wallet.md).
//!
//! - [`api`] — `wrkz-wallet-api`: the HTTP API of `src/walletapi/ApiDispatcher.cpp`.
//! - [`cli`] — `wrkz-wallet`: the interactive wallet of `src/zedwallet++/`.
//! - [`crypto`] — the wallet file cipher and the API password hash.
//! - [`ipc`] — the same daemon over a local socket, for `ipc://` addresses.
//! - [`listen`] — the TCP, IPv6 and IPC listener `wrkz-wallet-api` and
//!   `wrkz-service` serve on.
//! - [`logging`] — `--log-level` and `--log-file` of the three programs, onto
//!   `wrkz_rpc::log`.
//! - [`daemon`] — the daemon HTTP/JSON-RPC client used by the wallet.
//! - [`mod@file`] — the wallet file: header, JSON schema, open and save.
//! - [`sync`] — block download, scanning, balances and sync status.
//! - [`transfer`] — transaction construction, the fee loop, relaying.
//! - [`platform`] — the clock, threads and sleep, which a browser lacks.
//! - [`txpow`] — the client for an external transaction proof-of-work server.
//! - [`http`] — the daemon and that server over a pluggable HTTP transport, the
//!   path Rust Pluton Wallet takes on every platform.
//!
//! `api`, `cli`, `ipc`, `listen` and `logging` need the `frontends` feature
//! and the HTTP client in [`daemon`] needs `native`; both are on by default.
//! Without them the crate is the wallet core alone, which builds for
//! wasm32-unknown-unknown.

#[cfg(feature = "frontends")]
pub mod api;
#[cfg(feature = "frontends")]
pub mod cli;
pub mod crypto;
pub mod daemon;
pub mod file;
pub mod http;
#[cfg(feature = "frontends")]
pub mod ipc;
#[cfg(feature = "frontends")]
pub mod listen;
#[cfg(feature = "frontends")]
pub mod logging;
pub mod platform;
pub mod sync;
pub mod transfer;
pub mod txpow;
