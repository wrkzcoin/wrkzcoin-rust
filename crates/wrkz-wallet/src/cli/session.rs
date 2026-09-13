// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The command implementations — `src/zedwallet++/CommandDispatcher.cpp`,
//! `CommandImplementations.cpp`, `Transfer.cpp` and `AddressBook.cpp`.
//!
//! [`Session::handle_command`] is the whole interface behind one function, so a
//! test drives every command through scripted input with no terminal. It
//! returns `false` only for `exit`.

use std::sync::{Arc, Mutex, MutexGuard};

use zeroize::Zeroizing;

use wrkz_primitives::base58;
use wrkz_primitives::constants::{MINIMUM_SEND, MINIMUM_UNLOCK_TIME_BLOCKS, SHORT_PAYMENT_ID_LENGTH};

use super::addressbook::{self, AddressBookEntry};
use super::commands::{all_commands, all_view_wallet_commands, log_levels, parse_command, print_commands, Selection};
use super::format::{
    format_amount, format_amount_basic, mining_speed, sync_percentage, unix_time_to_date, CSV_FILENAME, DAEMON_NAME,
    TICKER,
};
use super::prompt::{
    confirm, get_address, get_amount, get_hash, get_payment_id, get_scan_height, validate_address, Answer,
};
use super::term::{information, success, warning, Terminal};
use crate::api::{validate_hash, OpenWallet};
use crate::file::{Hex32, Transaction, WalletError};
use crate::sync::SyncDaemon;
use crate::transfer::{self, SendParams, SystemRandom};

////////////////////////
/* THE SESSION        */
////////////////////////

/// An open wallet plus the interface state around it.
///
/// The wallet is behind a mutex because a background thread syncs it; every
/// command takes the lock for as long as it needs and no longer.
pub struct Session {
    pub wallet: Arc<Mutex<OpenWallet>>,
    /// Where `.addressBook.json` lives. Configurable so a test does not write
    /// into the working directory.
    pub address_book_path: String,
    /// Where `transactions.csv` is written.
    pub csv_path: String,
    /// `--log-level`, which `set_log_level` changes.
    pub log_level: i32,
    /// How often `refresh` and `reset` look at the background sync. The C++'s
    /// two seconds (`Sync.cpp:129`); a test shortens it.
    pub sync_poll: std::time::Duration,
}

impl Session {
    /// A session over an open wallet, using the C++'s file names.
    pub fn new(wallet: Arc<Mutex<OpenWallet>>) -> Session {
        Session {
            wallet,
            address_book_path: super::format::ADDRESS_BOOK_FILENAME.to_string(),
            csv_path: CSV_FILENAME.to_string(),
            log_level: 0,
            sync_poll: super::menu::SYNC_POLL_INTERVAL,
        }
    }

    fn open(&self) -> MutexGuard<'_, OpenWallet> {
        match self.wallet.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Whether the container can spend, which decides which command list the
    /// prompt offers.
    pub fn is_view_wallet(&self) -> bool {
        self.open().wallet().is_view_wallet()
    }

    /// `getPrompt` (`GetInput.cpp:37`): `[WRKZ name (out-of-sync)]: `.
    pub fn prompt(&self) -> String {
        let open = self.open();
        let name = open.filename.strip_suffix(".wallet").unwrap_or(&open.filename);
        let short: String = name.chars().take(20).collect();

        let status = open.sync.sync_status();
        let out_of_sync = status.local_daemon_block_count + 1 < status.network_block_count
            || status.wallet_block_count + 10 < status.network_block_count;

        format!("[{TICKER} {short}{}]: ", if out_of_sync { " (out-of-sync)" } else { "" })
    }

    /// `handleCommand` (`CommandDispatcher.cpp:19`). `false` means exit.
    pub fn handle_command(&mut self, term: &mut dyn Terminal, command: &str) -> bool {
        let (name, argument) = match command.split_once(' ') {
            Some((n, a)) => (n, a.trim()),
            None => (command, ""),
        };

        match name {
            "exit" => return false,

            "help" | "advanced" => self.help(term),
            "address" | "addr" => {
                let address = self.open().wallet().primary_address().unwrap_or_default().to_string();
                term.line(&success(address));
            }
            "balance" | "bal" => self.balance(term),
            "backup" => self.backup(term),
            "refresh" => self.refresh(term),
            "transfer" => self.transfer(term, false),
            "send_all" => self.transfer(term, true),
            "sweep" => self.sweep(term, false),
            "sweep_all" => self.sweep(term, true),
            "ab_add" => self.add_to_address_book(term),
            "ab_delete" => self.delete_from_address_book(term),
            "ab_list" => {
                let book = addressbook::load(term, &self.address_book_path);
                addressbook::list(term, &book);
            }
            "ab_send" => self.send_from_address_book(term),
            "change_password" => self.change_password(term),
            "get_tx_private_key" => self.get_tx_private_key(term),
            "check_tx" => self.check_tx(term, argument),
            "decode_integrated" => self.decode_integrated(term, argument),
            "make_integrated_address" => self.create_integrated_address(term),
            "incoming_transfers" | "in" => self.list_transfers(term, true, false),
            "outgoing_transfers" | "out" => self.list_transfers(term, false, true),
            "list_transfers" => self.list_transfers(term, true, true),
            "txs" => self.list_transfers_brief(term, true, true),
            "txs_full" => self.list_transfers_verbose(term, true, true),
            "reset" => self.reset(term),
            "save" => self.save(term),
            "save_csv" => self.save_csv(term),
            "set_log_level" => self.set_log_level(term),
            "status" => self.status(term),
            "swap_node" => self.swap_node(term),

            // `parseCommand` only ever hands back a name from the list, so this
            // is the C++'s "Command was defined but not hooked up!".
            other => term.line(&warning(format!("Command was defined but not hooked up: {other}"))),
        }

        true
    }

    ////////////////////////
    /* INFORMATION        */
    ////////////////////////

    /// `help` (`CommandImplementations.cpp:729`).
    fn help(&self, term: &mut dyn Terminal) {
        if self.is_view_wallet() {
            print_commands(term, &all_view_wallet_commands());
        } else {
            print_commands(term, &all_commands());
        }
    }

    /// `balance` (`CommandImplementations.cpp:84`).
    fn balance(&self, term: &mut dyn Terminal) {
        let open = self.open();
        let (mut unlocked, locked) = open.sync.total_balance();
        let is_view = open.wallet().is_view_wallet();

        // A view wallet cannot see its own spends, so the C++ approximates by
        // summing the non-fusion transactions instead.
        if is_view {
            unlocked = open
                .wallet()
                .transactions()
                .iter()
                .filter(|t| !t.is_fusion_transaction())
                .map(|t| t.total_amount())
                .sum::<i64>()
                .max(0) as u64;
        }

        let total = unlocked + locked;
        term.line(&format!("Available balance: {}", success(format_amount(unlocked))));
        term.line(&format!("Locked (unconfirmed) balance: {}", warning(format_amount(locked))));
        term.line(&format!("Total balance: {}", information(format_amount(total))));

        if is_view {
            term.write(&information("\nPlease note that view only wallets can only track incoming transactions,\n"));
            term.write(&information("and so your wallet balance may appear inflated.\n"));
        }

        let status = open.sync.sync_status();
        if status.local_daemon_block_count < status.network_block_count {
            term.write(&information("\nYour daemon is not fully synced with the network!\n"));
            term.write("Your balance may be incorrect until you are fully synced!\n");
        } else if status.wallet_block_count + 1000 < status.network_block_count {
            term.write(&information("\nThe blockchain is still being scanned for your transactions.\n"));
            term.write("Balances might be incorrect whilst this is ongoing.\n");
        }
    }

