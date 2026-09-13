// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Verifying a batch of ring signatures across the available cores.
//!
//! [`curve::check_ring_signature`](crate::curve::check_ring_signature) is the
//! single largest cost of validating a block outside the checkpoint zone: one
//! call per key input, each one two ed25519 scalar multiplications per ring
//! member. A transaction with many inputs — a fusion transaction has at least
//! twelve — pays it once per input, and every one of those calls is a pure
//! function of its own arguments. This module runs them concurrently and
//! reports the outcome **in input order**, so a caller sees exactly what a
//! sequential loop would have seen.
//!
//! # Why this is safe to run concurrently
//!
//! `wrkz_check_ring_signature` (`c/cn_shim.c`) touches no shared mutable
//! state:
//!
//! - its working buffer is a `malloc`/`free` pair local to the call;
//! - everything else is on its own stack (`ge_p3`, `ge_dsmp`, the two scalars);
//! - it calls into `crypto-ops.c`, `crypto-ops-data.c`, `hash.c` and
//!   `keccak.c`, none of which defines a mutable object at file scope — the
//!   ref10 tables (`fe_d`, `ge_base`, …) are `const`, `cn_fast_hash` keeps its
//!   `hash_state` on the stack, and `keccakf` works in place on the caller's
//!   buffer;
//! - it never enters the CryptoNight family, so it never allocates the
//!   thread-local scratchpad of `crate::cryptonight`.
//!
//! The scratchpad is the one piece of per-thread state in this crate. It cannot
//! be allocated here, but the workers call [`crate::release_thread_scratchpad`]
//! before they end anyway, so the invariant survives someone later adding a
//! `cn_*` hash to the batch. (It is a Rust thread-local, so a thread that exits
//! holding one frees it regardless.)
//!
//! # Determinism
//!
//! Verification is a pure function, so *which* thread checks a ring cannot
//! change its verdict; only the order failures are discovered in can vary.
//! [`first_invalid_ring`] therefore does not return the first failure it hears
//! about: it keeps the **lowest** failing index in an atomic, and a worker
//! stops only once that index is strictly below every index it could still
//! reach. The result is the same index a `for` loop with an early `return`
//! would have produced, run to run and thread count to thread count. See
//! [`first_invalid_ring`] for the argument in full.

use crate::curve::{check_ring_signature, KeyImage, PublicKey, Signature};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Batches smaller than this always take the sequential path.
///
/// Below a handful of rings the thread hand-off costs more than the work it
/// hands off, and the common transaction on this chain has two inputs.
pub const PARALLEL_THRESHOLD: usize = 4;

/// Ceiling on [`default_threads`], whatever the machine reports.
///
/// Verification is compute-bound and scales close to linearly, but a validator
/// is rarely the only thing on the box, and a 128-core host has no business
/// spawning 128 threads to check twenty signatures.
pub const MAX_DEFAULT_THREADS: usize = 32;

/// How many threads a batch uses when the caller expresses no preference:
/// [`std::thread::available_parallelism`] capped at [`MAX_DEFAULT_THREADS`],
/// or 1 when the platform will not say.
///
/// Queried once and cached: `available_parallelism` is a system call, and this
/// is read once per transaction.
///
/// A daemon that also serves RPC while it syncs usually wants **fewer** than
/// this — every thread here is a core not answering a request, and the
/// scheduler cannot tell the two apart. Set it explicitly there
/// (`wrkz_chain::Config::validate_threads`); an offline replay, which has the
/// machine to itself, wants the default.
pub fn default_threads() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, MAX_DEFAULT_THREADS)
    })
}

/// One ring signature to verify: the arguments of
/// [`curve::check_ring_signature`](crate::curve::check_ring_signature),
/// borrowed.
///
/// Every field is a shared reference to plain bytes, so a slice of these is
/// `Send + Sync` and can be handed to scoped threads without copying a ring.
#[derive(Clone, Copy, Debug)]
pub struct RingCheck<'a> {
    /// The transaction prefix hash the ring signs.
    pub prefix_hash: &'a [u8; 32],
    /// The input's key image.
    pub image: &'a KeyImage,
    /// The ring members, resolved from the chain.
    pub pubs: &'a [PublicKey],
    /// One signature per ring member.
    pub sigs: &'a [Signature],
}

impl RingCheck<'_> {
    /// Verify this one ring, exactly as
    /// [`curve::check_ring_signature`](crate::curve::check_ring_signature)
    /// does — it is that call.
    pub fn verify(&self) -> bool {
        check_ring_signature(self.prefix_hash, self.image, self.pubs, self.sigs)
    }
}

