// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-service`: the JSON-RPC wallet service, option for option with the C++
//! `src/walletservice` where the option means the same thing here.
//!
//! See the crate documentation for the one difference that is not drop-in: the
//! container is the modern `WalletBackend` format, not WalletGreen's.

#![forbid(unsafe_code)]

use std::sync::Arc;

use wrkz_service::serve::{self, ServeConfig};
use wrkz_service::{ServiceConfig, ServiceState};
use wrkz_wallet::file::{SecretKey, Wallet};
use wrkz_wallet::listen::IpcConfig;
use zeroize::Zeroizing;

const USAGE: &str = "\
wrkz-service: the WrkzCoin JSON-RPC wallet service

  wrkz-service -w container.wallet -p PASSWORD --rpc-password SECRET
  wrkz-service -g -w new.wallet -p PASSWORD          # generate a container and exit

Core
  -h, --help                 this message
  -v, --version              version and commit

Daemon
  --daemon-address <ip>      daemon host (default 127.0.0.1). An absolute path,
                             an @name or an ipc://path connects over the
                             daemon's local IPC socket instead
  --daemon-port <port>       daemon RPC port (default 17856)
  --daemon-ssl               the daemon URL is https (needs the https feature)

Service
  -c, --config <file>        read settings from a key=value or JSON file
  --dump-config              print the effective settings and exit
  -l, --log-file <file>      log file (default service.log)
  --log-level <0-5>          0 fatal, 1 error, 2 warning, 3 info, 4 debug,
                             5 trace (default 3)

Wallet
  -w, --container-file <f>   wallet container file
  -p, --container-password   wallet container password
  -g, --generate-container   generate a new container and exit
  --view-key <key>           with -g: import this secret view key
  --spend-key <key>          with -g: import this secret spend key
  --mnemonic-seed <seed>     with -g: import this 25 word seed
  --scan-height <n>          with -g: start scanning at this height
  --address                  print the container's addresses and exit
  --skip-coinbase-transactions   do not scan coinbase outputs

Network
  --bind-address <ip>        interface for the RPC (default 127.0.0.1)
  --bind-port <port>         port for the RPC (default 7856)
  --bind-ipc-path <path>     serve the RPC on a local IPC socket at this path
                             instead of a TCP port, e.g.
                             /run/wrkz/wrkz-service.sock; @name for the Linux
                             abstract namespace. Not available on Windows
  --bind-ipc-mode <mode>     octal permissions for the socket file (default
                             0600); 0660 with --bind-ipc-group to share it
  --bind-ipc-group <group>   group to own the socket file

Notifications
  --tx-notify <cmd|url>      run a command, or POST to an http(s):// URL, for
                             every new wallet transaction. Placeholders: %s
                             hash, %h height (0 = unconfirmed), %a amount,
                             %f fee, %p payment id, %c confirmed 0/1
  --tx-confirmed-notify <cmd|url>
                             the same, once a transaction is in a block
  --notify-during-sync       also notify while the wallet is far behind the
                             daemon (default: suppressed)

RPC
  --rpc-password <password>  the password every request must carry in its
                             `password` member, on the port or the socket.
                             Required
  --rpc-legacy-security      no password at all. INSECURE; last resort
  --enable-cors <domain>     Access-Control-Allow-Origin; * for all

Containers from wrkz-wallet, wrkz-wallet-api and the Pluton apps open as they
are. A WalletGreen container written by the C++ wrkz-service does not: convert
it with the C++ wrkz-walletupgrader first.
";

/// `WalletServiceConfiguration::logFile` (`WalletServiceConfiguration.h:51`):
/// the C++ service always writes a log, here unless `-l` says otherwise.
const DEFAULT_LOG_FILE: &str = "service.log";

