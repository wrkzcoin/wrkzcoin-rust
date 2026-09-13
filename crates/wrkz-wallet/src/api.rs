// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet-api`: the wallet's HTTP API, route for route with the C++
//! `ApiDispatcher` (`src/walletapi/ApiDispatcher.cpp`, `.h`).
//!
//! The dispatcher is a pure function — [`dispatch`] takes a parsed request and
//! an [`ApiState`] and returns a response — so every route, status code and
//! error body is testable without a socket, a daemon or a terminal.
//! [`serve`] is the thin listener that puts it on a port, reusing
//! [`wrkz_rpc::http`] for HTTP/1.1 rather than growing a second server.
//!
//! # Routes
//!
//! Every route below is one `ApiDispatcher::setupRoutes` registration
//! (`ApiDispatcher.cpp:121`), in its order. "State" is the `WalletState`
//! argument; "view" is whether a view-only wallet may call it.
//!
//! | Method | Path | Handler (C++) | State | View |
//! | --- | --- | --- | --- | --- |
//! | POST | `/wallet/open` | `openWallet` `:642` | closed | yes |
//! | POST | `/wallet/import/key` | `keyImportWallet` `:657` | closed | yes |
//! | POST | `/wallet/import/seed` | `seedImportWallet` `:692` | closed | yes |
//! | POST | `/wallet/import/view` | `importViewWallet` `:720` | closed | yes |
//! | POST | `/wallet/create` | `createWallet` `:757` | closed | yes |
//! | POST | `/addresses/create` | `createAddress` `:772` | open | **no** |
//! | POST | `/addresses/import` | `importAddress` `:786` | open | **no** |
//! | POST | `/addresses/import/deterministic` | `importDeterministicAddress` `:813` | open | **no** |
//! | POST | `/addresses/import/view` | `importViewAddress` `:840` | open | yes |
//! | POST | `/addresses/validate` | `validateAddress` `:868` | any | yes |
//! | POST | `/transactions/send/prepared` | `sendPreparedTransaction` `:918` | open | **no** |
//! | POST | `/transactions/prepare/basic` | `makeBasicTransaction(false)` `:947` | open | **no** |
//! | POST | `/transactions/send/basic` | `makeBasicTransaction(true)` `:947` | open | **no** |
//! | POST | `/transactions/prepare/advanced` | `makeAdvancedTransaction(false)` `:1005` | open | **no** |
//! | POST | `/transactions/send/advanced` | `makeAdvancedTransaction(true)` `:1005` | open | **no** |
//! | POST | `/transactions/send/sweep` | `sendSweepTransaction` `:1106` | open | **no** |
//! | POST | `/transactions/send/sweep/all` | `sendSweepAllTransaction` `:1152` | open | **no** |
//! | POST | `/export/json` | `exportToJSON` `:1194` | open | yes |
//! | DELETE | `/wallet` | `closeWallet` `:1220` | open | yes |
//! | DELETE | `/addresses/{address}` | `deleteAddress` `:1229` | open | yes |
//! | DELETE | `/transactions/prepared/{hash}` | `deletePreparedTransaction` `:1249` | open | **no** |
//! | PUT | `/save` | `saveWallet` `:1275` | open | yes |
//! | PUT | `/reset` | `resetWallet` `:1288` | open | yes |
//! | PUT | `/node` | `setNodeInfo` `:1330` | open | yes |
//! | PUT | `/sync/refresh` | `refreshSync` `:1357` | open | yes |
//! | GET | `/node` | `getNodeInfo` `:1385` | open | yes |
//! | GET | `/keys` | `getPrivateViewKey` `:1411` | open | yes |
//! | GET | `/keys/{address}` | `getSpendKeys` `:1422` | open | **no** |
//! | GET | `/keys/mnemonic/{address}` | `getMnemonicSeed` `:1447` | open | **no** |
//! | GET | `/status` | `getStatus` `:1471` | open | yes |
//! | GET | `/addresses` | `getAddresses` `:1511` | open | yes |
//! | GET | `/addresses/primary` | `getPrimaryAddress` `:1521` | open | yes |
//! | GET | `/addresses/{address}/{paymentID}` | `createIntegratedAddress` `:1531` | open | yes |
//! | GET | `/transactions` | `getTransactions` `:1555` | open | yes |
//! | GET | `/transactions/unconfirmed` | `getUnconfirmedTransactions` `:1567` | open | yes |
//! | GET | `/transactions/unconfirmed/{address}` | `getUnconfirmedTransactionsForAddress` `:1579` | open | yes |
//! | GET | `/transactions/{startHeight}` | `getTransactionsFromHeight` `:1616` | open | yes |
//! | GET | `/transactions/{startHeight}/{endHeight}` | `getTransactionsFromHeightToHeight` `:1646` | open | yes |
//! | GET | `/transactions/address/{address}/{startHeight}` | `…FromHeightWithAddress` `:1694` | open | yes |
//! | GET | `/transactions/address/{address}/{startHeight}/{endHeight}` | `…ToHeightWithAddress` `:1751` | open | yes |
//! | GET | `/transactions/privatekey/{hash}` | `getTxPrivateKey` `:1900` | open | **no** |
//! | GET | `/transactions/hash/{hash}` | `getTransactionDetails` `:1822` | open | yes |
//! | GET | `/transactions/paymentid/{hash}` | `getTransactionsByPaymentId` `:1867` | open | yes |
//! | GET | `/transactions/paymentid` | `getTransactionsWithPaymentId` `:1891` | open | yes |
//! | GET | `/balance` | `getBalance` `:1841` | open | yes |
//! | GET | `/balance/{address}` | `getBalanceForAddress` `:1854` | open | yes |
//! | GET | `/balances` | `getBalances` `:1874` | open | yes |
//! | OPTIONS | `*` | `handleOptions` `:1926` | — | — |
//!
//! `{address}` is `ApiConstants::addressRegex` — the literal prefix `Wrkz`
//! followed by 94 alphanumerics — `{hash}` is 64 hex characters, and
//! `{paymentID}` is 16 or 64 hex characters (`Constants.h`). A path that does
//! not match any of these is **404**, before authentication, because httplib
//! never reaches the middleware for it.
//!
//! # Middleware
//!
//! `ApiDispatcher::middleware` (`ApiDispatcher.cpp:501`), in order:
//!
//! 1. The `Access-Control-Allow-Origin` header, when `--enable-cors` set one.
//! 2. `X-API-KEY`: missing or wrong is **401** with an empty body. The header
//!    is compared by its PBKDF2-SHA256 hash over a per-process random salt,
//!    10,000 iterations (`ApiConstants::PBKDF2_ITERATIONS`), so the comparison
//!    does not run over the password itself.
//! 3. The wallet state: an operation needing an open wallet with none open, or
//!    a `POST /wallet/*` with one already open, is **403** with an empty body.
//! 4. A view wallet calling a route that forbids it is **400** with
//!    `{"errorCode": 39, "errorMessage": …}`.
//! 5. The handler. An `Error` is **400** with `{"errorCode", "errorMessage"}`;
//!    otherwise the handler's own status code.
//! 6. A missing or mistyped JSON parameter is **400** with an *empty* body
//!    (the C++ catches `json::exception` and only sets `res.status`).
//!
//! # Transport
//!
//! [`serve`] puts the dispatcher on [`crate::listen`]: TCP on
//! `--rpc-bind-ip`, a second listener on IPv6 with `--rpc-use-ipv6` and
//! `--rpc-bind-ipv6-address`, and a local socket with `--rpc-ipc-path`,
//! `--rpc-ipc-mode` and `--rpc-ipc-group` — the three `httplib::Server`s of
//! `ApiDispatcher.cpp:79-121`, every one with the same routes.
//!
//! - **The IPC socket still needs `X-API-KEY`**, as the C++'s help text says
//!   ("The X-API-KEY password is still required on this socket",
//!   `ParseArguments.cpp:104`): its middleware has no transport exception, and
//!   neither does [`dispatch`]. The socket file's mode decides who may connect
//!   at all; the key decides who may do anything.
//! - An IPv6 or IPC listener that cannot come up is a warning, and the API
//!   keeps serving on the others (`ApiDispatcher.cpp:442`, `:453`). Only the
//!   IPv4 listener is fatal.
//! - A JSON answer of at least a kilobyte is gzipped for a client that sends
//!   `Accept-Encoding: gzip`, as `cpp-httplib` does when built with zlib.
//! - Each request is logged — peer, method, path, status, time — at the level
//!   [`crate::listen`] describes. The C++ prints every request and its body on
//!   standard output, and on a wrong key prints the expected one next to it
//!   (`ApiDispatcher.cpp:528-547`, `:645`); neither the body nor either key is
//!   ever logged here.
//!
//! # Locking
//!
//! `ApiDispatcher::middleware` takes its mutex exclusively only to open or
//! close a wallet and shared for everything else; sends serialise on
//! `WalletBackend::m_transactionMutex`, and the synchroniser applies blocks
//! under locks of its own (`ApiDispatcher.cpp:512-526`). Here the synchroniser
//! owns the container, and the same guarantees come from three locks, always
//! taken in this order:
//!
//! 1. **the transaction lock**, by every route that changes anything, for the
//!    whole change. Two sends never build at once, so they cannot pick the same
//!    inputs, and nothing closes, resets or re-points the wallet under a send.
//! 2. **the working wallet**, by the sync thread for a whole step — daemon
//!    round trip included, so a step is applied all at once — and by a change
//!    only while it applies. A send builds its transaction, searches its proof
//!    of work and relays it without it, and takes it only to record the send.
//! 3. **the view**, only to swap an `Arc`.
//!
//! [`RouteSpec::write`](route::RouteSpec::write) decides which a route gets. A
//! read-only route is handed the published [`WalletView`] and nothing else, so
//! it cannot wait for a sync round trip, a proof of work or a relay; [`view`]
//! says what it sees.
//!
//! # Notifications
//!
//! `--tx-notify` runs for every transaction a sync step records, as the C++'s
//! `onTransaction` subscriber does, and `--notify-during-sync` lets it run for
//! one a day or more behind the daemon; see [`notify`].
//!
//! # What is not here
//!
//! - **`nodeFee`.** `Nigel` fills it from a `/fee` route that `RpcServer.cpp`
//!   does not serve (see `wrkz_rpc`'s "Routes the C++ does not have"), so it is
//!   always `0` and `""` here, which is what a WrkzCoin daemon produces.

