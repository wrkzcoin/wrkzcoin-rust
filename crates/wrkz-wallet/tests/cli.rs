// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet` driven without a terminal: every command dispatched against
//! scripted input, with the output asserted — including the assertion that no
//! secret is printed unless the command's whole purpose is to print it.
//!
//! The wallet is built from the published spec/05 keys (see
//! `tests/fixtures/README.md`; none of them has ever held funds) and the daemon
//! is [`CannedDaemon`], so everything here is offline and deterministic.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zeroize::Zeroizing;

use wrkz_wallet::api::OpenWallet;
use wrkz_wallet::cli::session::Session;
use wrkz_wallet::cli::term::ScriptedTerminal;
use wrkz_wallet::daemon::{
    DaemonError, GlobalIndexes, Info, RandomOuts, SendResult, SyncRequest, TransactionsStatus, WalletSyncData,
};
use wrkz_wallet::file::{Hex32, SecretKey, Transaction, TransactionInput, Transfer, Wallet};
use wrkz_wallet::sync::{SyncConfig, SyncDaemon, Synchronizer};
use wrkz_wallet::transfer::TransferDaemon;

////////////////////////
/* CONSTANTS          */
////////////////////////

/// Deliberately not a word that appears in any menu line, so the "no
/// command prints a secret" test cannot pass by accident.
const PASSWORD: &str = "throwaway-passphrase-9182";
const ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
const SPEND_SECRET: &str = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
const VIEW_SECRET: &str = "779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700";
const MNEMONIC: &str = "eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell different \
                        dating intended richly slower inundate ridges slug inundate ridges slug were rotate rudely \
                        viewpoint";
const HASH: &str = "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f";
const NETWORK_HEIGHT: u64 = 4_213_000;

/// A destination that parses but is not ours.
fn foreign() -> &'static str {
    static ADDRESS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ADDRESS.get_or_init(|| Wallet::create_new(0).expect("keys").primary_address().expect("address").to_string())
}

////////////////////////
/* CANNED DAEMON      */
////////////////////////

#[derive(Default)]
struct CannedDaemon {
    /// Set when `/get_transactions_status` should fail, for the `check_tx`
    /// "failed to query daemon" branch.
    fail_status: AtomicBool,
    /// Whether the hash asked about is reported as being in a block.
    in_block: AtomicBool,
}

impl SyncDaemon for CannedDaemon {
    fn wallet_sync_data(&self, _req: &SyncRequest) -> Result<WalletSyncData, DaemonError> {
        Ok(WalletSyncData {
            items: Vec::new(),
            scanned_to_height: None,
            synced: true,
            top_block: None,
            status: "OK".into(),
        })
    }

    fn global_indexes_for_range(&self, _start: u64, _end: u64) -> Result<GlobalIndexes, DaemonError> {
        Ok(GlobalIndexes { indexes: Vec::new(), status: "OK".into() })
    }

    fn transactions_status(&self, hashes: &[String]) -> Result<TransactionsStatus, DaemonError> {
        if self.fail_status.load(Ordering::SeqCst) {
            return Err(DaemonError::Transport("no daemon".into()));
        }
        let (in_block, unknown) = if self.in_block.load(Ordering::SeqCst) {
            (hashes.to_vec(), Vec::new())
        } else {
            (Vec::new(), hashes.to_vec())
        };
        Ok(TransactionsStatus {
            transactions_in_pool: Vec::new(),
            transactions_in_block: in_block,
            transactions_unknown: unknown,
            status: "OK".into(),
        })
    }

    fn info(&self) -> Result<Info, DaemonError> {
        Ok(Info {
            height: NETWORK_HEIGHT + 1,
            network_height: NETWORK_HEIGHT + 1,
            difficulty: 60_000,
            incoming_connections_count: 4,
            outgoing_connections_count: 4,
            lite_start_height: 0,
            sync_features: vec!["skipEmptyBlocks".into()],
            compression: Some("none".into()),
            synced: true,
            top_block_hash: None,
            supported_height: None,
            upgrade_heights: Vec::new(),
            version: Some("0.4.8".into()),
            status: "OK".into(),
        })
    }
}

impl TransferDaemon for CannedDaemon {
    fn random_outs(&self, _amounts: &[u64], _outs_count: u64) -> Result<RandomOuts, DaemonError> {
        Ok(RandomOuts { outs: Vec::new(), status: "OK".into() })
    }

    fn send_raw_transaction(&self, _tx_hex: &str) -> Result<SendResult, DaemonError> {
        Ok(SendResult { status: "OK".into(), error: None })
    }
}

