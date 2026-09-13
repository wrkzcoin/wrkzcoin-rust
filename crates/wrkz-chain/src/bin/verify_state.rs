// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-verify-state`: rebuild a chain state someone handed you — a snapshot,
//! a copy of another node's `DIR/state` — from its own block bodies, and
//! compare every record ([`wrkz_chain::verify`]).
//!
//! ```text
//! wrkz-verify-state --given <state dir> --state <new state dir>
//!                   [--threads N] [--progress N] [--to H] [--batch-blocks N] [--sync-every N]
//! ```
//!
//! The given state is opened **read-only** and never written. The rebuilt one
//! is complete, carries every block body and every per-block output list, is
//! tagged `linear`, and is what a daemon serves: point `--data-dir` at its
//! parent. Either may be served once this prints `VERIFY OK`.
//!
//! Resumable: run it again on the same `--state` and it continues from where
//! it got to. Ctrl-C (or `SIGTERM`) stops at a block boundary, commits and
//! exits 0. The first record that differs stops the run, names the block and
//! the record, and exits 1.
//!
//! All the work is in `wrkz_chain::verify`; this is argument parsing and the
//! two RocksDB opens.

use std::path::PathBuf;
use wrkz_chain::replay::ReplayOptions;
use wrkz_chain::verify::verify_state;
use wrkz_chain::{interrupt, ChainState, Checkpoints, Config};
use wrkz_storage::batch::BatchStore;
use wrkz_storage::rocks::{DbConfig, RocksStore};

const USAGE: &str = "usage: wrkz-verify-state --given <state-dir> --state <new-state-dir> \
[--threads N] [--progress N] [--to H] [--batch-blocks N] [--sync-every N]";

fn main() {
    match run() {
        Ok(true) => println!("VERIFY OK"),
        Ok(false) => println!("VERIFY INCOMPLETE: run again on the same --state to continue"),
        Err(e) => {
            eprintln!("VERIFY FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// `Ok(true)` when the whole given state was rebuilt and matched.
fn run() -> Result<bool, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut given: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut threads = wrkz_pow::parallel::default_threads();
    let mut batch_blocks = wrkz_storage::batch::DEFAULT_BATCH_POINTS;
    let mut options = ReplayOptions { sync_every: 250_000, ..ReplayOptions::default() };
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let value = || argv.get(i + 1).cloned().ok_or_else(|| format!("{flag}: missing value\n{USAGE}"));
        let number = || -> Result<u64, String> {
            value()?.parse::<u64>().map_err(|_| format!("{flag}: not a non-negative number\n{USAGE}"))
        };
        match flag {
            "--given" => given = Some(PathBuf::from(value()?)),
            "--state" => state = Some(PathBuf::from(value()?)),
            "--threads" => threads = number()?.clamp(1, 1024) as usize,
            "--progress" => options.progress = number()?.clamp(1, u64::from(u32::MAX)) as u32,
            "--to" => options.to = Some(number()?.min(u64::from(u32::MAX)) as u32),
            "--batch-blocks" => batch_blocks = number()?.clamp(1, u64::from(u32::MAX)) as u32,
            "--sync-every" => options.sync_every = number()?.min(u64::from(u32::MAX)) as u32,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
        // Every flag above takes a value.
        i += 2;
    }
    let given = given.ok_or_else(|| format!("--given is required\n{USAGE}"))?;
    let state = state.ok_or_else(|| format!("--state is required\n{USAGE}"))?;
    if given == state {
        return Err("--given and --state must be two different directories".to_string());
    }

    // Before anything is opened, so an interrupt during the opens still stops
    // the loop.
    interrupt::install();
    options.stop = Some(interrupt::flag());

    // Read by key, the way a node reads it: block cache rather than write
    // buffer, as the replay opens a C++ database.
    let given_store = RocksStore::open_read_only(&given, &DbConfig::source(512))
        .map_err(|e| format!("opening the given state at {}: {e}", given.display()))?;
    let given_cfg =
        Config { store_raw_blocks: true, unwind_history: u32::MAX, validate_threads: 1, ..Config::default() };
    let given_chain = ChainState::open(given_store, given_cfg, Checkpoints::mainnet())
        .map_err(|e| format!("the given state: {e}"))?;

    // Bulk-loaded like an import: big memtables, no write-ahead log, batched
    // commits. The resume height travels in the same batch as its blocks.
    let state_cfg = DbConfig::import(threads.min(32) as i32);
    let store =
        RocksStore::open(&state, &state_cfg).map_err(|e| format!("opening our state at {}: {e}", state.display()))?;
    let store = BatchStore::with_limits(store, batch_blocks, wrkz_storage::batch::DEFAULT_BATCH_BYTES);
    let cfg =
        Config { store_raw_blocks: true, unwind_history: u32::MAX, validate_threads: threads, ..Config::default() };
    let mut chain =
        ChainState::open_or_genesis(store, cfg, Checkpoints::mainnet()).map_err(|e| format!("our state: {e}"))?;

    println!(
        "verifying {} into {} on {threads} threads, committing every {batch_blocks} blocks",
        given.display(),
        state.display()
    );
    let mut log = |line: &str| println!("{line}");
    let report = verify_state(&given_chain, &mut chain, &options, &mut log)?;
    Ok(!report.stopped && report.top == report.source_top)
}
