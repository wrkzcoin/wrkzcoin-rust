// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! BLAKE-256 (the SHA-3 finalist, 14 rounds), the first CryptoNight finalizer.
//!
//! A line-for-line port of the vendored `blake256.c` (`blake256_hash`), kept
//! bit-length oriented as the C is, so its padding arithmetic — including the
//! `nullt` counter rule and the modulo-512 test at the top of `update` — is
//! the C's own rather than a reading of the specification.

const SIGMA: [[usize; 16]; 14] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
];

const CST: [u32; 16] = [
    0x243F6A88, 0x85A308D3, 0x13198A2E, 0x03707344, 0xA4093822, 0x299F31D0, 0x082EFA98, 0xEC4E6C89, 0x452821E6,
    0x38D01377, 0xBE5466CF, 0x34E90C6C, 0xC0AC29B7, 0xC97C50DD, 0x3F84D5B5, 0xB5470917,
];

const PADDING: [u8; 64] = {
    let mut p = [0u8; 64];
    p[0] = 0x80;
    p
};

struct State {
    h: [u32; 8],
    s: [u32; 4],
    t: [u32; 2],
    /// Bits waiting in `buf`.
    buflen: usize,
    nullt: bool,
    buf: [u8; 64],
}

impl State {
    fn new() -> Self {
        State {
            h: [0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19],
            s: [0; 4],
            t: [0; 2],
            buflen: 0,
            nullt: false,
            buf: [0; 64],
        }
    }

    /// `blake256_compress` (blake256.c:61).
    fn compress(&mut self, block: &[u8]) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_be_bytes(block[4 * i..4 * i + 4].try_into().unwrap());
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&self.h);
        for i in 0..4 {
            v[8 + i] = self.s[i] ^ CST[i];
        }
        v[12..].copy_from_slice(&CST[4..8]);
        if !self.nullt {
            v[12] ^= self.t[0];
            v[13] ^= self.t[0];
            v[14] ^= self.t[1];
            v[15] ^= self.t[1];
        }
        for sigma in &SIGMA {
            let mut g = |a: usize, b: usize, c: usize, d: usize, e: usize| {
                v[a] = v[a].wrapping_add(m[sigma[e]] ^ CST[sigma[e + 1]]).wrapping_add(v[b]);
                v[d] = (v[d] ^ v[a]).rotate_right(16);
                v[c] = v[c].wrapping_add(v[d]);
                v[b] = (v[b] ^ v[c]).rotate_right(12);
                v[a] = v[a].wrapping_add(m[sigma[e + 1]] ^ CST[sigma[e]]).wrapping_add(v[b]);
                v[d] = (v[d] ^ v[a]).rotate_right(8);
                v[c] = v[c].wrapping_add(v[d]);
                v[b] = (v[b] ^ v[c]).rotate_right(7);
            };
            g(0, 4, 8, 12, 0);
            g(1, 5, 9, 13, 2);
            g(2, 6, 10, 14, 4);
            g(3, 7, 11, 15, 6);
            g(3, 4, 9, 14, 14);
            g(2, 7, 8, 13, 12);
            g(0, 5, 10, 15, 8);
            g(1, 6, 11, 12, 10);
        }
        for (i, x) in v.iter().enumerate() {
            self.h[i % 8] ^= x;
        }
        for i in 0..8 {
            self.h[i] ^= self.s[i % 4];
        }
    }

    fn count_block(&mut self) {
        self.t[0] = self.t[0].wrapping_add(512);
        if self.t[0] == 0 {
            self.t[1] = self.t[1].wrapping_add(1);
        }
    }

    /// `blake256_update` (blake256.c:138); `bits` is always a multiple of 8 here.
    fn update(&mut self, mut data: &[u8], mut bits: u64) {
        let mut left = self.buflen >> 3;
        let fill = 64 - left;
        if left != 0 && ((bits >> 3) & 0x3F) as usize >= fill {
            self.buf[left..].copy_from_slice(&data[..fill]);
            self.count_block();
            let buf = self.buf;
            self.compress(&buf);
            data = &data[fill..];
            bits -= (fill << 3) as u64;
            left = 0;
        }
        while bits >= 512 {
            self.count_block();
            self.compress(&data[..64]);
            data = &data[64..];
            bits -= 512;
        }
        if bits > 0 {
            let n = (bits >> 3) as usize;
            self.buf[left..left + n].copy_from_slice(&data[..n]);
            self.buflen = (left << 3) + bits as usize;
        } else {
            self.buflen = 0;
        }
    }

    /// `blake256_final_h(S, digest, 0x81, 0x01)` (blake256.c:180).
    fn finalize(mut self) -> [u8; 32] {
        let buflen = self.buflen as u32;
        let lo = self.t[0].wrapping_add(buflen);
        let mut hi = self.t[1];
        if lo < buflen {
            hi = hi.wrapping_add(1);
        }
        let mut msglen = [0u8; 8];
        msglen[..4].copy_from_slice(&hi.to_be_bytes());
        msglen[4..].copy_from_slice(&lo.to_be_bytes());

        if buflen == 440 {
            // One padding byte.
            self.t[0] = self.t[0].wrapping_sub(8);
            self.update(&[0x81], 8);
        } else {
            if buflen < 440 {
                // Enough space to fill the block.
                if buflen == 0 {
                    self.nullt = true;
                }
                self.t[0] = self.t[0].wrapping_sub(440 - buflen);
                self.update(&PADDING, (440 - buflen) as u64);
            } else {
                // Two compressions.
                self.t[0] = self.t[0].wrapping_sub(512 - buflen);
                self.update(&PADDING, (512 - buflen) as u64);
                self.t[0] = self.t[0].wrapping_sub(440);
                self.update(&PADDING[1..], 440);
                self.nullt = true;
            }
            self.update(&[0x01], 8);
            self.t[0] = self.t[0].wrapping_sub(8);
        }
        self.t[0] = self.t[0].wrapping_sub(64);
        self.update(&msglen, 64);

        let mut out = [0u8; 32];
        for (i, h) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&h.to_be_bytes());
        }
        out
    }
}

/// `blake256_hash` (blake256.c:237).
pub(crate) fn blake256(data: &[u8]) -> [u8; 32] {
    let mut s = State::new();
    s.update(data, data.len() as u64 * 8);
    s.finalize()
}

#[cfg(test)]
mod tests {
    /// The BLAKE-256 test vectors of the SHA-3 submission (one zero byte, and
    /// 72 zero bytes: the second takes the two-compression padding path).
    #[test]
    fn submission_vectors() {
        let hex = |b: [u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(super::blake256(&[0])), "0ce8d4ef4dd7cd8d62dfded9d4edb0a774ae6a41929a74da23109e8f11139c87");
        assert_eq!(hex(super::blake256(&[0; 72])), "d419bad32d504fb7d44d460c42c5593fe544fa4c135dec31e21bd9abdcc22d41");
    }
}
