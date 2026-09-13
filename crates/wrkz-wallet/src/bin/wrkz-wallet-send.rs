// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet-send` — build (and optionally relay) a transaction from a
//! wallet file, so the first mainnet send of the port can be made by hand and
//! inspected before it goes out (spec/12 stage 2 step 4, spec/10 "Transaction construction").
//!
//! ```text
//! cargo run --release -p wrkz-wallet --bin wrkz-wallet-send -- \
//!     --wallet mine.wallet --password secret \
//!     --daemon http://node-fin.wrkz.work:17856 \
//!     --to Wrkz... --amount 1000 [--payment-id HEX] \
//!     [--fee N | --fee-per-byte RATE] [--mixin N] [--dry-run]
//! ```
//!
//! `--dry-run` stops after building: it prints the hex, the fee, the size, the
//! proof of work and every input the transaction spends, and touches neither
//! the daemon nor the wallet file. Without it the transaction is relayed
//! through `/sendrawtransaction` and the wallet file is written back with the
//! inputs locked, the change recorded as unconfirmed and the transaction
//! private key stored.
//!
//! The wallet must already be synced (`wrkz-wallet-sync`, or the C++ wallet):
//! this tool does not scan. It refuses to relay when the wallet's height is
//! more than a block behind the daemon's, because inputs it has not seen spent
//! would be double spends.

use std::process::ExitCode;

use wrkz_wallet::daemon::Daemon;
use wrkz_wallet::file::Wallet;
use wrkz_wallet::transfer::{
    self, FeeType, PreparedTransaction, SeededRandom, SendParams, SystemRandom, TransferRandom,
};

const USAGE: &str = "\
wrkz-wallet-send — build and relay a transaction from a wallet file

    --wallet FILE        the wallet file to spend from            (required)
    --password PASS      its password                             (default: empty)
    --daemon URL         daemon base URL                          (required)
    --to ADDR            destination address (standard or integrated)  (required)
    --amount N           atomic units to send                     (required unless --send-all)
    --payment-id HEX     16 or 64 hex characters
    --fee N              a fixed fee in atomic units
    --fee-per-byte RATE  atomic units per byte (default: the network minimum)
    --mixin N            ring size minus one (default: the tier default at the network height)
    --change-address A   where change goes (default: the primary address)
    --from ADDR          a subwallet to spend from; repeatable (default: all)
    --unlock-time N      block index or unix time (default: networkHeight + 20 + 15)
    --send-all           send the whole balance, taking the fee out of the amount
    --pow-threads N      threads for the transaction proof-of-work search (default: all cores)
    --seed HEX           32-byte seed for a reproducible build (testing only)
    --dry-run            build and print, do not relay and do not save
    --yes                relay without the confirmation prompt
";

/// Sentinel for "the user asked for the usage", which is not an error.
const HELP_REQUESTED: &str = "\u{0}help";