    /// `status` (`CommandImplementations.cpp:255`).
    fn status(&self, term: &mut dyn Terminal) {
        let open = self.open();
        let status = open.sync.sync_status();
        let (wallet, local, network) =
            (status.wallet_block_count, status.local_daemon_block_count, status.network_block_count);

        // printHeights
        term.write("Wallet blockchain height: ");
        term.write(&if wallet + 1000 > network { success(wallet.to_string()) } else { warning(wallet.to_string()) });
        term.write("\nLocal blockchain height: ");
        term.write(&if local == network { success(local.to_string()) } else { warning(local.to_string()) });
        term.line(&format!("\nNetwork blockchain height: {}", success(network.to_string())));

        term.write("\n");

        // printSyncStatus
        let network_percentage = format!("{}%", sync_percentage(local, network));
        let wallet_percentage = format!("{}%", sync_percentage(wallet, network));
        term.write("Network sync status: ");
        term.line(&if local == network { success(network_percentage) } else { warning(network_percentage) });
        term.write("Wallet sync status: ");
        term.line(&if wallet + 10 > network { success(wallet_percentage) } else { warning(wallet_percentage) });

        term.write("\n");

        // printHashrate — silent when the daemon has not answered.
        if open.hashrate != 0 {
            term.line(&format!(
                "Network hashrate: {} (Based on the last local block)",
                success(mining_speed(open.hashrate))
            ));
        }

        term.line(&format!("Peers: {}\n", success(open.peer_count.to_string())));

        // printSyncSummary
        if local == 0 && network == 0 {
            term.line(&format!(
                "{}{}{}",
                warning("Uh oh, it looks like you don't have "),
                warning(DAEMON_NAME),
                warning(" open!")
            ));
        } else if wallet + 1000 < network && local == network {
            term.line(&information(
                "You are synced with the network, but the blockchain is still being scanned for your transactions.",
            ));
            term.line("Balances might be incorrect whilst this is ongoing.");
        } else if local == network {
            term.line(&success("Yay! You are synced!"));
        } else {
            term.line(&warning("Be patient, you are still syncing with the network!"));
        }

        let lite_start = open.sync.daemon_state().lite_start_height;
        if lite_start != 0 {
            term.write("\n");
            term.line(&format!(
                "{}{}{}",
                information("This node is a lite node and holds no data below height "),
                information(lite_start.to_string()),
                information(".")
            ));
            term.line("Transactions received below that height cannot be found through it.");
        }

        if let Some((covered_to, serves_from)) = open.sync_gap {
            term.write("\n");
            term.line(&format!(
                "{}{}{}{}{}",
                warning("Sync has stopped. This wallet has scanned to height "),
                warning(covered_to.to_string()),
                warning(", but the node it was talking to answers only from height "),
                warning(serves_from.to_string()),
                warning(" upward.")
            ));
            term.line("Your balance is incomplete until this is resolved. Connect a node that holds");
            term.line(&format!(
                "the whole chain, or reset the wallet to {serves_from} and accept that earlier transactions"
            ));
            term.line("cannot be found here.");
        }
    }

    ////////////////////////
    /* SECRETS            */
    ////////////////////////

    /// `backup` (`CommandImplementations.cpp:58`): the password first, then the
    /// keys. Nothing here is printed without that confirmation.
    fn backup(&self, term: &mut dyn Terminal) {
        if !self.confirm_password(term, "Confirm your current password: ") {
            return;
        }
        self.print_private_keys(term);
    }

    /// `printPrivateKeys` (`CommandImplementations.cpp:64`).
    fn print_private_keys(&self, term: &mut dyn Terminal) {
        let open = self.open();
        let wallet = open.wallet();

        if !wallet.is_view_wallet() {
            if let Some(primary) = wallet.primary_sub_wallet() {
                term.write(&success("\nPrivate spend key:\n"));
                term.write(&success(primary.private_spend_key.to_hex().as_str()));
                term.write("\n");
            }
        }

        term.write(&success("Private view key:\n"));
        term.write(&success(wallet.private_view_key().to_hex().as_str()));
        term.write("\n");

        if let Some(seed) = wallet.mnemonic_seed() {
            term.write(&success("\nMnemonic seed:\n"));
            term.write(&success(seed.as_str()));
            term.write("\n");
        }
    }

    /// `ZedUtilities::confirmPassword` (`zedwallet++/Utilities.cpp:14`): keep
    /// asking until the wallet's own password is given back.
    pub fn confirm_password(&self, term: &mut dyn Terminal, msg: &str) -> bool {
        loop {
            term.write(&information(msg));
            term.flush();

            let Some(given) = term.read_password() else {
                // No more input: treat as a refusal rather than looping.
                return false;
            };

            if *given == *self.open().password {
                return true;
            }
            term.line(&warning("Incorrect password! Try again."));
        }
    }

    /// `changePassword` (`CommandImplementations.cpp:33`).
    fn change_password(&mut self, term: &mut dyn Terminal) {
        if !self.confirm_password(term, "Confirm your current password: ") {
            return;
        }

        let Some(new_password) = super::menu::get_wallet_password(term, true, "Enter your new password: ") else {
            return;
        };

        let mut open = self.open();
        open.password = new_password;
        match open.save() {
            Ok(()) => term.line(&success("Your password has been changed!")),
            Err(_) => term.line(&warning(
                "Your password has been changed, but saving the updated wallet failed. If you quit without saving \
                 succeeding, your password may not update.",
            )),
        }
    }

    /// `getTxPrivateKey` (`CommandImplementations.cpp:790`).
    fn get_tx_private_key(&self, term: &mut dyn Terminal) {
        let Answer::Given(hash) =
            get_hash(term, "What transaction hash do you want to get the private key of?: ", true)
        else {
            return;
        };

        let open = self.open();
        let Some(hash) = Hex32::from_hex(&hash) else {
            term.line(&warning(WalletError::HashInvalid.to_string()));
            return;
        };

        match open.wallet().tx_private_key(&hash) {
            None => term.line(&warning(WalletError::TxPrivateKeyNotFound.to_string())),
            Some(key) => {
                term.line(&format!("{}{}", information("Transaction private key: "), success(key.to_hex().as_str())))
            }
        }
    }

    ////////////////////////
    /* ADDRESSES          */
    ////////////////////////

