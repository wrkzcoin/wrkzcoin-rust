// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet`: the interactive wallet, command for command with the C++
//! `zedwallet++` (`src/zedwallet++/`).
//!
//! Everything the interface does goes through [`term::Terminal`], so the whole
//! program can be driven from a script with no tty — which is how it is tested
//! — and so there is exactly one place that can echo a password.
//!
//! # The opening menu
//!
//! `startupCommands()` (`Commands.cpp:13`): `open`, `create`, `seed_restore`,
//! `key_restore`, `view_wallet`, `exit`. Each may be typed by name or by its
//! one-based number, and `--wallet-file` or `--password` skips straight to
//! `open` (`Menu.cpp:154`).
//!
//! When the daemon does not answer, `nodeDownCommands()` offers `try_again`,
//! `continue`, `swap_node` and `exit` (`Menu.cpp:92`).
//!
//! # The wallet commands
//!
//! `allCommands()` (`Commands.cpp:35`), in its order. "View" is whether a
//! view-only wallet is offered the command at all — the C++ filters the list
//! rather than refusing the command.
//!
//! | Command | C++ | View |
//! | --- | --- | --- |
//! | `help`, `advanced` | `help` `CommandImplementations.cpp:729` | yes |
//! | `exit` | `handleCommand` `:52` | yes |
//! | `status` | `status` `:255` | yes |
//! | `refresh` | `refresh` `:382` | yes |
//! | `swap_node` | `swapNode` `:778` | yes |
//! | `address`, `addr` | `handleCommand` `:32` | yes |
//! | `balance`, `bal` | `balance` `:84` | yes |
//! | `incoming_transfers`, `in` | `listTransfers(true, false)` `:505` | yes |
//! | `outgoing_transfers`, `out` | `listTransfers(false, true)` | **no** |
//! | `list_transfers` | `listTransfers(true, true)` | **no** |
//! | `txs` | `listTransfersBrief` `:745` | **no** |
//! | `txs_full` | `listTransfersVerbose` `:596` | **no** |
//! | `transfer` | `transfer(sendAll = false)` `Transfer.cpp:91` | **no** |
//! | `ab_send` | `sendFromAddressBook` `AddressBook.cpp:198` | **no** |
//! | `send_all` | `transfer(sendAll = true)` | **no** |
//! | `sweep` | `sweep(sweepAll = false)` `Transfer.cpp:396` | **no** |
//! | `sweep_all` | `sweep(sweepAll = true)` | **no** |
//! | `get_tx_private_key` | `getTxPrivateKey` `:790` | yes |
//! | `check_tx [hash]` | `checkTx` `:825` | yes |
//! | `decode_integrated [addr]` | `decodeIntegrated` `:922` | yes |
//! | `ab_add` | `addToAddressBook` `AddressBook.cpp:58` | yes |
//! | `ab_delete` | `deleteFromAddressBook` `:278` | yes |
//! | `ab_list` | `listAddressBook` `:333` | yes |
//! | `make_integrated_address` | `createIntegratedAddress` `:664` | yes |
//! | `backup` | `backup` `:58` | yes |
//! | `change_password` | `changePassword` `:33` | yes |
//! | `save` | `save` `:645` | yes |
//! | `save_csv` | `saveCSV` `:389` | yes |
//! | `reset` | `reset` `:305` | yes |
//! | `set_log_level` | `setLogLevel` `:983` | yes |
//!
//! `check_tx` and `decode_integrated` are the only two that take an inline
//! argument, and the argument keeps its case (`Menu.h:110`).
//!
//! # Secrets
//!
//! A password is read through [`term::Terminal::read_password`], which turns the
//! terminal echo off and hands back a [`zeroize::Zeroizing`] string; it is never
//! written back to the terminal and never logged. A private key, a mnemonic
//! seed and a transaction private key are printed only by the commands whose
//! whole purpose is to print them — `backup` (which asks for the password
//! first), `get_tx_private_key`, and the key dump a freshly created wallet
//! shows once — and `tests/cli.rs` asserts that nothing else prints them.
//!
//! # Logging
//!
//! `--log-level` and `--log-file` are zedwallet++'s — `Logger::LogLevel`, 0
//! disabled to 5 trace, default 1 (fatal) — through [`crate::logging`], and
//! `set_log_level` changes the level while the wallet runs, as
//! `Logger::logger.setLogLevel` does. What is logged is what `WalletBackend`
//! logs: the sync thread's progress, the forks it resolves and the
//! transactions it adds, a daemon that stops answering, and a save that fails.
//!
//! The C++ writes each line to standard output, straight across whatever
//! prompt is on the screen (`ZedWallet.cpp:90`). Here the lines go to standard
//! error, and while the wallet waits at a prompt on a terminal the prompt is
//! taken off before a line and drawn again after it ([`term::StdTerminal`]),
//! so what the user is typing stays on the last row.
//!
//! # What is not here
//!
//! - **Tab completion and command history.** `linenoise` supplies both in the
//!   C++ (`GetInput.cpp:65`). Reproducing them means a raw-mode line editor,
//!   which is a much bigger surface than the rest of this file; the prompt
//!   reads a line plainly instead.
//! - **Colour on a Windows console below the ANSI-capable builds.** ANSI is
//!   emitted directly rather than translated; [`term::set_colour`] turns it off.

