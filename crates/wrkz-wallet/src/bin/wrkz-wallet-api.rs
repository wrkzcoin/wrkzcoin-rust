// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet-api` — the wallet's HTTP API, route for route with the C++
//! `wrkz-wallet-api` (`src/walletapi/`).
//!
//! ```text
//! wrkz-wallet-api --rpc-password secret [--rpc-bind-ip 127.0.0.1] [-p 7856]
//!                 [--rpc-use-ipv6 --rpc-bind-ipv6-address ::1]
//!                 [--rpc-ipc-path /run/wrkz/wallet-api.sock]
//!                 [--enable-cors '*'] [--log-level 3] [--log-file api.log]
//!                 [--no-console] [--threads 4] [--skip-coinbase-transactions]
//! ```
//!
//! Every route and every response body lives in [`wrkz_wallet::api`]; this
//! binary is the argument parser, the listeners and the `exit`/`quit` console
//! the C++ `main` provides (`walletapi/WalletApi.cpp:16`).
//!
//! Exit codes: `0` on a clean shutdown, `1` for a bad argument or an IPv4
//! listener that could not bind — the `exit(1)`s of
//! `walletapi/ParseArguments.cpp` and `ApiDispatcher::start`.

use std::process::ExitCode;
use std::sync::Arc;

use wrkz_wallet::api::{self, serve, ApiConfig, ApiState};
use wrkz_wallet::listen::{self, IpcConfig};

/// The version line, in the shape `wrkz-node --version` prints
/// (`version_line` in `crates/wrkz-node/src/bin/node.rs`).
///
/// `WRKZ_GIT_COMMIT` is set at compile time by `build.rs` and is absent
/// outside a git checkout.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: Option<&str> = option_env!("WRKZ_GIT_COMMIT");

fn version_line() -> String {
    match GIT_COMMIT {
        Some(commit) if !commit.is_empty() => format!(
            "wrkz-wallet-api {VERSION} ({commit}), wallet API compatible with WrkzCoin {}",
            wrkz_rpc::DAEMON_VERSION
        ),
        _ => format!("wrkz-wallet-api {VERSION}, wallet API compatible with WrkzCoin {}", wrkz_rpc::DAEMON_VERSION),
    }
}

/// `options.help({})` of `walletapi/ParseArguments.cpp:44`, in its groups.
const USAGE: &str = "\
Usage: wrkz-wallet-api [OPTION...]

Core:
  -h, --help                        Display this help message
      --log-level #                 Specify log level: 0 disabled, 1 fatal, 2 warning, 3 info, 4 debug,
                                    5 trace (default: 0)
      --log-file <file>             Specify filepath to log to. Logging to file is disabled by default
      --no-console                  If set, will not provide an interactive console
      --scan-coinbase-transactions  Scan miner/coinbase transactions (the default; kept for compatibility)
      --skip-coinbase-transactions  Do not scan miner/coinbase transactions (alias: --skip-coinbase). Syncs
                                    faster, but block rewards paid to the wallet are not seen
      --threads #                   Specify number of wallet sync threads (default: one per core, at most 16)
  -v, --version                     Output software version information

Network:
  -p, --port <port>                 The port to listen on for http requests (default: 7856)
      --rpc-bind-ip arg             Interface IP address for the RPC service (default: 127.0.0.1)
      --rpc-bind-ipv6-address <ipv6>
                                    IPv6 bind address for the API service (e.g. ::1). Empty disables IPv6.
      --rpc-use-ipv6                Enable IPv6 support for the API service
      --rpc-ipc-path <path>         Also serve the API on a local IPC socket at this path, for example
                                    /run/wrkz/wallet-api.sock. Prefix with @ for the Linux abstract
                                    namespace. Empty disables IPC (default). Not available on Windows. The
                                    X-API-KEY password is still required on this socket
      --rpc-ipc-mode <mode>         Octal permissions for the IPC socket file. The default 0600 restricts it
                                    to the user running the API; use 0660 together with --rpc-ipc-group to
                                    share it (default: 0600)
      --rpc-ipc-group <group>       Group to own the IPC socket file, for a 0660 shared setup