pub mod handlers;
pub mod notify;
pub mod pretty;
pub mod route;
pub mod serve;
pub mod view;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use sha2::{Digest, Sha256};
use wrkz_rpc::server::constant_time_eq;
use zeroize::Zeroizing;

use crate::crypto;
use crate::daemon::{
    self, Daemon, GlobalIndexes, Info, RandomOuts, SendResult, SyncRequest, TransactionsStatus, WalletSyncData,
};
use crate::file::{Hex32, PublicKey, Result, SecretKey, Transaction, Wallet, WalletError};
use crate::sync::{SyncConfig, SyncDaemon, SyncStep, Synchronizer};
use crate::transfer::{PreparedTransaction, TransferDaemon};
use wrkz_rpc::log::Level;
use wrkz_rpc::notify::Notifier;

pub use route::dispatch;
pub use view::WalletView;

////////////////////////
/* CONFIGURATION      */
////////////////////////

/// `ApiConfig` (`src/walletapi/ParseArguments.h:14`).
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// `--rpc-bind-ip`, default `127.0.0.1`.
    pub rpc_bind_ip: String,
    /// `-p`/`--port`, default `CryptoNote::SERVICE_DEFAULT_PORT`.
    pub port: u16,
    /// `--rpc-bind-ipv6-address`; empty disables the IPv6 listener.
    pub rpc_bind_ipv6_address: String,
    /// `--rpc-use-ipv6`. The IPv6 listener runs only with this *and* an
    /// address, as `m_ipv6Host` does (`ApiDispatcher.cpp:48`).
    pub rpc_use_ipv6: bool,
    /// `--rpc-ipc-path`: an absolute path or `@name`; empty for no IPC socket.
    pub rpc_ipc_path: String,
    /// `--rpc-ipc-mode`, default [`wrkz_rpc::ipc::DEFAULT_MODE`] (`0600`).
    pub rpc_ipc_mode: u32,
    /// `--rpc-ipc-group`; empty keeps the process's group.
    pub rpc_ipc_group: String,
    /// `-r`/`--rpc-password`. Required unless `--help` or `--version`.
    pub rpc_password: String,
    /// `--enable-cors <domain>`; empty adds no header at all.
    pub cors_header: String,
    /// `--log-level`, `0`..=`5` in `Logger::LogLevel` numbering, default `0`
    /// (disabled); see [`crate::logging`].
    pub log_level: i32,
    /// `--log-file`.
    pub log_file: Option<String>,
    /// `--no-console`.
    pub no_console: bool,
    /// `--threads`: the threads that scan downloaded blocks
    /// ([`crate::sync::SyncConfig::scan_threads`]), which is what
    /// `m_walletSyncThreads` is handed to in the C++. Defaults to one per core,
    /// at most sixteen, as `SyncConfig` does; the C++ default is every core.
    pub threads: u32,
    /// `--tx-notify`: a command or an `http(s)://` URL ([`wrkz_rpc::notify`]);
    /// empty for none.
    pub tx_notify: String,
    /// `--notify-during-sync`.
    pub notify_during_sync: bool,
    /// `--skip-coinbase-transactions` / `--skip-coinbase`. Coinbases are
    /// scanned unless this is set, unlike the C++, which skipped them unless
    /// `--scan-coinbase-transactions` was given; that flag is still accepted
    /// and changes nothing.
    pub skip_coinbase_transactions: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            rpc_bind_ip: "127.0.0.1".into(),
            port: SERVICE_DEFAULT_PORT,
            rpc_bind_ipv6_address: String::new(),
            rpc_use_ipv6: false,
            rpc_ipc_path: String::new(),
            rpc_ipc_mode: wrkz_rpc::ipc::DEFAULT_MODE,
            rpc_ipc_group: String::new(),
            rpc_password: String::new(),
            cors_header: String::new(),
            log_level: 0,
            log_file: None,
            no_console: false,
            threads: crate::sync::SyncConfig::default().scan_threads as u32,
            tx_notify: String::new(),
            notify_during_sync: false,
            skip_coinbase_transactions: false,
        }
    }
}

