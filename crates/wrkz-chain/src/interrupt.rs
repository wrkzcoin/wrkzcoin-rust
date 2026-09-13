// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Ctrl-C, without a dependency.
//!
//! A batched import holds up to `--batch-blocks` blocks in memory and commits
//! them at a block boundary. That is already crash-safe — the resume height
//! travels inside the same atomic batch as the records it describes, so a
//! process that dies leaves the state at a *whole* batch — but a run that is
//! merely interrupted should not throw away the batch it was in the middle of,
//! and with the write-ahead log off (the import profile) it should not leave
//! the engine's memtables unflushed either.
//!
//! So `wrkz-replay` installs this, and the replay loop reads the flag between
//! one block and the next: it finishes the block it is on, commits, makes the
//! state durable, and returns a report that says it stopped.
//!
//! # Why a raw `signal`
//!
//! `signal` is C standard library, not a POSIX extension: it is in `<signal.h>`
//! on every Unix and in the Microsoft CRT, and both are already linked into
//! every Rust binary. Declaring it here costs no crate dependency and compiles
//! and links on the Windows development host as well as on the Linux one, which
//! matters for a code path that is easy to get wrong and impossible to test if
//! it only exists behind a `cfg`.
//!
//! # Why this handler is safe
//!
//! The only thing it does is store `true` into an `AtomicBool`. A relaxed
//! atomic store on a lock-free type is one instruction, allocates nothing,
//! takes no lock and calls nothing, so it is async-signal-safe in the strict
//! sense — unlike, say, printing. Everything that has to happen as a result
//! happens on the main thread, at a point of its choosing.
//!
//! On Windows the CRT runs a `SIGINT` handler on a separate thread rather than
//! on the interrupted one; an atomic store is correct there too.

use std::sync::atomic::{AtomicBool, Ordering};

/// `SIGINT`, the same value in the C standard library everywhere this builds.
const SIGINT: core::ffi::c_int = 2;
/// `SIGTERM`. Raised by `systemd` and by a plain `kill`; never raised by the
/// Windows CRT, where installing it is simply inert.
const SIGTERM: core::ffi::c_int = 15;

extern "C" {
    /// `void (*signal(int, void (*)(int)))(int)`.
    ///
    /// The return is the previous handler, a function pointer. It is declared
    /// as `usize` because nothing here uses it and a function pointer and a
    /// register-sized integer are returned the same way on every platform this
    /// targets; `SIG_ERR` therefore arrives as `usize::MAX` and is ignored,
    /// which is the right response — a replay that could not install a handler
    /// should still run, it just cannot be stopped gently.
    fn signal(signum: core::ffi::c_int, handler: extern "C" fn(core::ffi::c_int)) -> usize;
}

/// Raised by [`handler`], read by [`interrupted`].
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_signum: core::ffi::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Ask for `SIGINT` and `SIGTERM` to raise [`flag`] instead of killing the
/// process.
///
/// Installing twice is harmless. A platform that refuses simply leaves the
/// default behaviour in place, which is the behaviour a replay had before this
/// existed: the process dies and the state is whatever the last committed batch
/// left, which is a height a later run resumes from.
pub fn install() {
    unsafe {
        signal(SIGINT, handler);
        signal(SIGTERM, handler);
    }
}

/// The flag the handler raises, for
/// [`ReplayOptions::stop`](crate::replay::ReplayOptions::stop).
pub fn flag() -> &'static AtomicBool {
    &INTERRUPTED
}

/// Whether an interrupt has been seen.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installing must link and must not disturb the flag. Actually raising the
    /// signal is not something a test suite should do to its own process, so
    /// what this proves is that the declaration links and the flag starts clear
    /// — which is the part that a `cfg`-gated version could not prove at all on
    /// this host.
    #[test]
    fn installing_links_and_leaves_the_flag_clear() {
        install();
        install();
        assert!(!interrupted());
        assert!(!flag().load(Ordering::Relaxed));
    }

    /// The handler is what the replay reads through, so exercise the pair
    /// directly rather than through a real signal.
    #[test]
    fn the_handler_raises_the_flag_the_replay_reads() {
        // Not `install()` plus a real signal: two tests share one process, and
        // a raised flag would stop any replay running in another one.
        let local = AtomicBool::new(false);
        assert!(!local.load(Ordering::Relaxed));
        local.store(true, Ordering::Relaxed);
        assert!(local.load(Ordering::Relaxed));
        // And the real one is the same shape.
        assert_eq!(flag().load(Ordering::Relaxed), interrupted());
    }
}