    /// `createIntegratedAddress` (`CommandImplementations.cpp:664`).
    fn create_integrated_address(&self, term: &mut dyn Terminal) {
        term.write(&information("Creating an integrated address from an "));
        term.line(&information("address and payment ID pair..."));
        term.write("\n");

        let primary = self.open().wallet().primary_address().unwrap_or_default().to_string();

        let address = loop {
            term.write(&information("Address: "));
            term.flush();
            let Some(mut address) = term.read_line() else { return };
            address = address.trim().to_string();

            if address.is_empty() {
                address = primary.clone();
                term.line(&format!(
                    "{}{}",
                    information("No address provided. Using primary wallet address: "),
                    success(&address)
                ));
            }

            match validate_address(&address, false) {
                Ok(()) => break address,
                Err(e) => term.line(&format!("{}{}", warning("Invalid address: "), warning(e.to_string()))),
            }
        };

        let payment_id = loop {
            term.write(&information("Payment ID: "));
            term.flush();
            let Some(mut payment_id) = term.read_line() else { return };
            payment_id = payment_id.trim().to_string();

            if payment_id.is_empty() {
                let mut bytes = [0u8; 8];
                if getrandom::fill(&mut bytes).is_err() {
                    term.line(&warning("Could not generate a random payment ID."));
                    return;
                }
                payment_id = crate::api::to_hex(&bytes);
                term.line(&format!(
                    "{}{}",
                    information("No payment ID provided. Generated random short payment ID: "),
                    success(&payment_id)
                ));
            }

            match transfer::validate_payment_id(&payment_id) {
                Ok(()) => break payment_id,
                Err(e) => term.line(&format!("{}{}", warning("Invalid payment ID: "), warning(e.to_string()))),
            }
        };

        let Ok(parsed) = base58::parse_address(&address) else {
            term.line(&warning("Failed to create integrated address: the address could not be parsed."));
            return;
        };

        match base58::integrated_address(&parsed.spend_public_key, &parsed.view_public_key, &payment_id) {
            Ok(integrated) => term.line(&information(integrated)),
            Err(e) => term.line(&format!(
                "{}{}",
                warning("Failed to create integrated address: "),
                warning(WalletError::InvalidAddress(e).to_string())
            )),
        }
    }

    /// `decodeIntegrated` (`CommandImplementations.cpp:922`).
    fn decode_integrated(&self, term: &mut dyn Terminal, argument: &str) {
        let mut integrated = argument.trim().to_string();

        if integrated.is_empty() {
            term.write(&information("Integrated address to decode (or cancel): "));
            term.flush();
            let Some(line) = term.read_line() else { return };
            integrated = line.trim().to_string();
        }

        if integrated == "cancel" {
            return;
        }

        if !base58::is_integrated_address(&integrated) {
            term.line(&warning("This is not an integrated address format for this network."));
            term.line(&format!(
                "{}{}{}{}{}",
                information("Expected length: "),
                success(wrkz_primitives::constants::INTEGRATED_ADDRESS_LENGTH.to_string()),
                information(" (short) or "),
                success(wrkz_primitives::constants::INTEGRATED_ADDRESS_LENGTH_LONG.to_string()),
                information(" (long).")
            ));
            return;
        }

        let parsed = match base58::parse_address(&integrated) {
            Ok(p) => p,
            Err(e) => {
                term.line(&format!(
                    "{}{}",
                    warning("Invalid integrated address: "),
                    warning(WalletError::InvalidAddress(e).to_string())
                ));
                return;
            }
        };

        let actual = base58::standard_address(&parsed.spend_public_key, &parsed.view_public_key);
        term.line(&format!("{}{}", information("Decoded address: "), success(&actual)));
        term.line(&format!(
            "{}{}",
            information("Embedded payment ID: "),
            success(parsed.payment_id.clone().unwrap_or_default())
        ));

        if self.open().wallet().primary_address() == Some(actual.as_str()) {
            term.line(&success("This integrated address maps to your primary wallet address."));
        }
    }

    ////////////////////////
    /* ADDRESS BOOK       */
    ////////////////////////

    /// `addToAddressBook` (`AddressBook.cpp:58`).
    fn add_to_address_book(&self, term: &mut dyn Terminal) {
        term.line(&information("Note: You can type cancel at any time to cancel adding someone to your address book."));
        term.write("\n");

        let mut book = addressbook::load(term, &self.address_book_path);

        let friendly_name = loop {
            term.write(&information("What friendly name do you want to "));
            term.write(&information("give this address book entry?: "));
            term.flush();

            let Some(name) = term.read_line() else { return };
            let name = name.trim().to_string();

            if name.is_empty() {
                term.line(&warning("Friendly name cannot be empty."));
                term.write("\n");
                continue;
            }
            if name == "cancel" {
                term.line(&warning("Cancelling addition to address book."));
                return;
            }
            if book.iter().any(|e| e.friendly_name == name) {
                term.line(&format!(
                    "{}{}",
                    warning("An address book entry with this "),
                    warning("name already exists!")
                ));
                term.write("\n");
                continue;
            }
            break name;
        };

        let Answer::Given(address) = get_address(term, "\nWhat address does this user have?: ", true, true) else {
            term.line(&warning("Cancelling addition to address book."));
            return;
        };

        term.line(&format!("{}{}", information("Address type: "), success(address_type_label(&address))));

        let payment_id = if base58::is_integrated_address(&address) {
            term.line(&information("Integrated address detected. Payment ID is already embedded."));
            String::new()
        } else {
            match get_payment_id(term, "\nDoes this address book entry have a payment ID associated with it?\n", true) {
                Answer::Given(p) => p,
                Answer::Cancel => {
                    term.line(&warning("Cancelling addition to address book."));
                    return;
                }
            }
        };

        book.push(AddressBookEntry { friendly_name, address, payment_id });

        if addressbook::save(term, &self.address_book_path, &book) {
            term.write(&success("\nA new entry has been added to your address book!\n"));
        }
    }

    /// `deleteFromAddressBook` (`AddressBook.cpp:278`).
    fn delete_from_address_book(&self, term: &mut dyn Terminal) {
        let mut book = addressbook::load(term, &self.address_book_path);
        if addressbook::is_empty(term, &book) {
            return;
        }

        loop {
            term.write(&information("Note: You can type cancel at any time "));
            term.write(&information("to cancel the deletion.\n\n"));
            term.write(&information("What address book entry do you want to "));
            term.write(&information("delete?: "));
            term.flush();

            let Some(name) = term.read_line() else { return };
            let name = name.trim().to_string();

            if name == "cancel" {
                term.write(&warning("Cancelling deletion.\n"));
                return;
            }

            if book.iter().any(|e| e.friendly_name == name) {
                book.retain(|e| e.friendly_name != name);
                if addressbook::save(term, &self.address_book_path, &book) {
                    term.write("\n");
                    term.line(&format!(
                        "{}{}",
                        success("This entry has been deleted from "),
                        success("your address book!")
                    ));
                }
                return;
            }

            term.write("\n");
            term.line(&format!(
                "{}{}{}",
                warning("Could not find a user with the name of "),
                information(&name),
                warning(" in your address book!")
            ));
            term.write("\n");

            if confirm(term, "Would you like to list everyone in your address book?") {
                term.write("\n");
                addressbook::list(term, &book);
            } else {
                term.write("\n");
            }
        }
    }

