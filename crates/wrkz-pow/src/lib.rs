// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! WrkzCoin hashing, proof of work and curve primitives.
//!
//! The hashing and proof of work are Rust ports of the reference C of
//! wrkzcoin commit `8d89d7bf`; that C stays in `wrkz-pow-ref` (feature `pow`)
//! as the oracle `tests/reference_diff.rs` compares every function against.
//! The curve operations still call the reference C (`wrkz-pow-ref`, always
//! built) until their own port. Everything here is consensus; every function is
//! pinned by `spec/vectors/primitives.txt`.
//!
//! - [`cn_fast_hash`] — Keccak-256 with the original `0x01` padding (not SHA3).
//! - [`cn_slow_hash_v0`], [`cn_lite_slow_hash_v1`], [`cn_turtle_lite_slow_hash_v2`],
//!   [`chukwa_slow_hash`], [`cn_upx`] — the five proofs of work, selected by
//!   block major version through [`pow_hash_for_block_version`].
//! - [`tree_hash`] and friends — Merkle root of transaction hashes.
//! - [`check_hash`] — the difficulty test.
//! - [`curve`] — ed25519 operations of `spec/03-crypto-primitives.md`.
//! - [`parallel`] — verifying a batch of ring signatures across the cores,
//!   with the same verdict a sequential loop would reach.

#![deny(unsafe_op_in_unsafe_fn)]

mod cryptonight;
mod keccak;
mod tree;

pub use tree::{tree_branch, tree_depth, tree_hash, tree_hash_from_branch_with_path};

pub type Hash = [u8; 32];

/// Parameters of one CryptoNight wrapper from `src/crypto/hash.h`
/// (`spec/02-hashing.md`, "The CryptoNight family").
///
/// `page_size` is how much the C allocates; the hash only ever touches the
/// first `scratchpad` bytes, which is all this implementation allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CnParams {
    pub light: i32,
    pub variant: i32,
    pub page_size: u32,
    pub scratchpad: u32,
    pub iterations: u32,
    pub mask: u64,
}

/// `cn_slow_hash_v0`, hash.h:97: block major 1–3, legacy wallet key.
pub const CN_V0: CnParams = CnParams {
    light: 0,
    variant: 0,
    page_size: 2_097_152,
    scratchpad: 2_097_152,
    iterations: 1_048_576,
    mask: 0x1F_FFF0,
};
/// `cn_lite_slow_hash_v1`, hash.h:158: block major 4. Input MUST be >= 43 bytes.
pub const CN_LITE_V1: CnParams =
    CnParams { light: 1, variant: 1, page_size: 2_097_152, scratchpad: 1_048_576, iterations: 524_288, mask: 0xF_FFF0 };
/// `cn_turtle_lite_slow_hash_v2`, hash.h:357: block major 5.
pub const CN_TURTLE_LITE_V2: CnParams =
    CnParams { light: 1, variant: 2, page_size: 262_144, scratchpad: 262_144, iterations: 131_072, mask: 0x1_FFF0 };
/// `cn_upx`, hash.h:372: block major 7 and transaction proof of work.
pub const CN_UPX: CnParams =
    CnParams { light: 2, variant: 2, page_size: 131_072, scratchpad: 131_072, iterations: 32_768, mask: 0x1_FFF0 };

/// Keccak-256 (original padding). Every non-PoW hash of the protocol.
pub fn cn_fast_hash(data: &[u8]) -> Hash {
    keccak::cn_fast_hash(data)
}

/// Generic CryptoNight entry point (`hash-ops.h:64`) with explicit parameters.
///
/// Panics for variant 1 on inputs shorter than 43 bytes, which the C code
/// aborts on; block hashing blobs are always longer. Also panics on parameters
/// no CryptoNight variant uses: a variant outside 0–2, a scratchpad that is not
/// a whole number of 128-byte lines, or a mask reaching past the scratchpad.
pub fn cn_slow_hash(data: &[u8], p: CnParams) -> Hash {
    check_params(data, p);
    cryptonight::cn_slow_hash(data, p, false)
}

fn check_params(data: &[u8], p: CnParams) {
    assert!((0..=2).contains(&p.variant), "CryptoNight variant {} does not exist", p.variant);
    assert!(p.variant != 1 || data.len() >= 43, "variant 1 requires >= 43 bytes of input");
    assert!(p.scratchpad > 0 && p.scratchpad.is_multiple_of(128), "scratchpad must be whole 128-byte lines");
    // Every address is `x & mask` with the low four bits clear, and the variant
    // 2 shuffle touches the rest of that 64-byte line.
    assert!(p.mask & 0xF == 0 && (p.mask | 0x3F) < u64::from(p.scratchpad), "mask reaches past the scratchpad");
}

