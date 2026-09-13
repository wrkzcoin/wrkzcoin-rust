// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The opening menu, the wallet loop, and opening or creating a wallet —
//! `src/zedwallet++/Menu.cpp`, `Open.cpp`, `Sync.cpp` and `ZedWallet.cpp`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeroize::Zeroizing;

use super::commands::{node_down_commands, parse_command, print_commands, startup_commands, Selection};
use super::format::{CONTACT_LINK, DAEMON_NAME, TICKER};
use super::prompt::{confirm, get_daemon_address, get_private_key, get_scan_height, validate_address, Answer};
use super::session::{print_transfer_one_line, Session};
use super::term::{information, success, warning, Terminal};
use super::ZedConfig;
use crate::api::{DynDaemon, OpenWallet, SyncLog};
use crate::daemon::Daemon;
use crate::file::{Wallet, WalletError};
use crate::sync::{SyncConfig, Synchronizer};
use wrkz_rpc::log::Level;

/// The wallet as the interface holds it: shared with the sync thread.
pub type SharedWallet = Arc<Mutex<OpenWallet>>;

fn lock(wallet: &SharedWallet) -> std::sync::MutexGuard<'_, OpenWallet> {
    match wallet.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

////////////////////////
/* PASSWORDS          */
////////////////////////

/// `getWalletPassword(verifyPwd, msg)` (`Open.cpp:404`), which is
/// `Tools::PasswordContainer::read_password`.
///
/// The password is never echoed and never printed back. `None` means the input
/// ended.
pub fn get_wallet_password(term: &mut dyn Terminal, verify: bool, msg: &str) -> Option<Zeroizing<String>> {
    loop {
        term.write(&information(msg));
        term.flush();
        let first = term.read_password()?;

        if !verify {
            return Some(first);
        }

        term.write(&information("Confirm your new password: "));
        term.flush();
        let second = term.read_password()?;

        if *first == *second {
            return Some(second);
        }

        term.line(&warning("Passwords do not match, try again."));
    }
}

////////////////////////
/* DAEMON             */
////////////////////////

/// Build a daemon client for `(host, port, ssl)`.
///
/// An IPC address (`/path`, `@name`, `ipc://path`) goes over a local socket and
/// ignores the port and the TLS flag: there is no TLS on a local socket, and
/// the kernel already decides who may open it
/// (`zedwallet++/GetInput.cpp:380`).
pub fn make_daemon(host: &str, port: u16, ssl: bool) -> Result<DynDaemon, String> {
    if crate::ipc::is_ipc_address(host) {
        return crate::ipc::IpcDaemon::new(host).map(|d| DynDaemon(Arc::new(d)));
    }
    let scheme = if ssl { "https" } else { "http" };
    Daemon::new(&format!("{scheme}://{host}:{port}")).map(|d| DynDaemon(Arc::new(d))).map_err(|e| e.to_string())
}

/// `WalletBackend::swapNode`: a new daemon under the same container, keeping
/// the sync progress the wallet file holds.
pub fn rebuild_daemon(open: &mut OpenWallet, host: &str, port: u16, ssl: bool) -> Result<(), String> {
    let daemon = make_daemon(host, port, ssl)?;
    let config = open.sync.config().clone();
    let wallet = std::mem::take(open.sync.wallet_mut());
    open.sync = Synchronizer::with_config(daemon, wallet, config);
    open.daemon_host = host.to_string();
    open.daemon_port = port;
    open.daemon_ssl = ssl;
    open.sync_gap = None;
    open.refresh_info();
    Ok(())
}

/// The synchronizer the command line asks for: `--threads` scanning threads,
/// the thread count zedwallet++ hands `WalletBackend::openWallet`, and
/// `--skip-coinbase-transactions`.
pub fn sync_config(config: &ZedConfig) -> SyncConfig {
    SyncConfig {
        skip_coinbase_transactions: config.skip_coinbase_transactions,
        scan_threads: config.threads.max(1) as usize,
        ..SyncConfig::default()
    }
}

