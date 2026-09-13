// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The daemon process itself, with no terminal on stdin.
//!
//! The console must never be the reason a node stops working. Two runs of the
//! real binary cover the two ways it can end up without an operator:
//!
//! - **stdin is not a terminal** (a pipe here, `/dev/null` under systemd): the
//!   reader is not started at all, and the daemon runs exactly as it did before
//!   the console existed;
//! - **`--no-console`**: the same, plus the periodic status line stays off,
//!   which is what the flag has always meant.
//!
//! Both runs are offline — no listener, no seeds, no RPC — so nothing here
//! touches the network. They are killed after they have proved they are up.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where a run's data directory and log go. Removed on the way in, not on the
/// way out, so a failure leaves the log to read.
fn work_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wrkz-console-proc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the work directory");
    dir
}

/// Start the daemon with its stdin on a pipe we never write to — which is what
/// "not a terminal" means to the reader — and its log going to a file.
fn start(name: &str, extra: &[&str]) -> (Child, std::path::PathBuf) {
    let dir = work_dir(name);
    let log = dir.join("daemon.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrkz-node"));
    command
        .arg("--data-dir")
        .arg(&dir)
        .arg("--log-file")
        .arg(&log)
        // Offline: no listener, no compiled-in seeds, no RPC port to collide
        // with another test.
        .args(["--no-listen", "--no-default-seeds", "--no-rpc"])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("the daemon binary runs");
    (child, log)
}

/// Wait until the log file contains `needle`, or give up.
fn wait_for(log: &std::path::Path, needle: &str, within: Duration) -> String {
    let deadline = Instant::now() + within;
    let mut text = String::new();
    while Instant::now() < deadline {
        text.clear();
        if let Ok(mut f) = std::fs::File::open(log) {
            let _ = f.read_to_string(&mut text);
            if text.contains(needle) {
                return text;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    text
}

#[test]
fn the_daemon_runs_normally_with_stdin_closed() {
    let (mut child, log) = start("piped-stdin", &[]);
    // The daemon is up once it has opened the state and started the engine.
    let text = wait_for(&log, "chain state opened at height", Duration::from_secs(30));
    assert!(text.contains("chain state opened at height"), "the daemon never came up:\n{text}");

    // The reader was never started, so nothing announced it. Everything else
    // about the run is unchanged.
    assert!(
        !text.contains("reading commands on stdin"),
        "a pipe is not a terminal and must not get a command reader:\n{text}"
    );
    assert!(!text.contains("console disabled"), "--no-console was not passed:\n{text}");

    // Still running a moment later: the reader did not exit the process, spin,
    // or take the engine down with it.
    std::thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().expect("try_wait").is_none(), "the daemon exited on its own:\n{}", read(&log));

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn no_console_starts_no_reader_and_keeps_its_old_meaning() {
    let (mut child, log) = start("no-console", &["--no-console"]);
    let text = wait_for(&log, "console disabled (--no-console)", Duration::from_secs(30));
    assert!(text.contains("console disabled (--no-console)"), "the flag is not honoured:\n{text}");
    assert!(text.contains("chain state opened at height"), "the daemon still comes up:\n{text}");
    assert!(!text.contains("reading commands on stdin"), "{text}");

    std::thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().expect("try_wait").is_none(), "the daemon exited on its own:\n{}", read(&log));

    let _ = child.kill();
    let _ = child.wait();
}

fn read(log: &std::path::Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// What `--log-file` actually contains, from a real run.
///
/// Two things an operator depends on and neither of which a unit test can
/// prove end to end: the prefix is a date they can read rather than a count of
/// seconds since 1970, and not one byte of terminal decoration is in the file.
/// The second is the whole reason the prompt redraw asks about stderr
/// separately from stdout.
#[test]
fn the_log_file_carries_readable_stamps_and_no_terminal_escapes() {
    let (mut child, log) = start("log-format", &[]);
    let text = wait_for(&log, "chain state opened at height", Duration::from_secs(30));
    assert!(text.contains("chain state opened at height"), "the daemon never came up:\n{text}");

    assert!(!text.contains('\u{1b}'), "an escape sequence reached the log file:\n{text}");
    assert!(!text.contains('\r'), "a carriage return reached the log file:\n{text}");

    let mut checked = 0;
    for line in text.lines().filter(|l| l.starts_with('[')) {
        // `[2026-09-10 14:03:22.123Z INFO   ] ...`
        let (prefix, rest) = line.split_once("] ").expect("every line has a prefix and a message");
        let prefix = prefix.trim_start_matches('[');
        let (stamp, level) = prefix.split_at(24);
        assert_eq!(&stamp[4..5], "-", "an ISO date, not an epoch: {line}");
        assert_eq!(&stamp[10..11], " ", "{line}");
        assert_eq!(&stamp[13..14], ":", "{line}");
        assert!(stamp.starts_with("20") && stamp.ends_with('Z'), "{line}");
        assert!(["ERROR", "WARNING", "INFO", "DEBUG", "TRACE"].contains(&level.trim()), "the C++ level names: {line}");
        assert!(!rest.is_empty(), "{line}");
        checked += 1;
    }
    assert!(checked >= 3, "the run logged almost nothing to check:\n{text}");

    let _ = child.kill();
    let _ = child.wait();
}