////////////////////////
/* HARNESS            */
////////////////////////

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("wrkz-wallet-cli-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    fn join(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How the wallet under test is built.
#[derive(Clone, Copy, Default)]
struct Options {
    view_only: bool,
    funded: bool,
    with_transactions: bool,
}

struct Cli {
    session: Session,
    /// Held so the directory outlives the session that writes into it.
    _dir: TempDir,
}

impl Cli {
    fn new(tag: &str) -> Cli {
        Cli::with(tag, Options::default())
    }

    fn with(tag: &str, options: Options) -> Cli {
        let dir = TempDir::new(tag);
        let filename = dir.join("test.wallet");

        let mut wallet = if options.view_only {
            Wallet::import_view_only(&SecretKey::from_hex(VIEW_SECRET).unwrap(), ADDRESS, NETWORK_HEIGHT).unwrap()
        } else {
            Wallet::import_from_keys(
                &SecretKey::from_hex(SPEND_SECRET).unwrap(),
                &SecretKey::from_hex(VIEW_SECRET).unwrap(),
                NETWORK_HEIGHT,
            )
            .unwrap()
        };

        if options.funded {
            wallet.sub_wallets.sub_wallet[0].unspent_inputs.push(TransactionInput {
                amount: 100_000_000,
                block_height: 4_200_000,
                global_output_index: Some(1),
                key: Hex32([1u8; 32]),
                key_image: Hex32([2u8; 32]),
                parent_transaction_hash: Hex32::from_hex(HASH).unwrap(),
                private_ephemeral: None,
                spend_height: 0,
                transaction_index: 0,
                transaction_public_key: Hex32([3u8; 32]),
                unlock_time: 0,
            });
        }

        if options.with_transactions {
            let spend_key = wallet.primary_sub_wallet().unwrap().public_spend_key;
            // One incoming, one outgoing, one fusion (which every listing skips).
            wallet.sub_wallets.transactions.push(Transaction {
                block_height: 4_200_001,
                fee: 0,
                hash: Hex32::from_hex(HASH).unwrap(),
                is_coinbase_transaction: true,
                payment_id: "0102030405060708".into(),
                timestamp: 1_700_000_000,
                transfers: vec![Transfer { amount: 500_000, public_key: spend_key }],
                unlock_time: 0,
            });
            wallet.sub_wallets.transactions.push(Transaction {
                block_height: 4_200_002,
                fee: 10_000,
                hash: Hex32([9u8; 32]),
                is_coinbase_transaction: false,
                payment_id: String::new(),
                timestamp: 1_700_000_600,
                transfers: vec![Transfer { amount: -200_000, public_key: spend_key }],
                unlock_time: 0,
            });
            wallet.sub_wallets.transactions.push(Transaction {
                block_height: 4_200_003,
                fee: 0,
                hash: Hex32([7u8; 32]),
                is_coinbase_transaction: false,
                payment_id: String::new(),
                timestamp: 1_700_001_200,
                transfers: vec![Transfer { amount: 100, public_key: spend_key }],
                unlock_time: 0,
            });
        }

        wallet.save(&filename, PASSWORD).unwrap();

        let daemon = wrkz_wallet::api::DynDaemon(Arc::new(CannedDaemon::default()));
        let mut open = OpenWallet {
            sync: Synchronizer::with_config(daemon, wallet, SyncConfig::default()),
            filename,
            password: Zeroizing::new(PASSWORD.to_string()),
            daemon_host: "127.0.0.1".into(),
            daemon_port: 17856,
            daemon_ssl: false,
            prepared: Vec::new(),
            peer_count: 0,
            hashrate: 0,
            sync_gap: None,
            stop: Arc::new(AtomicBool::new(false)),
        };
        open.refresh_info();

        let mut session = Session::new(Arc::new(Mutex::new(open)));
        session.address_book_path = dir.join(".addressBook.json");
        session.csv_path = dir.join("transactions.csv");
        // `refresh` and `reset` watch the background sync; there is none here,
        // so poll fast enough that the "syncing may be stuck" path is reached
        // in milliseconds rather than the real forty seconds.
        session.sync_poll = std::time::Duration::from_millis(1);

        Cli { session, _dir: dir }
    }

    /// Run one command with scripted answers, and return everything printed.
    fn run<I, S>(&mut self, command: &str, input: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut term = ScriptedTerminal::new(input);
        let carry_on = self.session.handle_command(&mut term, command);
        assert!(carry_on || command == "exit", "only `exit` stops the loop");
        term.take_output()
    }
}

////////////////////////
/* DISPATCH           */
////////////////////////

#[test]
fn every_command_in_the_table_is_hooked_up() {
    // A generous script so no prompt runs out of input; the point is only that
    // none of them falls through to "not hooked up".
    let answers = ["cancel", "cancel", "cancel", "cancel", "cancel", "cancel", "n", "n", "n", "n", "n", "n"];

    for command in wrkz_wallet::cli::commands::all_commands() {
        if command.name == "exit" {
            continue;
        }
        let mut cli = Cli::new("dispatch");
        let output = cli.run(command.name, answers);
        assert!(!output.contains("not hooked up"), "`{}` is in the table but not dispatched:\n{output}", command.name);
    }
}

#[test]
fn exit_is_the_only_command_that_stops_the_loop() {
    let cli = Cli::new("exit");
    let mut session = cli.session;
    let mut term = ScriptedTerminal::new(Vec::<String>::new());
    assert!(!session.handle_command(&mut term, "exit"));
    assert!(session.handle_command(&mut term, "address"));
}

////////////////////////
/* SECRETS            */
////////////////////////

#[test]
fn no_command_prints_a_secret_unless_that_is_what_it_is_for() {
    // `backup` asks for the password and then prints the keys; that is its
    // whole job. `get_tx_private_key` prints one transaction's key. Nothing
    // else may print any of these three.
    let answers = ["cancel", "cancel", "cancel", "cancel", "cancel", "cancel", "n", "n", "n", "n", "n", "n"];

    for command in wrkz_wallet::cli::commands::all_commands() {
        if matches!(command.name, "exit" | "backup" | "get_tx_private_key") {
            continue;
        }
        let mut cli = Cli::with("secrets", Options { with_transactions: true, ..Default::default() });
        let output = cli.run(command.name, answers);

        assert!(!output.contains(SPEND_SECRET), "`{}` printed the private spend key:\n{output}", command.name);
        assert!(!output.contains(VIEW_SECRET), "`{}` printed the private view key:\n{output}", command.name);
        assert!(!output.contains(MNEMONIC), "`{}` printed the mnemonic seed:\n{output}", command.name);
        assert!(!output.contains(PASSWORD), "`{}` printed the password:\n{output}", command.name);
    }
}

#[test]
fn backup_asks_for_the_password_before_it_prints_anything() {
    let mut cli = Cli::new("backup");

    // Two wrong passwords, then the right one.
    let output = cli.run("backup", ["wrong", "also wrong", PASSWORD]);
    assert_eq!(output.matches("Incorrect password! Try again.").count(), 2);
    assert!(output.contains("Private spend key:"));
    assert!(output.contains(SPEND_SECRET));
    assert!(output.contains("Private view key:"));
    assert!(output.contains(VIEW_SECRET));
    assert!(output.contains("Mnemonic seed:"));
    assert!(output.contains(MNEMONIC));
    // The password itself is never echoed back.
    assert!(!output.contains("wrong"));
}

#[test]
fn backup_prints_nothing_when_the_password_is_never_given() {
    let mut cli = Cli::new("backupfail");
    let output = cli.run("backup", ["wrong"]);
    assert!(output.contains("Incorrect password! Try again."));
    assert!(!output.contains(SPEND_SECRET), "a refused password must not reach the keys:\n{output}");
    assert!(!output.contains(MNEMONIC));
}

#[test]
fn a_view_wallet_backup_has_no_spend_key_and_no_seed() {
    let mut cli = Cli::with("viewbackup", Options { view_only: true, ..Default::default() });
    let output = cli.run("backup", [PASSWORD]);
    assert!(!output.contains("Private spend key:"), "{output}");
    assert!(output.contains("Private view key:"));
    assert!(output.contains(VIEW_SECRET));
    assert!(!output.contains("Mnemonic seed:"));
}

#[test]
fn a_transaction_private_key_is_only_printed_when_it_is_asked_for_by_hash() {
    let mut cli = Cli::new("txkey");

    // No key stored for this hash.
    let output = cli.run("get_tx_private_key", [HASH]);
    assert!(output.contains("Couldn't find the private key for this transaction"), "{output}");

    // Store one, then ask again.
    {
        let mut open = cli.session.wallet.lock().unwrap();
        let key = SecretKey::from_hex(SPEND_SECRET).unwrap();
        open.wallet_mut().store_tx_private_key(Hex32::from_hex(HASH).unwrap(), key);
    }
    let output = cli.run("get_tx_private_key", [HASH]);
    assert!(output.contains("Transaction private key: "));
    assert!(output.contains(SPEND_SECRET));

    // `cancel` at the prompt prints nothing.
    let output = cli.run("get_tx_private_key", ["cancel"]);
    assert!(!output.contains(SPEND_SECRET), "{output}");
}

#[test]
fn change_password_needs_the_old_one_and_takes_the_new_one_twice() {
    let mut cli = Cli::new("changepw");

    let output = cli.run("change_password", [PASSWORD, "new one", "typo"]);
    assert!(output.contains("Passwords do not match, try again."), "{output}");

    let mut cli = Cli::new("changepw2");
    let output = cli.run("change_password", [PASSWORD, "new one", "new one"]);
    assert!(output.contains("Your password has been changed!"), "{output}");
    assert!(!output.contains("new one"), "the new password must not be echoed:\n{output}");

    // The file really was rewritten with it.
    let filename = cli.session.wallet.lock().unwrap().filename.clone();
    assert!(Wallet::open(&filename, "new one").is_ok());
    assert!(Wallet::open(&filename, PASSWORD).is_err());
}

////////////////////////
/* INFORMATION        */
////////////////////////

#[test]
fn address_and_its_alias_print_the_primary_address() {
    let mut cli = Cli::new("address");
    assert_eq!(cli.run("address", Vec::<String>::new()).trim(), ADDRESS);
    assert_eq!(cli.run("addr", Vec::<String>::new()).trim(), ADDRESS);
}

#[test]
fn balance_prints_the_three_lines_and_warns_while_scanning() {
    let mut cli = Cli::with("balance", Options { funded: true, ..Default::default() });
    let output = cli.run("balance", Vec::<String>::new());

    assert!(output.contains("Available balance: 1,000,000.00 WRKZ"), "{output}");
    assert!(output.contains("Locked (unconfirmed) balance: 0.00 WRKZ"), "{output}");
    assert!(output.contains("Total balance: 1,000,000.00 WRKZ"), "{output}");
    // The wallet has scanned nothing, so it says the balance may be wrong.
    assert!(output.contains("The blockchain is still being scanned for your transactions."), "{output}");
}

#[test]
fn a_view_wallet_balance_says_it_may_be_inflated() {
    let mut cli = Cli::with("viewbal", Options { view_only: true, with_transactions: true, ..Default::default() });
    let output = cli.run("balance", Vec::<String>::new());
    assert!(output.contains("view only wallets can only track incoming transactions"), "{output}");
    assert!(output.contains("your wallet balance may appear inflated"), "{output}");
}

#[test]
fn status_prints_the_heights_the_percentages_and_a_summary() {
    let mut cli = Cli::new("status");
    let output = cli.run("status", Vec::<String>::new());

    assert!(output.contains("Wallet blockchain height: 0"), "{output}");
    assert!(output.contains(&format!("Local blockchain height: {NETWORK_HEIGHT}")), "{output}");
    assert!(output.contains(&format!("Network blockchain height: {NETWORK_HEIGHT}")), "{output}");
    assert!(output.contains("Network sync status: 100.00%"), "{output}");
    assert!(output.contains("Wallet sync status: 0.00%"), "{output}");
    assert!(output.contains("Peers: 8"), "{output}");
    // `get_mining_speed` switches unit on `>` 1e3, and 60,000/60 is exactly
    // 1,000, so it stays in H/s.
    assert!(output.contains("Network hashrate: 1000.00 H/s (Based on the last local block)"), "{output}");
    // Daemon synced, wallet far behind.
    assert!(output.contains("You are synced with the network, but the blockchain is still being scanned"), "{output}");
}

#[test]
fn help_and_advanced_print_the_same_list() {
    let mut cli = Cli::new("help");
    let help = cli.run("help", Vec::<String>::new());
    let advanced = cli.run("advanced", Vec::<String>::new());
    assert_eq!(help, advanced);
    assert!(help.contains("transfer"));
    assert!(help.contains("[Transactions] Send WRKZ to someone"));
    assert!(help.contains("set_log_level"));
}

#[test]
fn a_view_wallets_help_hides_the_spending_commands() {
    let mut cli = Cli::with("viewhelp", Options { view_only: true, ..Default::default() });
    let output = cli.run("help", Vec::<String>::new());
    assert!(output.contains("balance"));
    assert!(!output.contains("[Transactions] Send WRKZ to someone"), "{output}");
    assert!(!output.contains("sweep_all"), "{output}");
}

#[test]
fn the_prompt_shows_the_wallet_name_and_whether_it_is_out_of_sync() {
    let cli = Cli::new("prompt");
    let prompt = cli.session.prompt();
    assert!(prompt.starts_with("[WRKZ "), "{prompt}");
    assert!(prompt.ends_with(" (out-of-sync)]: "), "{prompt}");
}

////////////////////////
/* TRANSACTION LISTS  */
////////////////////////

#[test]
fn the_transfer_listings_skip_fusion_transactions_and_total_correctly() {
    let mut cli = Cli::with("lists", Options { with_transactions: true, ..Default::default() });

    let output = cli.run("list_transfers", Vec::<String>::new());
    assert!(output.contains("[IN] h:4200001"), "{output}");
    assert!(output.contains("+5,000.00 WRKZ"), "{output}");
    assert!(output.contains("pid:0102030405060708"), "{output}");
    assert!(output.contains("[OUT] h:4200002"), "{output}");
    assert!(output.contains("-2,000.00 WRKZ"), "{output}");
    assert!(output.contains("fee:100.00 WRKZ"), "{output}");
    // The zero-fee non-coinbase transaction is a fusion, and is skipped.
    assert!(!output.contains("+1.00 WRKZ"), "a fusion transaction must not be listed:\n{output}");

    assert!(output.contains("1 incoming transactions, totalling 5,000.00 WRKZ"), "{output}");
    assert!(output.contains("1 outgoing transactions, totalling 2,000.00 WRKZ"), "{output}");

    // `in` and `out` filter, and only print their own half of the summary.
    let output = cli.run("in", Vec::<String>::new());
    assert!(output.contains("[IN] "));
    assert!(!output.contains("[OUT] "));
    assert!(!output.contains("outgoing transactions, totalling"));

    let output = cli.run("out", Vec::<String>::new());
    assert!(output.contains("[OUT] "));
    assert!(!output.contains("[IN] "));
    assert!(!output.contains("incoming transactions, totalling"));
}

#[test]
fn txs_is_paged_and_txs_full_is_verbose() {
    let mut cli = Cli::with("txsfull", Options { with_transactions: true, ..Default::default() });

    let output = cli.run("txs", Vec::<String>::new());
    assert!(output.contains("Displayed 2 transfer(s)."), "{output}");

    let output = cli.run("txs_full", Vec::<String>::new());
    assert!(output.contains("Incoming transfer:"), "{output}");
    assert!(output.contains("Outgoing transfer:"), "{output}");
    assert!(output.contains("Total Spent: 2,000.00 WRKZ"), "{output}");
    assert!(output.contains("Fee: 100.00 WRKZ"), "{output}");
    assert!(output.contains("Payment ID: 0102030405060708"), "{output}");
}

#[test]
fn save_csv_writes_the_cpp_header_and_one_line_per_transaction() {
    let mut cli = Cli::with("csv", Options { with_transactions: true, ..Default::default() });

    let output = cli.run("save_csv", Vec::<String>::new());
    assert!(output.contains("Saving CSV file..."), "{output}");
    assert!(output.contains("CSV successfully written to "), "{output}");

    let csv = std::fs::read_to_string(&cli.session.csv_path).expect("csv written");
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("Timestamp,Block Height,Hash,Amount,In/Out"));
    assert_eq!(lines.next(), Some(&*format!("2023-11-14 22:13,4200001,{HASH},5000.00,IN")));
    let outgoing_hash = Hex32([9u8; 32]).to_hex();
    assert_eq!(lines.next(), Some(&*format!("2023-11-14 22:23,4200002,{outgoing_hash},2000.00,OUT")));
    // The fusion transaction is skipped, so nothing else.
    assert_eq!(lines.next(), None);
}