/// Wrap a freshly opened container as an [`OpenWallet`].
fn open_wallet_from(
    wallet: Wallet,
    filename: String,
    password: Zeroizing<String>,
    config: &ZedConfig,
) -> Result<OpenWallet, String> {
    let daemon = make_daemon(&config.host, config.port, config.ssl)?;
    let sync_config = sync_config(config);

    let mut open = OpenWallet {
        sync: Synchronizer::with_config(daemon, wallet, sync_config),
        filename,
        password,
        daemon_host: config.host.clone(),
        daemon_port: config.port,
        daemon_ssl: config.ssl,
        prepared: Vec::new(),
        peer_count: 0,
        hashrate: 0,
        sync_gap: None,
        stop: Arc::new(AtomicBool::new(false)),
    };
    open.refresh_info();
    Ok(open)
}

////////////////////////
/* THE SELECTION MENU */
////////////////////////

/// What `selectionScreen` decided.
pub enum Launch {
    /// The user chose `exit`, or an action failed and they gave up.
    Exit,
    /// A wallet is open. `sync` is false for a freshly created wallet, which
    /// the C++ does not make wait for a scan it knows is empty.
    Open { wallet: SharedWallet, sync: bool },
}

/// `selectionScreen` (`Menu.cpp:18`).
pub fn selection_screen(term: &mut dyn Terminal, config: &ZedConfig) -> Launch {
    loop {
        let command = get_action(term, config);

        if command == "exit" {
            return Launch::Exit;
        }

        let Some(open) = handle_launch_command(term, &command, config) else {
            term.line(&information("Returning to selection screen..."));
            continue;
        };

        let wallet: SharedWallet = Arc::new(Mutex::new(open));

        if !check_node_status(term, &wallet) {
            return Launch::Exit;
        }

        // `getNodeFee` is always zero here: no WrkzCoin daemon serves the
        // `/fee` route `Nigel` reads it from, so the C++'s node-fee warning
        // can never fire and is not reproduced.

        if command == "create" {
            term.write(&information(
                "\nYour wallet is syncing with the network in the background.\nUntil this is completed new \
                 transactions might not show up.\nUse the status command to check the progress.\n",
            ));
            return Launch::Open { wallet, sync: false };
        }

        return Launch::Open { wallet, sync: true };
    }
}

/// `getAction` (`Menu.cpp:154`): the menu, unless `--wallet-file` or
/// `--password` already said "open".
pub fn get_action(term: &mut dyn Terminal, config: &ZedConfig) -> String {
    if config.wallet_given || config.pass_given {
        return "open".to_string();
    }

    let commands = startup_commands();
    print_commands(term, &commands);

    match parse_command(term, &commands, &commands, "What would you like to do?: ") {
        Selection::Command(c) => c,
        Selection::Exit => "exit".to_string(),
    }
}

/// `checkNodeStatus` (`Menu.cpp:92`): `false` means the user chose to exit.
pub fn check_node_status(term: &mut dyn Terminal, wallet: &SharedWallet) -> bool {
    loop {
        if daemon_online(wallet) {
            return true;
        }

        term.line(&warning(format!(
            "It looks like {DAEMON_NAME} isn't open!\n\nEnsure {DAEMON_NAME} is open and has finished syncing. (It \
             will often not respond when syncing)\nIf it's still not working, try restarting {DAEMON_NAME} (or try a \
             different remote node).\nThe daemon sometimes gets stuck.\nAlternatively, perhaps {DAEMON_NAME} can't \
             communicate with any peers.\n\nThe wallet can't function fully until it can communicate with the \
             network."
        )));

        let commands = node_down_commands();
        print_commands(term, &commands);

        let Selection::Command(command) = parse_command(term, &commands, &commands, "What would you like to do?: ")
        else {
            return false;
        };

        match command.as_str() {
            "try_again" => continue,
            "exit" => return false,
            "continue" => return true,
            "swap_node" => {
                let (host, port, ssl) = get_daemon_address(term);
                term.write(&information("\nSwapping node, this may take some time...\n"));
                match rebuild_daemon(&mut lock(wallet), &host, port, ssl) {
                    Ok(()) => term.write(&success("Node swap complete.\n\n")),
                    Err(e) => term.line(&format!("{}{}", warning("Could not swap node: "), warning(e))),
                }
            }
            _ => return false,
        }
    }
}

/// `WalletBackend::daemonOnline`: the daemon answered `/info` at all.
fn daemon_online(wallet: &SharedWallet) -> bool {
    let mut open = lock(wallet);
    open.refresh_info();
    open.sync.daemon_state().network_block_count != 0
}

