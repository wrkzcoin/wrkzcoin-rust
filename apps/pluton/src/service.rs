// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The wallet: it owns the open [`Wallet`], answers [`Command`]s and takes one
//! sync step at a time. Nothing here draws, and nothing here knows whether it
//! is a background thread or a Web Worker — the platform supplies an
//! [`HttpTransport`] and a [`Storage`], and drives [`Service::tick`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use wrkz_primitives::constants::{TRANSACTION_POW_PASS_WITH_FEE, TRANSACTION_POW_PASS_WITH_FEE_HEIGHT};
use wrkz_wallet::file::{SecretKey, Wallet, WalletError};
use wrkz_wallet::http::{HttpDaemon, HttpTransport, PowOutcome};
use wrkz_wallet::platform;
use wrkz_wallet::sync::{SyncConfig, SyncDaemon, SyncStep, Synchronizer};
use wrkz_wallet::transfer::{self, FeeType, FusionParams, PreparedTransaction, SendParams, SystemRandom};
use wrkz_wallet::txpow::TxPowServer;

use crate::protocol::{
    parse_amount, Command, Event, NewWallet, NoticeKind, PreparedSend, SyncProgress, TransactionRow, WalletSummary,
};

/// The node a wallet talks to until someone changes it in Settings.
pub const DEFAULT_NODE: &str = "http://node-fin.wrkz.work:17856";

/// The browser can only reach a node over HTTPS that also allows its origin,
/// so the web build starts from the same node's TLS address.
pub const DEFAULT_NODE_BROWSER: &str = "https://node-fin.wrkz.work";

/// Offered in Settings, switched off until someone turns it on.
pub const SUGGESTED_POW_SERVER: &str = "https://txpow.wrkz.work";

/// Saved at most this often while syncing, and always on close.
const SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// `/info` this often, the cadence `Nigel`'s background thread keeps.
const INFO_INTERVAL: Duration = Duration::from_secs(10);

/// Between polls once the wallet is synced.
const SYNCED_INTERVAL: Duration = Duration::from_secs(10);

/// Where wallet files and the settings live: a directory on a desktop or a
/// phone, the browser's own storage on the web.
pub trait Storage {
    /// The wallet names this device holds.
    fn list(&self) -> Vec<String>;
    fn load(&self, name: &str) -> Result<Vec<u8>, String>;
    fn save(&mut self, name: &str, bytes: &[u8]) -> Result<(), String>;
    fn exists(&self, name: &str) -> bool;
}

/// What Settings remembers between runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub node_url: String,
    /// Empty means every proof of work is computed on this device.
    pub pow_server_url: String,
    pub pow_api_key: String,
    /// Offered first the next time the wallet starts.
    pub last_wallet: String,
}

impl Settings {
    /// The defaults for this platform.
    pub fn new(browser: bool) -> Self {
        Settings {
            node_url: if browser { DEFAULT_NODE_BROWSER.into() } else { DEFAULT_NODE.into() },
            pow_server_url: String::new(),
            pow_api_key: String::new(),
            last_wallet: String::new(),
        }
    }
}

/// The file the settings live in, beside the wallets.
const SETTINGS_FILE: &str = "settings.json";

struct Open<T: HttpTransport> {
    name: String,
    password: String,
    sync: Synchronizer<HttpDaemon<T>>,
    /// Changed since the last save.
    dirty: bool,
}

/// The wallet, off the drawing thread.
pub struct Service<T: HttpTransport + Clone, S: Storage> {
    transport: T,
    /// The proof-of-work server's own client: small replies, no redirects.
    pow_transport: T,
    storage: S,
    settings: Settings,
    /// The browser has one thread and no fast hashing, so it pays the fee that
    /// skips the proof of work unless a server is configured.
    browser: bool,
    open: Option<Open<T>>,
    prepared: Option<PreparedTransaction>,
    last_save_ms: u64,
    last_info_ms: u64,
}