#[test]
fn save_csv_says_so_when_there_is_nothing_to_save() {
    let mut cli = Cli::new("csvempty");
    let output = cli.run("save_csv", Vec::<String>::new());
    assert!(output.contains("You have no transactions to save to the CSV!"), "{output}");
    assert!(!std::path::Path::new(&cli.session.csv_path).exists());
}

#[test]
fn check_tx_reports_the_wallet_and_the_node_separately() {
    let mut cli = Cli::with("checktx", Options { with_transactions: true, ..Default::default() });

    // Inline argument, found in the wallet, unknown to the daemon.
    let output = cli.run(&format!("check_tx {HASH}"), Vec::<String>::new());
    assert!(output.contains("Wallet lookup: found (confirmed incoming)"), "{output}");
    assert!(output.contains("block: 4200001, amount: 5,000.00 WRKZ"), "{output}");
    assert!(output.contains("payment ID: 0102030405060708"), "{output}");
    assert!(output.contains("Node lookup: transaction is unknown to daemon"), "{output}");

    // Prompted, and a hash the wallet has never seen.
    let unknown = "0".repeat(64);
    let output = cli.run("check_tx", [unknown.clone()]);
    assert!(output.contains("Wallet lookup: not found in this wallet"), "{output}");

    // A hash that is not one.
    let output = cli.run("check_tx notahash", Vec::<String>::new());
    assert!(output.contains("Invalid hash: "), "{output}");
}