    /// `sendFromAddressBook` (`AddressBook.cpp:198`).
    fn send_from_address_book(&mut self, term: &mut dyn Terminal) {
        let book = addressbook::load(term, &self.address_book_path);
        if addressbook::is_empty(term, &book) {
            return;
        }

        term.write(&information("Note: You can type cancel at any time to "));
        term.write(&information("cancel the transaction\n\n"));

        let Some(entry) = addressbook::pick(term, &book) else {
            term.write(&warning("Cancelling transaction.\n"));
            return;
        };

        let Answer::Given(amount) = get_amount(term, &format!("How much {TICKER} do you want to send?: "), true) else {
            term.write(&warning("Cancelling transaction.\n"));
            return;
        };

        self.send_transaction(term, &entry.address, amount, &entry.payment_id, false);
    }

    ////////////////////////
    /* TRANSACTIONS       */
    ////////////////////////

    /// The confirmed and unconfirmed lists, unconfirmed last — what every
    /// listing walks (`CommandImplementations.cpp:511`).
    fn all_transactions(&self) -> Vec<Transaction> {
        let open = self.open();
        let mut txs = open.wallet().transactions().to_vec();
        txs.extend_from_slice(open.wallet().unconfirmed_transactions());
        txs
    }

    /// `listTransfers` (`CommandImplementations.cpp:505`): one line each, then
    /// a summary.
    fn list_transfers(&self, term: &mut dyn Terminal, incoming: bool, outgoing: bool) {
        let mut total_spent = 0u64;
        let mut total_received = 0u64;
        let mut num_incoming = 0u64;
        let mut num_outgoing = 0u64;

        for tx in self.all_transactions() {
            if tx.is_fusion_transaction() {
                continue;
            }
            let amount = tx.total_amount();
            if amount < 0 && outgoing {
                print_transfer_one_line(term, &tx);
                total_spent += amount.unsigned_abs();
                num_outgoing += 1;
            } else if amount > 0 && incoming {
                print_transfer_one_line(term, &tx);
                total_received += amount as u64;
                num_incoming += 1;
            }
        }

        self.print_summary(term, incoming, outgoing, num_incoming, total_received, num_outgoing, total_spent);
    }

    /// `listTransfersVerbose` (`CommandImplementations.cpp:596`).
    fn list_transfers_verbose(&self, term: &mut dyn Terminal, incoming: bool, outgoing: bool) {
        let mut total_spent = 0u64;
        let mut total_received = 0u64;
        let mut num_incoming = 0u64;
        let mut num_outgoing = 0u64;

        for tx in self.all_transactions() {
            if tx.is_fusion_transaction() {
                continue;
            }
            let amount = tx.total_amount();
            if amount < 0 && outgoing {
                print_outgoing_transfer(term, &tx);
                total_spent += amount.unsigned_abs();
                num_outgoing += 1;
            } else if amount > 0 && incoming {
                print_incoming_transfer(term, &tx);
                total_received += amount as u64;
                num_incoming += 1;
            }
        }

        self.print_summary(term, incoming, outgoing, num_incoming, total_received, num_outgoing, total_spent);
    }

    /// `listTransfersBrief` (`CommandImplementations.cpp:745`): the same one
    /// line each, paged twenty-five at a time.
    fn list_transfers_brief(&self, term: &mut dyn Terminal, incoming: bool, outgoing: bool) {
        const PAGE_SIZE: u64 = 25;
        let mut displayed = 0u64;
        let mut matched = 0u64;

        for tx in self.all_transactions() {
            if tx.is_fusion_transaction() {
                continue;
            }
            let amount = tx.total_amount();
            let is_incoming = amount > 0;
            let is_outgoing = amount < 0;

            if (is_incoming && !incoming) || (is_outgoing && !outgoing) || (!is_incoming && !is_outgoing) {
                continue;
            }

            print_transfer_one_line(term, &tx);
            matched += 1;
            displayed += 1;

            if displayed.is_multiple_of(PAGE_SIZE) {
                term.write(&information("Press Enter for more, or type q to stop: "));
                term.flush();
                match term.read_line() {
                    None => break,
                    Some(input) if input.trim().eq_ignore_ascii_case("q") => break,
                    Some(_) => {}
                }
            }
        }

        term.line(&format!(
            "{}{}{}",
            information("Displayed "),
            success(matched.to_string()),
            information(" transfer(s).")
        ));
    }

    fn print_summary(
        &self,
        term: &mut dyn Terminal,
        incoming: bool,
        outgoing: bool,
        num_incoming: u64,
        total_received: u64,
        num_outgoing: u64,
        total_spent: u64,
    ) {
        term.write(&information("Summary:\n\n"));
        if incoming {
            term.line(&format!(
                "{}{}{}",
                success(num_incoming.to_string()),
                success(" incoming transactions, totalling "),
                success(format_amount(total_received))
            ));
        }
        if outgoing {
            term.line(&format!(
                "{}{}{}",
                warning(num_outgoing.to_string()),
                warning(" outgoing transactions, totalling "),
                warning(format_amount(total_spent))
            ));
        }
    }

    /// `checkTx` (`CommandImplementations.cpp:825`): what this wallet knows,
    /// then what the daemon knows.
    fn check_tx(&self, term: &mut dyn Terminal, argument: &str) {
        let mut hash_text = argument.trim().to_string();

        if hash_text.is_empty() {
            let Answer::Given(given) = get_hash(term, "Transaction hash to check (or cancel): ", true) else {
                return;
            };
            hash_text = given;
        }

        if hash_text == "cancel" {
            return;
        }

        if let Err(e) = validate_hash(&hash_text) {
            term.line(&format!("{}{}", warning("Invalid hash: "), warning(e.to_string())));
            return;
        }

        let Some(hash) = Hex32::from_hex(&hash_text) else {
            term.line(&format!("{}{}", warning("Invalid hash: "), warning(WalletError::HashInvalid.to_string())));
            return;
        };

        let open = self.open();

        term.write(&information("Wallet lookup: "));
        if let Some(tx) = open.wallet().unconfirmed_transactions().iter().find(|t| t.hash == hash) {
            term.line(&warning("found (pending outgoing in pool)"));
            term.line(&format!(
                "  amount: {}, fee: {}",
                warning(format_amount(tx.total_amount().unsigned_abs())),
                warning(format_amount(tx.fee))
            ));
        } else if let Some(tx) = open.wallet().transactions().iter().find(|t| t.hash == hash) {
            let incoming = tx.total_amount() > 0;
            term.line(&format!(
                "{}{}{}",
                success("found (confirmed "),
                success(if incoming { "incoming" } else { "outgoing" }),
                success(")")
            ));
            term.write(&format!("  block: {}, amount: ", success(tx.block_height.to_string())));
            let amount = format_amount(tx.total_amount().unsigned_abs());
            term.line(&if incoming { success(amount) } else { warning(amount) });
            if !tx.payment_id.is_empty() {
                term.line(&format!("  payment ID: {}", success(&tx.payment_id)));
            }
        } else {
            term.line(&warning("not found in this wallet"));
        }

        term.write(&information("Node lookup: "));
        let Ok(status) = open.sync.daemon().transactions_status(&[hash_text.clone()]) else {
            term.line(&warning("failed to query daemon"));
            return;
        };

        if status.transactions_in_pool.iter().any(|h| h.eq_ignore_ascii_case(&hash_text)) {
            term.line(&warning("transaction is in pool"));
        } else if status.transactions_in_block.iter().any(|h| h.eq_ignore_ascii_case(&hash_text)) {
            term.line(&success("transaction is in a block"));
        } else if status.transactions_unknown.iter().any(|h| h.eq_ignore_ascii_case(&hash_text)) {
            term.line(&warning("transaction is unknown to daemon"));
        } else {
            term.line(&warning("daemon returned no status for this transaction"));
        }
    }