/// `CryptoNote::SERVICE_DEFAULT_PORT` (`config/CryptoNoteConfig.h:539`).
pub const SERVICE_DEFAULT_PORT: u16 = 7856;

/// `CryptoNote::RPC_DEFAULT_PORT`, the default a request's `daemonPort` takes.
pub const RPC_DEFAULT_PORT: u16 = wrkz_primitives::constants::RPC_DEFAULT_PORT;

////////////////////////
/* DAEMON PLUMBING    */
////////////////////////

/// Everything an open wallet asks of a daemon: sync and transaction
/// construction. A trait so a test can hand the dispatcher a canned daemon and
/// drive every route offline.
pub trait WalletDaemon: SyncDaemon + TransferDaemon + Send + Sync {}

impl<T: SyncDaemon + TransferDaemon + Send + Sync + ?Sized> WalletDaemon for T {}

/// A shared [`WalletDaemon`], so [`Synchronizer`] can be monomorphised once,
/// and so a send can use the daemon without holding the wallet whose
/// synchroniser owns it ([`WalletView::daemon`]). A clone shares the client.
#[derive(Clone)]
pub struct DynDaemon(pub Arc<dyn WalletDaemon>);

impl SyncDaemon for DynDaemon {
    fn wallet_sync_data(&self, req: &SyncRequest) -> daemon::Result<WalletSyncData> {
        self.0.wallet_sync_data(req)
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> daemon::Result<GlobalIndexes> {
        self.0.global_indexes_for_range(start, end)
    }

    fn transactions_status(&self, hashes: &[String]) -> daemon::Result<TransactionsStatus> {
        self.0.transactions_status(hashes)
    }

    fn info(&self) -> daemon::Result<Info> {
        self.0.info()
    }
}

impl TransferDaemon for DynDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> daemon::Result<RandomOuts> {
        self.0.random_outs(amounts, outs_count)
    }

