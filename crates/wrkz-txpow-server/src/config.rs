// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The command line: the options, defaults and checks of the C++
//! `TxPowServerConfig.cpp`, so an operator's existing command line or systemd
//! unit runs unchanged.

use crate::log::Level;

/// Everything the server is started with (`TxPowServerConfig`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Where to listen. An IPv6 literal works here too.
    pub bind_ip: String,
    pub bind_port: u16,
    /// A second listener on this IPv6 address, same port. Empty is none.
    pub bind_ipv6_address: String,
    /// Reverse proxies whose `X-Real-IP` / `X-Forwarded-For` are believed.
    pub trusted_proxies: Vec<String>,
    /// `Access-Control-Allow-Origin`. Empty sends no CORS headers.
    pub cors_header: String,
    /// When set, every request but `/health` must carry it as `X-API-KEY`.
    pub api_key: String,
    /// Hashing threads; `0` on the command line is one per hardware thread.
    pub threads: usize,
    /// Requests per minute from one client address; `0` is unlimited.
    pub rate_limit_per_minute: u32,
    /// Jobs accepted per minute across all clients; `0` is unlimited.
    pub max_jobs_per_minute: u32,
    /// Jobs waiting for the workers before new ones are refused.
    pub max_queue: u32,
    /// Anything harder is refused. Two inputs and six outputs need 66,000.
    pub max_difficulty: u64,
    /// The longest a client may ask a request to wait for its result.
    pub max_wait_ms: u32,
    /// A queued job older than this is dropped instead of solved.
    pub job_timeout_seconds: u32,
    /// How long a finished result stays available for polling.
    pub result_ttl_seconds: u32,
    pub log_level: Level,
    pub log_file: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind_ip: "127.0.0.1".into(),
            bind_port: 17870,
            bind_ipv6_address: String::new(),
            trusted_proxies: Vec::new(),
            cors_header: String::new(),
            api_key: String::new(),
            threads: 0,
            rate_limit_per_minute: 60,
            max_jobs_per_minute: 120,
            max_queue: 64,
            max_difficulty: 1_000_000,
            max_wait_ms: 30_000,
            job_timeout_seconds: 600,
            result_ttl_seconds: 300,
            log_level: Level::Info,
            log_file: None,
        }
    }
}

/// `options.help({})` of the C++, in its groups.
pub const USAGE: &str = "\
Usage: wrkz-txpow-server [OPTION...]

Core:
  -h, --help                    Display this help message
  -v, --version                 Output software version information
      --log-level <level>       One of trace, debug, info, warning, fatal, disabled (default: info)
      --log-file <file>         Also append log lines to <file>

Network:
      --bind-ip <ip>            Interface to listen on. Use 0.0.0.0 or :: to accept remote wallets
                                (default: 127.0.0.1)
      --bind-port #             TCP port to listen on (default: 17870)
      --bind-ipv6-address <ipv6>
                                Additional IPv6 address to listen on, same port. Empty disables it
      --trusted-proxy <ip>      Address of a reverse proxy in front of this server, e.g. 127.0.0.1 for a
                                local nginx. Requests from it are attributed to the client in X-Real-IP or
                                X-Forwarded-For. Repeat or comma-separate for several
      --enable-cors <domain>    Value for the Access-Control-Allow-Origin header, for the web wallet. Use *
                                for any origin
      --api-key <key>           Require this value in the X-API-KEY header on every request

Work:
      --threads #               Hashing threads. 0 uses one per hardware thread (default: 0)
      --rate-limit #            Requests per minute allowed from one client address. 0 disables the limit
                                (default: 60)
      --max-jobs-per-minute #   Jobs accepted per minute across all clients. 0 disables the limit
                                (default: 120)
      --max-queue #             Jobs allowed to wait for a free worker before new ones are refused
                                (default: 64)
      --max-difficulty #        Refuse jobs whose difficulty is above this (default: 1000000)
      --max-wait-ms #           Longest a request may be held open waiting for its result (default: 30000)
      --job-timeout #           Seconds after which an uncollected job is dropped (default: 600)
      --result-ttl #            Seconds a finished result stays available for polling (default: 300)
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Config),
    Help,
    Version,
    /// Exit with status 1 after printing `message`, and the usage when
    /// `show_usage` (an option the parser did not understand, as cxxopts).
    Error {
        message: String,
        show_usage: bool,
    },
}

fn parse_error(message: String) -> Parsed {
    Parsed::Error {
        message: format!("Error: Unable to parse command line argument options: {message}"),
        show_usage: true,
    }
}

fn invalid(message: impl Into<String>) -> Parsed {
    Parsed::Error { message: message.into(), show_usage: false }
}