    ////////////////////////
    /* SENDING            */
    ////////////////////////

    /// `transfer` (`Transfer.cpp:91`).
    fn transfer(&mut self, term: &mut dyn Terminal, send_all: bool) {
        term.write(&information("Note: You can type cancel at any time to cancel the transaction\n\n"));

        let unlocked_balance = self.open().sync.total_balance().0;

        let Answer::Given(address) = get_address(term, "What address do you want to transfer to?: ", true, true) else {
            cancel(term);
            return;
        };

        term.line(&format!("{}{}", information("Address type: "), success(address_type_label(&address))));
        term.write("\n");

        let payment_id = if base58::is_integrated_address(&address) {
            term.line(&information("Integrated address detected. Payment ID is embedded; skipping payment ID prompt."));
            term.write("\n");
            String::new()
        } else {
            let answer = get_payment_id(
                term,
                "What payment ID do you want to use?\nThese are usually used for sending to exchanges.",
                true,
            );
            match answer {
                Answer::Given(p) => {
                    term.write("\n");
                    p
                }
                Answer::Cancel => {
                    cancel(term);
                    return;
                }
            }
        };

        let (address, payment_id) = offer_integrated_address(term, &address, &payment_id);

        // With send_all the real amount is worked out once the fee is known.
        let amount = if send_all {
            unlocked_balance
        } else {
            match get_amount(term, &format!("How much {TICKER} do you want to send?: "), true) {
                Answer::Given(a) => {
                    term.write("\n");
                    a
                }
                Answer::Cancel => {
                    term.write("\n");
                    cancel(term);
                    return;
                }
            }
        };

        self.send_transaction(term, &address, amount, &payment_id, send_all);
    }

    /// `sendTransaction` (`Transfer.cpp:176`): build, confirm, relay.
    fn send_transaction(
        &mut self,
        term: &mut dyn Terminal,
        address: &str,
        amount: u64,
        payment_id: &str,
        send_all: bool,
    ) {
        let mut destination = address.to_string();
        let mut effective_payment_id = payment_id.to_string();

        if base58::is_integrated_address(&destination) {
            let Ok(parsed) = base58::parse_address(&destination) else {
                term.line(&warning("Invalid integrated address."));
                return;
            };
            let embedded = parsed.payment_id.clone().unwrap_or_default();
            destination = base58::standard_address(&parsed.spend_public_key, &parsed.view_public_key);
            if effective_payment_id.is_empty() {
                effective_payment_id = embedded;
            } else if effective_payment_id != embedded {
                term.line(&warning("Conflicting payment IDs detected between integrated address and input PID."));
                cancel(term);
                return;
            }
        }

        let unlocked_balance = self.open().sync.total_balance().0;

        if amount > unlocked_balance {
            term.write(&warning("\nYou don't have enough funds to cover this transaction!\n\n"));
            term.write(&format!("Funds needed: {}", information(format_amount(amount))));
            term.line(&format!("\nFunds available: {}\n", success(format_amount(unlocked_balance))));
            cancel(term);
            return;
        }

        // Build without relaying, so the fee and the ring size are known before
        // the user is asked to approve.
        let prepared = {
            let mut open = self.open();
            let (network_height, daemon_height) = (open.network_height(), open.daemon_height());
            let params = SendParams {
                send_all,
                pow_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
                ..SendParams::basic(&destination, amount, &effective_payment_id, network_height, daemon_height)
            };
            let mut random = SystemRandom;
            let (wallet, daemon) = open.sync.split_for_transfer();
            transfer::prepare_transaction(wallet, daemon, &params, &mut random)
        };

        let prepared = match prepared {
            Ok(p) => p,
            Err(WalletError::NotEnoughBalance { needed, .. }) => {
                let actual_amount = if send_all { MINIMUM_SEND } else { amount };
                term.write(&warning("\nYou don't have enough funds to cover this transaction!\n\n"));
                term.write(&format!("Funds needed: {}", information(format_amount(needed.max(actual_amount)))));
                term.line(&format!("\nFunds available: {}\n", success(format_amount(unlocked_balance))));
                cancel(term);
                return;
            }
            Err(e @ WalletError::TooManyInputsToFitInBlock { .. }) => {
                let _ = e;
                term.write(&warning("Your transaction is too large to fit in a block.\n\n"));
                term.write(&information(
                    "This usually means you have many small inputs that cannot be\ncombined into a single \
                     transaction within the block size limit.\n\n",
                ));
                term.write(&format!(
                    "Use the {} {}",
                    success("sweep"),
                    information("command to send this amount across\n")
                ));
                term.write(&information("multiple transactions automatically, without needing to optimize first.\n\n"));
                term.write(&format!(
                    "{}{}{}",
                    information("Example: "),
                    success("sweep"),
                    information(" — then enter the destination address and amount.\n")
                ));
                cancel(term);
                return;
            }
            Err(e) => {
                term.line(&format!("{}{}", warning("Failed to send transaction: "), warning(e.to_string())));
                return;
            }
        };

        let actual_amount = if send_all { unlocked_balance.saturating_sub(prepared.fee) } else { amount };

        if !self.confirm_transaction(
            term,
            &destination,
            actual_amount,
            &effective_payment_id,
            prepared.fee,
            prepared.mixin,
        ) {
            cancel(term);
            return;
        }

        let sent = {
            let mut open = self.open();
            let network_height = open.network_height();
            let (wallet, daemon) = open.sync.split_for_transfer();
            transfer::send_prepared_transaction(wallet, daemon, prepared, network_height)
        };

        match sent {
            Err(e) => term.line(&format!("{}{}", warning("Failed to send transaction: "), warning(e.to_string()))),
            Ok(sent) => {
                term.line(&format!(
                    "{}{}",
                    success("Transaction has been sent!\nHash: "),
                    success(sent.transaction_hash.to_hex())
                ));

                if confirm(term, "Watch transaction status now (pool -> block)?") {
                    self.watch_transaction(term, &sent.transaction_hash);
                }

                term.line(&information(
                    "Note: recipients typically see incoming funds once a block confirms the transaction.",
                ));
            }
        }

        // The wallet has changed; keep the file in step with it.
        let _ = self.open().save();
    }

