// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-wallet` — the interactive wallet, command for command with the C++
//! `zedwallet++` (`src/zedwallet++/`).
//!
//! ```text
//! wrkz-wallet [-w mine] [-p secret] [-r node-fin.wrkz.work:17856]
//!             [--log-level 2] [--log-file wallet.log]
//!             [--threads 4] [--skip-coinbase-transactions]
//! ```
//!
//! The interface itself is [`wrkz_wallet::cli`]; this binary is the argument
//! parser and the standard-input terminal.
//!
//! Exit codes: `0` on a clean exit, `1` for a bad argument.

use std::process::ExitCode;

use wrkz_wallet::cli::{self, term, ZedConfig};

/// The version line, in the shape `wrkz-node --version` prints
/// (`version_line` in `crates/wrkz-node/src/bin/node.rs`).
///
/// `WRKZ_GIT_COMMIT` is set at compile time by `build.rs` and is absent
/// outside a git checkout.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: Option<&str> = option_env!("WRKZ_GIT_COMMIT");

fn version_line() -> String {
    match GIT_COMMIT {
        Some(commit) if !commit.is_empty() => {
            format!("wrkz-wallet {VERSION} ({commit}), wallet compatible with WrkzCoin {}", wrkz_rpc::DAEMON_VERSION)
        }
        _ => format!("wrkz-wallet {VERSION}, wallet compatible with WrkzCoin {}", wrkz_rpc::DAEMON_VERSION),
    }
}

/// `options.help({})` of `zedwallet++/ParseArguments.cpp:40`.
const USAGE: &str = "\
Usage: wrkz-wallet [OPTION...]

Core:
  -h, --help                        Display this help message
  -v, --version                     Output software version information