/// Parse `args` (without the program name). `--name value` and `--name=value`
/// both work, as with cxxopts.
pub fn parse_arguments<I: IntoIterator<Item = String>>(args: I) -> Parsed {
    let mut config = Config::default();
    let (mut help, mut version) = (false, false);
    let mut log_level = "info".to_string();

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> Result<String, String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => args
                    .next()
                    .ok_or_else(|| format!("Option '{}' is missing an argument", name.trim_start_matches('-'))),
            }
        };
        macro_rules! number {
            ($field:expr) => {
                match value().and_then(|v| v.parse().map_err(|_| format!("Argument '{v}' failed to parse"))) {
                    Ok(n) => $field = n,
                    Err(e) => return parse_error(e),
                }
            };
        }
        macro_rules! text {
            ($field:expr) => {
                match value() {
                    Ok(v) => $field = v,
                    Err(e) => return parse_error(e),
                }
            };
        }

        match name.as_str() {
            "-h" | "--help" => help = true,
            "-v" | "--version" => version = true,
            "--log-level" => text!(log_level),
            "--log-file" => match value() {
                Ok(v) => config.log_file = Some(v),
                Err(e) => return parse_error(e),
            },
            "--bind-ip" => text!(config.bind_ip),
            "--bind-port" => number!(config.bind_port),
            "--bind-ipv6-address" => text!(config.bind_ipv6_address),
            "--trusted-proxy" => match value() {
                Ok(v) => config
                    .trusted_proxies
                    .extend(v.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string)),
                Err(e) => return parse_error(e),
            },
            "--enable-cors" => text!(config.cors_header),
            "--api-key" => text!(config.api_key),
            "--threads" => number!(config.threads),
            "--rate-limit" => number!(config.rate_limit_per_minute),
            "--max-jobs-per-minute" => number!(config.max_jobs_per_minute),
            "--max-queue" => number!(config.max_queue),
            "--max-difficulty" => number!(config.max_difficulty),
            "--max-wait-ms" => number!(config.max_wait_ms),
            "--job-timeout" => number!(config.job_timeout_seconds),
            "--result-ttl" => number!(config.result_ttl_seconds),
            other => return parse_error(format!("Option '{}' does not exist", other.trim_start_matches('-'))),
        }
    }

    if help {
        return Parsed::Help;
    }
    if version {
        return Parsed::Version;
    }

    match Level::parse(&log_level) {
        Some(level) => config.log_level = level,
        None => return invalid("--log-level must be one of trace, debug, info, warning, fatal, disabled"),
    }
    if config.threads == 0 {
        config.threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    }
    if config.max_queue == 0 {
        return invalid("--max-queue must be at least 1");
    }
    if config.max_difficulty == 0 {
        return invalid("--max-difficulty must be at least 1");
    }
    if config.bind_port == 0 {
        return invalid("--bind-port must be between 1 and 65535");
    }

    // An origin is `*`, `null` or scheme://host[:port]; browsers silently
    // ignore anything else, so a typo would only surface as a web wallet that
    // cannot connect. The accident worth catching is an unquoted `*`, which
    // the shell expands to the first file name in the directory.
    let cors = &config.cors_header;
    if !cors.is_empty() && cors != "*" && cors != "null" {
        if !(cors.starts_with("http://") || cors.starts_with("https://")) {
            return invalid(format!(
                "--enable-cors must be *, null, or a full origin such as https://web-wallet.example.com - got \
                 \"{cors}\"\nIf you meant any origin, quote it: --enable-cors '*'  (unquoted, the shell expands * \
                 to a filename)"
            ));
        }
        if cors.ends_with('/') {
            return invalid(format!(
                "--enable-cors must not have a trailing slash: an origin is scheme://host[:port], and browsers \
                 compare it verbatim - got \"{cors}\""
            ));
        }
    }

    Parsed::Run(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Parsed {
        parse_arguments(line.split_whitespace().map(str::to_string))
    }

    fn run(line: &str) -> Config {
        match parse(line) {
            Parsed::Run(c) => c,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn the_defaults_are_the_cpp_defaults() {
        let c = run("--threads 3");
        assert_eq!((c.bind_ip.as_str(), c.bind_port), ("127.0.0.1", 17870));
        assert_eq!((c.rate_limit_per_minute, c.max_jobs_per_minute, c.max_queue), (60, 120, 64));
        assert_eq!((c.max_difficulty, c.max_wait_ms), (1_000_000, 30_000));
        assert_eq!((c.job_timeout_seconds, c.result_ttl_seconds), (600, 300));
        assert_eq!((c.threads, c.log_level), (3, Level::Info));
        assert!(run("").threads >= 1, "0 becomes the hardware thread count");
    }

    #[test]
    fn the_documented_command_lines_parse() {
        let c = run("--bind-ip 0.0.0.0 --bind-port 17870 --threads 8");
        assert_eq!((c.bind_ip.as_str(), c.threads), ("0.0.0.0", 8));
        let c = run("--bind-ip=127.0.0.1 --trusted-proxy 127.0.0.1,10.0.0.2 --trusted-proxy ::1 --enable-cors * \
                     --api-key s3cret --log-level DEBUG --log-file pow.log --max-wait-ms=20000");
        assert_eq!(c.trusted_proxies, ["127.0.0.1", "10.0.0.2", "::1"]);
        assert_eq!((c.cors_header.as_str(), c.api_key.as_str()), ("*", "s3cret"));
        assert_eq!((c.log_level, c.log_file.as_deref(), c.max_wait_ms), (Level::Debug, Some("pow.log"), 20_000));
    }

    #[test]
    fn bad_lines_are_refused_as_the_cpp_refuses_them() {
        assert_eq!(parse("-h"), Parsed::Help);
        assert_eq!(parse("--version"), Parsed::Version);
        let refused = |line: &str, needle: &str| match parse(line) {
            Parsed::Error { message, .. } => assert!(message.contains(needle), "{line}: {message}"),
            other => panic!("{line}: {other:?}"),
        };
        refused("--frobnicate", "Option 'frobnicate' does not exist");
        refused("--threads many", "failed to parse");
        refused("--bind-port", "missing an argument");
        refused("--max-queue 0", "--max-queue must be at least 1");
        refused("--max-difficulty 0", "--max-difficulty must be at least 1");
        refused("--bind-port 0", "--bind-port must be between 1 and 65535");
        refused("--log-level loud", "--log-level must be one of");
        refused("--enable-cors 1.wallet", "quote it");
        refused("--enable-cors https://wallet.example.com/", "trailing slash");
        assert_eq!(run("--enable-cors https://rust-wallet.wrkz.work").cors_header, "https://rust-wallet.wrkz.work");
    }
}
