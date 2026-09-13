// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! What the wallet asks of the machine it runs on, in one place, so the same
//! core runs natively and in a browser (wasm32-unknown-unknown). A browser has
//! no system clock through `std`, cannot spawn a `std::thread`, and must not
//! block its thread in a sleep: `SystemTime::now()` and `thread::spawn` panic
//! there.
//!
//! In the browser the wallet runs inside a Web Worker (apps/pluton), which
//! waits between sync steps with a timer, so [`sleep`] has nothing to do.

use std::time::Duration;

/// Seconds since the Unix epoch, as the C++ `std::time(nullptr)`.
pub fn now_seconds() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        (js_sys::Date::now() / 1000.0) as u64
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

/// Milliseconds on a clock that only has to be good for measuring waits of a
/// few minutes (a proof-of-work server's deadline): monotonic natively, the
/// wall clock in a browser.
pub fn now_millis() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
    }
}

/// Threads worth using for block scanning and the transaction proof of work:
/// every core natively, one in a browser. Both scanning and the proof-of-work
/// search give the same result at every thread count.
pub fn available_threads() -> usize {
    if cfg!(all(target_arch = "wasm32", target_os = "unknown")) {
        1
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    }
}

/// Block the calling thread for `d`; a no-op in a browser (see the module docs).
pub fn sleep(d: Duration) {
    if !cfg!(all(target_arch = "wasm32", target_os = "unknown")) && !d.is_zero() {
        std::thread::sleep(d);
    }
}
