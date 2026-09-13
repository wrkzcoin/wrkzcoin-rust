// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-txpow-server` — the transaction proof of work, computed for wallets.
//!
//! ```text
//! wrkz-txpow-server --bind-ip 0.0.0.0 --bind-port 17870 --threads 8
//! ```
//!
//! docs/TXPOW-SERVER.md has every option, the reverse-proxy set-up and the
//! protocol. Exit codes: `0` after a clean shutdown, `1` for a bad option, a
//! log file that cannot be opened, or a port that cannot be bound.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use wrkz_txpow_server::api::{Api, ApiConfig};
use wrkz_txpow_server::config::{parse_arguments, Parsed, USAGE};
use wrkz_txpow_server::log::{Level, Logger};
use wrkz_txpow_server::serve::{self, ServeConfig};
use wrkz_txpow_server::service::{Limits, PowService};
use wrkz_txpow_server::{signal, version_line};

fn main() -> ExitCode {
    let config = match parse_arguments(std::env::args().skip(1)) {
        Parsed::Run(config) => config,
        Parsed::Help => {
            println!("{}\n\n{USAGE}", version_line());
            return ExitCode::SUCCESS;
        }
        Parsed::Version => {
            println!("{}", version_line());
            return ExitCode::SUCCESS;
        }
        Parsed::Error { message, show_usage } => {
            println!("{message}");
            if show_usage {
                println!("\n{USAGE}");
            }
            return ExitCode::from(1);
        }
    };

    let logger = match &config.log_file {
        None => Logger::new(config.log_level),
        Some(path) => match Logger::with_file(config.log_level, path) {
            Ok(l) => l,
            Err(e) => {
                println!("Could not open log file {path}: {e}");
                return ExitCode::from(1);
            }
        },
    };
    let logger = Arc::new(logger);

    println!("{}\n", version_line());

    let limits = Limits {
        threads: config.threads,
        max_queue: config.max_queue as usize,
        max_difficulty: config.max_difficulty,
        job_timeout: Duration::from_secs(config.job_timeout_seconds.into()),
        result_ttl: Duration::from_secs(config.result_ttl_seconds.into()),
        fixed_difficulty: None,
    };
    let service = PowService::start(limits, Arc::clone(&logger));
    let api = Arc::new(Api::new(ApiConfig::from(&config), Arc::clone(&service), Arc::clone(&logger)));
    let serve_config = ServeConfig {
        bind_ip: config.bind_ip.clone(),
        bind_port: config.bind_port,
        bind_ipv6_address: config.bind_ipv6_address.clone(),
        workers: ServeConfig::workers_for(config.max_queue as usize),
        ..ServeConfig::default()
    };

    let mut running = match serve::start(api, serve_config, Arc::clone(&logger)) {
        Ok(r) => r,
        Err(e) => {
            logger.log(
                Level::Fatal,
                format!(
                    "Could not bind to {}:{} ({e}). Is the port in use, or the address not local?",
                    config.bind_ip, config.bind_port
                ),
            );
            service.stop();
            return ExitCode::from(1);
        }
    };

    let listening: Vec<String> = running
        .addrs()
        .iter()
        .map(|a| if a.is_ipv6() { format!("http://[{}]:{}", a.ip(), a.port()) } else { format!("http://{a}") })
        .collect();
    let per_minute = |n: u32| if n == 0 { "unlimited".to_string() } else { n.to_string() };
    println!(
        "Tx PoW server listening on {}\n  hashing threads: {}\n  queue: {} jobs, max difficulty {}\n  limits: {} \
         requests/min per address, {} jobs/min overall\n  api key: {}\n  statistics: GET /stats\n",
        listening.join(" and "),
        config.threads,
        config.max_queue,
        config.max_difficulty,
        per_minute(config.rate_limit_per_minute),
        per_minute(config.max_jobs_per_minute),
        if config.api_key.is_empty() { "not required" } else { "required" },
    );

    signal::install();
    while !signal::stop_requested() {
        std::thread::sleep(Duration::from_millis(250));
    }

    println!("Shutting down...");
    // The service first: it answers every held long poll, so the workers the
    // listeners join are free at once.
    service.stop();
    running.stop();
    ExitCode::SUCCESS
}
