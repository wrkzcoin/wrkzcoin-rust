// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-service`: the JSON-RPC wallet service, method for method with the C++
//! `src/walletservice` (`PaymentServiceJsonRpcServer`, `WalletService`).
//!
//! One container, opened at startup and held open, one background sync thread,
//! and one route: `POST /json_rpc`. Everything else about the shape is in
//! [`rpc`]; the method set is [`methods::METHODS`].
//!
//! # What this is, and what it is not
//!
//! The C++ `wrkz-service` is built on `WalletGreen`, the legacy wallet, and
//! opens **WalletGreen containers**. `spec/12-roadmap.md` lists `WalletGreen`
//! under "things that must not be copied", and this port has no reason to
//! grow a second container format: the wallet core here reads and writes the
//! modern `WalletBackend` container, which is what `wrkz-wallet`,
//! `wrkz-wallet-api` and the Flutter apps use.
//!
//! So: **the API is the C++ service's, the container is the modern one.** An
//! integration that speaks JSON-RPC to `wrkz-service` needs no change, and a
//! container from `wrkz-wallet`, `wrkz-wallet-api` or Pluton opens as it is.
//! A WalletGreen container from the C++ service does not, and no converter is
//! planned here; the C++ `wrkz-walletupgrader` handles the case if one ever
//! turns up. Said here, in `--help` and in `docs/SERVICE.md`, because it is
//! the one thing that is not drop-in.
//!
//! Two consequences follow from the container, and both are answered rather
//! than hidden:
//!
//! - `getBlockHashes` and the block-ranged `getTransactions` /
//!   `getTransactionHashes` want a block hash per height. A WalletGreen
//!   container stores every one; a modern container stores the last 100 plus a
//!   sparse checkpoint, because that is all syncing needs. The hashes the
//!   container does not have are fetched from the daemon, which is what
//!   [`methods::block_hashes`] does, and `blockCount` is capped
//!   ([`MAX_BLOCK_COUNT`]) so one call cannot ask for a million round trips.
//! - `export` writes the modern container's JSON, not WalletGreen's.
//!
//! # Authentication
//!
//! The password is a **member of the request object**, not a header, and is
//! compared as `cn_slow_hash_v0(password)` against the same hash of
//! `--rpc-password` (`PaymentServiceJsonRpcServer.cpp:186`). That is a
//! two-megabyte scratchpad per request, which is the C++'s choice and is kept:
//! an integration that already sends the password to a service on loopback is
//! not made safer by changing it, and changing it would break nothing but
//! would make the two implementations disagree about cost.
//! `--rpc-legacy-security` turns the check off entirely, as it does there.
//!
//! The same holds on the local socket `--bind-ipc-path` serves on instead of
//! the port ([`serve`]): the check lives in the JSON-RPC layer, which does not
//! know the transport.
//!
//! # Notifications and logging
//!
//! `--tx-notify`, `--tx-confirmed-notify` and `--notify-during-sync` are
//! [`notify`]. `--log-level` and `-l` go through `wrkz_wallet::logging` onto
//! the one process logger, in the C++'s `Logging::Level` numbering (0 fatal
//! to 5 trace, default 3), with `service.log` as the default file the C++
//! always writes. No password, key or request body is logged; a request is a
//! debug line with its peer, path and status, and its method name.

#![forbid(unsafe_code)]

pub mod errors;
pub mod methods;
pub mod notify;
pub mod rpc;
pub mod serve;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use wrkz_rpc::json::Json;
use wrkz_rpc::log::Level;
use wrkz_wallet::api::{DaemonFactory, OpenWallet};

pub use errors::{AppError, Result};

/// `CryptoNote::SERVICE_DEFAULT_PORT` (`config/CryptoNoteConfig.h:539`).
pub const DEFAULT_PORT: u16 = wrkz_wallet::api::SERVICE_DEFAULT_PORT;

/// The most blocks one `getBlockHashes`, `getTransactions` or
/// `getTransactionHashes` call may span.
///
/// Not in the C++, which answers from a container that holds every block hash
/// and so has nothing to bound. Here a range wider than the container's own
/// recent window is filled from the daemon, and a caller asking for a million
/// blocks would be asking the daemon for a million blocks. Callers of the C++
/// service use windows of a few hundred.
pub const MAX_BLOCK_COUNT: u64 = 1000;