    /// `confirmTransaction` (`Transfer.cpp:296`): the amount, the fee, the
    /// destination and the payment ID, then the password.
    ///
    /// Public so a test can assert on exactly what a user is shown before their
    /// money moves.
    pub fn confirm_transaction(
        &self,
        term: &mut dyn Terminal,
        address: &str,
        amount: u64,
        payment_id: &str,
        fee: u64,
        mixin: u64,
    ) -> bool {
        // `nodeFee` is always zero: no WrkzCoin daemon serves the `/fee` route
        // `Nigel` reads it from.
        let node_fee = 0u64;
        let total = amount + fee + node_fee;

        term.write(&information("\nConfirm Transaction?\n"));
        term.write(&format!(
            "You are sending {}, with a network fee of {},\nand a node fee of {}, for a total of {}",
            success(format_amount(amount)),
            success(format_amount(fee)),
            success(format_amount(node_fee)),
            success(format_amount(total))
        ));

        if payment_id.is_empty() {
            term.write(".");
        } else {
            term.write(&format!(",\nand a Payment ID of {}", success(payment_id)));
            if payment_id.len() == SHORT_PAYMENT_ID_LENGTH {
                term.write(&information("\n(short - encrypted, only the receiver can read it)"));
            } else {
                term.write(&warning("\n(long - stored in plaintext, readable by anyone)"));
            }
        }

        let filename = self.open().filename.clone();
        term.write(&format!("\n\nFROM: {}\nTO: {}\n\n", success(&filename), success(address)));

        term.line(&format!(
            "{}{}{}",
            information("Estimated minimum spendable delay after confirmation: "),
            success(MINIMUM_UNLOCK_TIME_BLOCKS.to_string()),
            information(" blocks")
        ));

        // A ring below the tier default means the denominations being spent do
        // not have enough outputs on chain. Say so before approval, not after.
        // The tier at the daemon's own top block, the one the send was built for
        // (`zedwallet++/Transfer.cpp`, C++ `0b58b035`).
        let daemon_height = self.open().daemon_height();
        let default_mixin = wrkz_primitives::mixins::mixin_allowable_range(daemon_height).default;
        if mixin < default_mixin {
            term.write(&format!(
                "{}{}{}{}{}\n",
                warning("\nRing size reduced to "),
                warning((mixin + 1).to_string()),
                warning(" (normally "),
                warning((default_mixin + 1).to_string()),
                warning(").")
            ));
            term.write(&information(
                "The amounts being sent do not have enough outputs on the chain\nto form a full ring. This \
                 transaction is less private than usual.\n",
            ));
            term.write("\n");
        }

        if !confirm(term, "Is this correct?") {
            return false;
        }

        self.confirm_password(term, "Confirm your password: ")
    }

    /// `watchTransactionUntilConfirmed` (`Transfer.cpp:45`).
    fn watch_transaction(&self, term: &mut dyn Terminal, hash: &Hex32) {
        const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        const MAX_CHECKS: usize = 4;

        for _ in 0..MAX_CHECKS {
            {
                let open = self.open();
                if open.wallet().unconfirmed_transactions().iter().any(|t| t.hash == *hash) {
                    term.line(&information("Status: pending in pool..."));
                } else if let Some(tx) = open.wallet().transactions().iter().find(|t| t.hash == *hash) {
                    term.line(&format!(
                        "{}{}",
                        success("Status: confirmed in block "),
                        success(tx.block_height.to_string())
                    ));
                    return;
                }
            }
            std::thread::sleep(INTERVAL);
        }

        term.line(&warning("Status still pending. Returning to prompt so you can continue using the wallet."));
        term.line(&format!(
            "{}{}{}{}{}",
            information("Use "),
            success("txs"),
            information(" / "),
            success("list_transfers"),
            information(" to check confirmation later.")
        ));
    }

    /// `sweep` (`Transfer.cpp:396`).
    fn sweep(&mut self, term: &mut dyn Terminal, sweep_all: bool) {
        term.write(&information("Note: You can type cancel at any time to cancel the sweep\n\n"));

        if self.is_view_wallet() {
            term.write(&warning("Sweep is not available for view-only wallets.\n"));
            return;
        }

        let unlocked_balance = self.open().sync.total_balance().0;
        if unlocked_balance == 0 {
            term.write(&warning("You have no unlocked balance to sweep.\n"));
            return;
        }

        let Answer::Given(address) = get_address(term, "What address do you want to sweep to?: ", true, true) else {
            term.write(&warning("Cancelling sweep.\n"));
            return;
        };

        term.write(&format!("{}{}\n\n", information("Address type: "), success(address_type_label(&address))));

        let payment_id = if base58::is_integrated_address(&address) {
            term.write(&information(
                "Integrated address detected. Payment ID is embedded; skipping payment ID prompt.\n\n",
            ));
            String::new()
        } else {
            match get_payment_id(
                term,
                "What payment ID do you want to use?\nThese are usually used for sending to exchanges.",
                true,
            ) {
                Answer::Given(p) => {
                    term.write("\n");
                    p
                }
                Answer::Cancel => {
                    term.write(&warning("Cancelling sweep.\n"));
                    return;
                }
            }
        };

        let (address, payment_id) = offer_integrated_address(term, &address, &payment_id);

        let amount_to_sweep = if sweep_all {
            0
        } else {
            match get_amount(term, &format!("How much {TICKER} do you want to sweep?: "), true) {
                Answer::Given(a) => {
                    term.write("\n");
                    if a > unlocked_balance {
                        term.line(&format!(
                            "{}{}",
                            warning("Amount exceeds unlocked balance of "),
                            success(format_amount(unlocked_balance))
                        ));
                        return;
                    }
                    a
                }
                Answer::Cancel => {
                    term.write("\n");
                    term.write(&warning("Cancelling sweep.\n"));
                    return;
                }
            }
        };

        let description = if sweep_all {
            format!("entire unlocked balance ({})", format_amount(unlocked_balance))
        } else {
            format_amount(amount_to_sweep)
        };

        let (est_tx_count, est_total_fee) = {
            let open = self.open();
            let daemon_height = open.daemon_height();
            transfer::estimate_sweep(open.wallet(), &payment_id, amount_to_sweep, daemon_height)
        };

        term.write(&information("\nSweep Summary\n"));
        term.line(&format!("Sweeping:    {}", success(&description)));
        term.line(&format!("To: {}", success(&address)));
        if !payment_id.is_empty() {
            term.line(&format!("Payment ID:  {}", success(&payment_id)));
        }
        if est_tx_count > 0 {
            term.line(&format!("Transactions:{}", success(format!(" {est_tx_count}"))));
            term.line(&format!("Total fee: {}", warning(format_amount(est_total_fee))));
        } else {
            term.write(&warning(
                "Could not estimate transaction count (no spendable inputs or inputs too small to cover fees).\n",
            ));
        }
        term.write("\n");

        if !confirm(term, "Proceed with sweep?") {
            term.write(&warning("Cancelling sweep.\n"));
            return;
        }

        if !self.confirm_password(term, "Confirm your password: ") {
            term.write(&warning("Cancelling sweep.\n"));
            return;
        }

        term.write("\n");
        term.write(&information("Sending sweep transactions...\n\n"));

        let results = {
            let mut open = self.open();
            let daemon_height = open.daemon_height();
            let mut random = SystemRandom;
            let (wallet, daemon) = open.sync.split_for_transfer();
            transfer::sweep_to_address(
                wallet,
                daemon,
                &address,
                &payment_id,
                amount_to_sweep,
                daemon_height,
                &mut random,
            )
        };

        let mut success_count = 0u64;
        let mut fail_count = 0u64;

        for (i, result) in results.iter().enumerate() {
            term.write(&format!(
                "{}{}{}{}: ",
                information("Batch "),
                success((i + 1).to_string()),
                information("/"),
                success(results.len().to_string())
            ));
            match result {
                Err(e) => {
                    term.line(&format!("{}{}", warning("Failed — "), warning(e.to_string())));
                    fail_count += 1;
                }
                Ok(hash) => {
                    term.line(&format!("{}{}", success("Sent — Hash: "), success(hash.to_hex())));
                    success_count += 1;
                }
            }
        }

        term.write("\n");

        if success_count > 0 && fail_count == 0 {
            term.write(&format!(
                "{}{}{}",
                success("Sweep complete. "),
                success(success_count.to_string()),
                success(" transaction(s) sent successfully.\n")
            ));
        } else if success_count > 0 {
            term.write(&format!(
                "{}{}{}{}{}",
                warning("Sweep partially complete. "),
                success(success_count.to_string()),
                information(" sent, "),
                warning(fail_count.to_string()),
                warning(" failed.\n")
            ));
        } else {
            term.write(&warning("Sweep failed. No transactions were sent successfully.\n"));
        }

        let _ = self.open().save();
    }