////////////////////////
/* ADDRESSES          */
////////////////////////

#[test]
fn make_integrated_address_defaults_to_the_primary_address_and_a_random_id() {
    let mut cli = Cli::new("integrated");

    // Both blank: the primary address and a generated short payment ID.
    let output = cli.run("make_integrated_address", ["", ""]);
    assert!(output.contains("No address provided. Using primary wallet address: "), "{output}");
    assert!(output.contains("No payment ID provided. Generated random short payment ID: "), "{output}");

    // The last line is the integrated address itself.
    let integrated = output.lines().last().unwrap().trim().to_string();
    assert_eq!(integrated.len(), 120, "a short integrated address is 120 characters: {integrated}");

    // And it decodes back to the primary address.
    let output = cli.run(&format!("decode_integrated {integrated}"), Vec::<String>::new());
    assert!(output.contains(&format!("Decoded address: {ADDRESS}")), "{output}");
    assert!(output.contains("This integrated address maps to your primary wallet address."), "{output}");
}

#[test]
fn decode_integrated_refuses_a_standard_address_with_the_expected_lengths() {
    let mut cli = Cli::new("decode");
    let output = cli.run(&format!("decode_integrated {ADDRESS}"), Vec::<String>::new());
    assert!(output.contains("This is not an integrated address format for this network."), "{output}");
    assert!(output.contains("Expected length: 120 (short) or 186 (long)."), "{output}");
}