/// [`cn_slow_hash`] with the table-driven AES forced, for tests that hold it
/// against the hardware path.
#[doc(hidden)]
pub fn cn_slow_hash_software_aes(data: &[u8], p: CnParams) -> Hash {
    check_params(data, p);
    cryptonight::cn_slow_hash(data, p, true)
}

/// CryptoNight finalizer `which & 3` (0 Blake-256, 1 Groestl-256, 2 JH-256,
/// 3 Skein-512-256), exposed for the differential tests.
#[doc(hidden)]
pub fn cn_finalizer(which: usize, data: &[u8]) -> Hash {
    cryptonight::finalize(which, data)
}

/// The whole Keccak-1600 state after absorbing `data` (`hash_process`),
/// exposed for the differential tests.
#[doc(hidden)]
pub fn keccak1600(data: &[u8]) -> [u8; 200] {
    keccak::keccak1600(data)
}

pub fn cn_slow_hash_v0(data: &[u8]) -> Hash {
    cn_slow_hash(data, CN_V0)
}

pub fn cn_lite_slow_hash_v1(data: &[u8]) -> Hash {
    cn_slow_hash(data, CN_LITE_V1)
}

pub fn cn_turtle_lite_slow_hash_v2(data: &[u8]) -> Hash {
    cn_slow_hash(data, CN_TURTLE_LITE_V2)
}

pub fn cn_upx(data: &[u8]) -> Hash {
    cn_slow_hash(data, CN_UPX)
}

/// Chukwa (`hash.h:468`): argon2id version 1.3, t=4, m=256 KiB, p=1, salt =
/// first 16 bytes of the input, 32-byte tag. Block major 6.
///
/// Panics on inputs shorter than 16 bytes.
pub fn chukwa_slow_hash(data: &[u8]) -> Hash {
    assert!(data.len() >= 16, "chukwa requires >= 16 bytes of input (the salt)");
    let params = argon2::Params::new(256, 4, 1, Some(32)).expect("the Chukwa parameters are valid");
    let mut out = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(data, &data[..16], &mut out)
        .expect("argon2id takes any password with a 16-byte salt");
    out
}

/// Which proof-of-work function a block of the given major version uses
/// (`HASHING_ALGORITHMS_BY_BLOCK_VERSION`, CryptoNoteConfig.h:462).
/// Returns `None` for versions with no entry; a node MUST refuse such blocks.
pub fn pow_hash_for_block_version(major_version: u8, data: &[u8]) -> Option<Hash> {
    Some(match major_version {
        1..=3 => cn_slow_hash_v0(data),
        4 => cn_lite_slow_hash_v1(data),
        5 => cn_turtle_lite_slow_hash_v2(data),
        6 => chukwa_slow_hash(data),
        7 => cn_upx(data),
        _ => return None,
    })
}

/// `tree_hash_from_branch(branch, depth, leaf, path = NULL)` (`tree-hash.c:111`).
/// With an empty branch it returns the leaf unchanged, the only case on this chain.
pub fn tree_hash_from_branch(branch: &[Hash], leaf: &Hash) -> Hash {
    tree_hash_from_branch_with_path(branch, leaf, None)
}

/// Release this thread's CryptoNight scratchpad.
///
/// Each thread keeps a 128 KiB–2 MiB scratchpad on the heap between hashes,
/// freed when the thread exits. Call this from a thread that hashed once and
/// will stay alive without hashing again, to give the memory back early.
/// Hashing after the call simply allocates a new scratchpad.
pub fn release_thread_scratchpad() {
    cryptonight::release_scratchpad();
}

/// `check_hash(hash, difficulty)` (`src/common/CheckDifficulty.cpp:44`):
/// true iff `hash` (as a 256-bit little-endian integer) times `difficulty`
/// does not overflow 256 bits.
pub fn check_hash(hash: &Hash, difficulty: u64) -> bool {
    let w = |i: usize| u64::from_le_bytes(hash[8 * i..8 * i + 8].try_into().unwrap()) as u128;
    let d = difficulty as u128;
    // Schoolbook 256x64 product, tracking only the carries; any carry out of
    // the top word rejects, exactly like the word-by-word C code.
    let p3 = w(3) * d;
    if (p3 >> 64) != 0 {
        return false;
    }
    let p0 = w(0) * d;
    let p1 = w(1) * d;
    let p2 = w(2) * d;
    let s1 = (p1 & u64::MAX as u128) + (p0 >> 64);
    let s2 = (p2 & u64::MAX as u128) + (p1 >> 64) + (s1 >> 64);
    let s3 = p3 + (p2 >> 64) + (s2 >> 64);
    (s3 >> 64) == 0
}

pub mod curve;
pub mod parallel;