/// What the command line and the configuration file add up to.
struct Args {
    cfg: ServiceConfig,
    help: bool,
    version: bool,
    dump_config: bool,
    generate: bool,
    print_addresses: bool,
    view_key: Option<String>,
    spend_key: Option<String>,
    mnemonic_seed: Option<String>,
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("wrkz-service: {message}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = parse(&argv)?;

    if args.help {
        print!("{USAGE}");
        return Ok(0);
    }
    if args.version {
        println!("wrkz-service {}{}", env!("CARGO_PKG_VERSION"), commit_suffix());
        return Ok(0);
    }
    if args.dump_config {
        print!("{}", dump(&args.cfg));
        return Ok(0);
    }

    if args.cfg.container_file.is_empty() {
        return Err("no container: pass --container-file (-w)".into());
    }
    if args.cfg.container_password.is_empty() {
        return Err("no container password: pass --container-password (-p)".into());
    }
    if args.generate {
        let wallet = generate(&mut args)?;
        wallet
            .save(&args.cfg.container_file, &args.cfg.container_password)
            .map_err(|e| format!("cannot write {}: {e}", args.cfg.container_file))?;
        for address in wallet.addresses() {
            println!("{address}");
        }
        println!("container written to {}", args.cfg.container_file);
        return Ok(0);
    }

    let wallet = Wallet::open(&args.cfg.container_file, &args.cfg.container_password)
        .map_err(|e| format!("cannot open {}: {e}", args.cfg.container_file))?;

    if args.print_addresses {
        for address in wallet.addresses() {
            println!("{address}");
        }
        return Ok(0);
    }

    check_rpc_password(&args)?;

    // `HttpServer::startIpc` throws on a platform without IPC sockets, which
    // ends the service; so does this, before anything is opened.
    if !args.cfg.bind_ipc_path.is_empty() && !wrkz_rpc::ipc::supported() {
        return Err(format!("--bind-ipc-path: {}", wrkz_rpc::ipc::UNSUPPORTED));
    }

    start_logging(&args.cfg)?;

    let open = wrkz_wallet::api::open_container(
        wallet,
        args.cfg.container_file.clone(),
        Zeroizing::new(args.cfg.container_password.clone()),
        args.cfg.daemon_address.clone(),
        args.cfg.daemon_port,
        args.cfg.daemon_ssl,
        args.cfg.skip_coinbase_transactions,
        &wrkz_service::real_daemon_factory(),
    )
    .map_err(|e| format!("cannot reach the daemon: {e}"))?;
    log_info(format_args!("Opened container {}", args.cfg.container_file));

    let serve_config = serve_config(&args.cfg);
    let where_ = match &serve_config.ipc {
        Some(ipc) => {
            format!("the local {} (mode {})", wrkz_rpc::ipc::describe(&ipc.path), wrkz_rpc::ipc::format_mode(ipc.mode))
        }
        None => serve_config.bind.clone(),
    };
    let state = Arc::new(ServiceState::new(args.cfg, open));
    let mut running =
        serve::start(Arc::clone(&state), serve_config).map_err(|e| format!("cannot serve on {where_}: {e}"))?;

    // `PaymentGateService.cpp:205`.
    match (running.local_addr(), running.ipc_path()) {
        (Some(addr), _) => println!("wrkz-service listening on http://{addr}/json_rpc"),
        (None, Some(_)) => println!("wrkz-service listening on {where_}, path /json_rpc"),
        (None, None) => {}
    }
    log_info(format_args!("JSON-RPC server started on {where_}"));

    // The same shutdown `wrkz-wallet-api` has: `exit` at a terminal, and with
    // no terminal (systemd, a container) a wait. Ctrl-C, SIGTERM and SIGHUP end
    // either the way `exit` does, so the container is saved below.
    wrkz_rpc::signal::install();
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!("Type exit to save and shut down.");
        for line in wrkz_rpc::signal::stdin_lines() {
            match line.trim() {
                "exit" | "quit" | "stop" => break,
                "save" => match state.read().save() {
                    Ok(()) => println!("saved"),
                    Err(e) => println!("save failed: {e}"),
                },
                "status" => {
                    let open = state.read();
                    let s = open.sync.sync_status();
                    println!(
                        "wallet {} / daemon {} / network {}, {} peers",
                        s.wallet_block_count, s.local_daemon_block_count, s.network_block_count, open.peer_count
                    );
                }
                "help" => println!("exit, save, status"),
                other if !other.is_empty() => println!("unknown command: {other}"),
                _ => {}
            }
        }
    } else {
        while !state.stopping.load(std::sync::atomic::Ordering::SeqCst) && !wrkz_rpc::signal::stop_requested() {
            std::thread::sleep(wrkz_rpc::signal::POLL);
        }
    }