/// `handleLaunchCommand` (`CommandDispatcher.cpp:194`).
fn handle_launch_command(term: &mut dyn Terminal, command: &str, config: &ZedConfig) -> Option<OpenWallet> {
    match command {
        "create" => create_wallet(term, config),
        "open" => open_wallet(term, config),
        "seed_restore" => import_wallet_from_seed(term, config),
        "key_restore" => import_wallet_from_keys(term, config),
        "view_wallet" => import_view_wallet(term, config),
        _ => None,
    }
}

////////////////////////
/* OPEN AND IMPORT    */
////////////////////////

/// `openWallet` (`Open.cpp:216`): the filename once, then the password until it
/// is right.
fn open_wallet(term: &mut dyn Terminal, config: &ZedConfig) -> Option<OpenWallet> {
    let filename = get_existing_wallet_filename(term, config)?;
    let mut initial = true;

    loop {
        let password = if initial && config.pass_given {
            Zeroizing::new(config.wallet_pass.clone())
        } else {
            get_wallet_password(term, false, "Enter password: ")?
        };

        match Wallet::open(&filename, &password) {
            Err(WalletError::WrongPassword) => {
                // Never reuse a command-line password that was wrong, or this
                // loops for ever.
                initial = false;
                term.write("\n");
                term.line(&warning("Incorrect password! Try again."));
                term.write("\n");
            }
            Err(e) => {
                term.line(&warning(format!("Failed to open wallet: {e}")));
                return None;
            }
            Ok(wallet) => {
                let address = wallet.primary_address().unwrap_or_default().to_string();
                let open = match open_wallet_from(wallet, filename, password, config) {
                    Ok(open) => open,
                    Err(e) => {
                        term.line(&warning(format!("Failed to open wallet: {e}")));
                        return None;
                    }
                };
                term.line(&information(format!("\nYour wallet {address} has been successfully opened!\n")));
                return Some(open);
            }
        }
    }
}

/// `createWallet` (`Open.cpp:186`).
fn create_wallet(term: &mut dyn Terminal, config: &ZedConfig) -> Option<OpenWallet> {
    let filename = get_new_wallet_filename(term)?;
    let password = get_wallet_password(term, true, "Give your new wallet a password: ")?;

    // The daemon's height decides where the new wallet starts scanning, so it
    // has to answer before the container exists.
    let network_height = match make_daemon(&config.host, config.port, config.ssl) {
        Ok(daemon) => crate::sync::SyncDaemon::info(&daemon).map(|i| i.network_height.saturating_sub(1)).unwrap_or(0),
        Err(_) => 0,
    };

    let wallet = match Wallet::create_new(network_height) {
        Ok(w) => w,
        Err(e) => {
            term.line(&warning(format!("Failed to create wallet: {e}")));
            return None;
        }
    };

    if let Err(e) = wallet.save(&filename, &password) {
        term.line(&warning(format!("Failed to create wallet: {e}")));
        return None;
    }

    let open = match open_wallet_from(wallet, filename, password, config) {
        Ok(open) => open,
        Err(e) => {
            term.line(&warning(format!("Failed to create wallet: {e}")));
            return None;
        }
    };

    term.write("\n");
    prompt_save_keys(term, &open);
    term.line(&format!("{}{}", warning("If you lose these your wallet cannot be "), warning("recreated!")));
    term.write("\n");

    Some(open)
}

/// `importWalletFromSeed` (`Open.cpp:138`).
fn import_wallet_from_seed(term: &mut dyn Terminal, config: &ZedConfig) -> Option<OpenWallet> {
    let seed = loop {
        term.write(&information("Enter your mnemonic phrase (25 words): "));
        term.flush();
        let seed = Zeroizing::new(term.read_line()?.trim().to_string());

        match wrkz_primitives::mnemonic::mnemonic_to_private_key(&seed) {
            Ok(_) => break seed,
            Err(e) => {
                term.write("\n");
                term.line(&warning(WalletError::InvalidMnemonic(e).to_string()));
                term.write("\n");
            }
        }
    };

    let filename = get_new_wallet_filename(term)?;
    let password = get_wallet_password(term, true, "Give your new wallet a password: ")?;
    let scan_height = get_scan_height(term);

    finish_import(term, Wallet::import_from_mnemonic(&seed, scan_height), filename, password, config, false)
}