/// `WalletServiceConfiguration` (`walletservice/WalletServiceConfiguration.h`),
/// reduced to what this port acts on.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    /// `--bind-address`.
    pub bind_address: String,
    /// `--bind-port`.
    pub bind_port: u16,
    /// `--bind-ipc-path`: serve on this local socket instead of the port.
    /// Empty for the port.
    pub bind_ipc_path: String,
    /// `--bind-ipc-mode`, default [`wrkz_rpc::ipc::DEFAULT_MODE`] (`0600`).
    pub bind_ipc_mode: u32,
    /// `--bind-ipc-group`; empty keeps the process's group.
    pub bind_ipc_group: String,
    /// `--rpc-password`. Required unless `--rpc-legacy-security`.
    pub rpc_password: String,
    /// `--rpc-legacy-security`: no password check at all.
    pub legacy_security: bool,
    /// `--enable-cors <domain>`; empty adds no header.
    pub cors_header: String,
    /// `--container-file`.
    pub container_file: String,
    /// `--container-password`.
    pub container_password: String,
    /// `--daemon-address`. An absolute path, `@name` or `ipc://path` goes over
    /// the daemon's local socket instead.
    pub daemon_address: String,
    /// `--daemon-port`.
    pub daemon_port: u16,
    /// Whether the daemon URL is https. Not a C++ option; `--daemon-address`
    /// may name a scheme instead.
    pub daemon_ssl: bool,
    /// `--log-level`, in `Logging::Level` numbering: 0 fatal to 5 trace,
    /// default 3 (`WalletServiceConfiguration.h:60`).
    pub log_level: i32,
    /// `-l`/`--log-file`; `None` is the C++'s `service.log`.
    pub log_file: Option<String>,
    /// `--tx-notify`: a command or an `http(s)://` URL; empty for none.
    pub tx_notify: String,
    /// `--tx-confirmed-notify`: the same, once a transaction is in a block.
    pub tx_confirmed_notify: String,
    /// `--notify-during-sync`.
    pub notify_during_sync: bool,
    /// `--scan-height`, used only when generating a container.
    pub scan_height: u64,
    /// `--skip-coinbase-transactions`. Coinbases are scanned unless this is
    /// set, as in `wrkz-wallet-api`.
    pub skip_coinbase_transactions: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            bind_address: "127.0.0.1".into(),
            bind_port: DEFAULT_PORT,
            bind_ipc_path: String::new(),
            bind_ipc_mode: wrkz_rpc::ipc::DEFAULT_MODE,
            bind_ipc_group: String::new(),
            rpc_password: String::new(),
            legacy_security: false,
            cors_header: String::new(),
            container_file: String::new(),
            container_password: String::new(),
            daemon_address: "127.0.0.1".into(),
            daemon_port: wrkz_wallet::api::RPC_DEFAULT_PORT,
            daemon_ssl: false,
            log_level: 3,
            log_file: None,
            tx_notify: String::new(),
            tx_confirmed_notify: String::new(),
            notify_during_sync: false,
            scan_height: 0,
            skip_coinbase_transactions: false,
        }
    }
}

/// The open container, the configuration, and the hashed RPC password.
pub struct ServiceState {
    pub config: ServiceConfig,
    /// `ConfigurationManager::rpcSecret`: `cn_slow_hash_v0` of the password
    /// (`main.cpp`), compared against the same hash of what a request sends.
    rpc_secret: [u8; 32],
    /// The one container. Unlike `wrkz-wallet-api` there is no "no wallet
    /// open" state: the service opens its container at startup and exits if
    /// it cannot.
    pub wallet: RwLock<OpenWallet>,
    /// `--tx-notify` and `--tx-confirmed-notify`, and what they remember.
    pub notify: notify::TxNotifiers,
    /// Set when the sync thread should stop.
    pub stopping: Arc<AtomicBool>,
}

impl ServiceState {
    /// The service over `wallet`, with its notification hooks started when
    /// the configuration names any.
    pub fn new(config: ServiceConfig, wallet: OpenWallet) -> ServiceState {
        let rpc_secret = wrkz_pow::cn_slow_hash_v0(config.rpc_password.as_bytes());
        let notify = notify::TxNotifiers::new(
            &config.tx_notify,
            &config.tx_confirmed_notify,
            config.notify_during_sync,
            wallet.wallet(),
        );
        ServiceState {
            config,
            rpc_secret,
            wallet: RwLock::new(wallet),
            notify,
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `processJsonRpcRequest` (`PaymentServiceJsonRpcServer.cpp:180`): the
    /// password member hashed with `cn_slow_hash_v0` against `rpcSecret`.
    ///
    /// Compared in constant time, which the C++ does not do — it uses
    /// `operator!=` on the hash. Both compare 32 bytes of a slow hash of the
    /// secret, so this changes no answer; it only removes a timing signal that
    /// costs nothing to remove.
    pub fn password_matches(&self, given: Option<&Json>) -> bool {
        if self.config.legacy_security {
            return true;
        }
        let Some(Json::Str(password)) = given else { return false };
        let hashed = wrkz_pow::cn_slow_hash_v0(password.as_bytes());
        wrkz_rpc::server::constant_time_eq(&hashed, &self.rpc_secret)
    }

    /// The container, for reading.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, OpenWallet> {
        self.wallet.read().unwrap_or_else(|p| p.into_inner())
    }

    /// The container, for writing.
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, OpenWallet> {
        self.wallet.write().unwrap_or_else(|p| p.into_inner())
    }
}

/// Everything one JSON-RPC request needs, so a test can drive [`dispatch`]
/// without a socket.
pub fn dispatch(state: &ServiceState, body: &[u8]) -> Json {
    let Ok(request) = wrkz_rpc::json::parse(body, rpc::parse_limits()) else {
        return rpc::parse_error();
    };
    let env = rpc::Envelope::of(&request);

    if !state.password_matches(env.password) {
        return rpc::invalid_password(env.id);
    }

    let Some(Json::Str(method)) = env.method else {
        // Missing, or present and not a string: both are "Invalid Request".
        return rpc::invalid_request(env.id);
    };

    // The name only, and escaped: never a parameter, which is where keys,
    // seeds and passwords travel.
    wrkz_wallet::logging::log(Level::Debug, format_args!("JSON-RPC {method:?}"));

    let Some(handler) = methods::lookup(method) else {
        return rpc::method_not_found(env.id);
    };

    match handler(state, &env.params) {
        Ok(value) => rpc::result(env.id, value),
        Err(e) => rpc::app_error(env.id, &e),
    }
}

/// The factory `wrkz-service` runs with, the same one `wrkz-wallet-api` uses.
pub fn real_daemon_factory() -> DaemonFactory {
    wrkz_wallet::api::real_daemon_factory()
}