struct Args {
    wallet: String,
    password: String,
    daemon: String,
    to: String,
    amount: u64,
    payment_id: String,
    fee: Option<FeeType>,
    mixin: Option<u64>,
    change_address: String,
    from: Vec<String>,
    unlock_time: u64,
    send_all: bool,
    pow_threads: usize,
    seed: Option<[u8; 32]>,
    dry_run: bool,
    yes: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        wallet: String::new(),
        password: String::new(),
        daemon: String::new(),
        to: String::new(),
        amount: 0,
        payment_id: String::new(),
        fee: None,
        mixin: None,
        change_address: String::new(),
        from: Vec::new(),
        unlock_time: 0,
        send_all: false,
        pow_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        seed: None,
        dry_run: false,
        yes: false,
    };

    let mut it = std::env::args().skip(1);

    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));

        match arg.as_str() {
            "--wallet" => args.wallet = value()?,
            "--password" => args.password = value()?,
            "--daemon" => args.daemon = value()?,
            "--to" => args.to = value()?,
            "--amount" => {
                let v = value()?;
                args.amount = v.parse().map_err(|_| format!("--amount {v} is not a number"))?;
            }
            "--payment-id" => args.payment_id = value()?,
            "--fee" => {
                let v = value()?;
                args.fee = Some(FeeType::FixedFee(v.parse().map_err(|_| format!("--fee {v} is not a number"))?));
            }
            "--fee-per-byte" => {
                let v = value()?;
                args.fee =
                    Some(FeeType::FeePerByte(v.parse().map_err(|_| format!("--fee-per-byte {v} is not a number"))?));
            }
            "--mixin" => {
                let v = value()?;
                args.mixin = Some(v.parse().map_err(|_| format!("--mixin {v} is not a number"))?);
            }
            "--change-address" => args.change_address = value()?,
            "--from" => args.from.push(value()?),
            "--unlock-time" => {
                let v = value()?;
                args.unlock_time = v.parse().map_err(|_| format!("--unlock-time {v} is not a number"))?;
            }
            "--send-all" => args.send_all = true,
            "--pow-threads" => {
                let v = value()?;
                args.pow_threads = v.parse().map_err(|_| format!("--pow-threads {v} is not a number"))?;
            }
            "--seed" => {
                let v = value()?;
                let bytes = decode_hex(&v).ok_or_else(|| "--seed must be 64 hex characters".to_string())?;
                let bytes: [u8; 32] = bytes.try_into().map_err(|_| "--seed must be 64 hex characters".to_string())?;
                args.seed = Some(bytes);
            }
            "--dry-run" => args.dry_run = true,
            "--yes" => args.yes = true,
            "-h" | "--help" => return Err(HELP_REQUESTED.to_string()),
            other => return Err(format!("unknown argument {other}")),
        }
    }

    if args.wallet.is_empty() {
        return Err("--wallet is required".into());
    }
    if args.daemon.is_empty() {
        return Err("--daemon is required".into());
    }
    if args.to.is_empty() {
        return Err("--to is required".into());
    }
    if args.amount == 0 && !args.send_all {
        return Err("--amount is required (or --send-all)".into());
    }

    Ok(args)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) if msg == HELP_REQUESTED => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("{msg}\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match run(args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<ExitCode, String> {
    let daemon = Daemon::new(&args.daemon).map_err(|e| format!("bad --daemon: {e}"))?;

    let info = daemon.info().map_err(|e| format!("could not reach the daemon: {e}"))?;
    // `Nigel::networkBlockCount()`: the network's top **index**, `/info`'s count
    // minus one (`Nigel.cpp:867`), which is what every height-dependent rule of a
    // send is judged at — the mixin tier above all. The daemon's pool judges a
    // transaction at its top index too, so with the raw count a send built while
    // the tip is 4,299,999 would take the 4,300,000 tier one block early and be
    // refused.
    let network_height = info.network_height.saturating_sub(1);

    let mut wallet = Wallet::open(&args.wallet, &args.password)
        .map_err(|e| format!("could not open the wallet: {e} (code {})", e.code()))?;

    if wallet.is_view_wallet() {
        return Err("this is a view wallet; it holds no spend key and cannot send".into());
    }

    let wallet_height = wallet.wallet_height();
    let behind = network_height.saturating_sub(wallet_height);
    if behind > 1 {
        eprintln!(
            "warning: the wallet is at {wallet_height} and the network at {network_height} ({behind} blocks behind). \
             Sync it first, or inputs it has not seen spent will be refused as double spends."
        );
        if !args.dry_run && !args.yes {
            return Err("refusing to relay from an unsynced wallet; pass --yes to override".into());
        }
    }

    let (unlocked, locked) = wallet.balance(network_height);
    eprintln!(
        "wallet {} | height {wallet_height}/{network_height} | unlocked {unlocked} | locked {locked}",
        args.wallet
    );

    let params = SendParams {
        destinations: vec![(args.to.clone(), args.amount)],
        mixin: args.mixin.unwrap_or_else(|| wrkz_primitives::mixins::mixin_allowable_range(network_height).default),
        fee: args.fee.unwrap_or(FeeType::MinimumFee),
        payment_id: args.payment_id.clone(),
        addresses_to_take_from: args.from.clone(),
        change_address: args.change_address.clone(),
        unlock_time: args.unlock_time,
        extra_data: Vec::new(),
        send_all: args.send_all,
        network_height,
        pow_threads: args.pow_threads.max(1),
    };

    eprintln!(
        "building: {} atomic to {} | mixin {} | {} | proof-of-work threads {}",
        params.destinations[0].1,
        params.destinations[0].0,
        params.mixin,
        match params.fee {
            FeeType::MinimumFee => "minimum fee".to_string(),
            FeeType::FeePerByte(r) => format!("fee per byte {r}"),
            FeeType::FixedFee(f) => format!("fixed fee {f}"),
        },
        params.pow_threads,
    );

    let started = std::time::Instant::now();

    let prepared = match args.seed {
        Some(seed) => build(&wallet, &daemon, &params, &mut SeededRandom::new(seed)),
        None => build(&wallet, &daemon, &params, &mut SystemRandom),
    }
    .map_err(|e| format!("build failed: {e} (code {})", e.code()))?;

    report(&prepared, started.elapsed(), network_height);

    if args.dry_run {
        eprintln!("\n--dry-run: nothing was relayed and the wallet file was not written.");
        return Ok(ExitCode::SUCCESS);
    }

    if !args.yes {
        eprintln!("\nAbout to relay {} to {}. Re-run with --yes to send.", prepared.transaction_hash, args.daemon);
        return Ok(ExitCode::from(3));
    }

    let sent = transfer::send_prepared_transaction(&mut wallet, &daemon, prepared, network_height)
        .map_err(|e| format!("relay failed: {e} (code {})", e.code()))?;

    wallet
        .save(&args.wallet, &args.password)
        .map_err(|e| format!("RELAYED but could not save the wallet: {e}. The inputs are spent on chain; re-sync."))?;

    println!("{}", sent.transaction_hash);
    eprintln!("relayed and saved. The change confirms in about {} blocks.", 20 + 15);

    Ok(ExitCode::SUCCESS)
}

