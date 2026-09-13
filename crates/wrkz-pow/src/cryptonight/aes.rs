// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! AES as CryptoNight uses it (`aesb.c`, `slow-hash-x86.c:292`).
//!
//! Two operations, neither of them standard AES encryption:
//!
//! - one round, `aesenc(block, key)`: SubBytes, ShiftRows, MixColumns, then
//!   xor with `key` — exactly the x86 `AESENC` instruction;
//! - the "pseudo round": ten such rounds with the first ten AES-256 round keys
//!   of a 32-byte key, with no initial AddRoundKey and no short final round.
//!
//! Three backends compute the same function: AES-NI on x86_64, the ARMv8
//! crypto extensions on aarch64 (both chosen at run time), and a port of the
//! table-driven `aesb.c` everywhere else. None of this needs to be constant
//! time: every input is public block data.

pub(crate) type Block = [u8; 16];

/// One AES round implementation. `pseudo_round` is shared by all three.
pub(crate) trait Aes {
    fn round(block: Block, key: &Block) -> Block;

    #[inline(always)]
    fn pseudo_round(mut block: Block, keys: &[Block; 10]) -> Block {
        for k in keys {
            block = Self::round(block, k);
        }
        block
    }
}

#[rustfmt::skip]
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// The four forward tables of `aesb.c:145` (`u0`..`u3` over the S-box): entry
/// `x` of table `n` is the MixColumns column of `SBOX[x]`, rotated `8n` bits.
const T: [[u32; 256]; 4] = {
    let mut t = [[0u32; 256]; 4];
    let mut x = 0;
    while x < 256 {
        let s = SBOX[x] as u32;
        // f2 (aesb.c:126): multiply by x in GF(2^8), reduced by 0x11b.
        let s2 = (s << 1) ^ (((s >> 7) & 1) * 0x11b);
        let s3 = s2 ^ s;
        // u0(p) = bytes2word(f2(p), p, p, f3(p)), little-endian byte order.
        let w = s2 | (s << 8) | (s << 16) | (s3 << 24);
        t[0][x] = w;
        t[1][x] = w.rotate_left(8);
        t[2][x] = w.rotate_left(16);
        t[3][x] = w.rotate_left(24);
        x += 1;
    }
    t
};

/// The table-driven round of `aesb.c:155` (`aesb_single_round`).
pub(crate) struct Soft;

impl Aes for Soft {
    #[inline(always)]
    fn round(block: Block, key: &Block) -> Block {
        let w = |b: &Block, c: usize| u32::from_le_bytes([b[4 * c], b[4 * c + 1], b[4 * c + 2], b[4 * c + 3]]);
        let x = [w(&block, 0), w(&block, 1), w(&block, 2), w(&block, 3)];
        let mut out = [0u8; 16];
        for c in 0..4 {
            // fwd_rnd (aesb.c:73): column c takes byte r of column c + r.
            let y = w(key, c)
                ^ T[0][(x[c] & 0xff) as usize]
                ^ T[1][((x[(c + 1) & 3] >> 8) & 0xff) as usize]
                ^ T[2][((x[(c + 2) & 3] >> 16) & 0xff) as usize]
                ^ T[3][(x[(c + 3) & 3] >> 24) as usize];
            out[4 * c..4 * c + 4].copy_from_slice(&y.to_le_bytes());
        }
        out
    }
}