////////////////////////
/* ADDRESS BOOK       */
////////////////////////

#[test]
fn the_address_book_round_trips_through_the_file_the_cpp_writes() {
    let mut cli = Cli::new("ab");

    let output = cli.run("ab_list", Vec::<String>::new());
    assert!(output.contains("Your address book is empty!"), "{output}");

    // name, address, payment id
    let output = cli.run("ab_add", ["alice", foreign(), "0102030405060708", "n"]);
    assert!(output.contains("Address type: standard"), "{output}");
    assert!(output.contains("A new entry has been added to your address book!"), "{output}");

    let text = std::fs::read_to_string(&cli.session.address_book_path).expect("address book written");
    assert!(text.starts_with("[\n  {\n"), "two-space indentation, like nlohmann at setw(2):\n{text}");
    assert!(text.contains("\"friendlyName\": \"alice\""), "{text}");
    assert!(text.contains("\"paymentID\": \"0102030405060708\""), "{text}");

    let output = cli.run("ab_list", Vec::<String>::new());
    assert!(output.contains("Address Book Entry: 1 | alice"), "{output}");
    assert!(output.contains(&format!("Address: {}", foreign())), "{output}");
    assert!(output.contains("Payment ID: 0102030405060708"), "{output}");

    // A duplicate name is refused.
    let output = cli.run("ab_add", ["alice", "cancel"]);
    assert!(output.contains("An address book entry with this name already exists!"), "{output}");

    // Delete it.
    let output = cli.run("ab_delete", ["alice"]);
    assert!(output.contains("This entry has been deleted from your address book!"), "{output}");
    let output = cli.run("ab_list", Vec::<String>::new());
    assert!(output.contains("Your address book is empty!"), "{output}");
}