Daemon:
  -r, --remote-daemon <host:port>   The daemon host:port to use for node operations. For IPv6 use bracket
                                    notation, e.g. [::1]:17856 (default: 127.0.0.1:17856)
      --ssl                         Use SSL when connecting to the daemon (https://). An IPC socket is never
                                    asked about TLS.

Wallet:
  -w, --wallet-file <file>          Open the wallet <file>
  -p, --password <pass>             Use the password <pass> to open the wallet
      --log-level #                 Specify log level: 0 disabled, 1 fatal, 2 warning, 3 info, 4 debug,
                                    5 trace (default: 1)
      --log-file <file>             Specify filepath to log to. Logging to file is disabled by default
      --threads #                   Specify number of wallet sync threads (default: one per core, at most 16)
      --skip-coinbase-transactions  Do not scan miner/coinbase transactions (alias: --skip-coinbase). Syncs
                                    faster, but block rewards paid to this wallet are not seen
      --scan-coinbase-transactions  Scan miner/coinbase transactions (the default; kept for compatibility)
";

enum Parsed {
    Run(ZedConfig),
    Help,
    Version,
    Error(String),
}

fn parse_arguments<I: Iterator<Item = String>>(args: I) -> Parsed {
    let mut config = ZedConfig::default();
    let mut remote_daemon = String::new();
    let mut help = false;
    let mut version = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
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
            "--ssl" => config.ssl = true,
            "--skip-coinbase-transactions" | "--skip-coinbase" => config.skip_coinbase_transactions = true,
            // Coinbases are scanned by default; kept so old command lines run.
            "--scan-coinbase-transactions" => {}

            "-r" | "--remote-daemon" => match value(&name) {
                Ok(v) => remote_daemon = v,
                Err(e) => return Parsed::Error(e),
            },
            "-w" | "--wallet-file" => match value(&name) {
                Ok(v) => {
                    config.wallet_file = v;
                    config.wallet_given = true;
                }
                Err(e) => return Parsed::Error(e),
            },
            // An empty password is valid, so what matters is that the option
            // appeared at all (`ParseArguments.cpp:130`).
            "-p" | "--password" => match value(&name) {
                Ok(v) => {
                    config.wallet_pass = v;
                    config.pass_given = true;
                }
                Err(e) => return Parsed::Error(e),
            },
            "--log-level" => {
                match value(&name).and_then(|v| v.parse::<i32>().map_err(|_| format!("{v} is not a number"))) {
                    Ok(level) => config.log_level = level,
                    Err(e) => return Parsed::Error(e),
                }
            }
            "--log-file" => match value(&name) {
                Ok(v) => config.log_file = Some(v),
                Err(e) => return Parsed::Error(e),
            },
            "--threads" => {
                match value(&name).and_then(|v| v.parse::<u32>().map_err(|_| format!("{v} is not a number"))) {
                    Ok(threads) => config.threads = threads,
                    Err(e) => return Parsed::Error(e),
                }
            }

            other => {
                return Parsed::Error(format!(
                    "Error: Unable to parse command line argument options: unrecognised option '{other}'"
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

    if !(0..=5).contains(&config.log_level) {
        return Parsed::Error("Log level must be between 0 and 5!".into());
    }

    if config.threads == 0 {
        return Parsed::Error("Thread count must be at least 1".into());
    }

    if !remote_daemon.is_empty() {
        if wrkz_wallet::ipc::is_ipc_address(&remote_daemon) {
            // `ParseArguments.cpp:170`: without this the path would be handed
            // to the resolver as a hostname and fail later with a connection
            // error that says nothing useful.
            if let Some(reason) = wrkz_wallet::ipc::unsupported_reason(&remote_daemon) {
                return Parsed::Error(format!("--remote-daemon names an IPC socket, but {reason}."));
            }
            config.host = remote_daemon;
        } else {
            match cli::prompt::parse_daemon_address(&remote_daemon) {
                Some((host, port)) => {
                    config.host = host;
                    config.port = port;
                }
                None => return Parsed::Error("There was an error parsing the --remote-daemon you specified".into()),
            }
        }
    }

    // `https` is on by default; only `--no-default-features` can land here.
    if config.ssl && !wrkz_wallet::daemon::HTTPS_SUPPORTED {
        return Parsed::Error("--ssl needs a build with TLS: this one was built with --no-default-features.".into());
    }

    Parsed::Run(config)
}

fn main() -> ExitCode {
    let config = match parse_arguments(std::env::args().skip(1)) {
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

    // `ParseArguments.cpp:142`: a log file that cannot be opened is fatal.
    if let Some(path) = &config.log_file {
        if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            println!(
                "Failed to open log file. Please ensure you specified a valid filepath and have permissions to \
                 create files in this directory. Error: {e}"
            );
            return ExitCode::FAILURE;
        }
    }
    // `ZedWallet.cpp:82-101`.
    let level = wrkz_wallet::logging::wallet_level(config.log_level);
    if let Err(e) = wrkz_wallet::logging::configure(level, config.log_file.as_deref().map(std::path::Path::new)) {
        println!("Failed to open log file: {e}");
        return ExitCode::FAILURE;
    }

    // ANSI colour, the way `ColouredMsg` paints. Off when the output is
    // redirected, so a captured log is plain.
    term::set_colour(std::env::var_os("NO_COLOR").is_none());

    println!("{}\n", version_line());

    let mut terminal = term::StdTerminal::default();
    let code = cli::run(&mut terminal, &config);
    wrkz_rpc::log::flush_file();

    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Parsed {
        parse_arguments(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_is_valid_and_shows_the_menu() {
        let Parsed::Run(config) = parse(&[]) else { panic!("should parse") };
        assert!(!config.wallet_given);
        assert!(!config.pass_given);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 17856);
    }

    #[test]
    fn a_wallet_or_a_password_skips_the_menu() {
        let Parsed::Run(config) = parse(&["-w", "mine.wallet"]) else { panic!("should parse") };
        assert!(config.wallet_given);
        assert_eq!(config.wallet_file, "mine.wallet");

        // An empty password is a password, so `passGiven` still fires.
        let Parsed::Run(config) = parse(&["--password", ""]) else { panic!("should parse") };
        assert!(config.pass_given);
        assert_eq!(config.wallet_pass, "");
    }

    #[test]
    fn the_remote_daemon_is_split_into_host_and_port() {
        let Parsed::Run(config) = parse(&["-r", "node-fin.wrkz.work:17856"]) else { panic!("should parse") };
        assert_eq!(config.host, "node-fin.wrkz.work");
        assert_eq!(config.port, 17856);

        let Parsed::Run(config) = parse(&["--remote-daemon=[::1]:1234"]) else { panic!("should parse") };
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 1234);

        assert!(matches!(parse(&["-r", "host:nope"]), Parsed::Error(e) if e.contains("remote-daemon")));
    }

    #[test]
    fn an_ipc_remote_daemon_is_taken_whole_or_refused_with_a_reason() {
        let parsed = parse(&["-r", "/run/wrkz/daemon.sock"]);
        if wrkz_wallet::ipc::supported() {
            let Parsed::Run(config) = parsed else { panic!("should parse") };
            assert_eq!(config.host, "/run/wrkz/daemon.sock");
        } else {
            assert!(matches!(parsed, Parsed::Error(e) if e.contains("names an IPC socket")));
        }

        // `ipc://` and `@name` are the other two forms.
        assert!(wrkz_wallet::ipc::is_ipc_address("ipc:///run/w.sock"));
        assert!(wrkz_wallet::ipc::is_ipc_address("@wrkzd"));
    }

    #[test]
    fn ssl_is_available_in_the_default_build() {
        // The daemon prompt only asks "Does this daemon support SSL?" when the
        // build can honour a yes, so the default build has to be able to.
        const { assert!(wrkz_wallet::daemon::HTTPS_SUPPORTED, "the default build must include TLS") };
        let Parsed::Run(config) = parse(&["--ssl", "-r", "node.example:443"]) else { panic!("should parse") };
        assert!(config.ssl);
    }

    #[test]
    fn coinbases_are_scanned_unless_skipped() {
        let Parsed::Run(config) = parse(&[]) else { panic!("should parse") };
        assert!(!config.skip_coinbase_transactions);

        let Parsed::Run(config) = parse(&["--scan-coinbase-transactions"]) else { panic!("should parse") };
        assert!(!config.skip_coinbase_transactions);

        for flag in ["--skip-coinbase-transactions", "--skip-coinbase"] {
            let Parsed::Run(config) = parse(&[flag]) else { panic!("should parse") };
            assert!(config.skip_coinbase_transactions, "{flag}");
        }

        // Skip is the only one that changes anything, so it wins either way round.
        let Parsed::Run(config) = parse(&["--skip-coinbase", "--scan-coinbase-transactions"]) else {
            panic!("should parse")
        };
        assert!(config.skip_coinbase_transactions);
    }

    #[test]
    fn help_and_version_are_accepted() {
        assert!(matches!(parse(&["--help"]), Parsed::Help));
        assert!(matches!(parse(&["-v"]), Parsed::Version));
    }

    #[test]
    fn the_logging_and_thread_defaults_are_zedwalletpps() {
        let Parsed::Run(config) = parse(&[]) else { panic!("should parse") };
        assert_eq!(config.log_level, 1, "Logger::FATAL, `ZedConfig::logLevel`");
        assert_eq!(wrkz_wallet::logging::wallet_level(config.log_level), Some(wrkz_rpc::log::Level::Error));
        assert!(config.log_file.is_none());
        assert!(config.threads >= 1);

        let Parsed::Run(config) = parse(&["--log-level", "0", "--threads", "3"]) else { panic!("should parse") };
        assert!(wrkz_wallet::logging::wallet_level(config.log_level).is_none(), "0 is DISABLED");
        assert_eq!(config.threads, 3);
    }

    #[test]
    fn out_of_range_values_are_refused() {
        assert!(matches!(parse(&["--log-level", "6"]), Parsed::Error(e) if e.contains("between 0 and 5")));
        assert!(matches!(parse(&["--threads", "0"]), Parsed::Error(e) if e.contains("at least 1")));
        assert!(matches!(parse(&["--nope"]), Parsed::Error(e) if e.contains("nope")));
    }
}
