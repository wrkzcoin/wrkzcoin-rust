// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet-sync` — view-sync an address against a daemon and print what it
//! finds, so the result can be diffed against the C++ `wrkz-wallet` CLI on the same keys
//! (spec/12 stage 2 step 3, spec/10 "Acceptance for this document", item 3).
//!
//! ```text
//! cargo run -p wrkz-wallet --bin wrkz-wallet-sync -- \
//!     --daemon http://node-fin.wrkz.work:17856 \
//!     --view-key <64 hex> \
//!     --address Wrkz... \
//!     --scan-height 4200000 \
//!     [--out view.wallet] [--password secret] [--max-steps 100000] [--skip-coinbase] [--quiet]
//! ```
//!
//! The wallet built here is view-only: it finds incoming outputs and their
//! amounts, but it holds no spend key, so it cannot compute key images and
//! therefore cannot see those outputs being spent (spec/10, "Scanning"). The
//! transaction list and balance it prints are the incoming side only — which is
//! exactly what a the C++ `wrkz-wallet` CLI view wallet on the same keys shows.
//!
//! With `--out` the synced wallet is written as a normal wallet file, which
//! the C++ `wrkz-wallet` CLI can open.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use wrkz_wallet::daemon::Daemon;
use wrkz_wallet::file::{SecretKey, Wallet};
use wrkz_wallet::sync::{SyncConfig, SyncStep, Synchronizer};

const USAGE: &str = "\
wrkz-wallet-sync — view-sync an address and print its transactions and balance

    --daemon URL         daemon base URL, e.g. http://node-fin.wrkz.work:17856
    --view-key HEX       the private view key, 64 hex characters
    --address ADDR       the standard address the view key belongs to
    --scan-height N      block index to start scanning from (default 0)
    --out FILE           write the synced wallet to FILE
    --password PASS      password for --out (default: empty)
    --max-steps N        stop after N sync rounds (default: unlimited)
    --skip-coinbase      do not scan coinbase transactions
    --quiet              no progress lines on stderr
";

struct Args {
    daemon: String,
    view_key: String,
    address: String,
    scan_height: u64,
    out: Option<String>,
    password: String,
    max_steps: usize,
    skip_coinbase: bool,
    quiet: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        daemon: String::new(),
        view_key: String::new(),
        address: String::new(),
        scan_height: 0,
        out: None,
        password: String::new(),
        max_steps: usize::MAX,
        skip_coinbase: false,
        quiet: false,
    };

    let mut it = std::env::args().skip(1);

    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));

        match arg.as_str() {
            "--daemon" => args.daemon = value()?,
            "--view-key" => args.view_key = value()?,
            "--address" => args.address = value()?,
            "--scan-height" => {
                let v = value()?;
                args.scan_height = v.parse().map_err(|_| format!("--scan-height {v} is not a number"))?;
            }
            "--out" => args.out = Some(value()?),
            "--password" => args.password = value()?,
            "--max-steps" => {
                let v = value()?;
                args.max_steps = v.parse().map_err(|_| format!("--max-steps {v} is not a number"))?;
            }
            "--skip-coinbase" => args.skip_coinbase = true,
            "--quiet" => args.quiet = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }

    if args.daemon.is_empty() || args.view_key.is_empty() || args.address.is_empty() {
        return Err("--daemon, --view-key and --address are required".into());
    }

    Ok(args)
}

/// Two decimal places, as `CRYPTONOTE_DISPLAY_DECIMAL_POINT` says.
fn amount(atomic: u64) -> String {
    format!("{}.{:02} WRKZ", atomic / 100, atomic % 100)
}

fn signed_amount(atomic: i64) -> String {
    let sign = if atomic < 0 { "-" } else { "" };
    format!("{sign}{}", amount(atomic.unsigned_abs()))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}\n");
            }
            eprint!("{USAGE}");
            return if message.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
    };

    let view_key = match SecretKey::from_hex(&args.view_key) {
        Some(key) => key,
        None => {
            eprintln!("error: --view-key must be 64 hex characters");
            return ExitCode::FAILURE;
        }
    };

    let wallet = match Wallet::import_view_only(&view_key, &args.address, args.scan_height) {
        Ok(wallet) => wallet,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let daemon = match Daemon::new(&args.daemon) {
        Ok(daemon) => daemon,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let config = SyncConfig { skip_coinbase_transactions: args.skip_coinbase, ..SyncConfig::default() };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);

    if let Err(e) = sync.refresh_info() {
        eprintln!("error: could not reach the daemon: {e}");
        return ExitCode::FAILURE;
    }

    let network_height = sync.daemon_state().network_block_count;

    if !args.quiet {
        eprintln!("syncing {} from height {} to {network_height}", args.address, args.scan_height);
    }

    let mut last_report = Instant::now();
    let mut last_info = Instant::now();
    let mut steps = 0usize;

    loop {
        if steps >= args.max_steps {
            break;
        }
        steps += 1;

        if last_info.elapsed() >= Duration::from_secs(10) {
            let _ = sync.refresh_info();
            last_info = Instant::now();
        }

        let step = sync.sync_step();

        match step {
            SyncStep::Processed { .. } => {
                if !args.quiet && last_report.elapsed() >= Duration::from_secs(2) {
                    let status = sync.sync_status();
                    eprintln!(
                        "  height {} / {} ({} transactions)",
                        status.wallet_block_count,
                        status.network_block_count,
                        sync.wallet().transactions().len()
                    );
                    last_report = Instant::now();
                }
            }
            SyncStep::Synced { height } => {
                if !args.quiet {
                    eprintln!("synced at height {height}");
                }
                break;
            }
            SyncStep::Idle { backoff } => {
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
            }
            SyncStep::Failed { error, backoff } => {
                eprintln!("  daemon error: {error}; waiting {}s", backoff.as_secs());
                std::thread::sleep(backoff);
            }
            SyncStep::Gap { covered_to, daemon_serves_from } => {
                eprintln!(
                    "error: this daemon holds nothing below height {daemon_serves_from}, and this wallet has only \
                     scanned to {covered_to}. Use a daemon with the whole chain."
                );
                return ExitCode::FAILURE;
            }
        }
    }

    let status = sync.sync_status();
    let (unlocked, locked) = sync.wallet().balance(status.network_block_count);

    println!("address        {}", args.address);
    println!("scanned to     {} of {}", status.wallet_block_count, status.network_block_count);
    println!("transactions   {}", sync.wallet().transactions().len());
    println!("unlocked       {}", amount(unlocked));
    println!("locked         {}", amount(locked));
    println!();
    println!("{:<10} {:<66} {:>16} {:>12}  paymentID", "height", "hash", "amount", "fee");

    for tx in sync.wallet().transactions() {
        println!(
            "{:<10} {:<66} {:>16} {:>12}  {}",
            tx.block_height,
            tx.hash.to_hex(),
            signed_amount(tx.total_amount()),
            amount(tx.fee),
            tx.payment_id
        );
    }

    if let Some(path) = &args.out {
        let wallet = sync.into_wallet();
        match wallet.save(path, &args.password) {
            Ok(()) => eprintln!("wrote {path}"),
            Err(e) => {
                eprintln!("error: could not write {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}