#[test]
fn ab_send_needs_an_entry_first_and_can_be_cancelled() {
    let mut cli = Cli::new("absend");

    let output = cli.run("ab_send", Vec::<String>::new());
    assert!(output.contains("Your address book is empty!"), "{output}");

    cli.run("ab_add", ["bob", foreign(), "", "n"]);

    let output = cli.run("ab_send", ["cancel"]);
    assert!(output.contains("Cancelling transaction."), "{output}");

    let output = cli.run("ab_send", ["bob", "cancel"]);
    assert!(output.contains("Cancelling transaction."), "{output}");
}

////////////////////////
/* SENDING            */
////////////////////////

#[test]
fn transfer_can_be_cancelled_at_every_prompt() {
    let mut cli = Cli::with("cancel", Options { funded: true, ..Default::default() });

    for script in [vec!["cancel"], vec![foreign(), "cancel"], vec![foreign(), "", "cancel"]] {
        let output = cli.run("transfer", script.clone());
        assert!(output.contains("Cancelling transaction."), "{script:?}:\n{output}");
        assert!(output.contains("You can type cancel at any time"), "{output}");
    }
}

#[test]
fn transfer_refuses_an_amount_above_the_unlocked_balance_before_it_builds() {
    let mut cli = Cli::with("nofunds", Options::default());
    let output = cli.run("transfer", [foreign(), "", "1000"]);
    assert!(output.contains("You don't have enough funds to cover this transaction!"), "{output}");
    assert!(output.contains("Funds needed: 1,000.00 WRKZ"), "{output}");
    assert!(output.contains("Funds available: 0.00 WRKZ"), "{output}");
}

#[test]
fn a_payment_id_with_a_plain_address_offers_an_integrated_address() {
    let mut cli = Cli::with("pid", Options { funded: true, ..Default::default() });

    // A long payment ID: say plainly that it is public, then offer to fold it
    // into the address. Declining leaves it alone.
    let long = "a".repeat(64);
    let output = cli.run("transfer", [foreign(), &long, "n", "cancel"]);
    assert!(
        output.contains("written to the chain in plaintext and anyone can read it"),
        "a long payment ID must be called out as public:\n{output}"
    );
    assert!(output.contains("A 16-character payment ID is encrypted to the recipient instead."), "{output}");
    assert!(output.contains("Combine this address and payment ID into an integrated address? (Y/n): "), "{output}");

    // A short one is encrypted, and says so.
    let mut cli = Cli::with("pid2", Options { funded: true, ..Default::default() });
    let output = cli.run("transfer", [foreign(), "0102030405060708", "n", "cancel"]);
    assert!(output.contains("it is encrypted to the recipient"), "{output}");

    // Accepting builds the integrated address and uses it.
    let mut cli = Cli::with("pid3", Options { funded: true, ..Default::default() });
    let output = cli.run("transfer", [foreign(), "0102030405060708", "y", "cancel"]);
    assert!(output.contains("Using integrated address: "), "{output}");
    // The prompt before it left the line unfinished, so this is a mid-line find.
    let marker = "Using integrated address: ";
    let start = output.find(marker).expect("the integrated address is printed") + marker.len();
    let integrated = output[start..].lines().next().unwrap().trim().to_string();
    assert_eq!(integrated.len(), 120);
}

#[test]
fn an_integrated_address_skips_the_payment_id_prompt() {
    let mut cli = Cli::with("integratedsend", Options { funded: true, ..Default::default() });

    let parsed = wrkz_primitives::base58::parse_address(foreign()).unwrap();
    let integrated = wrkz_primitives::base58::integrated_address(
        &parsed.spend_public_key,
        &parsed.view_public_key,
        "0102030405060708",
    )
    .unwrap();

    let output = cli.run("transfer", [integrated.as_str(), "cancel"]);
    assert!(output.contains("Address type: integrated-short"), "{output}");
    assert!(
        output.contains("Integrated address detected. Payment ID is embedded; skipping payment ID prompt."),
        "{output}"
    );
    assert!(!output.contains("Hit enter for the default of no payment ID"), "{output}");
}