    println!("saving and shutting down");
    log_info(format_args!("JSON-RPC server stopped, stopping wallet service..."));
    running.stop();
    let saved = state.read().save();
    wrkz_rpc::log::flush_file();
    if let Err(e) = saved {
        eprintln!("wrkz-service: the container did not save cleanly: {e}");
        return Ok(1);
    }
    Ok(0)
}

fn log_info(args: std::fmt::Arguments<'_>) {
    wrkz_wallet::logging::log(wrkz_rpc::log::Level::Info, args);
}

/// `PaymentGateService::init` (`PaymentGateService.cpp:69-91`): the level in the
/// C++ `Logging::Level` numbering, and a log file that is always written and
/// must open.
fn start_logging(cfg: &ServiceConfig) -> Result<(), String> {
    let path = cfg.log_file.clone().unwrap_or_else(|| DEFAULT_LOG_FILE.to_string());
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("couldn't open log file {path}: {e}"))?;
    let level = wrkz_wallet::logging::service_level(cfg.log_level);
    wrkz_wallet::logging::configure(level, Some(std::path::Path::new(&path)))
        .map_err(|e| format!("couldn't open log file {path}: {e}"))
}

/// Where to listen: the socket instead of the port when `--bind-ipc-path` is
/// given (`PaymentGateService.cpp:203-217`).
fn serve_config(cfg: &ServiceConfig) -> ServeConfig {
    if cfg.bind_ipc_path.is_empty() {
        return ServeConfig { bind: format!("{}:{}", cfg.bind_address, cfg.bind_port), ..Default::default() };
    }
    ServeConfig {
        bind: String::new(),
        ipc: Some(IpcConfig {
            path: cfg.bind_ipc_path.clone(),
            mode: cfg.bind_ipc_mode,
            group: cfg.bind_ipc_group.clone(),
        }),
        ..Default::default()
    }
}

/// Whether this run will listen on a socket, and so needs the RPC secret.
///
/// `-g` writes a container and `--address` prints one; both exit without ever
/// serving, and demanding the secret to do either would be asking for
/// something the run has no use for. Kept as a function rather than as the
/// order of two `if`s, so that moving the checks around cannot quietly
/// reintroduce it.
fn will_serve(args: &Args) -> bool {
    !args.generate && !args.print_addresses && !args.help && !args.version && !args.dump_config
}

/// `--rpc-password` is required of a run that will listen, unless
/// `--rpc-legacy-security` says to serve without one.
fn check_rpc_password(args: &Args) -> Result<(), String> {
    if will_serve(args) && args.cfg.rpc_password.is_empty() && !args.cfg.legacy_security {
        return Err(
            "no RPC password: pass --rpc-password, or --rpc-legacy-security to serve without one (insecure)".into()
        );
    }
    Ok(())
}

/// The commit a build from a git checkout was made at, as `wrkz-node` reports it.
fn commit_suffix() -> String {
    match option_env!("WRKZ_GIT_COMMIT") {
        Some(commit) if !commit.is_empty() => format!(" ({commit})"),
        _ => String::new(),
    }
}