/// `importWalletFromKeys` (`Open.cpp:98`).
fn import_wallet_from_keys(term: &mut dyn Terminal, config: &ZedConfig) -> Option<OpenWallet> {
    let Answer::Given(spend) = get_private_key(term, "Enter your private spend key: ") else {
        return None;
    };
    let Answer::Given(view) = get_private_key(term, "Enter your private view key: ") else {
        return None;
    };

    let filename = get_new_wallet_filename(term)?;
    let password = get_wallet_password(term, true, "Give your new wallet a password: ")?;
    let scan_height = get_scan_height(term);

    finish_import(term, Wallet::import_from_keys(&spend, &view, scan_height), filename, password, config, false)
}

/// `importViewWallet` (`Open.cpp:23`).
fn import_view_wallet(term: &mut dyn Terminal, config: &ZedConfig) -> Option<OpenWallet> {
    term.line(&format!(
        "{}{}",
        warning("View wallets are only for viewing incoming "),
        warning("transactions, and cannot make transfers.")
    ));

    let create = confirm(term, "Is this OK?");
    term.write("\n");
    if !create {
        return None;
    }

    let Answer::Given(view) = get_private_key(term, "Private View Key: ") else {
        return None;
    };

    let address = loop {
        term.write(&information("Enter your public "));
        term.write(&information(TICKER));
        term.write(&information(" address: "));
        term.flush();
        let address = term.read_line()?.trim().to_string();

        match validate_address(&address, false) {
            Ok(()) => break address,
            Err(e) => term.line(&format!("{}{}", warning("Invalid address: "), warning(e.to_string()))),
        }
    };

    let filename = get_new_wallet_filename(term)?;
    let password = get_wallet_password(term, true, "Give your new wallet a password: ")?;
    let scan_height = get_scan_height(term);

    finish_import(term, Wallet::import_view_only(&view, &address, scan_height), filename, password, config, true)
}

fn finish_import(
    term: &mut dyn Terminal,
    wallet: Result<Wallet, WalletError>,
    filename: String,
    password: Zeroizing<String>,
    config: &ZedConfig,
    view_only: bool,
) -> Option<OpenWallet> {
    let wallet = match wallet {
        Ok(w) => w,
        Err(e) => {
            term.write(&warning(format!("Failed to import wallet: {e}")));
            return None;
        }
    };

    if let Err(e) = wallet.save(&filename, &password) {
        term.write(&warning(format!("Failed to import wallet: {e}")));
        return None;
    }

    let address = wallet.primary_address().unwrap_or_default().to_string();
    let open = match open_wallet_from(wallet, filename, password, config) {
        Ok(open) => open,
        Err(e) => {
            term.write(&warning(format!("Failed to import wallet: {e}")));
            return None;
        }
    };

    if view_only {
        term.line(&information(format!("\nYour view only wallet {address} has been successfully imported!\n")));
        view_wallet_msg(term);
    } else {
        term.line(&information(format!("\nYour wallet {address} has been successfully imported!\n")));
    }

    Some(open)
}

/// `viewWalletMsg` (`Open.cpp:411`).
fn view_wallet_msg(term: &mut dyn Terminal) {
    term.line(&information("Please remember that when using a view wallet you can only view incoming transactions!"));
    term.line(&format!(
        "{}{}{}",
        information("Therefore, if you have recieved transactions "),
        information("which you then spent, your balance will "),
        information("appear inflated.")
    ));
}

/// `promptSaveKeys` (`Open.cpp:421`).
fn prompt_save_keys(term: &mut dyn Terminal, open: &OpenWallet) {
    term.line("Welcome to your new wallet, here is your payment address:");
    term.line(&information(open.wallet().primary_address().unwrap_or_default()));
    term.write("\nPlease copy your secret keys and mnemonic seed and store them in a secure location:\n\n");

    let wallet = open.wallet();
    if let Some(primary) = wallet.primary_sub_wallet() {
        term.write(&success("\nPrivate spend key:\n"));
        term.write(&success(primary.private_spend_key.to_hex().as_str()));
        term.write("\n");
    }
    term.write(&success("Private view key:\n"));
    term.write(&success(wallet.private_view_key().to_hex().as_str()));
    term.write("\n");
    if let Some(seed) = wallet.mnemonic_seed() {
        term.write(&success("\nMnemonic seed:\n"));
        term.write(&success(seed.as_str()));
        term.write("\n");
    }

    term.write("\n");
}