fn build<R: TransferRandom>(
    wallet: &Wallet,
    daemon: &Daemon,
    params: &SendParams,
    random: &mut R,
) -> wrkz_wallet::file::Result<PreparedTransaction> {
    transfer::prepare_transaction(wallet, daemon, params, random)
}

fn report(prepared: &PreparedTransaction, elapsed: std::time::Duration, network_height: u64) {
    println!("hash        {}", prepared.transaction_hash);
    println!(
        "size        {} bytes (limit {})",
        prepared.size,
        wrkz_primitives::fees::wallet_max_tx_size(network_height)
    );
    println!("fee         {}", prepared.fee);
    println!("mixin       {} (ring size {})", prepared.mixin, prepared.mixin + 1);
    println!("unlock      {}", prepared.transaction.prefix.unlock_time);
    println!("change      {} to {}", prepared.change_required, prepared.change_address);
    if !prepared.payment_id.is_empty() {
        println!("payment id  {}", prepared.payment_id);
    }
    match prepared.pow_nonce {
        Some(nonce) => println!(
            "tx pow      difficulty {} solved in {} hashes ({:.1} s), nonce {}",
            prepared.pow_difficulty,
            prepared.pow_hashes,
            elapsed.as_secs_f64(),
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        None => println!("tx pow      not required (fee clears TRANSACTION_POW_PASS_WITH_FEE)"),
    }
    println!("signatures  {} rings, all verify: {}", prepared.rings.len(), prepared.verify_signatures());
    println!("outputs     {}", prepared.outputs.len());
    for output in &prepared.outputs {
        println!("            {:>20}  {}", output.amount, output.key);
    }
    println!("inputs      {}", prepared.inputs.len());
    for (i, input) in prepared.inputs.iter().enumerate() {
        println!(
            "            {:>20}  global index {:>10}  key image {}  ring {:?}",
            input.input.amount,
            input.input.global_output_index.unwrap_or(0),
            input.input.key_image,
            prepared.rings[i].ring.iter().map(|(g, _)| *g).collect::<Vec<_>>()
        );
    }
    println!("hex         {}", prepared.to_hex());
}