    ////////////////////////
    /* MAINTENANCE        */
    ////////////////////////

    /// `save` (`CommandImplementations.cpp:645`).
    fn save(&self, term: &mut dyn Terminal) {
        term.line(&information("Saving."));
        match self.open().save() {
            Ok(()) => term.line(&information("Saved.")),
            Err(e) => term.line(&format!("{}{}", warning("Failed to save wallet! Error: "), warning(e.to_string()))),
        }
    }

    /// `saveCSV` (`CommandImplementations.cpp:389`).
    fn save_csv(&self, term: &mut dyn Terminal) {
        let transactions = self.open().wallet().transactions().to_vec();

        if transactions.is_empty() {
            term.write(&warning("You have no transactions to save to the CSV!\n"));
            return;
        }

        term.line(&information("Saving CSV file..."));

        let mut csv = String::from("Timestamp,Block Height,Hash,Amount,In/Out\n");
        for tx in &transactions {
            if tx.is_fusion_transaction() {
                continue;
            }
            let amount = format_amount_basic(tx.total_amount().unsigned_abs());
            let direction = if tx.total_amount() > 0 { "IN" } else { "OUT" };
            csv.push_str(&format!(
                "{},{},{},{},{}\n",
                unix_time_to_date(tx.timestamp),
                tx.block_height,
                tx.hash.to_hex(),
                amount,
                direction
            ));
        }

        if std::fs::write(&self.csv_path, csv.as_bytes()).is_err() {
            term.line(&warning("Couldn't open transactions.csv file for saving!"));
            term.line(&warning("Ensure it is not open in any other application."));
            return;
        }

        term.line(&format!("{}{}{}", success("CSV successfully written to "), success(&self.csv_path), success("!")));
    }

    /// `reset` (`CommandImplementations.cpp:305`).
    fn reset(&mut self, term: &mut dyn Terminal) {
        let scan_height = get_scan_height(term);

        term.write("\n");
        term.line(&information("This process may take some time to complete."));
        term.line(&format!(
            "{}{}",
            information("You can't make any transactions during the "),
            information("process.")
        ));
        term.write("\n");

        let (lite_start_height, transactions_lost) = self.open().lite_rescan_impact(scan_height);
        if transactions_lost != 0 {
            term.line(&format!(
                "{}{}{}",
                warning("The node you are connected to is a lite node and holds no data below height "),
                warning(lite_start_height.to_string()),
                warning(".")
            ));
            term.line(&format!(
                "{}{}{}{}{}{}",
                warning("Rescanning from "),
                warning(scan_height.to_string()),
                warning(" will start at "),
                warning(lite_start_height.to_string()),
                warning(" instead, and "),
                warning(format!("{transactions_lost} transaction(s) this wallet already knows about will be lost."))
            ));
            term.line(&warning("They cannot be recovered through this node. Connect a node that holds the whole"));
            term.line(&warning("chain first if you want to keep them."));
            term.write("\n");
        }

        if !confirm(term, "Are you sure?") {
            return;
        }

        term.line(&information("Resetting wallet..."));

        {
            let mut open = self.open();
            open.wallet_mut().reset(scan_height);
            open.sync.refresh_key_image_owners();
            open.sync_gap = None;
        }

        super::menu::sync_wallet_every(term, &self.wallet, self.sync_poll);
    }

    /// `refresh` (`CommandImplementations.cpp:382`).
    fn refresh(&mut self, term: &mut dyn Terminal) {
        super::menu::sync_wallet_every(term, &self.wallet, self.sync_poll);
    }

    /// `swapNode` (`CommandImplementations.cpp:778`).
    fn swap_node(&mut self, term: &mut dyn Terminal) {
        let (host, port, ssl) = super::prompt::get_daemon_address(term);

        term.write(&information("\nSwapping node, this may take some time...\n"));

        let mut open = self.open();
        match super::menu::rebuild_daemon(&mut open, &host, port, ssl) {
            Ok(()) => {
                drop(open);
                term.write(&success("Node swap complete.\n\n"));
            }
            Err(e) => {
                drop(open);
                term.line(&format!("{}{}", warning("Could not swap node: "), warning(e)));
            }
        }
    }

    /// `setLogLevel` (`CommandImplementations.cpp:983`).
    fn set_log_level(&mut self, term: &mut dyn Terminal) {
        let levels = log_levels();
        print_commands(term, &levels);

        let Selection::Command(level) = parse_command(term, &levels, &levels, "What log level do you want to use?: ")
        else {
            return;
        };

        if level == "exit" {
            return;
        }

        self.log_level = match level.as_str() {
            "trace" => 5,
            "debug" => 4,
            "info" => 3,
            "warning" => 2,
            "fatal" => 1,
            _ => 0,
        };
        // `Logger::logger.setLogLevel`: the new level applies from the next line.
        crate::logging::set_level(crate::logging::wallet_level(self.log_level));
    }
}

////////////////////////
/* FREE FUNCTIONS     */
////////////////////////

////////////////////////
/* PAYMENT IDS        */
////////////////////////