impl<T: HttpTransport + Clone, S: Storage> Service<T, S> {
    /// Read the settings and wait for a wallet to be opened.
    pub fn new(transport: T, pow_transport: T, storage: S, browser: bool) -> Self {
        let settings = storage
            .load(SETTINGS_FILE)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| Settings::new(browser));
        Service {
            transport,
            pow_transport,
            storage,
            settings,
            browser,
            open: None,
            prepared: None,
            last_save_ms: 0,
            last_info_ms: 0,
        }
    }

    /// What Settings shows.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn save_settings(&mut self) {
        if let Ok(json) = serde_json::to_vec(&self.settings) {
            let _ = self.storage.save(SETTINGS_FILE, &json);
        }
    }

    /// A daemon client for the configured node, with the proof-of-work server
    /// attached when one is set.
    fn daemon(&self) -> Result<HttpDaemon<T>, String> {
        let mut daemon = HttpDaemon::new(&self.settings.node_url, self.transport.clone())
            .ok_or_else(|| format!("'{}' is not an http:// or https:// node URL", self.settings.node_url))?;
        if !self.settings.pow_server_url.is_empty() {
            let key = (!self.settings.pow_api_key.is_empty()).then_some(self.settings.pow_api_key.as_str());
            match TxPowServer::new(&self.settings.pow_server_url, key, self.pow_transport.clone()) {
                Ok(server) => daemon.set_pow_server(Some(server)),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(daemon)
    }

    /// Answer one command.
    pub fn handle(&mut self, command: Command) -> Vec<Event> {
        match command {
            Command::List => vec![Event::Wallets { names: self.storage.list() }],
            Command::Create { name, password, wallet } => self.create(&name, &password, wallet),
            Command::Open { name, password, bytes } => self.open(&name, &password, bytes),
            Command::Close => self.close(),
            Command::Save => match self.save_wallet() {
                Ok(events) => events,
                Err(e) => vec![error(e)],
            },
            Command::SetNode { url } => self.set_node(&url),
            Command::SetPowServer { url, api_key } => self.set_pow_server(&url, &api_key),
            Command::TestPowServer { url, api_key } => vec![self.test_pow_server(&url, &api_key)],
            Command::PrepareSend { address, amount, payment_id, send_all } => {
                self.prepare_send(&address, &amount, &payment_id, send_all)
            }
            Command::ConfirmSend => self.confirm_send(),
            Command::CancelSend => {
                self.prepared = None;
                vec![notice("The transaction was not sent.", NoticeKind::Info)]
            }
            Command::Optimize => self.optimize(),
            Command::RevealSecrets { password } => vec![self.reveal(&password)],
            Command::ChangePassword { old, new } => self.change_password(&old, &new),
            Command::Rescan { height } => self.rescan(height),
            Command::Shutdown => {
                let mut events = self.close();
                events.push(Event::Stopped);
                events
            }
        }
    }

    /// One round of syncing, and how long to wait before the next.
    pub fn tick(&mut self) -> (Vec<Event>, Duration) {
        let now = platform::now_millis();
        let Some(open) = self.open.as_mut() else {
            return (Vec::new(), Duration::from_millis(250));
        };

        if now.saturating_sub(self.last_info_ms) >= INFO_INTERVAL.as_millis() as u64 {
            let _ = open.sync.refresh_info();
            self.last_info_ms = now;
        }

        let step = open.sync.sync_step();
        let mut events = Vec::new();
        let (wait, changed) = match &step {
            SyncStep::Processed { transactions, .. } => {
                open.dirty = true;
                (Duration::ZERO, *transactions > 0)
            }
            SyncStep::Synced { .. } => (SYNCED_INTERVAL, false),
            SyncStep::Idle { backoff } => (*backoff, false),
            SyncStep::Failed { error, backoff } => {
                events.push(notice(format!("The node did not answer: {error}"), NoticeKind::Warning));
                (*backoff, false)
            }
            SyncStep::Gap { covered_to, daemon_serves_from } => {
                events.push(notice(
                    format!(
                        "This node only holds blocks from {daemon_serves_from} and this wallet has scanned to \
                         {covered_to}; syncing has stopped. Choose a node with the whole chain in Settings."
                    ),
                    NoticeKind::Error,
                ));
                (Duration::from_secs(30), false)
            }
        };

        events.push(self.progress_event());
        if changed {
            events.push(self.balance_event());
            events.push(self.history_event());
        }

        if self.should_save(now) {
            match self.save_wallet() {
                Ok(saved) => events.extend(saved),
                Err(e) => events.push(error(e)),
            }
        }
        (events, wait)
    }

    fn should_save(&self, now: u64) -> bool {
        self.open.as_ref().is_some_and(|o| o.dirty)
            && now.saturating_sub(self.last_save_ms) >= SAVE_INTERVAL.as_millis() as u64
    }

    fn progress_event(&self) -> Event {
        let progress = match self.open.as_ref() {
            None => SyncProgress::default(),
            Some(open) => {
                let status = open.sync.sync_status();
                SyncProgress {
                    wallet_height: status.wallet_block_count,
                    local_height: status.local_daemon_block_count,
                    network_height: status.network_block_count,
                    synced: status.is_synced(),
                }
            }
        };
        Event::Progress { progress }
    }

    fn balance_event(&self) -> Event {
        let (unlocked, locked) = self.open.as_ref().map(|o| o.sync.total_balance()).unwrap_or((0, 0));
        Event::Balance { unlocked, locked }
    }

    fn history_event(&self) -> Event {
        let mut transactions: Vec<TransactionRow> = self
            .open
            .as_ref()
            .map(|open| {
                open.sync
                    .wallet()
                    .transactions()
                    .iter()
                    .map(|tx| TransactionRow {
                        hash: tx.hash.to_hex(),
                        amount: tx.total_amount(),
                        fee: tx.fee,
                        block_height: tx.block_height,
                        timestamp: tx.timestamp,
                        payment_id: tx.payment_id.clone(),
                        is_coinbase: tx.is_coinbase_transaction,
                        is_fusion: tx.is_fusion_transaction(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Newest first, as every wallet lists them.
        transactions.sort_by(|a, b| b.block_height.cmp(&a.block_height).then(b.timestamp.cmp(&a.timestamp)));
        Event::History { transactions }
    }

    fn summary(&self, name: &str) -> WalletSummary {
        let wallet = self.open.as_ref().map(|o| o.sync.wallet());
        WalletSummary {
            name: name.to_string(),
            address: wallet.and_then(Wallet::primary_address).unwrap_or_default().to_string(),
            is_view_only: wallet.is_some_and(Wallet::is_view_wallet),
            has_seed: wallet.and_then(Wallet::mnemonic_seed).is_some(),
        }
    }

    /// The network's top block index, as every height-dependent rule is judged
    /// at; `0` until `/info` has answered.
    fn network_height(&self) -> u64 {
        self.open.as_ref().map(|o| o.sync.daemon_state().network_block_count).unwrap_or(0)
    }

    fn create(&mut self, name: &str, password: &str, new: NewWallet) -> Vec<Event> {
        if name.trim().is_empty() {
            return vec![error("A wallet needs a name.")];
        }
        if self.storage.exists(name) {
            return vec![error(format!("A wallet called '{name}' is already on this device."))];
        }
        // A new wallet starts at the tip, so it does not scan the whole chain.
        let network_height = match self.daemon().and_then(|d| d.info().map_err(|e| e.to_string())) {
            Ok(info) => info.top_index(),
            Err(e) => return vec![error(format!("The node could not be reached, so the wallet was not created: {e}"))],
        };

        let wallet = match new {
            NewWallet::Create => Wallet::create_new(network_height),
            NewWallet::Seed { seed, scan_height } => Wallet::import_from_mnemonic(seed.trim(), scan_height),
            NewWallet::Keys { spend_key, view_key, scan_height } => {
                match (SecretKey::from_hex(spend_key.trim()), SecretKey::from_hex(view_key.trim())) {
                    (Some(spend), Some(view)) => Wallet::import_from_keys(&spend, &view, scan_height),
                    _ => Err(WalletError::InvalidPrivateKey),
                }
            }
            NewWallet::ViewOnly { address, view_key, scan_height } => match SecretKey::from_hex(view_key.trim()) {
                Some(view) => Wallet::import_view_only(&view, address.trim(), scan_height),
                None => Err(WalletError::InvalidPrivateKey),
            },
        };
        let wallet = match wallet {
            Ok(w) => w,
            Err(e) => return vec![error(format!("The wallet could not be created: {e}"))],
        };
        let seed = wallet.mnemonic_seed().map(|s| s.to_string());

        let mut events = match self.start(name, password, wallet) {
            Ok(events) => events,
            Err(e) => return vec![error(e)],
        };
        // `Opened` is what a restore reports; a new wallet reports `Created`,
        // which is the one moment its seed is shown.
        events.retain(|e| !matches!(e, Event::Opened { .. }));
        events.insert(0, Event::Created { summary: self.summary(name), seed });
        events
    }

    fn open(&mut self, name: &str, password: &str, bytes: Option<Vec<u8>>) -> Vec<Event> {
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => match self.storage.load(name) {
                Ok(bytes) => bytes,
                Err(e) => return vec![error(format!("'{name}' could not be read: {e}"))],
            },
        };
        let wallet = match Wallet::from_file_bytes(&bytes, password) {
            Ok(wallet) => wallet,
            Err(WalletError::WrongPassword) => return vec![error("That password does not open this wallet.")],
            Err(e) => return vec![error(format!("'{name}' could not be opened: {e}"))],
        };
        match self.start(name, password, wallet) {
            Ok(events) => events,
            Err(e) => vec![error(e)],
        }
    }

    /// Take a wallet into service: a synchronizer on the configured node, the
    /// first progress, balance and history.
    fn start(&mut self, name: &str, password: &str, wallet: Wallet) -> Result<Vec<Event>, String> {
        let daemon = self.daemon()?;
        let mut sync = Synchronizer::with_config(daemon, wallet, SyncConfig::default());
        let _ = sync.refresh_info();
        self.open = Some(Open { name: name.to_string(), password: password.to_string(), sync, dirty: true });
        self.last_info_ms = platform::now_millis();
        self.last_save_ms = 0;
        self.settings.last_wallet = name.to_string();
        self.save_settings();

        let mut events = vec![Event::Opened { summary: self.summary(name) }];
        events.extend(self.save_wallet().unwrap_or_else(|e| vec![error(e)]));
        events.push(self.progress_event());
        events.push(self.balance_event());
        events.push(self.history_event());
        Ok(events)
    }

    fn close(&mut self) -> Vec<Event> {
        let mut events = match self.save_wallet() {
            Ok(events) => events,
            Err(e) => vec![error(e)],
        };
        self.open = None;
        self.prepared = None;
        events.push(Event::Closed);
        events
    }

    /// Write the open wallet. In a browser the bytes also go back to the page,
    /// which keeps them for the next visit.
    fn save_wallet(&mut self) -> Result<Vec<Event>, String> {
        let Some(open) = self.open.as_mut() else { return Ok(Vec::new()) };
        let bytes = open.sync.wallet().to_file_bytes(&open.password).map_err(|e| e.to_string())?;
        self.storage.save(&open.name, &bytes).map_err(|e| format!("The wallet could not be saved: {e}"))?;
        let name = open.name.clone();
        open.dirty = false;
        self.last_save_ms = platform::now_millis();
        Ok(if self.browser { vec![Event::Persist { name, bytes }] } else { Vec::new() })
    }

    fn set_node(&mut self, url: &str) -> Vec<Event> {
        let url = url.trim();
        if wrkz_wallet::http::normalize_url(url).is_none() {
            return vec![error(format!("'{url}' is not an http:// or https:// node URL."))];
        }
        self.settings.node_url = url.to_string();
        self.save_settings();
        // The synchronizer holds the old client, so rebuild it around the new
        // one; the wallet and everything it has scanned stay as they are.
        if let Some(open) = self.open.take() {
            let Open { name, password, sync, dirty } = open;
            let wallet = sync.into_wallet();
            match self.daemon() {
                Ok(daemon) => {
                    let mut sync = Synchronizer::with_config(daemon, wallet, SyncConfig::default());
                    let _ = sync.refresh_info();
                    self.open = Some(Open { name, password, sync, dirty });
                }
                Err(e) => return vec![error(e)],
            }
        }
        vec![notice(format!("Now using the node at {url}."), NoticeKind::Info), self.progress_event()]
    }

    fn set_pow_server(&mut self, url: &str, api_key: &str) -> Vec<Event> {
        let url = url.trim();
        if !url.is_empty() && wrkz_wallet::txpow::normalize_url(url).is_err() {
            return vec![error(format!("'{url}' is not an http:// or https:// server URL."))];
        }
        self.settings.pow_server_url = url.to_string();
        self.settings.pow_api_key = api_key.trim().to_string();
        self.save_settings();

        let message = if url.is_empty() {
            "The proof of work is now computed on this device.".to_string()
        } else {
            format!("The proof of work is now asked of {url}, and computed here if it does not answer.")
        };
        let mut events = vec![notice(message, NoticeKind::Info)];
        // The open wallet's client carries the server, so rebuild it.
        if self.open.is_some() {
            let node = self.settings.node_url.clone();
            events.extend(self.set_node(&node).into_iter().filter(|e| !matches!(e, Event::Notice { .. })));
        }
        events
    }

    fn test_pow_server(&self, url: &str, api_key: &str) -> Event {
        let key = (!api_key.trim().is_empty()).then(|| api_key.trim());
        match TxPowServer::new(url.trim(), key, self.pow_transport.clone()) {
            Err(e) => Event::PowServerProbe { ok: false, detail: e.to_string() },
            Ok(server) => {
                let probe = server.probe();
                let detail = if probe.ok {
                    format!(
                        "Answered in {} ms: {} hashing threads, {} of {} queue slots in use.",
                        probe.latency_ms, probe.threads, probe.queue, probe.capacity
                    )
                } else {
                    probe.error.unwrap_or_else(|| "The server did not answer.".into())
                };
                Event::PowServerProbe { ok: probe.ok, detail }
            }
        }
    }

    /// What a send should pay. Everywhere but a browser without a
    /// proof-of-work server, that is the network minimum.
    fn fee_for_send(&self, network_height: u64) -> FeeType {
        let bypass_available = network_height >= TRANSACTION_POW_PASS_WITH_FEE_HEIGHT;
        if self.browser && self.settings.pow_server_url.is_empty() && bypass_available {
            // In a browser the proof-of-work search is impractical, so pay the
            // fee that lets a transaction skip it, as the C++ web wallet does.
            FeeType::FixedFee(TRANSACTION_POW_PASS_WITH_FEE)
        } else {
            FeeType::MinimumFee
        }
    }

    fn prepare_send(&mut self, address: &str, amount: &str, payment_id: &str, send_all: bool) -> Vec<Event> {
        let Some(amount) = parse_amount(amount) else {
            return vec![error("That amount is not a number of WRKZ.")];
        };
        if self.open.as_ref().is_some_and(|o| o.sync.wallet().is_view_wallet()) {
            return vec![error("This is a view-only wallet; it cannot send.")];
        }
        let network_height = self.network_height();
        if network_height == 0 {
            return vec![error("The node has not answered yet, so no transaction can be built.")];
        }
        let fee = self.fee_for_send(network_height);
        let params = SendParams {
            send_all,
            fee,
            pow_threads: platform::available_threads(),
            ..SendParams::basic(address.trim(), amount, payment_id.trim(), network_height)
        };

        let Some(open) = self.open.as_mut() else { return vec![error("No wallet is open.")] };
        let (wallet, daemon) = open.sync.split_for_transfer();
        let prepared = transfer::prepare_transaction(wallet, daemon, &params, &mut SystemRandom);
        let pow = daemon.take_pow_outcome();

        match prepared {
            Err(e) => vec![error(format!("The transaction could not be built: {e}"))],
            Ok(prepared) => {
                let event = Event::SendPrepared {
                    prepared: PreparedSend {
                        address: address.trim().to_string(),
                        amount: if send_all { sent_amount(&prepared) } else { amount },
                        fee: prepared.fee,
                        ring_size: prepared.mixin + 1,
                        payment_id: payment_id.trim().to_string(),
                        pow: pow.map(describe_pow),
                    },
                };
                self.prepared = Some(prepared);
                vec![event]
            }
        }
    }

    fn confirm_send(&mut self) -> Vec<Event> {
        let Some(prepared) = self.prepared.take() else {
            return vec![error("There is nothing to send.")];
        };
        let network_height = self.network_height();
        let Some(open) = self.open.as_mut() else { return vec![error("No wallet is open.")] };
        let (wallet, daemon) = open.sync.split_for_transfer();

        match transfer::send_prepared_transaction(wallet, daemon, prepared, network_height) {
            Err(e) => vec![error(format!("The transaction was not sent: {e}"))],
            Ok(sent) => {
                open.dirty = true;
                let mut events = vec![Event::Sent { hash: sent.transaction_hash.to_hex(), fee: sent.fee }];
                events.extend(self.save_wallet().unwrap_or_else(|e| vec![error(e)]));
                events.push(self.balance_event());
                events.push(self.history_event());
                events
            }
        }
    }

    fn optimize(&mut self) -> Vec<Event> {
        let network_height = self.network_height();
        if network_height == 0 {
            return vec![error("The node has not answered yet.")];
        }
        let params = FusionParams { pow_threads: platform::available_threads(), ..FusionParams::basic(network_height) };
        let Some(open) = self.open.as_mut() else { return vec![error("No wallet is open.")] };
        let (wallet, daemon) = open.sync.split_for_transfer();

        match transfer::send_fusion_transaction_advanced(wallet, daemon, &params, &mut SystemRandom) {
            Err(WalletError::FullyOptimized) => {
                vec![notice("This wallet is already optimized; nothing to combine.", NoticeKind::Info)]
            }
            Err(e) => vec![error(format!("The wallet could not be optimized: {e}"))],
            Ok(sent) => {
                open.dirty = true;
                let mut events = vec![
                    notice("Small inputs were combined; the balance is unchanged.", NoticeKind::Info),
                    Event::Sent { hash: sent.transaction_hash.to_hex(), fee: sent.fee },
                ];
                events.extend(self.save_wallet().unwrap_or_else(|e| vec![error(e)]));
                events.push(self.history_event());
                events
            }
        }
    }

    fn reveal(&self, password: &str) -> Event {
        let Some(open) = self.open.as_ref() else { return error("No wallet is open.") };
        if password != open.password {
            return error("That password does not open this wallet.");
        }
        let wallet = open.sync.wallet();
        Event::Secrets {
            address: wallet.primary_address().unwrap_or_default().to_string(),
            seed: wallet.mnemonic_seed().map(|s| s.to_string()),
            spend_key: wallet
                .primary_sub_wallet()
                .filter(|sub| sub.has_spend_key())
                .map(|sub| sub.private_spend_key.to_hex().to_string()),
            view_key: wallet.private_view_key().to_hex().to_string(),
        }
    }

    fn change_password(&mut self, old: &str, new: &str) -> Vec<Event> {
        let Some(open) = self.open.as_mut() else { return vec![error("No wallet is open.")] };
        if old != open.password {
            return vec![error("That password does not open this wallet.")];
        }
        if new.is_empty() {
            return vec![error("The new password cannot be empty.")];
        }
        open.password = new.to_string();
        open.dirty = true;
        let mut events = match self.save_wallet() {
            Ok(events) => events,
            Err(e) => return vec![error(e)],
        };
        events.push(notice("The wallet file is now encrypted with the new password.", NoticeKind::Info));
        events
    }

    fn rescan(&mut self, height: u64) -> Vec<Event> {
        let Some(open) = self.open.take() else { return vec![error("No wallet is open.")] };
        let Open { name, password, sync, .. } = open;
        let mut wallet = sync.into_wallet();
        wallet.reset(height);

        match self.daemon() {
            Err(e) => vec![error(e)],
            Ok(daemon) => {
                let mut sync = Synchronizer::with_config(daemon, wallet, SyncConfig::default());
                let _ = sync.refresh_info();
                self.open = Some(Open { name, password, sync, dirty: true });
                let mut events =
                    vec![notice(format!("Scanning again from block {height}. This takes a while."), NoticeKind::Info)];
                events.push(self.progress_event());
                events.push(self.balance_event());
                events.push(self.history_event());
                events
            }
        }
    }
}

/// What a "send everything" actually sends: all the inputs hold, less the fee
/// and whatever goes back as change.
fn sent_amount(prepared: &PreparedTransaction) -> u64 {
    let inputs: u64 = prepared.inputs.iter().map(|owned| owned.input.amount).sum();
    inputs.saturating_sub(prepared.fee).saturating_sub(prepared.change_required)
}

fn describe_pow(outcome: PowOutcome) -> String {
    match outcome {
        PowOutcome::Solved => "The proof of work came from the server.".into(),
        PowOutcome::ComputedLocally(why) => format!("Computed here: {why}"),
    }
}

fn notice(message: impl Into<String>, kind: NoticeKind) -> Event {
    Event::Notice { message: message.into(), kind }
}

fn error(message: impl Into<String>) -> Event {
    Event::Notice { message: message.into(), kind: NoticeKind::Error }
}
