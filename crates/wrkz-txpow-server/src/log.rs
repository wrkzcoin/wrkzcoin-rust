// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Log lines to the console and, with `--log-file`, appended to a file too —
//! the C++ server's `Logger` callback (`main.cpp:44`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// `--log-level`, least to most severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warning,
    Fatal,
    /// Nothing is logged.
    Disabled,
}

impl Level {
    /// The names `--log-level` accepts, any case.
    pub fn parse(name: &str) -> Option<Level> {
        match name.to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warning" => Some(Level::Warning),
            "fatal" => Some(Level::Fatal),
            "disabled" => Some(Level::Disabled),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Fatal => "FATAL",
            Level::Disabled => "",
        }
    }
}

/// Where log lines go.
pub struct Logger {
    level: Level,
    file: Option<Mutex<File>>,
}

impl Logger {
    /// Lines at `level` and above, to the console.
    pub fn new(level: Level) -> Self {
        Self { level, file: None }
    }

    /// The same, also appended to `path`.
    pub fn with_file(level: Level, path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { level, file: Some(Mutex::new(file)) })
    }

    /// Whether a line at `level` would be written.
    pub fn enabled(&self, level: Level) -> bool {
        self.level != Level::Disabled && level != Level::Disabled && level >= self.level
    }

    pub fn log(&self, level: Level, message: impl AsRef<str>) {
        if !self.enabled(level) {
            return;
        }
        let line = format!("{} [{}] {}", utc_now(), level.label(), message.as_ref());
        println!("{line}");
        if let Some(file) = &self.file {
            let mut f = file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = writeln!(f, "{line}");
        }
    }
}

/// `2026-09-12 04:38:00 UTC`, without a date crate: days since the epoch to a
/// civil date by Howard Hinnant's `civil_from_days`.
fn utc_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);

    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_filter_as_the_cpp_logger_does() {
        let info = Logger::new(Level::Info);
        assert!(!info.enabled(Level::Debug));
        assert!(info.enabled(Level::Info) && info.enabled(Level::Fatal));
        assert!(!Logger::new(Level::Disabled).enabled(Level::Fatal));
        assert_eq!(Level::parse("WARNING"), Some(Level::Warning));
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn the_timestamp_is_a_utc_date() {
        let t = utc_now();
        assert_eq!(t.len(), "2026-09-12 04:38:00 UTC".len(), "{t}");
        assert!(t.starts_with("20") && t.ends_with(" UTC"), "{t}");
    }
}
