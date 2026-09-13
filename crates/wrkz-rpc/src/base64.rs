// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `Common::toBase64` (`src/common/StringTools.cpp:212`), which is what
//! `/getwalletsyncdata` uses when the caller asks for `"encoding":"base64"`.
//!
//! Standard alphabet, `=` padding, no line breaks. Only the encoder is here:
//! nothing this server reads is base64.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes the way the C++ daemon does.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let (chunks, remainder) = data.as_chunks::<3>();
    for c in chunks {
        let triple = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        out.push(ALPHABET[triple as usize & 0x3f] as char);
    }
    match remainder.len() {
        1 => {
            let triple = (remainder[0] as u32) << 16;
            out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let triple = ((remainder[0] as u32) << 16) | ((remainder[1] as u32) << 8);
            out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_hash_encodes_to_44_characters() {
        // `podToBase64` of a 32-byte hash: 44 characters, one '=' of padding.
        let h = [0xffu8; 32];
        let s = encode(&h);
        assert_eq!(s.len(), 44);
        assert!(s.ends_with('='));
        assert_eq!(encode(&[0u8; 32]), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }
}