    fn send_raw_transaction(&self, tx_hex: &str) -> daemon::Result<SendResult> {
        self.0.send_raw_transaction(tx_hex)
    }
}

/// How `(host, port, ssl)` becomes a daemon. The production one builds a
/// [`Daemon`]; a test's returns whatever it likes.
pub type DaemonFactory =
    Box<dyn Fn(&str, u16, bool) -> std::result::Result<Box<dyn WalletDaemon>, String> + Send + Sync>;

/// Build an [`OpenWallet`] over `wallet`: the daemon from `make_daemon`, the
/// synchronizer, and a first `/info` so a status or a balance read straight
/// after is not measured against height zero.
///
/// Shared with `wrkz-service`, which opens one container at startup and holds
/// it open; `ApiDispatcher`'s own `install` is this plus storing the result in
/// the dispatcher's state.
#[allow(clippy::too_many_arguments)]
pub fn open_container(
    wallet: Wallet,
    filename: String,
    password: Zeroizing<String>,
    host: String,
    port: u16,
    ssl: bool,
    skip_coinbase_transactions: bool,
    make_daemon: &DaemonFactory,
) -> std::result::Result<OpenWallet, String> {
    let config = SyncConfig { skip_coinbase_transactions, ..SyncConfig::default() };
    open_container_with(wallet, filename, password, host, port, ssl, config, make_daemon)
}

/// [`open_container`] with every synchronizer setting given, which is how
/// `--threads` reaches [`SyncConfig::scan_threads`].
#[allow(clippy::too_many_arguments)]
pub fn open_container_with(
    wallet: Wallet,
    filename: String,
    password: Zeroizing<String>,
    host: String,
    port: u16,
    ssl: bool,
    config: SyncConfig,
    make_daemon: &DaemonFactory,
) -> std::result::Result<OpenWallet, String> {
    let daemon = DynDaemon(Arc::from(make_daemon(&host, port, ssl)?));
    let mut open = OpenWallet {
        sync: Synchronizer::with_config(daemon, wallet, config),
        filename,
        password,
        daemon_host: host,
        daemon_port: port,
        daemon_ssl: ssl,
        prepared: Vec::new(),
        peer_count: 0,
        hashrate: 0,
        sync_gap: None,
        stop: Arc::new(AtomicBool::new(false)),
    };
    open.refresh_info();
    Ok(open)
}

/// The factory `wrkz-wallet-api` runs with.
///
/// A `daemonHost` in one of the IPC forms (`/path`, `@name`, `ipc://path`) goes
/// over a local socket and ignores `daemonPort` and `daemonSSL`; see
/// [`crate::ipc`].
pub fn real_daemon_factory() -> DaemonFactory {
    Box::new(|host, port, ssl| {
        if crate::ipc::is_ipc_address(host) {
            let daemon = crate::ipc::IpcDaemon::new(host)?;
            return Ok(Box::new(daemon));
        }
        let scheme = if ssl { "https" } else { "http" };
        let daemon = Daemon::new(&format!("{scheme}://{host}:{port}")).map_err(|e| e.to_string())?;
        Ok(Box::new(daemon))
    })
}

////////////////////////
/* THE OPEN WALLET    */
////////////////////////

/// The `WalletBackend` an open wallet is: the container, its synchronizer, the
/// file it came from, and the transactions prepared but not yet sent.
pub struct OpenWallet {
    /// Owns the [`Wallet`]; `sync.wallet()` is the container.
    pub sync: Synchronizer<DynDaemon>,
    /// `WalletBackend::getWalletLocation`.
    pub filename: String,
    /// Kept because `PUT /save` writes the same file with the same password,
    /// and zeroized when the wallet closes.
    pub password: Zeroizing<String>,
    pub daemon_host: String,
    pub daemon_port: u16,
    pub daemon_ssl: bool,
    /// `WalletBackend::m_preparedTransactions`, keyed by hash.
    pub prepared: Vec<PreparedTransaction>,
    /// `Nigel::peerCount`: `incoming_connections_count + outgoing…`.
    pub peer_count: u64,
    /// `Nigel::hashrate`: the last `/info` difficulty over the block time.
    pub hashrate: u64,
    /// Where the wallet stopped when a lite node could not serve the range.
    pub sync_gap: Option<(u64, u64)>,
    /// Set when the background sync thread should stop.
    pub stop: Arc<AtomicBool>,
}

