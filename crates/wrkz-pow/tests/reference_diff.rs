// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The Rust hashing against the reference C it replaced (`wrkz-pow-ref`,
//! feature `pow`), on inputs no vector file lists.
//!
//! `spec/02-hashing.md`, acceptance 3, asks for 10,000 random inputs of lengths
//! 43..512 per CryptoNight wrapper when a port reimplements them. The default
//! run takes a sample; `cargo test --release -p wrkz-pow --test reference_diff
//! -- --ignored` runs the full count and prints the Rust/C timing.

use wrkz_pow::{CnParams, CN_LITE_V1, CN_TURTLE_LITE_V2, CN_UPX, CN_V0};
use wrkz_pow_ref::pow as c;

/// xorshift64*: deterministic, so a failure names a reproducible seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
    fn hash(&mut self) -> [u8; 32] {
        self.bytes(32).try_into().unwrap()
    }
}

const VARIANTS: [(&str, CnParams); 4] =
    [("cn_v0", CN_V0), ("cn_lite_v1", CN_LITE_V1), ("cn_turtle_lite_v2", CN_TURTLE_LITE_V2), ("cn_upx", CN_UPX)];

fn c_slow_hash(data: &[u8], p: CnParams) -> [u8; 32] {
    c::cn_slow_hash(data, p.light, p.variant, p.page_size, p.scratchpad, p.iterations, p.mask)
}

fn threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16)
}

/// `count` random inputs of 43..=512 bytes through the Rust and the C, spread
/// over the cores. Each worker has its own seed, so a failure is reproducible.
fn compare_cryptonight(name: &str, p: CnParams, count: usize, seed: u64) {
    let workers = threads();
    std::thread::scope(|s| {
        for t in 0..workers {
            s.spawn(move || {
                let mut rng = Rng::new(seed ^ ((t as u64) << 32));
                for _ in (t..count).step_by(workers) {
                    let len = 43 + rng.below(512 - 43 + 1);
                    let data = rng.bytes(len);
                    let rust = wrkz_pow::cn_slow_hash(&data, p);
                    assert_eq!(rust, c_slow_hash(&data, p), "{name}: input {}", hex::encode(&data));
                }
            });
        }
    });
}

#[test]
fn keccak_matches_c() {
    let mut rng = Rng::new(1);
    // Every length through three blocks, so each padding position is covered.
    for len in 0..=3 * 136 + 1 {
        let data = rng.bytes(len);
        assert_eq!(wrkz_pow::cn_fast_hash(&data), wrkz_pow_ref::cn_fast_hash(&data), "len {len}");
        assert_eq!(wrkz_pow::keccak1600(&data), wrkz_pow_ref::keccak1600(&data), "len {len}");
    }
    for _ in 0..200 {
        let len = rng.below(5000);
        let data = rng.bytes(len);
        assert_eq!(wrkz_pow::cn_fast_hash(&data), wrkz_pow_ref::cn_fast_hash(&data));
    }
}

#[test]
fn finalizers_match_c() {
    let mut rng = Rng::new(2);
    for which in 0..4 {
        // 200 is the only length CryptoNight hands them; the rest covers every
        // padding branch (Blake-256's 440-bit boundary among them).
        for len in (0..=300).chain([200; 50]) {
            let data = rng.bytes(len);
            assert_eq!(
                wrkz_pow::cn_finalizer(which, &data),
                c::hash_extra(which, &data),
                "finalizer {which}, len {len}"
            );
        }
    }
}

#[test]
fn tree_functions_match_c() {
    let mut rng = Rng::new(3);
    for n in (1..=600).chain([1023, 1024, 1025, 2047, 2048, 2049, 4095, 4096]) {
        let leaves: Vec<[u8; 32]> = (0..n).map(|_| rng.hash()).collect();
        assert_eq!(wrkz_pow::tree_hash(&leaves), c::tree_hash(&leaves), "tree_hash, {n} leaves");
        assert_eq!(wrkz_pow::tree_depth(n), c::tree_depth(n), "tree_depth({n})");
        assert_eq!(wrkz_pow::tree_branch(&leaves), c::tree_branch(&leaves), "tree_branch, {n} leaves");
    }
    for len in 0..=256 {
        let branch: Vec<[u8; 32]> = (0..len).map(|_| rng.hash()).collect();
        let leaf = rng.hash();
        let path = rng.hash();
        assert_eq!(
            wrkz_pow::tree_hash_from_branch(&branch, &leaf),
            c::tree_hash_from_branch(&branch, &leaf, None),
            "no path, branch {len}"
        );
        assert_eq!(
            wrkz_pow::tree_hash_from_branch_with_path(&branch, &leaf, Some(&path)),
            c::tree_hash_from_branch(&branch, &leaf, Some(&path)),
            "path, branch {len}"
        );
    }
    // Past 256 entries the Rust reads the missing path bits as zero, which is
    // what the C gives with an explicitly zero-extended path.
    let branch: Vec<[u8; 32]> = (0..300).map(|_| rng.hash()).collect();
    let leaf = rng.hash();
    let path = rng.hash();
    let mut extended = path.to_vec();
    extended.resize(300usize.div_ceil(8), 0);
    assert_eq!(
        wrkz_pow::tree_hash_from_branch_with_path(&branch, &leaf, Some(&path)),
        c::tree_hash_from_branch(&branch, &leaf, Some(&extended))
    );
}