/// `-g`: a fresh container, or one restored from a seed or a key pair.
fn generate(args: &mut Args) -> Result<Wallet, String> {
    wrkz_wallet::file::check_new_wallet_filename(&args.cfg.container_file)
        .map_err(|e| format!("{}: {e}", args.cfg.container_file))?;
    let height = args.cfg.scan_height;
    match (args.mnemonic_seed.take(), args.spend_key.take(), args.view_key.take()) {
        (Some(seed), _, _) => Wallet::import_from_mnemonic(&seed, height).map_err(|e| format!("{e}")),
        (None, Some(spend), Some(view)) => {
            let spend = SecretKey::from_hex(&spend).ok_or("--spend-key is not 64 hex characters")?;
            let view = SecretKey::from_hex(&view).ok_or("--view-key is not 64 hex characters")?;
            Wallet::import_from_keys(&spend, &view, height).map_err(|e| format!("{e}"))
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            Err("importing from keys needs both --spend-key and --view-key".into())
        }
        // A brand new container starts at the tip: there is nothing behind it.
        (None, None, None) => Wallet::create_new(0).map_err(|e| format!("{e}")),
    }
}

/// The settings, in the C++'s `asString` order, as `key=value` lines that
/// `--config` reads back.
fn dump(cfg: &ServiceConfig) -> String {
    let mut out = String::new();
    let mut line = |k: &str, v: String| {
        out.push_str(k);
        out.push('=');
        out.push_str(&v);
        out.push('\n');
    };
    line("daemon-address", cfg.daemon_address.clone());
    line("daemon-port", cfg.daemon_port.to_string());
    line("bind-address", cfg.bind_address.clone());
    line("bind-port", cfg.bind_port.to_string());
    line("bind-ipc-path", cfg.bind_ipc_path.clone());
    line("bind-ipc-mode", wrkz_rpc::ipc::format_mode(cfg.bind_ipc_mode));
    line("bind-ipc-group", cfg.bind_ipc_group.clone());
    line("container-file", cfg.container_file.clone());
    // The two passwords are deliberately not printed: `--dump-config` is a
    // thing operators paste into issues.
    line("enable-cors", cfg.cors_header.clone());
    line("rpc-legacy-security", cfg.legacy_security.to_string());
    line("log-file", cfg.log_file.clone().unwrap_or_else(|| DEFAULT_LOG_FILE.into()));
    line("log-level", cfg.log_level.to_string());
    line("tx-notify", cfg.tx_notify.clone());
    line("tx-confirmed-notify", cfg.tx_confirmed_notify.clone());
    line("notify-during-sync", cfg.notify_during_sync.to_string());
    line("scan-height", cfg.scan_height.to_string());
    line("skip-coinbase-transactions", cfg.skip_coinbase_transactions.to_string());
    out
}

/// The keys that are switches. The C++ reads the text form's value as on when
/// it starts with `1` (`WalletServiceConfiguration.cpp:475`, `:516`) and the
/// JSON form's as a boolean; `true` is taken too, since that is what
/// `--dump-config` writes.
const SWITCHES: &[&str] = &[
    "rpc-legacy-security",
    "notify-during-sync",
    "daemon-ssl",
    "skip-coinbase-transactions",
    "skip-coinbase",
    "scan-coinbase-transactions",
];

/// `--config <file>`: `key=value` a line and the same key names
/// `--dump-config` writes. A line is a comment when its first non-blank
/// character is `#` or `;`, the C++'s rule (`WalletServiceConfiguration.cpp:379`),
/// so a command or a URL may contain either. A JSON object of the same keys
/// is accepted too, which is what the C++ `--save-config` produces.
fn read_config(path: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut argv = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') || line == "{" || line == "}" {
            continue;
        }
        let Some((key, value)) = config_item(line) else {
            return Err(format!("{path}: cannot read the line: {raw}"));
        };

        let switch = SWITCHES.contains(&key.as_str()) || value == "true" || value == "false";
        if switch {
            if value.starts_with('1') || value.eq_ignore_ascii_case("true") {
                argv.push(format!("--{key}"));
            }
            continue;
        }
        argv.push(format!("--{key}"));
        argv.push(value);
    }
    Ok(argv)
}