#[test]
fn the_confirmation_shows_the_amount_the_fee_and_the_destination() {
    let cli = Cli::with("confirm", Options { funded: true, ..Default::default() });
    let mut term = ScriptedTerminal::new(["n"]);

    let approved = cli.session.confirm_transaction(&mut term, foreign(), 1_000_000, "", 12_345, 3);
    assert!(!approved, "answering no must not approve the send");

    let output = term.take_output();
    assert!(output.contains("Confirm Transaction?"), "{output}");
    assert!(output.contains("You are sending 10,000.00 WRKZ"), "{output}");
    assert!(output.contains("with a network fee of 123.45 WRKZ"), "{output}");
    assert!(output.contains("and a node fee of 0.00 WRKZ"), "{output}");
    assert!(output.contains("for a total of 10,123.45 WRKZ."), "{output}");
    assert!(output.contains(&format!("TO: {}", foreign())), "{output}");
    assert!(output.contains("Estimated minimum spendable delay after confirmation: 15 blocks"), "{output}");
    assert!(output.contains("Is this correct? (Y/n): "), "{output}");
}

#[test]
fn the_confirmation_names_the_payment_id_and_says_which_kind_it_is() {
    let cli = Cli::with("confirmpid", Options { funded: true, ..Default::default() });

    let mut term = ScriptedTerminal::new(["n"]);
    cli.session.confirm_transaction(&mut term, foreign(), 1000, "0102030405060708", 100, 3);
    let output = term.take_output();
    assert!(output.contains("and a Payment ID of 0102030405060708"), "{output}");
    assert!(output.contains("(short - encrypted, only the receiver can read it)"), "{output}");

    let mut term = ScriptedTerminal::new(["n"]);
    cli.session.confirm_transaction(&mut term, foreign(), 1000, &"b".repeat(64), 100, 3);
    let output = term.take_output();
    assert!(output.contains("(long - stored in plaintext, readable by anyone)"), "{output}");
}

#[test]
fn the_confirmation_warns_when_the_ring_came_out_smaller_than_usual() {
    let cli = Cli::with("confirmring", Options { funded: true, ..Default::default() });

    // `MIXIN_LIMITS_V5` runs from height 1,000,000 to 4,300,000 and pins both
    // the minimum and the default at 1, so 0 is the only smaller ring there is.
    let default = wrkz_primitives::mixins::mixin_allowable_range(NETWORK_HEIGHT).default;
    assert_eq!(default, 1, "the tier this test builds on");

    let mut term = ScriptedTerminal::new(["n"]);
    cli.session.confirm_transaction(&mut term, foreign(), 1000, "", 100, 0);
    let output = term.take_output();
    assert!(output.contains("Ring size reduced to 1 (normally 2)."), "{output}");
    assert!(output.contains("less private than usual"), "{output}");
}

#[test]
fn the_confirmation_asks_for_the_password_after_the_yes() {
    let cli = Cli::with("confirmpw", Options { funded: true, ..Default::default() });

    let mut term = ScriptedTerminal::new(["y", "wrong", PASSWORD]);
    let approved = cli.session.confirm_transaction(&mut term, foreign(), 1000, "", 100, 7);
    assert!(approved);

    let output = term.take_output();
    assert!(output.contains("Confirm your password: "), "{output}");
    assert!(output.contains("Incorrect password! Try again."), "{output}");
    assert!(!output.contains(PASSWORD), "the password must never be echoed:\n{output}");
    assert_eq!(term.passwords_read, 2, "both attempts went through the hidden-echo reader");
}

////////////////////////
/* SWEEP              */
////////////////////////

#[test]
fn sweep_is_refused_on_a_view_wallet_and_on_an_empty_one() {
    let mut cli = Cli::with("sweepview", Options { view_only: true, ..Default::default() });
    let output = cli.run("sweep", Vec::<String>::new());
    assert!(output.contains("Sweep is not available for view-only wallets."), "{output}");

    let mut cli = Cli::new("sweepempty");
    let output = cli.run("sweep_all", Vec::<String>::new());
    assert!(output.contains("You have no unlocked balance to sweep."), "{output}");
}

#[test]
fn sweep_shows_a_summary_and_can_be_declined() {
    let mut cli = Cli::with("sweepsummary", Options { funded: true, ..Default::default() });

    // address, payment id, integrated-address offer (none asked, no id), confirm
    let output = cli.run("sweep_all", [foreign(), "", "n"]);
    assert!(output.contains("Sweep Summary"), "{output}");
    assert!(output.contains("Sweeping:    entire unlocked balance (1,000,000.00 WRKZ)"), "{output}");
    assert!(output.contains(&format!("To: {}", foreign())), "{output}");
    assert!(output.contains("Transactions: 1"), "{output}");
    assert!(output.contains("Total fee: "), "{output}");
    assert!(output.contains("Proceed with sweep? (Y/n): "), "{output}");
    assert!(output.contains("Cancelling sweep."), "{output}");
}