/// The index of the **lowest** entry of `checks` that fails to verify, or
/// `None` when every one of them verifies.
///
/// `threads` is an upper bound on the workers to use; 1 (or a batch below
/// [`PARALLEL_THRESHOLD`], or a machine that reports one core) takes the plain
/// sequential loop, which is the reference this is required to match.
///
/// # The determinism argument
///
/// Let `j` be the lowest index that fails. The claim is that this function
/// returns `Some(j)` on every run, whatever the thread count and whatever order
/// the workers happen to run in.
///
/// Each index is handed to exactly one worker: work is claimed in disjoint
/// batches through a single `fetch_add`, and a worker walks its batch in
/// ascending order. `lowest` starts at `usize::MAX` and only ever moves down,
/// by `fetch_min`, and only to an index that has actually failed — so at any
/// moment `lowest` is either `usize::MAX` or a genuine failing index.
///
/// A worker abandons an index `i` only when it observes `lowest < i`. Suppose
/// `j` were abandoned: then at that moment `lowest < j` held, so some index
/// below `j` had already failed, contradicting `j` being the lowest failing
/// index. So `j` is always verified, always fails, and is always folded in with
/// `fetch_min`. No index below `j` can ever be folded in, because none of them
/// fails. Hence the final `lowest` is exactly `j`.
///
/// The same argument covers the `None` case: with no failing index nothing is
/// ever folded in, no worker can observe a `lowest` below anything, so every
/// index is verified and the result stays `usize::MAX`.
///
/// Skipping is therefore a pure saving of work, never a change of answer: it is
/// what makes this match the sequential loop's early `return` rather than
/// merely agreeing with it about which blocks are valid.
pub fn first_invalid_ring(checks: &[RingCheck<'_>], threads: usize) -> Option<usize> {
    let workers = threads.min(checks.len() / 2);
    if checks.len() < PARALLEL_THRESHOLD || workers < 2 {
        return checks.iter().position(|c| !c.verify());
    }

    // Aim for a few batches per worker: enough to even out a batch of rings of
    // mixed sizes (a ring of 8 costs four times a ring of 2), few enough that
    // the shared counter is not touched once per signature.
    let batch = checks.len().div_ceil(workers * 4).max(1);
    let next = AtomicUsize::new(0);
    let lowest = AtomicUsize::new(usize::MAX);

    std::thread::scope(|scope| {
        for _ in 1..workers {
            let (next, lowest) = (&next, &lowest);
            scope.spawn(move || {
                run_worker(checks, batch, next, lowest);
                // Insurance, not a fix: nothing on this path allocates the
                // CryptoNight scratchpad, but a worker thread that exited
                // holding one would leak it, and this call is a no-op when
                // there is none. See the module docs.
                crate::release_thread_scratchpad();
            });
        }
        // The calling thread is a worker too, so a two-thread batch spawns one
        // thread rather than two. It must *not* release its scratchpad: it is
        // the thread that hashes the block proof of work, and taking its
        // scratchpad away would make it re-allocate megabytes per block.
        run_worker(checks, batch, &next, &lowest);
    });

    match lowest.load(Ordering::Acquire) {
        usize::MAX => None,
        i => Some(i),
    }
}

/// Claim batches until the work runs out or nothing this worker could still
/// reach can improve on the failure already found.
fn run_worker(checks: &[RingCheck<'_>], batch: usize, next: &AtomicUsize, lowest: &AtomicUsize) {
    loop {
        let start = next.fetch_add(batch, Ordering::Relaxed);
        if start >= checks.len() {
            return;
        }
        // Every index this worker can still reach is at or above `start`, and
        // the counter only moves up, so a failure below `start` settles the
        // answer as far as this worker is concerned.
        if lowest.load(Ordering::Acquire) < start {
            return;
        }
        let end = (start + batch).min(checks.len());
        for (offset, check) in checks[start..end].iter().enumerate() {
            let i = start + offset;
            if lowest.load(Ordering::Acquire) < i {
                return;
            }
            if !check.verify() {
                lowest.fetch_min(i, Ordering::AcqRel);
                // Nothing above `i` in this batch can be lower than `i`.
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve;

    /// A ring of `size` members whose signature is valid, plus the pieces the
    /// caller needs to keep alive to point a [`RingCheck`] at it.
    struct Ring {
        image: KeyImage,
        pubs: Vec<PublicKey>,
        sigs: Vec<Signature>,
    }

    fn ring(size: usize) -> Ring {
        let (sec, pk) = curve::generate_keys();
        let mut pubs = vec![pk];
        for _ in 1..size {
            pubs.push(curve::generate_keys().1);
        }
        let image = curve::generate_key_image(&pk, &sec);
        let sigs = curve::generate_ring_signature(&PREFIX, &image, &pubs, &sec, 0).expect("a real ring signs");
        Ring { image, pubs, sigs }
    }

    const PREFIX: [u8; 32] = [0x5a; 32];

    fn checks(rings: &[Ring]) -> Vec<RingCheck<'_>> {
        rings
            .iter()
            .map(|r| RingCheck { prefix_hash: &PREFIX, image: &r.image, pubs: &r.pubs, sigs: &r.sigs })
            .collect()
    }

    #[test]
    fn a_clean_batch_verifies_on_every_thread_count() {
        let rings: Vec<Ring> = (0..12).map(|i| ring(2 + i % 4)).collect();
        let checks = checks(&rings);
        for threads in 1..=8 {
            assert_eq!(first_invalid_ring(&checks, threads), None, "{threads} threads");
        }
    }

    #[test]
    fn several_failures_report_the_lowest_index_whatever_the_thread_count() {
        let mut rings: Vec<Ring> = (0..16).map(|_| ring(2)).collect();
        // Break 5, 6 and 13. A sequential loop stops at 5.
        for i in [5usize, 6, 13] {
            rings[i].sigs[0][0] ^= 0x01;
        }
        let checks = checks(&rings);
        assert_eq!(first_invalid_ring(&checks, 1), Some(5), "the sequential reference");
        for threads in 2..=16 {
            assert_eq!(first_invalid_ring(&checks, threads), Some(5), "{threads} threads");
        }
    }

    #[test]
    fn the_last_ring_failing_alone_is_still_found() {
        let mut rings: Vec<Ring> = (0..9).map(|_| ring(2)).collect();
        let last = rings.len() - 1;
        rings[last].sigs[0][32] ^= 0x80;
        let checks = checks(&rings);
        for threads in 1..=8 {
            assert_eq!(first_invalid_ring(&checks, threads), Some(last), "{threads} threads");
        }
    }

    /// Every single-failure position in turn, against the sequential path.
    ///
    /// This is what exercises the skip: a failure at index 0 lets the workers
    /// abandon everything above it, a failure at the last index lets them
    /// abandon nothing, and both must produce the index a `for` loop would.
    #[test]
    fn every_failure_position_agrees_with_the_sequential_path() {
        const N: usize = 11;
        let clean: Vec<Ring> = (0..N).map(|i| ring(2 + i % 3)).collect();
        for broken in 0..N {
            let mut rings: Vec<Ring> = (0..N).map(|i| ring(2 + i % 3)).collect();
            rings[broken].sigs[0][1] ^= 0x02;
            let checks = checks(&rings);
            let reference = first_invalid_ring(&checks, 1);
            assert_eq!(reference, Some(broken), "the sequential reference for {broken}");
            for threads in 2..=12 {
                assert_eq!(first_invalid_ring(&checks, threads), reference, "broken {broken}, {threads} threads");
            }
        }
        // And the untouched batch still passes, so the loop above was not
        // agreeing on a failure it invented.
        assert_eq!(first_invalid_ring(&checks(&clean), 12), None);
    }

    #[test]
    fn an_empty_batch_has_no_failure() {
        assert_eq!(first_invalid_ring(&[], 8), None);
    }

    #[test]
    fn default_threads_is_at_least_one_and_capped() {
        let n = default_threads();
        assert!((1..=MAX_DEFAULT_THREADS).contains(&n), "{n}");
        assert_eq!(n, default_threads(), "cached");
    }

    /// Not a test of speed — a measurement. It asserts only that the two paths
    /// agree; the timings are printed for whoever ran it.
    #[test]
    #[ignore = "a measurement, not a test; run with --release --ignored --nocapture"]
    fn parallel_verification_of_a_realistic_batch() {
        use std::time::Instant;

        const RINGS: usize = 2_000;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let threads = default_threads();
        // Ring sizes 2..=8, the spread a real window sees: the mixin tier caps
        // it at 2 from 1,000,000 but the chain below that carries wider rings.
        let rings: Vec<Ring> = (0..RINGS).map(|i| ring(2 + i % 7)).collect();
        let members: usize = rings.iter().map(|r| r.pubs.len()).sum();
        let checks = checks(&rings);

        let t = Instant::now();
        let sequential = first_invalid_ring(&checks, 1);
        let seq_secs = t.elapsed().as_secs_f64();

        let t = Instant::now();
        let parallel = first_invalid_ring(&checks, threads);
        let par_secs = t.elapsed().as_secs_f64();

        assert_eq!(sequential, parallel, "the two paths must agree");
        assert_eq!(sequential, None, "every ring in this batch is valid");

        println!("\n== ring signature batch verification ==");
        println!("{cores} logical cores, {threads} validation threads by default");
        println!("{RINGS} rings, sizes 2..=8, {members} ring members\n");
        println!("{:>7}  {:>9}  {:>11}  {:>8}", "threads", "seconds", "rings/s", "speedup");
        let row = |n: usize, secs: f64| {
            println!(
                "{n:>7}  {secs:>9.3}  {:>11.0}  {:>7.2}x",
                RINGS as f64 / secs.max(1e-9),
                seq_secs / secs.max(1e-9)
            );
        };
        row(1, seq_secs);
        let mut n = 2;
        while n < threads {
            let t = Instant::now();
            let r = first_invalid_ring(&checks, n);
            let secs = t.elapsed().as_secs_f64();
            assert_eq!(r, sequential, "{n} threads must agree with the sequential path");
            row(n, secs);
            n *= 2;
        }
        row(threads, par_secs);
        println!();
    }
}