/// One save at a time, of whichever copy: [`Wallet::save`] writes
/// `<file>.tmp` and renames it over the file, so two saves of the same wallet
/// at once — `PUT /save` from the published view while `PUT /reset` saves the
/// working wallet — would write the same temporary file.
static SAVING: Mutex<()> = Mutex::new(());

pub(crate) fn one_save_at_a_time() -> MutexGuard<'static, ()> {
    SAVING.lock().unwrap_or_else(|p| p.into_inner())
}

/// `WalletBackend::save` logs a save that fails at `FATAL`
/// (`WalletBackend.cpp:727`, `:744`). The path is the operator's own and
/// carries no secret; the password is not in the error.
pub(crate) fn log_failed_save(filename: &str, saved: &Result<()>) {
    if let Err(e) = saved {
        crate::logging::log(Level::Error, format_args!("Failed to save wallet {filename}: {e}"));
    }
}

impl OpenWallet {
    /// The wallet container.
    pub fn wallet(&self) -> &Wallet {
        self.sync.wallet()
    }

    /// The wallet container, mutably.
    pub fn wallet_mut(&mut self) -> &mut Wallet {
        self.sync.wallet_mut()
    }

    /// The height every balance and every send is measured against.
    pub fn network_height(&self) -> u64 {
        self.sync.daemon_state().network_block_count
    }

    /// `WalletBackend::save`.
    pub fn save(&self) -> Result<()> {
        let _one = one_save_at_a_time();
        let saved = self.wallet().save(&self.filename, &self.password);
        log_failed_save(&self.filename, &saved);
        saved
    }

    /// `WalletBackend::liteRescanImpact` (`WalletBackend.cpp:1809`):
    /// `(liteStartHeight, transactionsLost)`, both zero when nothing is at
    /// stake.
    pub fn lite_rescan_impact(&self, scan_height: u64) -> (u64, u64) {
        let lite_start = self.sync.daemon_state().lite_start_height;
        if lite_start == 0 || scan_height >= lite_start {
            return (0, 0);
        }
        let lost =
            self.wallet().transactions().iter().filter(|t| t.block_height != 0 && t.block_height < lite_start).count();
        (lite_start, lost as u64)
    }

    /// Fold a fresh `/info` into the cached peer count and hashrate, the way
    /// `Nigel::getDaemonInfo` does (`Nigel.cpp:885`).
    pub fn refresh_info(&mut self) {
        if let Ok(info) = self.sync.refresh_info() {
            self.peer_count = info.incoming_connections_count + info.outgoing_connections_count;
            self.hashrate = info.difficulty / wrkz_primitives::constants::DIFFICULTY_TARGET;
        }
        self.sync_gap = self.sync.sync_gap();
    }

    /// One round of the background sync loop. Returns how long to wait next.
    pub fn sync_once(&mut self) -> std::time::Duration {
        self.sync_round().wait
    }

    /// One round of the background sync loop, with what it did.
    pub fn sync_round(&mut self) -> SyncRound {
        let forks_before = self.sync.forks_resolved();
        let step = self.sync.sync_step();
        let wait = match &step {
            SyncStep::Processed { .. } => std::time::Duration::from_millis(0),
            SyncStep::Synced { .. } => std::time::Duration::from_secs(2),
            SyncStep::Idle { backoff } => (*backoff).max(std::time::Duration::from_millis(200)),
            SyncStep::Failed { backoff, .. } => *backoff,
            SyncStep::Gap { covered_to, daemon_serves_from } => {
                self.sync_gap = Some((*covered_to, *daemon_serves_from));
                std::time::Duration::from_secs(10)
            }
        };
        SyncRound { forks: self.sync.forks_resolved().saturating_sub(forks_before), step, wait }
    }
}

/// What one round of the background sync did: [`OpenWallet::sync_round`].
#[derive(Debug)]
pub struct SyncRound {
    /// The step, as [`Synchronizer::sync_step`] reported it.
    pub step: SyncStep,
    /// How long to wait before the next round.
    pub wait: std::time::Duration,
    /// Forks unwound during the step.
    pub forks: u64,
}

/// The sync thread's log: what `WalletSynchronizer` logs through
/// `Logger::logger` (`WalletSynchronizer.cpp:377-453`), from the front end
/// rather than from the wallet core, which does not log.
///
/// A daemon that stops answering is reported once, at warning, and again at
/// info when syncing resumes; the retries in between are debug lines, so a node
/// that is down for an hour does not fill the log. A stall at a lite node's
/// floor is reported once for as long as it lasts.
#[derive(Debug, Default)]
pub struct SyncLog {
    failing: bool,
    gap: Option<(u64, u64)>,
}