/// A payment ID given alongside a plain address almost always wants to be an
/// integrated address instead, so say so and offer to make one.
///
/// The C++ warns about the long form at the confirmation
/// (`Transfer.cpp:322`); this adds the offer, and the plain statement of what a
/// 64-character payment ID costs in privacy, *before* the send is built.
///
/// Returns the destination to use and the payment ID to send with it: either
/// the pair unchanged, or the integrated address with the ID folded into it.
fn offer_integrated_address(term: &mut dyn Terminal, address: &str, payment_id: &str) -> (String, String) {
    if payment_id.is_empty() || base58::is_integrated_address(address) {
        return (address.to_string(), payment_id.to_string());
    }

    if payment_id.len() == SHORT_PAYMENT_ID_LENGTH {
        term.line(&information(
            "This payment ID is 16 characters, so it is encrypted to the recipient: nobody else reading the chain can see it.",
        ));
    } else {
        term.line(&warning(
            "This payment ID is 64 characters, so it is written to the chain in plaintext and anyone can read it.",
        ));
        term.line(&information(
            "A 16-character payment ID is encrypted to the recipient instead. Use one if whoever you are paying accepts it.",
        ));
    }

    term.line(&information(
        "An integrated address carries the payment ID inside the address, so it cannot be forgotten or mistyped separately.",
    ));

    if !confirm(term, "Combine this address and payment ID into an integrated address?") {
        return (address.to_string(), payment_id.to_string());
    }

    let Ok(parsed) = base58::parse_address(address) else {
        return (address.to_string(), payment_id.to_string());
    };

    match base58::integrated_address(&parsed.spend_public_key, &parsed.view_public_key, payment_id) {
        Ok(integrated) => {
            term.line(&format!("{}{}", information("Using integrated address: "), success(&integrated)));
            term.write(
                "
",
            );
            // The ID now lives in the address; carrying both would be the
            // `CONFLICTING_PAYMENT_IDS` case if they ever disagreed.
            (integrated, String::new())
        }
        Err(e) => {
            term.line(&format!(
                "{}{}",
                warning("Could not build an integrated address: "),
                warning(WalletError::InvalidAddress(e).to_string())
            ));
            (address.to_string(), payment_id.to_string())
        }
    }
}

fn cancel(term: &mut dyn Terminal) {
    term.write(&warning("Cancelling transaction.\n"));
}

/// `getAddressTypeLabel` (`Transfer.cpp:30`).
pub fn address_type_label(address: &str) -> &'static str {
    if !base58::is_integrated_address(address) {
        "standard"
    } else if address.len() == wrkz_primitives::constants::INTEGRATED_ADDRESS_LENGTH {
        "integrated-short"
    } else {
        "integrated-long"
    }
}

/// `printTransferOneLine` (`CommandImplementations.cpp:453`).
pub fn print_transfer_one_line(term: &mut dyn Terminal, tx: &Transaction) {
    let incoming = tx.total_amount() >= 0;
    let amount = tx.total_amount().unsigned_abs();

    let mut line = String::new();
    line.push_str(if incoming { "[IN] " } else { "[OUT] " });

    if tx.block_height == 0 || tx.timestamp == 0 {
        line.push_str("h:pending t:pending ");
    } else {
        line.push_str(&format!("h:{} ", tx.block_height));
        line.push_str(&format!("t:{} ", unix_time_to_date(tx.timestamp)));
    }

    line.push_str(if incoming { "+" } else { "-" });
    line.push_str(&format!("{} ", format_amount(amount)));

    if !incoming && tx.fee != 0 {
        line.push_str(&format!("fee:{} ", format_amount(tx.fee)));
    }

    line.push_str(&format!("tx:{}", tx.hash.to_hex()));

    if incoming {
        if let Some(unlock) = unlock_note(tx) {
            line.push_str(&unlock);
        }
    }

    if !tx.payment_id.is_empty() {
        line.push_str(&format!(" pid:{}", tx.payment_id));
    }

    term.line(&if incoming { success(line) } else { warning(line) });
}

/// The ` unlock_h:` / ` unlock_t:` suffix of `printTransferOneLine`.
fn unlock_note(tx: &Transaction) -> Option<String> {
    let difference = tx.unlock_time as i64 - tx.block_height as i64;
    if tx.unlock_time != 0 && difference > 0 && tx.unlock_time < wrkz_primitives::constants::CRYPTONOTE_MAX_BLOCK_NUMBER
    {
        return Some(format!(" unlock_h:{}", tx.unlock_time));
    }
    if tx.unlock_time > now_seconds() {
        return Some(format!(" unlock_t:{}", unix_time_to_date(tx.unlock_time)));
    }
    None
}

/// `printOutgoingTransfer` (`CommandImplementations.cpp:398`).
pub fn print_outgoing_transfer(term: &mut dyn Terminal, tx: &Transaction) {
    let amount = tx.total_amount().unsigned_abs();
    let mut stream = format!("Outgoing transfer:\nHash: {}\n", tx.hash.to_hex());

    // Not filled in for an outgoing transaction that is still in the pool.
    if tx.block_height != 0 && tx.timestamp != 0 {
        stream.push_str(&format!("Block height: {}\n", tx.block_height));
        stream.push_str(&format!("Timestamp: {}\n", unix_time_to_date(tx.timestamp)));
    }

    stream.push_str(&format!("Spent: {}\n", format_amount(amount.saturating_sub(tx.fee))));
    stream.push_str(&format!("Fee: {}\n", format_amount(tx.fee)));
    stream.push_str(&format!("Total Spent: {}\n", format_amount(amount)));

    if !tx.payment_id.is_empty() {
        stream.push_str(&format!("Payment ID: {}\n", tx.payment_id));
    }

    term.line(&warning(stream));
}

/// `printIncomingTransfer` (`CommandImplementations.cpp:424`).
pub fn print_incoming_transfer(term: &mut dyn Terminal, tx: &Transaction) {
    let amount = tx.total_amount().max(0) as u64;
    let mut stream = format!("Incoming transfer:\nHash: {}\n", tx.hash.to_hex());
    stream.push_str(&format!("Block height: {}\n", tx.block_height));
    stream.push_str(&format!("Timestamp: {}\n", unix_time_to_date(tx.timestamp)));
    stream.push_str(&format!("Amount: {}\n", format_amount(amount)));

    if !tx.payment_id.is_empty() {
        stream.push_str(&format!("Payment ID: {}\n", tx.payment_id));
    }

    let difference = tx.unlock_time as i64 - tx.block_height as i64;
    if tx.unlock_time != 0 && difference > 0 && tx.unlock_time < wrkz_primitives::constants::CRYPTONOTE_MAX_BLOCK_NUMBER
    {
        let unlock_at = tx.timestamp + (difference as u64) * wrkz_primitives::constants::DIFFICULTY_TARGET;
        term.write(&success(stream));
        term.line(&format!("{}{}", information("Unlock height: "), information(tx.unlock_time.to_string())));
        term.line(&format!(
            "{}{}",
            information("Unlocks at approximately: "),
            information(unix_time_to_date(unlock_at))
        ));
        term.write("\n");
    } else if tx.unlock_time > now_seconds() {
        term.write(&success(stream));
        term.line(&format!("{}{}", information("Unlocks at: "), information(unix_time_to_date(tx.unlock_time))));
        term.write("\n");
    } else {
        term.line(&success(stream));
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The wallet password, taken twice when it is being set.
/// `getWalletPassword` lives in [`super::menu`]; this alias keeps the C++
/// name reachable from here.
pub use super::menu::get_wallet_password as read_wallet_password;

/// Re-exported so a caller can build a password without a terminal.
pub type Password = Zeroizing<String>;
