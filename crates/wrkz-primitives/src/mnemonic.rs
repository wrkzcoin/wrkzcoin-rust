// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! 25-word mnemonic seeds (`src/mnemonics/Mnemonics.cpp`, spec/05).

use crate::wordlist::ENGLISH;

const WL: u32 = 1626;

/// Index of `word` in [`ENGLISH`], which is sorted, so a binary search replaces
/// the 1626-entry scan the C++ `std::find` does.
fn word_index(word: &str) -> Option<usize> {
    ENGLISH.binary_search(&word).ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MnemonicError {
    /// Not 25 words.
    WrongLength(usize),
    /// A word not in the list.
    InvalidWord(String),
    InvalidChecksum,
    /// A word triple that does not decode (`val % 1626 != w1`).
    Invalid,
}

/// Standard reflected CRC-32 (polynomial 0xEDB88320), as `CRC32.h`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn checksum_word(words: &[&str]) -> &'static str {
    let mut trimmed = String::new();
    for w in words {
        trimmed.push_str(&w[..w.len().min(3)]);
    }
    let idx = (crc32(trimmed.as_bytes()) as u64 % words.len() as u64) as usize;
    // the checksum word is one of the given words; return the list entry with the same text
    let w = words[idx];
    word_index(w).map_or("", |i| ENGLISH[i])
}

/// `PrivateKeyToMnemonic`: 24 words from the 8 little-endian u32 chunks plus the checksum word.
pub fn private_key_to_mnemonic(key: &[u8; 32]) -> String {
    let mut words: Vec<&str> = Vec::with_capacity(25);
    for i in (0..32).step_by(4) {
        let val = u32::from_le_bytes(key[i..i + 4].try_into().unwrap());
        let w1 = val % WL;
        let w2 = (val / WL + w1) % WL;
        let w3 = (val / WL / WL + w2) % WL;
        words.push(ENGLISH[w1 as usize]);
        words.push(ENGLISH[w2 as usize]);
        words.push(ENGLISH[w3 as usize]);
    }
    let cs = checksum_word(&words);
    words.push(cs);
    words.join(" ")
}

/// `MnemonicToPrivateKey`: whitespace-separated words, case-insensitive.
/// The result is the raw 32 bytes; the caller validates with `sc_check`.
pub fn mnemonic_to_private_key(text: &str) -> Result<[u8; 32], MnemonicError> {
    let lowered: Vec<String> = text.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    if lowered.len() != 25 {
        return Err(MnemonicError::WrongLength(lowered.len()));
    }
    let mut idx = Vec::with_capacity(25);
    let mut words: Vec<&str> = Vec::with_capacity(25);
    for w in &lowered {
        let Some(i) = word_index(w) else {
            return Err(MnemonicError::InvalidWord(w.clone()));
        };
        idx.push(i as u32);
        words.push(ENGLISH[i]);
    }
    if checksum_word(&words[..24]) != words[24] {
        return Err(MnemonicError::InvalidChecksum);
    }
    let mut out = [0u8; 32];
    for (k, i) in (0..24).step_by(3).enumerate() {
        let (w1, w2, w3) = (idx[i], idx[i + 1], idx[i + 2]);
        let val = w1
            .wrapping_add(WL.wrapping_mul((WL - w1 + w2) % WL))
            .wrapping_add(WL.wrapping_mul(WL).wrapping_mul((WL - w2 + w3) % WL));
        if val % WL != w1 {
            return Err(MnemonicError::Invalid);
        }
        out[4 * k..4 * k + 4].copy_from_slice(&val.to_le_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell different dating intended richly slower inundate ridges slug inundate ridges slug were rotate rudely viewpoint";
    const KEY: &str = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";

    #[test]
    fn spec_vector_round_trip() {
        let key: [u8; 32] = hex::decode(KEY).unwrap().try_into().unwrap();
        assert_eq!(private_key_to_mnemonic(&key), SEED);
        assert_eq!(mnemonic_to_private_key(SEED), Ok(key));
        assert_eq!(mnemonic_to_private_key(&SEED.to_uppercase()), Ok(key));
    }

    #[test]
    fn rejects_bad_input() {
        let mut words: Vec<&str> = SEED.split(' ').collect();
        words[24] = "abbey";
        assert_eq!(mnemonic_to_private_key(&words.join(" ")), Err(MnemonicError::InvalidChecksum));
        assert_eq!(mnemonic_to_private_key("abbey"), Err(MnemonicError::WrongLength(1)));
        let mut w2: Vec<&str> = SEED.split(' ').collect();
        w2[0] = "notaword";
        assert!(matches!(mnemonic_to_private_key(&w2.join(" ")), Err(MnemonicError::InvalidWord(_))));
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(ENGLISH.len(), 1626);
    }

    #[test]
    fn random_keys_round_trip() {
        for seed in 0u8..40 {
            let (sec, _) = wrkz_pow::curve::generate_deterministic_keys(&[seed; 32]);
            let m = private_key_to_mnemonic(&sec);
            assert_eq!(m.split(' ').count(), 25);
            assert_eq!(mnemonic_to_private_key(&m), Ok(sec));
        }
    }
}
