// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The CryptoNight family (`spec/02-hashing.md`, "The CryptoNight family").
//!
//! One implementation for every variant this chain has used, following
//! `slow-hash-x86.c:547` — the file the reference vectors were produced with —
//! step for step: the Keccak state, the AES "explode" into the scratchpad, the
//! mixing loop with the variant 1 tweak and the variant 2 shuffle, division and
//! square root (`slow-hash-common.h`), the "implode" back into the state, and
//! one of four finalizers picked by the low two bits of the state.
//!
//! The scratchpad (`CnParams::scratchpad` bytes, 128 KiB to 2 MiB) lives on the
//! heap in a thread-local and is reused across hashes, as the C's is; it is
//! freed when the thread exits or on [`release_scratchpad`]. Every word is read
//! and written little-endian, which is what the C's `uint64_t *` casts do on
//! the little-endian machines it runs on.

mod aes;
mod blake256;

use crate::keccak::{keccak1600, keccakf};
use crate::{CnParams, Hash};
use aes::{Aes, Block, Soft};
use std::cell::RefCell;

thread_local! {
    static SCRATCHPAD: RefCell<Vec<Block>> = const { RefCell::new(Vec::new()) };
}

/// Frees this thread's scratchpad; the next hash allocates a new one.
pub(crate) fn release_scratchpad() {
    SCRATCHPAD.with_borrow_mut(|pad| *pad = Vec::new());
}

/// `cn_slow_hash` with `prehashed = 0`. The caller has checked `p`
/// ([`crate::cn_slow_hash`]). `software_aes` forces the table-driven AES, so
/// tests can hold it against the hardware path.
pub(crate) fn cn_slow_hash(data: &[u8], p: CnParams, software_aes: bool) -> Hash {
    let blocks = (p.scratchpad / 16) as usize;
    SCRATCHPAD.with_borrow_mut(|pad| {
        if pad.len() < blocks {
            // Replaced, not grown: nothing in the old contents is ever read.
            *pad = vec![[0u8; 16]; blocks];
        }
        let pad = &mut pad[..blocks];
        if !software_aes {
            #[cfg(target_arch = "x86_64")]
            if std::is_x86_feature_detected!("aes") {
                // SAFETY: AES-NI was detected on this CPU just above.
                return unsafe { hash_aesni(data, p, pad) };
            }
            #[cfg(target_arch = "aarch64")]
            if std::arch::is_aarch64_feature_detected!("aes") {
                // SAFETY: the ARMv8 AES instructions were detected just above.
                return unsafe { hash_armv8(data, p, pad) };
            }
        }
        hash_with::<Soft>(data, p, pad)
    })
}

/// # Safety
/// The CPU must support AES-NI.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "aes")]
unsafe fn hash_aesni(data: &[u8], p: CnParams, pad: &mut [Block]) -> Hash {
    hash_with::<aes::AesNi>(data, p, pad)
}

/// # Safety
/// The CPU must support the ARMv8 AES instructions.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "aes")]
unsafe fn hash_armv8(data: &[u8], p: CnParams, pad: &mut [Block]) -> Hash {
    hash_with::<aes::ArmAes>(data, p, pad)
}

#[inline(always)]
fn words(b: &Block) -> [u64; 2] {
    [u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap())]
}

#[inline(always)]
fn block(w: [u64; 2]) -> Block {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&w[0].to_le_bytes());
    b[8..].copy_from_slice(&w[1].to_le_bytes());
    b
}

#[inline(always)]
fn xor(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
    [a[0] ^ b[0], a[1] ^ b[1]]
}

#[inline(always)]
fn add(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
    [a[0].wrapping_add(b[0]), a[1].wrapping_add(b[1])]
}

/// `VARIANT2_SHUFFLE_ADD_SSE2` (`slow-hash-common.h:119`) on the three other
/// 16-byte chunks of `j`'s 64-byte line. UPX2 (`light == 2`) swaps which chunk
/// feeds the first two stores.
#[inline(always)]
fn shuffle_add(pad: &mut [Block], j: usize, a: [u64; 2], b: [u64; 2], b1: [u64; 2], light2: bool) {
    let c1 = words(&pad[j ^ 1]);
    let c2 = words(&pad[j ^ 2]);
    let c3 = words(&pad[j ^ 3]);
    let (to1, to2) = if light2 { (c1, c3) } else { (c3, c1) };
    pad[j ^ 1] = block(add(to1, b1));
    pad[j ^ 2] = block(add(to2, b));
    pad[j ^ 3] = block(add(c2, a));
}