#[test]
fn chukwa_matches_c() {
    let mut rng = Rng::new(4);
    for _ in 0..300 {
        let len = 16 + rng.below(300);
        let data = rng.bytes(len);
        assert_eq!(
            wrkz_pow::chukwa_slow_hash(&data),
            c::argon2id(4, 256, 1, &data, &data[..16]),
            "input {}",
            hex::encode(&data)
        );
    }
}

#[test]
fn cryptonight_matches_c_sample() {
    for (i, (name, p)) in VARIANTS.into_iter().enumerate() {
        let count = if p == CN_V0 { 48 } else { 160 };
        compare_cryptonight(name, p, count, 100 + i as u64);
    }
}

/// The table-driven AES must give the hardware path's hash (on a CPU without
/// AES instructions both calls take the tables, and the test is vacuous there).
#[test]
fn software_aes_matches_hardware() {
    let mut rng = Rng::new(5);
    for (name, p) in VARIANTS {
        for _ in 0..4 {
            let len = 43 + rng.below(200);
            let data = rng.bytes(len);
            assert_eq!(
                wrkz_pow::cn_slow_hash_software_aes(&data, p),
                wrkz_pow::cn_slow_hash(&data, p),
                "{name}: input {}",
                hex::encode(&data)
            );
        }
    }
}

/// A thread that switches between variants keeps one scratchpad, sized for the
/// largest; results must not depend on what a previous hash left in it.
#[test]
fn scratchpad_reuse_across_variants() {
    let data = [0x5au8; 76];
    let fresh: Vec<[u8; 32]> = VARIANTS
        .iter()
        .map(|(_, p)| {
            wrkz_pow::release_thread_scratchpad();
            wrkz_pow::cn_slow_hash(&data, *p)
        })
        .collect();
    for _ in 0..2 {
        for ((_, p), want) in VARIANTS.iter().rev().zip(fresh.iter().rev()) {
            assert_eq!(wrkz_pow::cn_slow_hash(&data, *p), *want);
        }
    }
}

/// spec/02-hashing.md acceptance 3 in full: 10,000 inputs per wrapper.
#[test]
#[ignore = "minutes of CryptoNight; run with --release -- --ignored"]
fn cryptonight_matches_c_10000() {
    for (i, (name, p)) in VARIANTS.into_iter().enumerate() {
        let started = std::time::Instant::now();
        compare_cryptonight(name, p, 10_000, 1000 + i as u64);
        eprintln!("{name}: 10000 inputs agree ({:.1?})", started.elapsed());
    }
}

/// Single-thread time per hash, Rust against C.
#[test]
#[ignore = "timing; run with --release -- --ignored --nocapture"]
fn speed_against_c() {
    let data = [7u8; 76];
    let time = |f: &dyn Fn() -> [u8; 32], n: u32| {
        f();
        let t = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(f());
        }
        t.elapsed() / n
    };
    for (name, p) in VARIANTS {
        let n = if p == CN_V0 { 20 } else { 100 };
        let rust = time(&|| wrkz_pow::cn_slow_hash(&data, p), n);
        let reference = time(&|| c_slow_hash(&data, p), n);
        eprintln!("{name:>18}: rust {rust:>10.2?}  c {reference:>10.2?}");
    }
    let rust = time(&|| wrkz_pow::chukwa_slow_hash(&data), 100);
    let reference = time(&|| c::argon2id(4, 256, 1, &data, &data[..16]), 100);
    eprintln!("{:>18}: rust {rust:>10.2?}  c {reference:>10.2?}", "chukwa");
    let blob = [9u8; 76];
    let rust = time(&|| wrkz_pow::cn_fast_hash(&blob), 100_000);
    let reference = time(&|| wrkz_pow_ref::cn_fast_hash(&blob), 100_000);
    eprintln!("{:>18}: rust {rust:>10.2?}  c {reference:>10.2?}", "cn_fast_hash");
}
