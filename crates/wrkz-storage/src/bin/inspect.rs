// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Open a C++ node's RocksDB read-only and check it with the port
//! (spec/11 acceptance 1–3).
//!
//!     wrkz-db-inspect <db-dir> [--from N] [--count N] [--rings N] [--quiet]
//!
//! Default: the top 1000 headers. For every block in range: the stored hash
//! equals the hash of the stored raw block, prev links, the major version
//! rule, and the proof of work against the difficulty derived from the
//! cumulative difficulties (outside the checkpoint zone this is what the
//! node itself verified). `--rings N` additionally resolves the ring members
//! of the last N non-coinbase transactions in range and verifies their ring
//! signatures. Exit code 0 means every check passed.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::block_major_version_for_index;
use wrkz_primitives::tx::Transaction;
use wrkz_storage::reader::ChainReader;
use wrkz_storage::rocks::{DbConfig, RocksStore};

const USAGE: &str = "usage: wrkz-db-inspect <db-dir> [--from N] [--count N] [--rings N] [--quiet]";

fn die(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The value of a `--flag N` pair. A tool whose job is to verify a database must
/// never quietly check something other than what it was asked to check, so an
/// absent or unparsable value is fatal rather than a fallback to the default.
fn value<T: std::str::FromStr>(args: &[String], i: usize) -> T {
    let Some(raw) = args.get(i + 1) else { die(&format!("{}: missing value", args[i])) };
    match raw.parse() {
        Ok(v) => v,
        Err(_) => die(&format!("{}: {raw:?} is not a number", args[i])),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        die("no database directory given");
    }
    if args[1].starts_with("--") {
        die(&format!("expected a database directory, got {:?}", args[1]));
    }
    let mut from: Option<u32> = None;
    let mut count: u32 = 1000;
    let mut rings: usize = 0;
    let mut quiet = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                from = Some(value(&args, i));
                i += 1;
            }
            "--count" => {
                count = value(&args, i);
                i += 1;
            }
            "--rings" => {
                rings = value(&args, i);
                i += 1;
            }
            "--quiet" => quiet = true,
            other => die(&format!("unknown argument {other:?}")),
        }
        i += 1;
    }
    if count == 0 {
        die("--count must be at least 1");
    }
    match run(PathBuf::from(&args[1]), from, count, rings, quiet) {
        Ok(()) => println!("INSPECT OK"),
        Err(e) => {
            eprintln!("INSPECT FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run(
    path: PathBuf,
    from: Option<u32>,
    count: u32,
    rings: usize,
    quiet: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !path.join("CURRENT").is_file() {
        return Err(format!(
            "{} is not a RocksDB directory (no CURRENT file); use the daemon's DB directory, by default ~/.WRKZCoin/DB, with Wrkzd stopped or on a copy",
            path.display()
        )
        .into());
    }
    let store = RocksStore::open_read_only(&path, &DbConfig::default())?;
    let r = ChainReader::new(store);
    let version = r.schema_version()?;
    let top = r.last_block_index()?;
    let txs = r.transactions_count().ok();
    println!("db {} schema {:?} top index {} txs {:?}", path.display(), version, top, txs);
    if version != Some(wrkz_primitives::constants::DB_SCHEME_VERSION) {
        return Err(format!("unexpected schema version {version:?}").into());
    }
    // `count >= 1` is enforced by main; both of these have to survive a `--from`
    // near the top of the range.
    let start = from.unwrap_or(top.saturating_sub(count - 1));
    if start > top {
        return Err(format!("--from {start} is above the top block index {top}").into());
    }
    let end = start.saturating_add(count - 1).min(top);
    let mut prev_hash: Option<[u8; 32]> =
        if start == 0 { None } else { r.block_info(start - 1)?.map(|b| b.block_hash) };
    let mut checked = 0u64;
    let mut pending_txs: Vec<(u32, Vec<u8>)> = Vec::new();
    for index in start..=end {
        let raw = r.raw_block(index)?.ok_or("raw block")?;
        let block = BlockTemplate::from_bytes(&raw.block)?;
        let h = r.header_with_block(index, Some(&block))?.ok_or_else(|| format!("no header for {index}"))?;
        let body = h.body()?;
        let computed = block.hash()?;
        if computed != h.hash {
            return Err(format!(
                "block {index}: stored hash {} != computed {}",
                hex::encode(h.hash),
                hex::encode(computed)
            )
            .into());
        }
        if let Some(p) = prev_hash {
            if block.previous_block_hash != p {
                return Err(format!("block {index}: prev link broken").into());
            }
        }
        if block.major_version != block_major_version_for_index(index as u64) {
            return Err(format!(
                "block {index}: major version {} vs rule {}",
                block.major_version,
                block_major_version_for_index(index as u64)
            )
            .into());
        }
        if r.block_index_by_hash(&h.hash)? != Some(index) {
            return Err(format!("block {index}: hash->index mismatch").into());
        }
        let hashes = r.block_tx_hashes(index)?;
        if hashes.len() != raw.transactions.len() + 1 || hashes.len() as u64 != body.num_txes {
            return Err(
                format!("block {index}: tx hash list {} vs raw {}", hashes.len(), raw.transactions.len() + 1).into()
            );
        }
        for (t, want) in raw.transactions.iter().zip(&hashes[1..]) {
            if Transaction::from_bytes(t)?.hash()? != *want {
                return Err(format!("block {index}: transaction hash mismatch").into());
            }
            if rings > 0 {
                // Only the last `rings` are verified, but which those are is not
                // known until the scan ends; keep a window instead of the range.
                if pending_txs.len() == rings {
                    pending_txs.remove(0);
                }
                pending_txs.push((index, t.clone()));
            }
        }
        if index > 0 && !block.check_proof_of_work(h.difficulty)? {
            return Err(format!("block {index}: proof of work fails at difficulty {}", h.difficulty).into());
        }
        if !quiet {
            println!(
                "{index} v{} {} diff {} reward {} size {} txs {} ts {}",
                body.major_version,
                hex::encode(h.hash),
                h.difficulty,
                body.reward,
                h.block_size,
                body.num_txes,
                h.timestamp
            );
        }
        prev_hash = Some(h.hash);
        checked += 1;
    }
    println!("{checked} blocks verified ({start}..={end})");
    if rings > 0 {
        let take = pending_txs.len().min(rings);
        let mut ok = 0;
        for (index, blob) in pending_txs.iter().rev().take(take) {
            let tx = Transaction::from_bytes(blob)?;
            if !r.verify_ring_signatures(&tx)? {
                return Err(format!(
                    "block {index}: ring signature verification failed for tx {}",
                    hex::encode(tx.hash()?)
                )
                .into());
            }
            ok += 1;
        }
        println!("{ok} transactions: ring members resolved and ring signatures verified");
    }
    Ok(())
}