/// `VARIANT2_INTEGER_MATH_SQRT_STEP_SSE2` + `VARIANT2_INTEGER_MATH_SQRT_FIXUP`
/// (`variant2_int_sqrt.h`): the integer part of `sqrt(2^64 + n) * 2 - 2^33`.
/// The double-precision estimate is off by at most one; the fixup makes the
/// result exact, so it equals `integer_square_root_v2` for every input.
#[inline(always)]
fn variant2_sqrt(n: u64) -> u64 {
    const EXP_DOUBLE_BIAS: u64 = 1023 << 52;
    let x = f64::from_bits((n >> 12) + EXP_DOUBLE_BIAS).sqrt();
    let r = (x.to_bits() - EXP_DOUBLE_BIAS) >> 19;
    let s = r >> 1;
    let b = r & 1;
    let r2 = s.wrapping_mul(s + b).wrapping_add(r << 32);
    let dec = r2.wrapping_add(b) > n;
    let inc = r2.wrapping_add(1 << 32) < n.wrapping_sub(s);
    r.wrapping_add(inc as u64).wrapping_sub(dec as u64)
}

/// The byte `VARIANT1_1` (`slow-hash-common.h:44`) rewrites.
#[inline(always)]
fn variant1_1(b: &mut Block) {
    let tmp = b[11];
    let index = (((tmp >> 3) & 6) | (tmp & 1)) << 1;
    b[11] = tmp ^ (((0x75310u32 >> index) & 0x30) as u8);
}

/// `extra_hashes[which & 3]` (`slow-hash-x86.c:578`).
pub(crate) fn finalize(which: usize, data: &[u8]) -> Hash {
    use groestl::Digest;
    match which & 3 {
        0 => blake256::blake256(data),
        1 => groestl::Groestl256::digest(data).into(),
        2 => jh::Jh256::digest(data).into(),
        _ => skein::Skein512_256::digest(data).into(),
    }
}

#[inline(always)]
fn hash_with<A: Aes>(data: &[u8], p: CnParams, pad: &mut [Block]) -> Hash {
    let variant = p.variant;
    let light2 = p.light == 2;
    let mask = p.mask;

    // Step 1: Keccak-1600 of the input fills the state.
    let mut state = keccak1600(data);
    let w = |st: &[u8; 200], i: usize| u64::from_le_bytes(st[8 * i..8 * i + 8].try_into().unwrap());
    let mut text: [Block; 8] = std::array::from_fn(|i| state[64 + 16 * i..80 + 16 * i].try_into().unwrap());

    // VARIANT1_INIT64 / VARIANT2_INIT64.
    let tweak1_2 = if variant == 1 { w(&state, 24) ^ u64::from_le_bytes(data[35..43].try_into().unwrap()) } else { 0 };
    let (mut division_result, mut sqrt_result) = if variant == 2 { (w(&state, 12), w(&state, 13)) } else { (0, 0) };
    let mut b1 = if variant == 2 { [w(&state, 8) ^ w(&state, 10), w(&state, 9) ^ w(&state, 11)] } else { [0, 0] };

    // Step 2: explode the state into the scratchpad with the key from bytes 0..32.
    let keys = aes::expand_key(state[..32].try_into().unwrap());
    for line in pad.as_chunks_mut::<8>().0 {
        for t in text.iter_mut() {
            *t = A::pseudo_round(*t, &keys);
        }
        *line = text;
    }

    let mut a = [w(&state, 0) ^ w(&state, 4), w(&state, 1) ^ w(&state, 5)];
    let mut b = [w(&state, 2) ^ w(&state, 6), w(&state, 3) ^ w(&state, 7)];

    // Step 3: the mixing loop (pre_aes / post_aes, slow-hash-x86.c:76).
    for _ in 0..p.iterations / 2 {
        let j = ((a[0] & mask) >> 4) as usize;
        let c = words(&A::round(pad[j], &block(a)));
        if variant == 2 {
            shuffle_add(pad, j, a, b, b1, light2);
        }
        pad[j] = block(xor(b, c));
        if variant == 1 {
            variant1_1(&mut pad[j]);
        }

        let j = ((c[0] & mask) >> 4) as usize;
        let mut d = words(&pad[j]);
        if variant == 2 {
            // VARIANT2_INTEGER_MATH_DIVISION_STEP and the square root.
            d[0] ^= division_result ^ (sqrt_result << 32);
            let dividend = c[1];
            let divisor = ((c[0] as u32).wrapping_add((sqrt_result << 1) as u32) | 0x8000_0001) as u64;
            division_result = ((dividend / divisor) as u32 as u64).wrapping_add((dividend % divisor) << 32);
            sqrt_result = variant2_sqrt(c[0].wrapping_add(division_result));
        }
        let product = (c[0] as u128) * (d[0] as u128);
        let (mut hi, mut lo) = ((product >> 64) as u64, product as u64);
        if variant == 2 {
            // VARIANT2_2.
            pad[j ^ 1] = block(xor(words(&pad[j ^ 1]), [hi, lo]));
            let e = words(&pad[j ^ 2]);
            hi ^= e[0];
            lo ^= e[1];
            shuffle_add(pad, j, a, b, b1, light2);
        }
        a = add(a, [hi, lo]);
        let mut stored = a;
        if variant == 1 {
            // VARIANT1_2: the stored copy only, after `a` is taken.
            stored[1] ^= tweak1_2;
        }
        pad[j] = block(stored);
        a = xor(a, d);
        b1 = b;
        b = c;
    }

    // Step 4: implode the scratchpad back into the state with the key from bytes 32..64.
    let keys = aes::expand_key(state[32..64].try_into().unwrap());
    let mut text: [Block; 8] = std::array::from_fn(|i| state[64 + 16 * i..80 + 16 * i].try_into().unwrap());
    for line in pad.as_chunks::<8>().0 {
        for (t, s) in text.iter_mut().zip(line) {
            *t = A::pseudo_round(block(xor(words(t), words(s))), &keys);
        }
    }
    for (i, t) in text.iter().enumerate() {
        state[64 + 16 * i..80 + 16 * i].copy_from_slice(t);
    }

    // Step 5: permute once more and finalize with the hash the state selects.
    keccakf(&mut state);
    finalize(state[0] as usize, &state)
}