Notifications:
      --tx-notify <cmd|url>         Run a command or POST to an http(s):// URL for every transaction confirmed
                                    for the open wallet. Placeholders: %s hash, %h height, %a amount, %f fee,
                                    %p payment id
      --notify-during-sync          Also fire tx-notify while the wallet is far behind the daemon (default:
                                    suppressed)

RPC:
      --enable-cors <domain>        Adds header 'Access-Control-Allow-Origin' to the RPC responses. Uses the
                                    value specified as the domain. Use * for all.
  -r, --rpc-password <password>     Specify the <password> to access the RPC server.
";

enum Parsed {
    Run(ApiConfig),
    Help,
    Version,
    Error(String),
}

fn parse_arguments<I: Iterator<Item = String>>(args: I) -> Parsed {
    let mut config = ApiConfig::default();
    let mut password_given = false;
    let mut help = false;
    let mut version = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        // `cxxopts` accepts `--name value` and `--name=value`.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => args.next().ok_or_else(|| format!("Option {name} requires an argument")),
            }
        };

        match name.as_str() {
            "-h" | "--help" => help = true,
            "-v" | "--version" => version = true,
            "--no-console" => config.no_console = true,
            "--skip-coinbase-transactions" | "--skip-coinbase" => config.skip_coinbase_transactions = true,
            // Coinbases are scanned by default; kept so old command lines run.
            "--scan-coinbase-transactions" => {}
            "--rpc-use-ipv6" => config.rpc_use_ipv6 = true,
            "--notify-during-sync" => config.notify_during_sync = true,

            "--log-level" => {
                match value(&name).and_then(|v| v.parse::<i32>().map_err(|_| format!("{v} is not a number"))) {
                    Ok(level) => config.log_level = level,
                    Err(e) => return Parsed::Error(e),
                }
            }
            "--log-file" => match value(&name) {
                Ok(path) => config.log_file = Some(path),
                Err(e) => return Parsed::Error(e),
            },
            "--threads" => {
                match value(&name).and_then(|v| v.parse::<u32>().map_err(|_| format!("{v} is not a number"))) {
                    Ok(threads) => config.threads = threads,
                    Err(e) => return Parsed::Error(e),
                }
            }
            "-p" | "--port" => {
                match value(&name).and_then(|v| v.parse::<u16>().map_err(|_| format!("{v} is not a port"))) {
                    Ok(port) => config.port = port,
                    Err(e) => return Parsed::Error(e),
                }
            }
            "--rpc-bind-ip" => match value(&name) {
                Ok(ip) => config.rpc_bind_ip = ip,
                Err(e) => return Parsed::Error(e),
            },
            "--rpc-bind-ipv6-address" => match value(&name) {
                Ok(v) => config.rpc_bind_ipv6_address = v,
                Err(e) => return Parsed::Error(e),
            },
            "--rpc-ipc-path" => match value(&name) {
                Ok(v) => config.rpc_ipc_path = v,
                Err(e) => return Parsed::Error(e),
            },
            // `ParseArguments.cpp:239`.
            "--rpc-ipc-mode" => match value(&name) {
                Ok(v) => match wrkz_rpc::ipc::parse_mode(&v) {
                    Some(mode) => config.rpc_ipc_mode = mode,
                    None => {
                        return Parsed::Error(format!("rpc-ipc-mode must be octal permissions such as 0600, got: {v}"))
                    }
                },
                Err(e) => return Parsed::Error(e),
            },
            "--rpc-ipc-group" => match value(&name) {
                Ok(v) => config.rpc_ipc_group = v,
                Err(e) => return Parsed::Error(e),
            },
            "--tx-notify" => match value(&name) {
                Ok(v) => config.tx_notify = v,
                Err(e) => return Parsed::Error(e),
            },
            "--enable-cors" => match value(&name) {
                Ok(v) => config.cors_header = v,
                Err(e) => return Parsed::Error(e),
            },
            "-r" | "--rpc-password" => match value(&name) {
                Ok(v) => {
                    config.rpc_password = v;
                    password_given = true;
                }
                Err(e) => return Parsed::Error(e),
            },

            other => {
                return Parsed::Error(format!(
                    "Unable to parse command line argument options: unrecognised option '{other}'"
                ))
            }
        }
    }

    if help {
        return Parsed::Help;
    }
    if version {
        return Parsed::Version;
    }

    // `ParseArguments.cpp:147`: the password is required for anything else.
    if !password_given {
        return Parsed::Error("You must specify an rpc-password!".into());
    }

    // `ParseArguments.cpp:172`.
    if !(0..=5).contains(&config.log_level) {
        return Parsed::Error("Log level must be between 0 and 5!".into());
    }

    // `ParseArguments.cpp:213`.
    if config.threads == 0 {
        return Parsed::Error("Thread count must be at least 1".into());
    }

    Parsed::Run(config)
}