////////////////////////
/* FILENAMES          */
////////////////////////

/// `getExistingWalletFileName` (`Open.cpp:322`): with or without the extension.
fn get_existing_wallet_filename(term: &mut dyn Terminal, config: &ZedConfig) -> Option<String> {
    let mut initial = true;

    loop {
        let name = if config.wallet_given && initial {
            config.wallet_file.clone()
        } else {
            term.write(&information("What is the name of the wallet "));
            term.write(&information("you want to open?: "));
            term.flush();
            term.read_line()?
        };
        initial = false;

        let with_extension = format!("{name}.wallet");

        if name.is_empty() {
            term.write(&warning("\nWallet name can't be blank! Try again.\n\n"));
        } else if std::path::Path::new(&name).is_file() {
            return Some(name);
        } else if std::path::Path::new(&with_extension).is_file() {
            return Some(with_extension);
        } else {
            term.write(&format!(
                "{}{}{}{}{}",
                warning("\nA wallet with the filename "),
                information(&name),
                warning(" or "),
                information(&with_extension),
                warning(" doesn't exist!\n")
            ));
            term.write("Ensure you entered your wallet name correctly.\n\n");
        }
    }
}

/// `getNewWalletFileName` (`Open.cpp:377`): always `<name>.wallet`, and never
/// one that already exists.
fn get_new_wallet_filename(term: &mut dyn Terminal) -> Option<String> {
    loop {
        term.write(&information("What would you like to call your "));
        term.write(&information("new wallet?: "));
        term.flush();

        let name = term.read_line()?;
        let filename = format!("{name}.wallet");

        if std::path::Path::new(&filename).exists() {
            term.write("\n");
            term.write(&format!(
                "{}{}{}",
                warning("A wallet with the filename "),
                information(&filename),
                warning(" already exists!")
            ));
            term.line("");
            term.line("Try another name.");
            term.write("\n");
        } else if name.is_empty() {
            term.write("\n");
            term.line(&warning("Wallet name can't be blank! Try again."));
            term.write("\n");
        } else {
            return Some(filename);
        }
    }
}

////////////////////////
/* SYNCING            */
////////////////////////

/// A running background sync thread. Dropping it stops and joins it.
pub struct SyncThread {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SyncThread {
    /// Start syncing `wallet` in the background, the way `WalletBackend` starts
    /// its synchronizer thread on open.
    pub fn start(wallet: SharedWallet) -> SyncThread {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            // `WalletSynchronizer::start` (`WalletSynchronizer.cpp:773`).
            crate::logging::log(Level::Debug, format_args!("Starting sync process"));
            let mut ticks_since_info = 0u32;
            let mut log = SyncLog::default();
            while !thread_stop.load(Ordering::SeqCst) {
                let wait = {
                    let mut open = lock(&wallet);
                    ticks_since_info += 1;
                    if ticks_since_info >= 40 {
                        ticks_since_info = 0;
                        open.refresh_info();
                    }
                    let round = open.sync_round();
                    log.record(&round, &open);
                    round.wait
                };
                // A back-off after a `429` is twenty seconds, and `exit` must
                // not sit it out.
                let deadline = std::time::Instant::now() + wait.max(Duration::from_millis(10));
                while !thread_stop.load(Ordering::SeqCst) {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    std::thread::sleep(left.min(Duration::from_millis(100)));
                }
            }
            // `WalletSynchronizer::stop` (`:813`).
            crate::logging::log(Level::Debug, format_args!("Stopping sync process"));
        });

        SyncThread { stop, handle: Some(handle) }
    }

    /// Stop and join.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How often `syncWallet` looks at the background sync (`Sync.cpp:129`).
pub const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How many unproductive rounds before it says syncing may be stuck
/// (`Sync.cpp:117`).
const STUCK_ROUNDS: u32 = 20;

/// `syncWallet` (`Sync.cpp:18`): watch the background sync and print progress
/// and every transaction it turns up, until the wallet catches the daemon.
pub fn sync_wallet(term: &mut dyn Terminal, wallet: &SharedWallet) {
    sync_wallet_every(term, wallet, SYNC_POLL_INTERVAL)
}