pub mod addressbook;
pub mod commands;
pub mod format;
pub mod menu;
pub mod prompt;
pub mod session;
pub mod term;

use wrkz_primitives::constants::RPC_DEFAULT_PORT;

/// `ZedConfig` (`src/zedwallet++/ParseArguments.h`).
#[derive(Clone, Debug)]
pub struct ZedConfig {
    /// `-w`/`--wallet-file`.
    pub wallet_file: String,
    /// Whether `--wallet-file` was given at all, which is what sends the menu
    /// straight to `open`.
    pub wallet_given: bool,
    /// `-p`/`--password`. An empty password is valid, so this is tracked
    /// separately from the string.
    pub wallet_pass: String,
    pub pass_given: bool,
    /// `-r`/`--remote-daemon`, split into host and port.
    pub host: String,
    pub port: u16,
    /// `--ssl`. Only usable in a build with TLS; see
    /// [`crate::daemon::HTTPS_SUPPORTED`].
    pub ssl: bool,
    /// `--log-level`, `0`..=`5` in `Logger::LogLevel` numbering, default `1`
    /// (fatal), as `ZedConfig::logLevel` (`zedwallet++/ParseArguments.h:34`);
    /// see [`crate::logging`].
    pub log_level: i32,
    /// `--log-file`.
    pub log_file: Option<String>,
    /// `--threads`: the threads that scan downloaded blocks
    /// ([`crate::sync::SyncConfig::scan_threads`]). Defaults to one per core,
    /// at most sixteen, as `SyncConfig` does; the C++ default is every core.
    pub threads: u32,
    /// `--skip-coinbase-transactions` / `--skip-coinbase`. Coinbases are
    /// scanned unless this is set, unlike the C++, which skipped them unless
    /// `--scan-coinbase-transactions` was given; that flag is still accepted
    /// and changes nothing.
    pub skip_coinbase_transactions: bool,
}

impl Default for ZedConfig {
    fn default() -> Self {
        ZedConfig {
            wallet_file: String::new(),
            wallet_given: false,
            wallet_pass: String::new(),
            pass_given: false,
            host: "127.0.0.1".to_string(),
            port: RPC_DEFAULT_PORT,
            ssl: false,
            log_level: 1,
            log_file: None,
            threads: crate::sync::SyncConfig::default().scan_threads as u32,
            skip_coinbase_transactions: false,
        }
    }
}

/// The whole program: the opening menu, then the wallet loop.
///
/// Returns the process exit code: `0` normally, `1` when the wallet could not
/// be opened at all.
pub fn run(term: &mut dyn term::Terminal, config: &ZedConfig) -> u8 {
    match menu::selection_screen(term, config) {
        menu::Launch::Exit => {
            term.line("Thanks for stopping by...");
            0
        }
        menu::Launch::Open { wallet, sync } => {
            let mut sync_thread = menu::SyncThread::start(std::sync::Arc::clone(&wallet));

            if sync {
                menu::sync_wallet(term, &wallet);
            }

            let mut session = session::Session::new(std::sync::Arc::clone(&wallet));
            session.log_level = config.log_level;
            menu::main_loop(term, &mut session);

            sync_thread.stop();

            term.write(&term::information("\nSaving and shutting down...\n"));
            let open = match wallet.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Err(e) = open.save() {
                term.line(&term::warning(format!("Failed to save wallet! Error: {e}")));
            }
            drop(open);

            term.line("Thanks for stopping by...");
            0
        }
    }
}