impl SyncLog {
    /// Log one round of `open`'s sync.
    pub fn record(&mut self, round: &SyncRound, open: &OpenWallet) {
        use crate::logging::{enabled, log};

        if round.forks > 0 {
            // `WalletSynchronizer.cpp:377`.
            log(Level::Info, format_args!("Blockchain forked, resolving... ({} fork(s) unwound)", round.forks));
        }

        match &round.step {
            SyncStep::Processed { blocks, height, .. } => {
                self.resumed(*height);
                if enabled(Level::Info) {
                    // `WalletSynchronizer.cpp:403`.
                    for hash in open.sync.last_step_added() {
                        log(Level::Info, format_args!("Adding transaction: {}", hash.to_hex()));
                    }
                }
                log(Level::Debug, format_args!("Processed {blocks} block(s), the wallet is at height {height}"));
            }
            SyncStep::Synced { height } => {
                self.resumed(*height);
                log(Level::Trace, format_args!("At the daemon's top block, height {height}"));
            }
            SyncStep::Idle { .. } => log(Level::Trace, format_args!("Nothing to sync this round")),
            SyncStep::Failed { error, backoff } => {
                if self.failing {
                    log(Level::Debug, format_args!("Still cannot sync: {error}; retrying in {}s", backoff.as_secs()));
                } else {
                    self.failing = true;
                    log(Level::Warn, format_args!("Failed to sync with the daemon: {error}. Retrying."));
                }
            }
            SyncStep::Gap { covered_to, daemon_serves_from } => {
                if self.gap != Some((*covered_to, *daemon_serves_from)) {
                    self.gap = Some((*covered_to, *daemon_serves_from));
                    log(
                        Level::Warn,
                        format_args!(
                            "Sync has stopped: the daemon holds no blocks below {daemon_serves_from}, and this wallet \
                             has only scanned to {covered_to}. Connect a daemon that holds the whole chain."
                        ),
                    );
                }
            }
        }
    }

    fn resumed(&mut self, height: u64) {
        if self.failing {
            self.failing = false;
            crate::logging::log(Level::Info, format_args!("Syncing with the daemon again, at height {height}"));
        }
        self.gap = None;
    }
}

////////////////////////
/* STATE              */
////////////////////////

/// Everything the dispatcher reads: the configuration, the hashed API key, and
/// the wallet — if one is open.
pub struct ApiState {
    pub config: ApiConfig,
    /// `ApiDispatcher::m_salt`, 16 random bytes per process.
    salt: [u8; crypto::SALT_SIZE],
    /// `ApiDispatcher::m_hashedPassword`.
    hashed_password: Zeroizing<String>,
    /// A salted SHA-256 of the key, which turns a wrong `X-API-KEY` away
    /// before the PBKDF2 ([`ApiState::password_matches`]).
    quick_digest: Zeroizing<[u8; 32]>,
    /// `ApiDispatcher::m_walletBackend`: the working wallet. The sync thread
    /// holds it for a whole step, the daemon round trip included, and a change
    /// holds it while it applies. Nothing that only reads takes it
    /// ([`ApiState::view`]).
    pub wallet: RwLock<Option<OpenWallet>>,
    /// What the read-only routes answer from: the working wallet as it stood
    /// after its last completed change ([`view`]).
    view: RwLock<Option<Arc<WalletView>>>,
    /// `WalletBackend::m_transactionMutex`, held by every route that changes
    /// the wallet for the whole of the change: no send can pick inputs another
    /// send is still spending, and nothing closes, resets or re-points the
    /// wallet while a send builds and relays.
    transactions: Mutex<()>,
    make_daemon: DaemonFactory,
    /// `ApiDispatcher::m_txNotifier`, when `--tx-notify` was given.
    tx_notifier: Option<Notifier>,
}

impl ApiState {
    /// A state with no wallet open, whose `X-API-KEY` is `config.rpc_password`,
    /// with its `--tx-notify` hook started when there is one.
    pub fn new(config: ApiConfig, make_daemon: DaemonFactory) -> ApiState {
        let salt = crypto::random_salt();
        let hashed_password = hash_password(&config.rpc_password, &salt);
        let quick_digest = quick_digest(&config.rpc_password, &salt);
        let tx_notifier = (!config.tx_notify.trim().is_empty()).then(|| notify::hook("tx-notify", &config.tx_notify));
        ApiState {
            config,
            salt,
            hashed_password,
            quick_digest,
            wallet: RwLock::new(None),
            view: RwLock::new(None),
            transactions: Mutex::new(()),
            make_daemon,
            tx_notifier,
        }
    }