#[test]
fn sweep_refuses_an_amount_above_the_unlocked_balance() {
    let mut cli = Cli::with("sweepover", Options { funded: true, ..Default::default() });
    let output = cli.run("sweep", [foreign(), "", "99999999"]);
    assert!(output.contains("Amount exceeds unlocked balance of 1,000,000.00 WRKZ"), "{output}");
}

////////////////////////
/* MAINTENANCE        */
////////////////////////

#[test]
fn save_writes_the_file_and_says_so() {
    let mut cli = Cli::new("save");
    let output = cli.run("save", Vec::<String>::new());
    assert_eq!(output, "Saving.\nSaved.\n");

    let filename = cli.session.wallet.lock().unwrap().filename.clone();
    assert!(Wallet::open(&filename, PASSWORD).is_ok());
}

#[test]
fn reset_asks_for_a_height_and_a_confirmation_and_clears_the_wallet() {
    let mut cli = Cli::with("reset", Options { with_transactions: true, ..Default::default() });

    // Declining leaves everything alone.
    let output = cli.run("reset", ["1,000,000", "n"]);
    assert!(output.contains("This process may take some time to complete."), "{output}");
    assert!(output.contains("Are you sure? (Y/n): "), "{output}");
    assert_eq!(cli.session.wallet.lock().unwrap().wallet().transactions().len(), 3);

    // Accepting drops them.
    let output = cli.run("reset", ["1000000", "y"]);
    assert!(output.contains("Resetting wallet..."), "{output}");
    assert_eq!(cli.session.wallet.lock().unwrap().wallet().transactions().len(), 0);
    assert_eq!(cli.session.wallet.lock().unwrap().wallet().sub_wallets.sub_wallet[0].sync_start_height, 1_000_000);
}

#[test]
fn threads_reach_the_synchronizer() {
    let config = wrkz_wallet::cli::ZedConfig { threads: 3, skip_coinbase_transactions: true, ..Default::default() };
    let sync = wrkz_wallet::cli::menu::sync_config(&config);
    assert_eq!(sync.scan_threads, 3, "--threads is the scanning thread count");
    assert!(sync.skip_coinbase_transactions);

    // The parser refuses `--threads 0`; were it ever to get this far, one
    // thread still scans.
    let config = wrkz_wallet::cli::ZedConfig { threads: 0, ..Default::default() };
    assert_eq!(wrkz_wallet::cli::menu::sync_config(&config).scan_threads, 1);
}

#[test]
fn set_log_level_takes_a_name_or_a_number() {
    let mut cli = Cli::new("loglevel");

    cli.run("set_log_level", ["Debug"]);
    assert_eq!(cli.session.log_level, 4);

    cli.run("set_log_level", ["6"]);
    assert_eq!(cli.session.log_level, 0, "6 is `Disabled`, the sixth entry");

    cli.run("set_log_level", ["Trace"]);
    assert_eq!(cli.session.log_level, 5);

    // And the logger follows, as `Logger::logger.setLogLevel` does. Logging is
    // process-wide, so this leaves it off again for the other tests.
    assert!(wrkz_wallet::logging::enabled(wrkz_rpc::log::Level::Trace));
    cli.run("set_log_level", ["Warning"]);
    assert!(wrkz_wallet::logging::enabled(wrkz_rpc::log::Level::Warn));
    assert!(!wrkz_wallet::logging::enabled(wrkz_rpc::log::Level::Info));
    cli.run("set_log_level", ["Disabled"]);
    assert!(!wrkz_wallet::logging::enabled(wrkz_rpc::log::Level::Error), "Disabled is off, errors included");
}

#[test]
fn swap_node_takes_a_new_address_and_reports_it() {
    let mut cli = Cli::new("swap");

    // The daemon prompt, then the SSL question in a TLS build.
    let script: Vec<&str> =
        if wrkz_wallet::daemon::HTTPS_SUPPORTED { vec!["node.example:1234", "n"] } else { vec!["node.example:1234"] };
    let output = cli.run("swap_node", script);
    assert!(output.contains("Swapping node, this may take some time..."), "{output}");
    assert!(output.contains("Node swap complete."), "{output}");

    let open = cli.session.wallet.lock().unwrap();
    assert_eq!(open.daemon_host, "node.example");
    assert_eq!(open.daemon_port, 1234);
    assert!(!open.daemon_ssl);
}

#[test]
fn swap_node_accepts_an_ipc_endpoint_or_refuses_it_with_a_reason() {
    let mut cli = Cli::new("swapipc");

    if wrkz_wallet::ipc::supported() {
        let output = cli.run("swap_node", ["/run/wrkz/daemon.sock"]);
        assert!(!output.contains("SSL"), "a local socket is never asked about TLS:\n{output}");
        assert_eq!(cli.session.wallet.lock().unwrap().daemon_host, "/run/wrkz/daemon.sock");
    } else {
        // The prompt refuses it and asks again; the empty answer takes the
        // default, so the node ends up as localhost.
        // The prompt refuses it before it ever gets to the SSL question, so the
        // script is the same either way.
        let output = cli.run("swap_node", ["@wrkzd", ""]);
        assert!(output.contains("not available on this platform"), "{output}");
    }
}
