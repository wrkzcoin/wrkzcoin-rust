// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Unsigned LEB128 (`src/common/Varint.h`, spec/04-serialization.md "varint").

use crate::{Error, Result};

/// Append the varint encoding of `v` to `out`.
pub fn write(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn encode(v: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    write(&mut out, v);
    out
}

/// Read a varint that must fit in `bits` bits (8, 16, 32 or 64). Rejects
/// overflow and a non-canonical zero continuation byte, like `read_varint`.
pub fn read_bits(input: &[u8], bits: u32) -> Result<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in input.iter().enumerate() {
        if shift >= bits {
            return Err(Error::BadVarint);
        }
        if b == 0 && shift != 0 {
            return Err(Error::BadVarint);
        }
        let piece = (b & 0x7f) as u64;
        // overflow check: the piece must fit in the remaining bits
        if shift + 7 > bits && (piece >> (bits - shift)) != 0 {
            return Err(Error::BadVarint);
        }
        value |= piece << shift;
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    Err(Error::Truncated)
}

pub fn read(input: &[u8]) -> Result<(u64, usize)> {
    read_bits(input, 64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_vectors() {
        let cases: &[(u64, &str)] = &[
            (0, "00"),
            (1, "01"),
            (127, "7f"),
            (128, "8001"),
            (300, "ac02"),
            (999730, "b2823d"),
            (4294967296, "8080808010"),
            (u64::MAX, "ffffffffffffffffff01"),
        ];
        for (v, h) in cases {
            assert_eq!(hex::encode(encode(*v)), *h);
            assert_eq!(read(&hex::decode(h).unwrap()).unwrap(), (*v, h.len() / 2));
        }
    }

    #[test]
    fn rejects_noncanonical_and_overflow() {
        assert_eq!(read(&[0x80, 0x00]), Err(Error::BadVarint));
        assert_eq!(read(&[0x80]), Err(Error::Truncated));
        assert_eq!(read_bits(&[0x80, 0x02], 8), Err(Error::BadVarint));
        assert_eq!(read_bits(&[0xff, 0x01], 8).unwrap().0, 255);
        assert!(read(&[0xff; 11]).is_err());
    }
}