#[cfg(test)]
mod tests {
    use super::variant2_sqrt;

    /// `integer_square_root_v2` (`variant2_int_sqrt.h:55`), the integer-only
    /// reference the float path must equal.
    fn integer_square_root_v2(mut n: u64) -> u64 {
        let mut r: u64 = 1 << 63;
        let mut bit: u64 = 1 << 60;
        while bit != 0 {
            let b = n < r.wrapping_add(bit);
            let n_next = n.wrapping_sub(r.wrapping_add(bit));
            let r_next = r.wrapping_add(bit * 2);
            if !b {
                n = n_next;
                r = r_next;
            }
            r >>= 1;
            bit >>= 2;
        }
        // The C returns uint32_t: the truncation drops the 2^33 of "- 2^33".
        u64::from(r.wrapping_mul(2).wrapping_add(u64::from(n > r)) as u32)
    }

    #[test]
    fn variant2_sqrt_matches_the_header_table_and_the_reference() {
        // The sample table of variant2_int_sqrt.h:41.
        for (n, want) in [
            (0u64, 0u64),
            (1 << 32, 0),
            ((1 << 32) + 1, 1),
            (1 << 50, 262140),
            ((1 << 55) + 20963331, 8384515),
            ((1 << 55) + 20963332, 8384516),
            ((1 << 62) + 26599786, 1013904242),
            ((1 << 62) + 26599787, 1013904243),
            (u64::MAX, 3558067407),
        ] {
            assert_eq!(variant2_sqrt(n), want, "n = {n}");
            assert_eq!(integer_square_root_v2(n), want, "reference, n = {n}");
        }
        // The boundaries, where the float estimate is most likely to be off by one.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for r in (0..=3558067407u64).step_by(9_973) {
            // n at which the result steps from r - 1 to r: ((r + 2^33) / 2)^2 - 2^64.
            let t = (r as u128 + (1u128 << 33)) * (r as u128 + (1u128 << 33));
            let n0 = (t / 4).saturating_sub(1u128 << 64);
            for n in [n0.saturating_sub(1), n0, n0 + 1] {
                let n = n.min(u64::MAX as u128) as u64;
                assert_eq!(variant2_sqrt(n), integer_square_root_v2(n), "n = {n}");
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            assert_eq!(variant2_sqrt(x), integer_square_root_v2(x), "n = {x}");
        }
    }
}