/// Where the listeners go: `ip:port`, the IPv6 listener only with
/// `--rpc-use-ipv6` *and* an address (`ApiDispatcher.cpp:48`), and the IPC
/// socket when a path survived the platform check.
fn serve_config(config: &ApiConfig) -> serve::ServeConfig {
    let bind_ipv6 = if config.rpc_use_ipv6 && !config.rpc_bind_ipv6_address.is_empty() {
        listen::ipv6_bind(&config.rpc_bind_ipv6_address, config.port)
    } else {
        String::new()
    };
    let ipc = (!config.rpc_ipc_path.is_empty()).then(|| IpcConfig {
        path: config.rpc_ipc_path.clone(),
        mode: config.rpc_ipc_mode,
        group: config.rpc_ipc_group.clone(),
    });
    serve::ServeConfig { bind: format!("{}:{}", config.rpc_bind_ip, config.port), bind_ipv6, ipc, ..Default::default() }
}

fn main() -> ExitCode {
    let mut config = match parse_arguments(std::env::args().skip(1)) {
        Parsed::Help => {
            println!("{}\n", version_line());
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Parsed::Version => {
            println!("{}", version_line());
            return ExitCode::SUCCESS;
        }
        Parsed::Error(message) => {
            println!("{message}");
            print!("\n{USAGE}");
            return ExitCode::FAILURE;
        }
        Parsed::Run(config) => config,
    };

    // `ParseArguments.cpp:245`: a platform with no IPC sockets drops the option
    // with a reason, rather than failing to start.
    if !config.rpc_ipc_path.is_empty() && !wrkz_rpc::ipc::supported() {
        println!("Ignoring --rpc-ipc-path: {}.", wrkz_rpc::ipc::UNSUPPORTED);
        config.rpc_ipc_path.clear();
    }

    // `ParseArguments.cpp:182`: a log file that cannot be opened is fatal.
    if let Some(path) = &config.log_file {
        if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            println!(
                "Failed to open log file. Please ensure you specified a valid filepath and have permissions to \
                 create files in this directory. Error: {e}"
            );
            return ExitCode::FAILURE;
        }
    }
    let level = wrkz_wallet::logging::wallet_level(config.log_level);
    if let Err(e) = wrkz_wallet::logging::configure(level, config.log_file.as_deref().map(std::path::Path::new)) {
        println!("Failed to open log file: {e}");
        return ExitCode::FAILURE;
    }

    println!("{}\n", version_line());

    let serve_config = serve_config(&config);
    let address = format!("http://{}:{}", config.rpc_bind_ip, config.port);
    let no_console = config.no_console;
    let ipc_mode = wrkz_rpc::ipc::format_mode(config.rpc_ipc_mode);
    let notify_during_sync = config.notify_during_sync;

    let state = Arc::new(ApiState::new(config, api::real_daemon_factory()));

    // `ApiDispatcher.cpp:65-76`, with the hook described rather than printed
    // whole, since a command or a URL may carry a token.
    if let Some(notifier) = state.tx_notifier() {
        if notifier.enabled() {
            let kind = if notifier.is_webhook() { "webhook" } else { "command" };
            let when =
                if notify_during_sync { "including during sync" } else { "suppressed while far behind the daemon" };
            println!("Transaction notifications enabled ({kind} {}), {when}", notifier.describe());
        } else {
            println!("--tx-notify value is not usable, notifications disabled (--log-level 2 says why).");
        }
    }

    let mut server = match serve::start(Arc::clone(&state), serve_config) {
        Ok(server) => server,
        Err(e) => {
            // `ApiDispatcher::start`: "Failed to start API server." then exit(1).
            println!("Failed to start API server: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("Want documentation on how to use the wallet-api?\nSee https://turtlecoin.github.io/wallet-api-docs/\n");
    println!("The api has been launched on {address}.");

    // `WalletApi.cpp:75-89`, told from what actually bound.
    if let Some(addr6) = server.local_addr6() {
        println!("The api is also listening on http://{addr6}");
    }
    if let Some(e) = server.ipv6_error() {
        println!("Failed to start IPv6 API server on {e}.");
    }
    if let Some(path) = server.ipc_path() {
        println!("The api is also listening on the local {} (mode {ipc_mode}).", wrkz_rpc::ipc::describe(path));
    }
    if let Some(e) = server.ipc_error() {
        println!("Failed to start IPC API server: {e}");
    }

    // Ctrl-C, SIGTERM (systemd, `docker stop`) and SIGHUP end the program the
    // way `exit` does, so the wallet below is saved rather than lost.
    wrkz_rpc::signal::install();
    if !no_console {
        println!("Type exit to save and shutdown.");
        for line in wrkz_rpc::signal::stdin_lines() {
            match line.trim() {
                "exit" | "quit" => break,
                "help" => println!("Type exit to save and shutdown."),
                _ => {}
            }
        }
    } else {
        // Nothing to read; wait for a signal.
        wrkz_rpc::signal::wait_for_stop();
    }

    println!("\nSaving and shutting down...\n");

    // The listeners and the sync thread first, so nothing moves under the save;
    // then `closeWallet`'s save of whatever is still open.
    server.stop();
    if let Ok(guard) = state.wallet.read() {
        if let Some(open) = guard.as_ref() {
            if let Err(e) = open.save() {
                println!("Failed to save wallet: {e}");
            }
        }
    }
    wrkz_rpc::log::flush_file();

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Parsed {
        parse_arguments(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn the_rpc_password_is_required() {
        assert!(matches!(parse(&["--rpc-bind-ip", "0.0.0.0"]), Parsed::Error(e) if e.contains("rpc-password")));
        assert!(matches!(parse(&["-r", "x"]), Parsed::Run(_)));
        assert!(matches!(parse(&["--rpc-password=x"]), Parsed::Run(_)));
    }

    #[test]
    fn help_and_version_do_not_need_a_password() {
        assert!(matches!(parse(&["--help"]), Parsed::Help));
        assert!(matches!(parse(&["-h"]), Parsed::Help));
        assert!(matches!(parse(&["--version"]), Parsed::Version));
        assert!(matches!(parse(&["-v"]), Parsed::Version));
    }

    #[test]
    fn every_option_the_cpp_takes_is_accepted() {
        let args = [
            "--rpc-password",
            "x",
            "--rpc-bind-ip",
            "0.0.0.0",
            "--port",
            "8080",
            "--rpc-bind-ipv6-address",
            "::1",
            "--rpc-use-ipv6",
            "--rpc-ipc-path",
            "/run/w.sock",
            "--rpc-ipc-mode",
            "0660",
            "--rpc-ipc-group",
            "wrkz",
            "--tx-notify",
            "echo %s",
            "--notify-during-sync",
            "--enable-cors",
            "*",
            "--log-level",
            "4",
            "--log-file",
            "api.log",
            "--no-console",
            "--threads",
            "4",
            "--scan-coinbase-transactions",
        ];
        let Parsed::Run(config) = parse(&args) else { panic!("should parse") };
        assert_eq!(config.rpc_bind_ip, "0.0.0.0");
        assert_eq!(config.port, 8080);
        assert_eq!(config.rpc_bind_ipv6_address, "::1");
        assert!(config.rpc_use_ipv6);
        assert_eq!(config.rpc_ipc_path, "/run/w.sock");
        assert_eq!(config.rpc_ipc_mode, 0o660);
        assert_eq!(config.rpc_ipc_group, "wrkz");
        assert_eq!(config.tx_notify, "echo %s");
        assert!(config.notify_during_sync);
        assert_eq!(config.cors_header, "*");
        assert_eq!(config.log_level, 4);
        assert_eq!(config.log_file.as_deref(), Some("api.log"));
        assert!(config.no_console);
        assert_eq!(config.threads, 4);
        assert!(!config.skip_coinbase_transactions);

        // And they reach the listeners.
        let serve = serve_config(&config);
        assert_eq!(serve.bind, "0.0.0.0:8080");
        assert_eq!(serve.bind_ipv6, "[::1]:8080", "the IPv6 listener shares the port, as the C++'s does");
        assert_eq!(serve.ipc, Some(IpcConfig { path: "/run/w.sock".into(), mode: 0o660, group: "wrkz".into() }));
        assert!(serve.gzip);
    }

    #[test]
    fn the_ipv6_listener_needs_both_the_flag_and_an_address() {
        let Parsed::Run(config) = parse(&["-r", "x", "--rpc-bind-ipv6-address", "::1"]) else { panic!() };
        assert!(serve_config(&config).bind_ipv6.is_empty(), "an address alone is not enough");
        let Parsed::Run(config) = parse(&["-r", "x", "--rpc-use-ipv6"]) else { panic!() };
        assert!(serve_config(&config).bind_ipv6.is_empty(), "nor is the flag alone");
        assert!(serve_config(&config).ipc.is_none());
    }

    #[test]
    fn coinbases_are_scanned_unless_skipped() {
        let Parsed::Run(config) = parse(&["-r", "x"]) else { panic!("should parse") };
        assert!(!config.skip_coinbase_transactions);

        for flag in ["--skip-coinbase-transactions", "--skip-coinbase"] {
            let Parsed::Run(config) = parse(&["-r", "x", flag]) else { panic!("should parse") };
            assert!(config.skip_coinbase_transactions, "{flag}");
        }

        // Skip is the only one that changes anything, so it wins either way round.
        let Parsed::Run(config) = parse(&["-r", "x", "--skip-coinbase", "--scan-coinbase-transactions"]) else {
            panic!("should parse")
        };
        assert!(config.skip_coinbase_transactions);
    }

    #[test]
    fn the_defaults_are_the_cpp_defaults() {
        let Parsed::Run(config) = parse(&["-r", "x"]) else { panic!("should parse") };
        assert_eq!(config.rpc_bind_ip, "127.0.0.1");
        assert_eq!(config.port, 7856);
        assert_eq!(config.log_level, 0);
        assert!(wrkz_wallet::logging::wallet_level(config.log_level).is_none(), "0 is DISABLED");
        assert!(config.cors_header.is_empty());
        assert!(!config.no_console);
        assert_eq!(config.rpc_ipc_mode, 0o600);
        assert!(config.threads >= 1);
        assert!(config.tx_notify.is_empty());
        assert!(!config.notify_during_sync);
    }

    #[test]
    fn out_of_range_values_are_refused_the_way_the_cpp_refuses_them() {
        assert!(matches!(parse(&["-r", "x", "--log-level", "9"]), Parsed::Error(e) if e.contains("between 0 and 5")));
        assert!(matches!(parse(&["-r", "x", "--threads", "0"]), Parsed::Error(e) if e.contains("at least 1")));
        assert!(matches!(parse(&["--nonsense"]), Parsed::Error(e) if e.contains("nonsense")));
        assert!(matches!(
            parse(&["-r", "x", "--rpc-ipc-mode", "rw-"]),
            Parsed::Error(e) if e == "rpc-ipc-mode must be octal permissions such as 0600, got: rw-"
        ));
    }
}
