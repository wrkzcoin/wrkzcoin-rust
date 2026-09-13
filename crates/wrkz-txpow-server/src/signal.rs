// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Ctrl-C and SIGTERM, as `wrkz-node` answers them (`daemon::install_signal_handlers`):
//! no extra crate, and a handler that does the one thing a signal handler may,
//! which is store to an atomic the main loop polls.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

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

/// True once a signal asked the server to stop.
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}
