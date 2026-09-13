// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Ctrl-C, SIGTERM and SIGHUP for the programs that are not the daemon:
//! `wrkz-wallet`, `wrkz-wallet-api`, `wrkz-service` and `wrkz-txpow-server`.
//!
//! The handler is the one `wrkz-node` installs
//! (`daemon::install_signal_handlers`): no extra crate, and it does the one
//! thing a signal handler may, which is store to an atomic the program polls.
//!
//! [`stdin_lines`] is the console those programs read while they wait. It
//! reads standard input on a thread of its own, because `signal(3)` restarts a
//! read the signal interrupted: a loop blocked in `read_line` on the main
//! thread would not see the flag until the next Enter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// How often a waiting program looks at the flag.
pub const POLL: Duration = Duration::from_millis(250);

/// Install the handlers. Afterwards [`stop_requested`] turns true on SIGINT,
/// SIGTERM or SIGHUP, or a console close or Ctrl-C on Windows.
pub fn install() {
    #[cfg(unix)]
    {
        const SIGHUP: i32 = 1;
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        unsafe extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        extern "C" fn handler(_signum: i32) {
            STOP_REQUESTED.store(true, Ordering::SeqCst);
        }
        // Through a function pointer: casting the function item straight to an
        // integer is linted (`function_casts_as_integer`), and CI denies it.
        let handler = handler as extern "C" fn(i32) as usize;
        // SAFETY: `signal` is the C library's, and `handler` does nothing but
        // an atomic store, which is async-signal-safe.
        unsafe {
            signal(SIGINT, handler);
            signal(SIGTERM, handler);
            signal(SIGHUP, handler);
        }
    }
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
        }
        unsafe extern "system" fn handler(_event: u32) -> i32 {
            STOP_REQUESTED.store(true, Ordering::SeqCst);
            1
        }
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}

/// True once a signal asked the program to stop.
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

/// Return once a signal asks the program to stop.
pub fn wait_for_stop() {
    while !stop_requested() {
        std::thread::sleep(POLL);
    }
}

/// Standard input a line at a time, without the line ending, until the input
/// ends or a signal asks the program to stop.
pub fn stdin_lines() -> impl Iterator<Item = String> {
    let (lines, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    Lines { rx, stopped: stop_requested }
}

/// The lines `rx` delivers, until it closes or `stopped` returns true.
struct Lines<F> {
    rx: Receiver<String>,
    stopped: F,
}

impl<F: Fn() -> bool> Iterator for Lines<F> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        while !(self.stopped)() {
            match self.rx.recv_timeout(POLL) {
                Ok(line) => return Some(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn lines_arrive_in_order_and_end_with_the_input() {
        let (tx, rx) = mpsc::channel();
        for line in ["save", "exit"] {
            tx.send(line.to_string()).unwrap();
        }
        drop(tx);
        let lines: Vec<String> = Lines { rx, stopped: || false }.collect();
        assert_eq!(lines, ["save", "exit"]);
    }

    #[test]
    fn a_stop_ends_the_lines_while_the_input_is_still_open() {
        let (tx, rx) = mpsc::channel::<String>();
        let polls = AtomicUsize::new(0);
        let mut lines = Lines { rx, stopped: || polls.fetch_add(1, Ordering::SeqCst) >= 2 };
        assert_eq!(lines.next(), None);
        assert_eq!(polls.load(Ordering::SeqCst), 3, "it waited twice, then saw the stop");
        drop(tx);
    }
}