/// [`sync_wallet`] with the poll interval given, so a test does not wait the
/// forty seconds it takes the real cadence to declare a stall.
pub fn sync_wallet_every(term: &mut dyn Terminal, wallet: &SharedWallet, poll: Duration) {
    let (mut wallet_height, local_height, network_height) = {
        let status = lock(wallet).sync.sync_status();
        (status.wallet_block_count, status.local_daemon_block_count, status.network_block_count)
    };

    if wallet_height == network_height {
        return;
    }

    // `Sync.cpp:28` writes `localDaemonBlockCount + 1 <= networkBlockCount`,
    // which is this.
    if local_height < network_height {
        term.write(&format!(
            "Your {DAEMON_NAME} isn't fully synced yet!\nUntil you are fully synced, you won't be able to send \
             transactions,\nand your balance may be missing or incorrect!\n\n"
        ));
    }

    if wallet_height == 0 {
        term.write(&information(
            "Scanning through the blockchain to find transactions that belong to you.\nPlease wait, this will take \
             some time.\n\n",
        ));
    } else {
        term.write(&information(
            "Scanning through the blockchain to find any new transactions you received\nwhilst your wallet wasn't \
             open.\nPlease wait, this may take some time.\n\n",
        ));
    }

    let mut stuck_counter = 0u32;
    let mut progress_printed = false;

    loop {
        let (new_height, local, transactions) = {
            let open = lock(wallet);
            let status = open.sync.sync_status();
            let transactions: Vec<_> =
                crate::api::transactions_range(open.wallet(), wallet_height, status.wallet_block_count)
                    .into_iter()
                    .filter(|t| !t.is_fusion_transaction())
                    .cloned()
                    .collect();
            (status.wallet_block_count, status.local_daemon_block_count, transactions)
        };

        if wallet_height >= local {
            break;
        }

        term.write(&format!("\r{}/{} ", success(new_height.to_string()), information(local.to_string())));
        term.flush();
        progress_printed = true;

        stuck_counter = if wallet_height == new_height { stuck_counter + 1 } else { 0 };

        for tx in &transactions {
            if progress_printed {
                term.write("\n");
                progress_printed = false;
            }
            print_transfer_one_line(term, tx);
        }

        wallet_height = new_height;

        if stuck_counter >= STUCK_ROUNDS {
            if progress_printed {
                term.write("\n");
                progress_printed = false;
            }
            term.line(&warning(format!(
                "Syncing may be stuck. Ensure your daemon or remote node is online, and not syncing.\n(Syncing often \
                 stalls wallet operation)\nGive the daemon a restart if possible.\nIf this persists, visit \
                 {CONTACT_LINK} for support."
            )));
            break;
        }

        // The C++ polls its background thread every two seconds.
        std::thread::sleep(poll);
    }

    if progress_printed {
        term.write("\n");
    }
}

////////////////////////
/* THE WALLET LOOP    */
////////////////////////

/// `printStartupHealth` (`Menu.cpp:161`).
fn print_startup_health(term: &mut dyn Terminal, wallet: &SharedWallet) {
    let open = lock(wallet);
    let status = open.sync.sync_status();

    term.line(&information("\nStartup Health Check"));
    term.line(&format!("Daemon reachable: {}", success(if status.network_block_count != 0 { "yes" } else { "no" })));
    term.line(&format!(
        "Wallet/local/network height: {}/{}/{}",
        success(status.wallet_block_count.to_string()),
        success(status.local_daemon_block_count.to_string()),
        success(status.network_block_count.to_string())
    ));
    term.line(&format!("Peers: {}", success(open.peer_count.to_string())));
}

/// `mainLoop` (`Menu.cpp:173`).
pub fn main_loop(term: &mut dyn Terminal, session: &mut Session) {
    print_startup_health(term, &session.wallet);

    if session.is_view_wallet() {
        print_commands(term, &super::commands::all_view_wallet_commands());
    } else {
        print_commands(term, &super::commands::all_commands());
    }

    loop {
        let commands = if session.is_view_wallet() {
            super::commands::all_view_wallet_commands()
        } else {
            super::commands::all_commands()
        };

        let prompt = session.prompt();
        let Selection::Command(command) = parse_command(term, &commands, &commands, &prompt) else {
            return;
        };

        if !session.handle_command(term, &command) {
            return;
        }
    }
}
