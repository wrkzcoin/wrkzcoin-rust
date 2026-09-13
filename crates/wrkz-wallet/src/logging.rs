// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `--log-level` and `--log-file` for `wrkz-wallet`, `wrkz-wallet-api` and
//! `wrkz-service`, onto the one process logger in [`wrkz_rpc::log`].
//!
//! The wallet programs do not grow a second logger: every line goes through
//! `wrkz_rpc::log`, with its timestamps, its level names, its capped and
//! rotated `--log-file` and its prompt handling. What this module adds is the
//! one thing that logger has no notion of — a level meaning *off* — and the
//! C++ numbering of each program's `--log-level`.
//!
//! # The numbering
//!
//! `zedwallet++` and `wallet-api` take `Logger::LogLevel`
//! (`src/logger/Logger.h:15`); the service takes `Logging::Level`
//! (`src/logging/ILogger.h:18`). They count in opposite directions from
//! different places, so each maps separately:
//!
//! | `--log-level` | `wrkz-wallet`, `wrkz-wallet-api` | `wrkz-service` |
//! | --- | --- | --- |
//! | 0 | `DISABLED`: nothing | `FATAL`: error |
//! | 1 | `FATAL`: error | `ERROR`: error |
//! | 2 | `WARNING`: warning | `WARNING`: warning |
//! | 3 | `INFO`: info | `INFO`: info |
//! | 4 | `DEBUG`: debug | `DEBUGGING`: debug |
//! | 5 | `TRACE`: trace | `TRACE`: trace |
//! | default | wallet `1`, API `0` | `3` |
//!
//! The defaults are the C++'s: `ZedConfig::logLevel = FATAL`
//! (`zedwallet++/ParseArguments.h:34`), `ApiConfig::logLevel = DISABLED`
//! (`walletapi/ParseArguments.h:44`) and `logLevel = Logging::INFO`
//! (`walletservice/WalletServiceConfiguration.h:60`). `wrkz_rpc::log` has one
//! error level where the C++ has `FATAL` and `ERROR`, so both land on it.
//!
//! # Where the lines go
//!
//! To **standard error**, and to the `--log-file` when one is given. The C++
//! wallets print log lines on standard output, in among the prompts, the
//! menus and — for `wrkz-wallet-api` — the request echo; here standard output
//! stays the program's own, as it is for `wrkz-node`. `--log-file` is appended
//! to, as the C++ appends, and in addition is capped
//! ([`wrkz_rpc::log::DEFAULT_MAX_FILE_BYTES`]) and rotated, which the C++ never
//! does.
//!
//! Until a program calls [`configure`], logging is **off**: the library used
//! from a test, or from Rust Pluton Wallet, writes nothing to anybody's
//! standard error.
//!
//! Nothing that logs through this module logs a secret: no key, no seed, no
//! password, no API key, no request body. The C++ `wrkz-wallet-api` prints the
//! expected RPC password next to a rejected one
//! (`ApiDispatcher.cpp:645`) and echoes every request body, keys included;
//! neither is reproduced.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wrkz_rpc::log::Level;

/// Whether any line is written at all. The level itself is `wrkz_rpc::log`'s.
static ON: AtomicBool = AtomicBool::new(false);

/// `Logger::LogLevel` (`src/logger/Logger.h:15`), as `wrkz-wallet` and
/// `wrkz-wallet-api` number it: `None` is `DISABLED`. Anything outside `0..=5`
/// is not a level, which both argument parsers refuse first.
pub fn wallet_level(number: i32) -> Option<Level> {
    match number {
        1 => Some(Level::Error),
        2 => Some(Level::Warn),
        3 => Some(Level::Info),
        4 => Some(Level::Debug),
        5 => Some(Level::Trace),
        _ => None,
    }
}

/// `Logging::Level` (`src/logging/ILogger.h:18`), as `wrkz-service` numbers
/// it. Past `TRACE` is still `TRACE`, and below `FATAL` is nothing, which is
/// what the C++'s `setMaxLevel` makes of them.
pub fn service_level(number: i32) -> Option<Level> {
    match number {
        i32::MIN..=-1 => None,
        0 | 1 => Some(Level::Error),
        2 => Some(Level::Warn),
        3 => Some(Level::Info),
        4 => Some(Level::Debug),
        _ => Some(Level::Trace),
    }
}

/// Set the level (`None` for off) and, when given, the file every line is
/// also appended to.
///
/// The file is created if it is missing, with its parent directory. A caller
/// that must refuse a path it cannot write — both C++ wallet programs exit
/// over one — checks before calling.
pub fn configure(level: Option<Level>, file: Option<&Path>) -> std::io::Result<()> {
    set_level(level);
    if let Some(path) = file {
        wrkz_rpc::log::set_file_at(path, wrkz_rpc::log::DEFAULT_MAX_FILE_BYTES)?;
    }
    Ok(())
}

/// Change the level while running: `set_log_level` at the wallet prompt.
pub fn set_level(level: Option<Level>) {
    match level {
        Some(level) => {
            wrkz_rpc::log::set_level(level);
            ON.store(true, Ordering::Relaxed);
        }
        None => ON.store(false, Ordering::Relaxed),
    }
}

/// Whether a line at `level` would be written, so a caller can skip building
/// one that would not.
pub fn enabled(level: Level) -> bool {
    ON.load(Ordering::Relaxed) && wrkz_rpc::log::enabled(level)
}

/// Write one line at `level`, if logging is on and the level passes.
pub fn log(level: Level, args: std::fmt::Arguments<'_>) {
    if enabled(level) {
        wrkz_rpc::log::log(level, args);
    }
}

/// A notification hook's sink ([`wrkz_rpc::notify::Options::log`]) that
/// respects this module's off switch.
pub fn notifier_log() -> wrkz_rpc::notify::LogFn {
    Arc::new(|level: Level, line: &str| log(level, format_args!("{line}")))
}

/// The webhook client for an `https://` hook: `ureq` with TLS, no redirects,
/// and the hook's timeout on the connection, the read and the write. `None` in
/// a build without TLS, where the hook then disables itself with the C++'s
/// warning.
pub fn notifier_post() -> Option<wrkz_rpc::notify::PostFn> {
    if !crate::daemon::HTTPS_SUPPORTED {
        return None;
    }
    Some(Arc::new(|url: &str, body: &str, timeout: std::time::Duration| {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .redirects(0)
            .build();
        match agent.post(url).set("Content-Type", "application/json").send_string(body) {
            Ok(response) => Ok(response.status()),
            Err(ureq::Error::Status(status, _)) => Ok(status),
            Err(ureq::Error::Transport(e)) => Err(e.to_string()),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_programs_numbering_maps_onto_the_one_logger() {
        assert_eq!(wallet_level(0), None, "DISABLED");
        assert_eq!(wallet_level(1), Some(Level::Error), "FATAL");
        assert_eq!(wallet_level(2), Some(Level::Warn));
        assert_eq!(wallet_level(3), Some(Level::Info));
        assert_eq!(wallet_level(4), Some(Level::Debug));
        assert_eq!(wallet_level(5), Some(Level::Trace));

        assert_eq!(service_level(0), Some(Level::Error), "FATAL");
        assert_eq!(service_level(1), Some(Level::Error), "ERROR");
        assert_eq!(service_level(3), Some(Level::Info), "the service default");
        assert_eq!(service_level(4), Some(Level::Debug), "DEBUGGING");
        assert_eq!(service_level(9), Some(Level::Trace));
        assert_eq!(service_level(-1), None);
    }
}