    /// The open wallet as the read-only routes see it, or `None` with none
    /// open. Never waits for the sync thread, a send or the daemon: the lock
    /// behind it is only ever held to swap an `Arc`.
    pub fn view(&self) -> Option<Arc<WalletView>> {
        self.view.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Publish `open`'s current state as the view, or clear it. Called with
    /// the working wallet held, after each change.
    pub fn publish(&self, open: Option<&OpenWallet>) {
        let view = open.map(|o| Arc::new(WalletView::of(o)));
        *self.view.write().unwrap_or_else(|p| p.into_inner()) = view;
    }

    /// Take `m_transactionMutex`.
    pub fn lock_transactions(&self) -> MutexGuard<'_, ()> {
        self.transactions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `--tx-notify` hook, when there is one.
    pub fn tx_notifier(&self) -> Option<&Notifier> {
        self.tx_notifier.as_ref()
    }

    /// Queue `--tx-notify` for what `open`'s last sync step recorded.
    pub fn announce_sync_step(&self, open: &OpenWallet) {
        if let Some(notifier) = &self.tx_notifier {
            notify::announce_sync_step(notifier, open, self.config.notify_during_sync);
        }
    }

    /// `ApiDispatcher::checkAuthenticated` (`ApiDispatcher.cpp:614`): the
    /// header hashed with the process salt, against the stored hash.
    ///
    /// In the C++ every wrong key costs the full 10,000 PBKDF2 rounds, so
    /// anyone who can reach the port can keep a core busy just by guessing.
    /// Here a salted SHA-256 turns a wrong key away for the price of one hash,
    /// and only a key that passes it goes on to the C++'s PBKDF2 comparison —
    /// so a correct key is checked, and answered, exactly as before. Nothing
    /// is weakened by keeping the fast digest: the key itself is in `config`.
    pub fn password_matches(&self, given: &str) -> bool {
        if !constant_time_eq(&quick_digest(given, &self.salt)[..], &self.quick_digest[..]) {
            return false;
        }
        // Compared as hex of a PBKDF2 output, without an early exit.
        let candidate = hash_password(given, &self.salt);
        constant_time_eq(candidate.as_bytes(), self.hashed_password.as_bytes())
    }

    /// Build a daemon for `(host, port, ssl)`.
    pub fn daemon(&self, host: &str, port: u16, ssl: bool) -> std::result::Result<DynDaemon, String> {
        (self.make_daemon)(host, port, ssl).map(|daemon| DynDaemon(Arc::from(daemon)))
    }

    /// [`open_container`] with this state's daemon factory and its
    /// `skip_coinbase_transactions` setting.
    pub fn open_container(
        &self,
        wallet: Wallet,
        filename: String,
        password: Zeroizing<String>,
        host: String,
        port: u16,
        ssl: bool,
    ) -> std::result::Result<OpenWallet, String> {
        let sync = SyncConfig {
            skip_coinbase_transactions: self.config.skip_coinbase_transactions,
            scan_threads: self.config.threads.max(1) as usize,
            ..SyncConfig::default()
        };
        open_container_with(wallet, filename, password, host, port, ssl, sync, &self.make_daemon)
    }
}

/// `ApiDispatcher::hashPassword` (`ApiDispatcher.cpp:2114`): PBKDF2-HMAC-SHA256
/// with the process salt and `ApiConstants::PBKDF2_ITERATIONS`, as hex.
fn hash_password(password: &str, salt: &[u8; crypto::SALT_SIZE]) -> Zeroizing<String> {
    let key = crypto::api_password_hash(password.as_bytes(), salt);
    Zeroizing::new(key.iter().map(|b| format!("{b:02x}")).collect())
}

/// SHA-256 over the process salt and the key: the cheap first check of
/// [`ApiState::password_matches`].
fn quick_digest(password: &str, salt: &[u8; crypto::SALT_SIZE]) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(password.as_bytes());
    Zeroizing::new(hasher.finalize().into())
}

////////////////////////
/* WALLET OPERATIONS  */
////////////////////////

/// `SubWallets::deleteSubWallet` (`SubWallets.cpp:220`).
pub fn delete_sub_wallet(wallet: &mut Wallet, address: &str) -> Result<()> {
    let spend_key = spend_key_of(address)?;

    let Some(index) = wallet.sub_wallets.sub_wallet.iter().position(|s| s.public_spend_key == spend_key) else {
        return Err(WalletError::AddressNotInWallet(address.to_string()));
    };

    if wallet.sub_wallets.sub_wallet[index].is_primary_address {
        return Err(WalletError::CannotDeletePrimaryAddress);
    }

    wallet.sub_wallets.sub_wallet.remove(index);
    wallet.sub_wallets.public_spend_keys.retain(|k| *k != spend_key);

    delete_address_transactions(&mut wallet.sub_wallets.transactions, &spend_key);
    delete_address_transactions(&mut wallet.sub_wallets.locked_transactions, &spend_key);

    Ok(())
}

/// `SubWallets::deleteAddressTransactions` (`SubWallets.cpp:264`): drop this
/// key's transfer from every transaction, and the transaction with it once no
/// transfer is left.
fn delete_address_transactions(transactions: &mut Vec<Transaction>, spend_key: &PublicKey) {
    for tx in transactions.iter_mut() {
        tx.transfers.retain(|t| t.public_key != *spend_key);
    }
    transactions.retain(|tx| !tx.transfers.is_empty());
}