/// One `key=value` line, or one `"key": value,` line of a JSON object, whose
/// string value is unquoted and unescaped. A `key=value` value is taken as it
/// is written, quotes included, since a command template is made of them.
fn config_item(line: &str) -> Option<(String, String)> {
    if let Some(rest) = line.strip_prefix('"') {
        let (key, rest) = rest.split_once('"')?;
        let value = rest.trim_start().strip_prefix(':')?.trim().trim_end_matches(',').trim_end();
        let value = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(quoted) => quoted.replace("\\\"", "\"").replace("\\\\", "\\"),
            None => value.to_string(),
        };
        return Some((key.to_string(), value));
    }
    let (key, value) = line.split_once(['=', ':'])?;
    Some((key.trim().to_string(), value.trim().to_string()))
}

/// The command line, with `--config` folded in first so a flag on the command
/// line wins over the same key in the file — the C++'s precedence.
fn parse(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        cfg: ServiceConfig::default(),
        help: false,
        version: false,
        dump_config: false,
        generate: false,
        print_addresses: false,
        view_key: None,
        spend_key: None,
        mnemonic_seed: None,
    };

    // The file first, then the command line over it.
    let mut all: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "-c" || argv[i] == "--config" {
            let path = argv.get(i + 1).ok_or("--config needs a file")?;
            all.extend(read_config(path)?);
            i += 2;
            continue;
        }
        i += 1;
    }
    all.extend_from_slice(argv);

    let mut i = 0;
    let value = |i: usize, name: &str| -> Result<String, String> {
        all.get(i + 1).cloned().ok_or_else(|| format!("{name} needs a value"))
    };
    let number = |text: &str, name: &str| -> Result<u64, String> {
        text.parse::<u64>().map_err(|_| format!("{name}: {text} is not a number"))
    };

    while i < all.len() {
        let arg = all[i].as_str();
        let mut took = 1;
        match arg {
            "-h" | "--help" => args.help = true,
            "-v" | "--version" => args.version = true,
            "--dump-config" => args.dump_config = true,
            "-g" | "--generate-container" => args.generate = true,
            "--address" => args.print_addresses = true,
            "--rpc-legacy-security" => args.cfg.legacy_security = true,
            "--daemon-ssl" => args.cfg.daemon_ssl = true,
            "--notify-during-sync" => args.cfg.notify_during_sync = true,
            "--skip-coinbase-transactions" | "--skip-coinbase" => args.cfg.skip_coinbase_transactions = true,
            // Accepted and ignored, as in `wrkz-wallet-api`: coinbases are
            // scanned by default here.
            "--scan-coinbase-transactions" => {}
            "-c" | "--config" => took = 2,
            "-w" | "--container-file" => {
                args.cfg.container_file = value(i, arg)?;
                took = 2;
            }
            "-p" | "--container-password" => {
                args.cfg.container_password = value(i, arg)?;
                took = 2;
            }
            "--rpc-password" => {
                args.cfg.rpc_password = value(i, arg)?;
                took = 2;
            }
            "--daemon-address" => {
                args.cfg.daemon_address = value(i, arg)?;
                took = 2;
            }
            "--daemon-port" => {
                args.cfg.daemon_port = number(&value(i, arg)?, arg)? as u16;
                took = 2;
            }
            "--bind-address" => {
                args.cfg.bind_address = value(i, arg)?;
                took = 2;
            }
            "--bind-port" => {
                args.cfg.bind_port = number(&value(i, arg)?, arg)? as u16;
                took = 2;
            }
            "--bind-ipc-path" => {
                args.cfg.bind_ipc_path = value(i, arg)?;
                took = 2;
            }
            // `WalletServiceConfiguration.cpp:264`.
            "--bind-ipc-mode" => {
                let mode = value(i, arg)?;
                args.cfg.bind_ipc_mode = wrkz_rpc::ipc::parse_mode(&mode)
                    .ok_or_else(|| format!("bind-ipc-mode must be octal permissions such as 0600: {mode}"))?;
                took = 2;
            }
            "--bind-ipc-group" => {
                args.cfg.bind_ipc_group = value(i, arg)?;
                took = 2;
            }
            "--tx-notify" => {
                args.cfg.tx_notify = value(i, arg)?;
                took = 2;
            }
            "--tx-confirmed-notify" => {
                args.cfg.tx_confirmed_notify = value(i, arg)?;
                took = 2;
            }
            "--enable-cors" => {
                args.cfg.cors_header = value(i, arg)?;
                took = 2;
            }
            "-l" | "--log-file" => {
                args.cfg.log_file = Some(value(i, arg)?);
                took = 2;
            }
            "--log-level" => {
                let text = value(i, arg)?;
                args.cfg.log_level = text.parse::<i32>().map_err(|_| format!("{arg}: {text} is not a number"))?;
                took = 2;
            }
            "--scan-height" => {
                args.cfg.scan_height = number(&value(i, arg)?, arg)?;
                took = 2;
            }
            "--view-key" => {
                args.view_key = Some(value(i, arg)?);
                took = 2;
            }
            "--spend-key" => {
                args.spend_key = Some(value(i, arg)?);
                took = 2;
            }
            "--mnemonic-seed" => {
                args.mnemonic_seed = Some(value(i, arg)?);
                took = 2;
            }
            other => return Err(format!("unknown option {other} (try --help)")),
        }
        i += took;
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(name: &str, text: &str) -> String {
        let dir = std::env::temp_dir().join(format!("wrkz-service-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    #[test]
    fn the_command_line_wins_over_the_configuration_file() {
        let path = temp_config(
            "service.conf",
            "bind-port=1234\ndaemon-address=10.0.0.1\n# a comment\nrpc-legacy-security=true\n",
        );
        let argv = vec!["--config".to_string(), path.clone(), "--bind-port".into(), "9999".into()];
        let args = parse(&argv).expect("parses");
        assert_eq!(args.cfg.bind_port, 9999, "the command line wins");
        assert_eq!(args.cfg.daemon_address, "10.0.0.1", "and the file still supplies the rest");
        assert!(args.cfg.legacy_security);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_dumped_configuration_reads_back_to_itself() {
        let mut cfg = ServiceConfig { bind_port: 4321, daemon_port: 1234, ..ServiceConfig::default() };
        cfg.container_file = "some.wallet".into();
        cfg.cors_header = "*".into();
        cfg.bind_ipc_path = "/run/wrkz/wrkz-service.sock".into();
        cfg.bind_ipc_mode = 0o660;
        cfg.bind_ipc_group = "wrkz".into();
        cfg.tx_notify = r#"/usr/local/bin/on-tx "%s" --amount=%a #tag"#.into();
        cfg.tx_confirmed_notify = "http://127.0.0.1:9000/confirmed?key=a:b".into();
        cfg.notify_during_sync = true;
        cfg.log_level = 4;
        let dumped = dump(&cfg);
        let path = temp_config("dumped.conf", &dumped);
        let args = parse(&["--config".to_string(), path.clone()]).expect("reads back");
        assert_eq!(dump(&args.cfg), dumped);
        assert_eq!(args.cfg.tx_notify, cfg.tx_notify, "quotes, `=`, `#` and all");
        assert_eq!(args.cfg.bind_ipc_mode, 0o660);
        assert!(args.cfg.notify_during_sync);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_new_option_is_taken_from_the_command_line() {
        let argv: Vec<String> = [
            "--bind-ipc-path",
            "@wrkz-service",
            "--bind-ipc-mode",
            "660",
            "--bind-ipc-group",
            "wrkz",
            "--tx-notify",
            "notify %s",
            "--tx-confirmed-notify",
            "https://hooks.example/confirmed",
            "--notify-during-sync",
            "-l",
            "wallet.log",
            "--log-level",
            "5",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let args = parse(&argv).expect("parses");
        assert_eq!(args.cfg.bind_ipc_path, "@wrkz-service");
        assert_eq!(args.cfg.bind_ipc_mode, 0o660);
        assert_eq!(args.cfg.bind_ipc_group, "wrkz");
        assert_eq!(args.cfg.tx_notify, "notify %s");
        assert_eq!(args.cfg.tx_confirmed_notify, "https://hooks.example/confirmed");
        assert!(args.cfg.notify_during_sync);
        assert_eq!(args.cfg.log_file.as_deref(), Some("wallet.log"));
        assert_eq!(args.cfg.log_level, 5);

        // The socket replaces the port, as in the C++.
        let serve = serve_config(&args.cfg);
        assert!(serve.bind.is_empty());
        assert_eq!(serve.ipc, Some(IpcConfig { path: "@wrkz-service".into(), mode: 0o660, group: "wrkz".into() }));
        let port = serve_config(&ServiceConfig::default());
        assert_eq!(port.bind, "127.0.0.1:7856");
        assert!(port.ipc.is_none());
    }

    #[test]
    fn a_mode_that_is_not_octal_permissions_is_refused() {
        let argv = vec!["--bind-ipc-mode".to_string(), "rw-".to_string()];
        let e = parse(&argv).err().expect("refused");
        assert_eq!(e, "bind-ipc-mode must be octal permissions such as 0600: rw-");
    }

    #[test]
    fn switches_and_comments_read_the_way_the_cpp_reads_them() {
        let path = temp_config(
            "switches.conf",
            "  # an indented comment\n; another\nnotify-during-sync=1\nrpc-legacy-security=0\n\
             tx-notify=notify #%s ;%h\n",
        );
        let args = parse(&["--config".to_string(), path.clone()]).expect("parses");
        assert!(args.cfg.notify_during_sync, "`1` is on");
        assert!(!args.cfg.legacy_security, "`0` is off");
        assert_eq!(args.cfg.tx_notify, "notify #%s ;%h", "a `#` inside a value is not a comment");
        let _ = std::fs::remove_file(&path);

        let path = temp_config(
            "saved.json",
            "{\n  \"notify-during-sync\": true,\n  \"tx-notify\": \"notify \\\"%s\\\"\",\n  \"bind-port\": 7000\n}\n",
        );
        let args = parse(&["--config".to_string(), path.clone()]).expect("the JSON form parses");
        assert!(args.cfg.notify_during_sync);
        assert_eq!(args.cfg.tx_notify, "notify \"%s\"");
        assert_eq!(args.cfg.bind_port, 7000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_log_level_is_numbered_and_defaulted_as_the_cpp_services() {
        let args = parse(&[]).expect("parses");
        assert_eq!(args.cfg.log_level, 3, "Logging::INFO");
        assert_eq!(wrkz_wallet::logging::service_level(args.cfg.log_level), Some(wrkz_rpc::log::Level::Info));
        assert!(dump(&args.cfg).contains("log-file=service.log\n"));
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_ignored() {
        assert!(parse(&["--not-a-thing".to_string()]).is_err());
    }

    #[test]
    fn only_a_run_that_listens_needs_the_rpc_password() {
        let serving = parse(&["-w".into(), "w".into(), "-p".into(), "pw".into()]).unwrap();
        assert!(will_serve(&serving));
        assert!(check_rpc_password(&serving).is_err(), "serving without a secret is refused");

        // `-g` writes a container and exits; `--address` prints one and exits.
        for flag in ["-g", "--address"] {
            let args = parse(&[flag.to_string(), "-w".into(), "w".into(), "-p".into(), "pw".into()]).unwrap();
            assert!(!will_serve(&args), "{flag} does not listen");
            assert!(check_rpc_password(&args).is_ok(), "{flag} must not demand an RPC secret it never uses");
        }

        // And the escape hatch still works for a run that does listen.
        let legacy =
            parse(&["-w".into(), "w".into(), "-p".into(), "pw".into(), "--rpc-legacy-security".into()]).unwrap();
        assert!(will_serve(&legacy));
        assert!(check_rpc_password(&legacy).is_ok());
    }
}
