// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-txpow-server`: computes the transaction proof of work on behalf of
//! wallets that would rather not spend their own CPU on it, such as phones and
//! browsers. It only ever sees the unsigned transaction prefix and hands back
//! the eight nonce bytes; it holds no keys and cannot alter or spend anything.
//!
//! Route for route, option for option and default for default the C++ server
//! (`src/txpowserver`), so a wallet — this repository's
//! `wrkz_wallet::txpow::TxPowServer` or the C++ `TxPowClient` — cannot tell
//! them apart. docs/TXPOW-SERVER.md is the operator's guide.
//!
//! - [`config`] — the command line (`TxPowServerConfig.cpp`).
//! - [`service`] — the job queue and the hashing threads (`PowService.cpp`).
//! - [`api`] — the routes, the API key, the rate limits, CORS (`HttpApi.cpp`).
//! - [`serve`] — the listeners and the connection workers.
//! - [`log`] — levelled log lines to the console and optionally a file.
//! - [`signal`] — Ctrl-C and SIGTERM.

pub mod api;
pub mod config;
pub mod log;
pub mod serve;
pub mod service;
pub mod signal;

/// The release, as every program here numbers it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Set at compile time by `build.rs`; absent outside a git checkout.
const GIT_COMMIT: Option<&str> = option_env!("WRKZ_GIT_COMMIT");

/// `wrkz-txpow-server 1.0.0 (a60f056)`, the line `--version` prints and
/// `/stats` and `/` report.
pub fn version_line() -> String {
    match GIT_COMMIT {
        Some(commit) if !commit.is_empty() => format!("wrkz-txpow-server {VERSION} ({commit})"),
        _ => format!("wrkz-txpow-server {VERSION}"),
    }
}
