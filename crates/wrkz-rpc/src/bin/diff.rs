// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-rpc-diff`: put the same requests to two daemons and diff the answers.
//!
//! ```text
//! wrkz-rpc-diff --reference http://node-fin.wrkz.work:17856 \
//!               --ours      http://127.0.0.1:17856 \
//!               [--endpoints info,height,getwalletsyncdata] \
//!               [--values] [--height 4213000] [--range-start 4213000] \
//!               [--timeout 30] [--list]
//! ```
//!
//! By default it compares **shapes**: every key present on one side must be
//! present on the other, with the same type. That is the comparison that means
//! anything between two nodes on different chains. `--values` additionally
//! compares values, skipping the whitelist in [`wrkz_rpc::diff::DEFAULT_WHITELIST`]
//! (clocks, peer identities, connection counts, and anything that follows from
//! the chain's own height) — use it only when both sides hold the same chain.
//!
//! Exit status is 0 when every probe is clean, 1 otherwise.

use std::time::Duration;
use wrkz_rpc::diff::Comparison;
use wrkz_rpc::http::client::parse_base;
use wrkz_rpc::probe::{self, Probe};

struct Args {
    reference: String,
    ours: String,
    endpoints: Option<Vec<String>>,
    values: bool,
    height: u64,
    range_start: u64,
    timeout: Duration,
    list: bool,
    /// Whether `--height` was given, rather than defaulted.
    height_given: bool,
}

fn usage() -> String {
    "wrkz-rpc-diff --reference URL --ours URL [--endpoints a,b,c] [--values] \
     [--height N] [--range-start N] [--timeout SECONDS] [--list]"
        .to_string()
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        reference: String::new(),
        ours: String::new(),
        endpoints: None,
        values: false,
        height: probe::REFERENCE_HEIGHT,
        range_start: probe::REFERENCE_HEIGHT,
        timeout: Duration::from_secs(30),
        list: false,
        height_given: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--reference" => args.reference = value()?,
            "--ours" => args.ours = value()?,
            "--endpoints" => args.endpoints = Some(value()?.split(',').map(|s| s.trim().to_string()).collect()),
            "--values" => args.values = true,
            "--list" => args.list = true,
            "--height" => {
                args.height = value()?.parse().map_err(|e| format!("--height: {e}"))?;
                args.range_start = args.height;
                args.height_given = true;
            }
            "--range-start" => {
                args.range_start = value()?.parse().map_err(|e| format!("--range-start: {e}"))?;
                args.height_given = true;
            }
            "--timeout" => args.timeout = Duration::from_secs(value()?.parse().map_err(|e| format!("--timeout: {e}"))?),
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let mut set: Vec<Probe> = probe::probes_at(args.height, args.range_start);
    if args.list {
        for p in &set {
            println!("{:<32} {} {}", p.name, p.method, p.path);
        }
        return;
    }
    if let Some(wanted) = &args.endpoints {
        set.retain(|p| wanted.iter().any(|w| w == p.name));
        if set.is_empty() {
            eprintln!("no probe matched --endpoints; use --list to see the names");
            std::process::exit(2);
        }
    }

    if args.reference.is_empty() || args.ours.is_empty() {
        eprintln!("{}", usage());
        std::process::exit(2);
    }
    let reference = match parse_base(&args.reference) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("--reference {e}");
            std::process::exit(2);
        }
    };
    let ours = match parse_base(&args.ours) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("--ours {e}");
            std::process::exit(2);
        }
    };

    // Without an explicit `--height`, probe the highest index both daemons
    // hold. A node still catching up otherwise reports every range endpoint as
    // a difference, which is content, not contract.
    if !args.height_given {
        match probe::common_top_index(&reference, &ours, args.timeout) {
            Ok(top) => {
                let start = top.saturating_sub(1);
                println!(
                    "both daemons hold block index {top}; probing heights {top} and range [{start}, {})",
                    start + 1
                );
                set = probe::probes_at(top, start);
                if let Some(wanted) = &args.endpoints {
                    set.retain(|p| wanted.iter().any(|w| w == p.name));
                }
            }
            Err(e) => println!("could not read both heights ({e}); probing the default height {}", args.height),
        }
    }

    let comparison = if args.values { Comparison::values() } else { Comparison::default() };
    println!(
        "comparing {} probes, mode {:?}\n  reference {}:{}\n  ours      {}:{}\n",
        set.len(),
        comparison.mode,
        reference.host,
        reference.port,
        ours.host,
        ours.port
    );

    let results = probe::run(&reference, &ours, &set, &comparison, args.timeout);
    let mut failed = 0usize;
    for r in &results {
        let status = if r.reference_status == r.ours_status {
            format!("{}", r.ours_status)
        } else {
            format!("{} vs {}", r.reference_status, r.ours_status)
        };
        if r.is_clean() {
            println!("ok    {:<32} [{status}]", r.name);
            continue;
        }
        failed += 1;
        println!("DIFF  {:<32} [{status}]", r.name);
        if let Some(e) = &r.error {
            println!("        error: {e}");
        }
        if r.reference_status != r.ours_status {
            println!("        status: reference {}, ours {}", r.reference_status, r.ours_status);
        }
        for d in &r.differences {
            println!("        {d}");
        }
    }

    println!("\n{} of {} probes clean", results.len() - failed, results.len());
    if failed > 0 {
        std::process::exit(1);
    }
}