/// The first ten AES-256 round keys of `key` (FIPS-197 key expansion; the
/// same schedule `aes_expand_key`, `slow-hash-x86.c:252`, derives with
/// `AESKEYGENASSIST` and `oaes_key_import_data` in software).
pub(crate) fn expand_key(key: &[u8; 32]) -> [Block; 10] {
    const RCON: [u8; 4] = [0x01, 0x02, 0x04, 0x08];
    let mut w = [[0u8; 4]; 40];
    for (i, word) in w.iter_mut().take(8).enumerate() {
        word.copy_from_slice(&key[4 * i..4 * i + 4]);
    }
    for i in 8..40 {
        let mut t = w[i - 1];
        if i % 8 == 0 {
            // RotWord, SubWord, Rcon.
            t = [SBOX[t[1] as usize] ^ RCON[i / 8 - 1], SBOX[t[2] as usize], SBOX[t[3] as usize], SBOX[t[0] as usize]];
        } else if i % 8 == 4 {
            t = t.map(|b| SBOX[b as usize]);
        }
        for k in 0..4 {
            w[i][k] = w[i - 8][k] ^ t[k];
        }
    }
    let mut keys = [[0u8; 16]; 10];
    for (r, key) in keys.iter_mut().enumerate() {
        for c in 0..4 {
            key[4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
        }
    }
    keys
}

/// `AESENC`. Only ever instantiated inside a function compiled with the `aes`
/// target feature, entered after run-time detection (`super::hash`).
#[cfg(target_arch = "x86_64")]
pub(crate) struct AesNi;

#[cfg(target_arch = "x86_64")]
impl Aes for AesNi {
    #[inline(always)]
    fn round(block: Block, key: &Block) -> Block {
        use core::arch::x86_64::{_mm_aesenc_si128, _mm_loadu_si128, _mm_storeu_si128};
        let mut out = [0u8; 16];
        // SAFETY: the loads and the store cover exactly the 16-byte arrays;
        // AESENC is available because this is only inlined into a function
        // compiled with `aes` and reached after `is_x86_feature_detected!("aes")`.
        unsafe {
            let r = _mm_aesenc_si128(_mm_loadu_si128(block.as_ptr().cast()), _mm_loadu_si128(key.as_ptr().cast()));
            _mm_storeu_si128(out.as_mut_ptr().cast(), r);
        }
        out
    }
}

/// `AESENC` from the ARMv8 crypto extensions: `AESE` with a zero key is
/// ShiftRows + SubBytes, `AESMC` is MixColumns, and the round key is xored
/// last. Same entry rule as [`AesNi`].
#[cfg(target_arch = "aarch64")]
pub(crate) struct ArmAes;

#[cfg(target_arch = "aarch64")]
impl Aes for ArmAes {
    #[inline(always)]
    fn round(block: Block, key: &Block) -> Block {
        use core::arch::aarch64::{vaeseq_u8, vaesmcq_u8, vdupq_n_u8, veorq_u8, vld1q_u8, vst1q_u8};
        let mut out = [0u8; 16];
        // SAFETY: the loads and the store cover exactly the 16-byte arrays;
        // the AES instructions are available because this is only inlined
        // into a function compiled with `aes` and reached after
        // `is_aarch64_feature_detected!("aes")`.
        unsafe {
            let r = veorq_u8(vaesmcq_u8(vaeseq_u8(vld1q_u8(block.as_ptr()), vdupq_n_u8(0))), vld1q_u8(key.as_ptr()));
            vst1q_u8(out.as_mut_ptr(), r);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS-197 appendix C.3: the AES-256 key schedule of 00 01 .. 1f
    /// (`round[r].k_sch`). The rest of the schedule is pinned by the
    /// differential tests against the C, whose x86 path derives it with
    /// `AESKEYGENASSIST`.
    #[test]
    fn key_schedule_matches_fips_197() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let ks = expand_key(&key);
        assert_eq!(ks[0], key[..16]);
        assert_eq!(ks[1], key[16..]);
        let hex = |b: &Block| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(&ks[2]), "a573c29fa176c498a97fce93a572c09c");
        assert_eq!(hex(&ks[3]), "1651a8cd0244beda1a5da4c10640bade");
    }

    /// With an all-zero key, one round of the zero block is MixColumns of
    /// SubBytes(0) = 0x63 everywhere, and MixColumns of a constant column is
    /// the constant itself.
    #[test]
    fn soft_round_of_zero() {
        assert_eq!(Soft::round([0; 16], &[0; 16]), [0x63; 16]);
    }

    #[test]
    fn hardware_round_agrees_with_the_tables() {
        let mut x = 0x0123_4567_89ab_cdefu64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut block = |_: ()| -> Block {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&next().to_le_bytes());
            b[8..].copy_from_slice(&next().to_le_bytes());
            b
        };
        let pairs: Vec<(Block, Block)> = (0..1000).map(|_| (block(()), block(()))).collect();
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("aes") {
            for (b, k) in &pairs {
                assert_eq!(AesNi::round(*b, k), Soft::round(*b, k));
            }
        }
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("aes") {
            for (b, k) in &pairs {
                assert_eq!(ArmAes::round(*b, k), Soft::round(*b, k));
            }
        }
        let _ = pairs;
    }
}