/// `SubWallets::importViewSubWallet` (`SubWallets.cpp:186`): a public spend key
/// added to a view-only container.
pub fn import_view_sub_wallet(wallet: &mut Wallet, public_spend_key: PublicKey, scan_height: u64) -> Result<String> {
    if !wallet.sub_wallets.is_view_wallet {
        return Err(WalletError::IllegalNonViewWalletOperation);
    }
    if wallet.sub_wallets.sub_wallet.iter().any(|s| s.public_spend_key == public_spend_key) {
        return Err(WalletError::SubWalletAlreadyExists);
    }

    let view_public = wrkz_pow::curve::secret_key_to_public_key(wallet.private_view_key().as_bytes())
        .ok_or(WalletError::InvalidPrivateKey)?;
    let address = wrkz_primitives::base58::standard_address(public_spend_key.as_bytes(), &view_public);

    wallet.sub_wallets.sub_wallet.push(crate::file::SubWallet {
        address: address.clone(),
        is_primary_address: false,
        private_spend_key: SecretKey::NULL,
        public_spend_key,
        sync_start_height: scan_height,
        sync_start_timestamp: 0,
        ..Default::default()
    });
    wallet.sub_wallets.public_spend_keys.push(public_spend_key);

    Ok(address)
}

/// `WalletBackend::getSpendKeys` (`WalletBackend.cpp:1706`):
/// `(publicSpendKey, privateSpendKey, walletIndex)`.
pub fn spend_keys(wallet: &Wallet, address: &str) -> Result<(PublicKey, SecretKey, u64)> {
    let spend_key = spend_key_of(address)?;
    let sub = wallet.sub_wallet(&spend_key).ok_or_else(|| WalletError::AddressNotInWallet(address.to_string()))?;
    Ok((sub.public_spend_key, sub.private_spend_key.clone(), sub.wallet_index))
}

/// `WalletBackend::getMnemonicSeedForAddress` (`WalletBackend.cpp:1738`).
pub fn mnemonic_seed_for_address(wallet: &Wallet, address: &str) -> Result<Zeroizing<String>> {
    let (_, private_spend_key, _) = spend_keys(wallet, address)?;
    if private_spend_key.is_null() {
        return Err(WalletError::IllegalViewWalletOperation);
    }
    let (derived, _) = wrkz_pow::curve::generate_view_from_spend(private_spend_key.as_bytes());
    if derived != *wallet.private_view_key().as_bytes() {
        return Err(WalletError::KeysNotDeterministic);
    }
    Ok(Zeroizing::new(wrkz_primitives::mnemonic::private_key_to_mnemonic(private_spend_key.as_bytes())))
}

/// `WalletBackend::getTransactionsRange` (`WalletBackend.cpp:1837`):
/// `[start, end)`, by block height.
pub fn transactions_range(wallet: &Wallet, start: u64, end: u64) -> Vec<&Transaction> {
    wallet.transactions().iter().filter(|t| t.block_height >= start && t.block_height < end).collect()
}

/// `SubWallets::getAddress(spendKey)`: the address that owns a spend key, or
/// the empty string, which is what the C++ hands the JSON when it does not know
/// the key.
pub fn address_for_spend_key(wallet: &Wallet, spend_key: &PublicKey) -> String {
    wallet.sub_wallet(spend_key).map(|s| s.address.clone()).unwrap_or_default()
}

/// `Utilities::addressToKeys(address).first`, refusing an integrated address
/// the way `validateAddresses(…, false)` does.
///
/// Routed through [`crate::transfer::validate_address`] so the checks run in
/// the C++'s order and report the C++'s codes: the length before the prefix,
/// and the prefix before base58 (`ValidateParameters.cpp:338`).
pub fn spend_key_of(address: &str) -> Result<PublicKey> {
    let parsed = crate::transfer::validate_address(address, false)?;
    Ok(Hex32(parsed.spend_public_key))
}

////////////////////////
/* HEX                */
////////////////////////

/// Lower-case hex, so the crate needs no `hex` dependency at run time.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

/// `Common::fromHex`: `None` for an odd length or a non-hex character.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// `validateHash` (`ValidateParameters.cpp`): 64 characters, all hex.
pub fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64 {
        return Err(WalletError::HashWrongLength);
    }
    if !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(WalletError::HashInvalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_configured_key_passes_either_check() {
        let config = ApiConfig { rpc_password: "hunter2".into(), ..ApiConfig::default() };
        let no_daemon: DaemonFactory = Box::new(|_: &str, _: u16, _: bool| Err("no daemon here".to_string()));
        let state = ApiState::new(config, no_daemon);
        assert!(state.password_matches("hunter2"));
        for wrong in ["", "hunter", "hunter3", "hunter22", "HUNTER2", "hunter2\0"] {
            assert!(!state.password_matches(wrong), "{wrong:?}");
        }
        // The fast digest is the key's and nothing else's.
        assert_eq!(*quick_digest("hunter2", &state.salt), *state.quick_digest);
        assert_ne!(*quick_digest("hunter2", &[0u8; crypto::SALT_SIZE]), *state.quick_digest, "salted");
    }
}
