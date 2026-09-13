// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Key and value documents (`DBUtils.h`, spec/11 "Keys and values are KV binary documents").
//!
//! ```text
//! key   = KVdoc{ <prefix>: { first: <prefix>, second: <key> } }
//! value = KVdoc{ <prefix>: <value> }
//! ```
//!
//! The KV type of each integer follows the C++ field width: block indexes and
//! global output indexes are `uint32` (type 6), amounts, timestamps and counts
//! `uint64` (type 5), `outputIndex` `uint16` (type 7).

use crate::{Result, StorageError};
use wrkz_primitives::kv::{self, Section, Value, TYPE_UINT32, TYPE_UINT64};

pub const BLOCK_INDEX_TO_KEY_IMAGE: &str = "0";
pub const BLOCK_INDEX_TO_TX_HASHES: &str = "1";
pub const BLOCK_INDEX_TO_RAW_BLOCK: &str = "4";
pub const BLOCK_HASH_TO_BLOCK_INDEX: &str = "5";
pub const BLOCK_INDEX_TO_BLOCK_INFO: &str = "6";
pub const KEY_IMAGE_TO_BLOCK_INDEX: &str = "7";
pub const BLOCK_INDEX_TO_BLOCK_HASH: &str = "8";
pub const TRANSACTION_HASH_TO_TRANSACTION_INFO: &str = "a";
pub const KEY_OUTPUT_AMOUNT: &str = "b";
pub const CLOSEST_TIMESTAMP_BLOCK_INDEX: &str = "e";
pub const PAYMENT_ID_TO_TX_HASH: &str = "f";
pub const TIMESTAMP_TO_BLOCKHASHES: &str = "g";
pub const KEY_OUTPUT_AMOUNTS_COUNT: &str = "h";
pub const KEY_OUTPUT_KEY: &str = "j";
pub const LAST_BLOCK_INDEX_KEY: &str = "last_block_index";
pub const KEY_OUTPUT_AMOUNTS_COUNT_KEY: &str = "key_amounts_count";
pub const TRANSACTIONS_COUNT_KEY: &str = "txs_count";
/// Plain ASCII key, decimal string value (`DatabaseBlockchainCache.cpp:612`).
pub const DB_VERSION_KEY: &[u8] = b"db_scheme_version";

/// The `second` part of a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPart {
    U32(u32),
    U64(u64),
    Hash([u8; 32]),
    Str(&'static str),
    /// `pair<uint64 amount, uint32 global index>` (prefix `j`)
    AmountIndex(u64, u32),
    /// `pair<Hash payment id, uint32 ordinal>` (prefix `f`)
    HashOrdinal([u8; 32], u32),
}

impl KeyPart {
    fn attach(self, s: Section, name: &str) -> Section {
        match self {
            KeyPart::U32(v) => s.u32(name, v),
            KeyPart::U64(v) => s.u64(name, v),
            KeyPart::Hash(h) => s.string(name, &h),
            KeyPart::Str(t) => s.string(name, t.as_bytes()),
            KeyPart::AmountIndex(a, i) => s.object(name, Section::new().u64("first", a).u32("second", i)),
            KeyPart::HashOrdinal(h, i) => s.object(name, Section::new().string("first", &h).u32("second", i)),
        }
    }
}

/// `DB::serializeKey(prefix, key)`.
pub fn key(prefix: &str, part: KeyPart) -> Vec<u8> {
    let inner = part.attach(Section::new().string("first", prefix.as_bytes()), "second");
    kv::encode(&Section::new().object(prefix, inner))
}

/// `DB::serialize(value, prefix)` for a scalar or structured value.
pub fn value(prefix: &str, v: Value) -> Vec<u8> {
    kv::encode(&Section { entries: vec![(prefix.to_string(), v)] })
}

pub fn value_u32(prefix: &str, v: u32) -> Vec<u8> {
    value(prefix, Value::Uint(v as u64, TYPE_UINT32))
}

pub fn value_u64(prefix: &str, v: u64) -> Vec<u8> {
    value(prefix, Value::Uint(v, TYPE_UINT64))
}

pub fn value_hash(prefix: &str, h: &[u8; 32]) -> Vec<u8> {
    value(prefix, Value::String(h.to_vec()))
}

/// A list of 32-byte hashes as a KV array of strings (empty list: the entry is absent).
///
/// Element order is the caller's. It is meaningful for prefix `1` (block
/// transaction hashes, coinbase first) but not for prefix `0`, whose C++ source
/// is an `unordered_set<KeyImage>`: those records cannot be compared byte for
/// byte with the C++ output, only as sets.
pub fn value_hashes(prefix: &str, hashes: &[[u8; 32]]) -> Vec<u8> {
    if hashes.is_empty() {
        return kv::encode(&Section::new());
    }
    value(prefix, Value::Array(hashes.iter().map(|h| Value::String(h.to_vec())).collect()))
}

/// Decode a value document and return the single entry named `prefix`.
pub fn decode_value(prefix: &str, doc: &[u8]) -> Result<Option<Value>> {
    let s = kv::decode(doc)?;
    Ok(s.get(prefix).cloned())
}

pub fn decode_u64(prefix: &str, doc: &[u8]) -> Result<u64> {
    match decode_value(prefix, doc)? {
        Some(Value::Uint(v, _)) => Ok(v),
        Some(Value::Int(v, _)) => Ok(v as u64),
        _ => Err(StorageError::Decode(format!("{prefix}: expected integer"))),
    }
}

pub fn decode_hash(prefix: &str, doc: &[u8]) -> Result<[u8; 32]> {
    match decode_value(prefix, doc)? {
        Some(Value::String(s)) if s.len() == 32 => Ok(s.try_into().unwrap()),
        _ => Err(StorageError::Decode(format!("{prefix}: expected 32-byte string"))),
    }
}

pub fn decode_hashes(prefix: &str, doc: &[u8]) -> Result<Vec<[u8; 32]>> {
    match decode_value(prefix, doc)? {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                Value::String(s) if s.len() == 32 => Ok(s.try_into().unwrap()),
                _ => Err(StorageError::Decode(format!("{prefix}: bad hash entry"))),
            })
            .collect(),
        _ => Err(StorageError::Decode(format!("{prefix}: expected hash array"))),
    }
}

pub fn decode_object(prefix: &str, doc: &[u8]) -> Result<Section> {
    match decode_value(prefix, doc)? {
        Some(Value::Object(s)) => Ok(s),
        _ => Err(StorageError::Decode(format!("{prefix}: expected object"))),
    }
}

/// Helpers for typed fields inside record objects.
///
/// The KV reader widens every integer to 64 bits the way the C++ one does, so
/// the narrowing back to the declared field width happens here, and a value
/// that does not fit is a decode error rather than a silent truncation.
pub(crate) fn req_u64(s: &Section, name: &'static str) -> Result<u64> {
    s.get_u64(name).ok_or(StorageError::Missing(name))
}
pub(crate) fn req_u32(s: &Section, name: &'static str) -> Result<u32> {
    narrow(req_u64(s, name)?, name)
}
pub(crate) fn req_u16(s: &Section, name: &'static str) -> Result<u16> {
    narrow(req_u64(s, name)?, name)
}
fn narrow<T: TryFrom<u64>>(v: u64, name: &'static str) -> Result<T> {
    T::try_from(v).map_err(|_| StorageError::Decode(format!("{name}: {v} out of range")))
}
pub(crate) fn req_hash(s: &Section, name: &'static str) -> Result<[u8; 32]> {
    let b = s.get_bytes(name).ok_or(StorageError::Missing(name))?;
    b.try_into().map_err(|_| StorageError::Decode(format!("{name}: length")))
}
/// An array of integers. A missing entry is an empty array (the writer omits
/// empty ones); an element that is not an integer is an error, so a malformed
/// record cannot quietly yield a short array that then misaligns against
/// `outputs` or `output_amounts`.
pub(crate) fn u64_array(s: &Section, name: &'static str) -> Result<Vec<u64>> {
    s.get_array(name)
        .iter()
        .map(|v| match v {
            Value::Uint(u, _) => Ok(*u),
            Value::Int(i, _) => Ok(*i as u64),
            _ => Err(StorageError::Decode(format!("{name}: non-integer array element"))),
        })
        .collect()
}
pub(crate) fn u32_array(s: &Section, name: &'static str) -> Result<Vec<u32>> {
    u64_array(s, name)?.into_iter().map(|v| narrow(v, name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_and_value_bytes() {
        // key "8"/"last_block_index" -> { "8": { first: "8", second: "last_block_index" } }
        let k = key(BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(LAST_BLOCK_INDEX_KEY));
        let mut want = kv::HEADER.to_vec();
        want.extend_from_slice(&[0x04, 1, b'8', 12, 0x08]); // 1 entry "8" object with 2 entries
        want.extend_from_slice(&[5, b'f', b'i', b'r', b's', b't', 10, 0x04, b'8']);
        want.extend_from_slice(&[6, b's', b'e', b'c', b'o', b'n', b'd', 10, 16 << 2]);
        want.extend_from_slice(b"last_block_index");
        assert_eq!(k, want);
        // value: uint32 4213650 under "8"
        let v = value_u32(BLOCK_INDEX_TO_BLOCK_HASH, 4213650);
        let mut wantv = kv::HEADER.to_vec();
        wantv.extend_from_slice(&[0x04, 1, b'8', 6]);
        wantv.extend_from_slice(&4213650u32.to_le_bytes());
        assert_eq!(v, wantv);
        assert_eq!(decode_u64("8", &v).unwrap(), 4213650);
        // pair key
        let k2 = key(KEY_OUTPUT_KEY, KeyPart::AmountIndex(10000, 1787441));
        let s = kv::decode(&k2).unwrap();
        let inner = s.get_object("j").unwrap();
        let second = inner.get_object("second").unwrap();
        assert_eq!(second.get_u64("first"), Some(10000));
        assert!(matches!(second.get("second"), Some(Value::Uint(1787441, 6))));
        // hashes
        let hs = [[1u8; 32], [2u8; 32]];
        assert_eq!(decode_hashes("1", &value_hashes("1", &hs)).unwrap(), hs);
        assert_eq!(decode_hashes("1", &value_hashes("1", &[])).unwrap(), Vec::<[u8; 32]>::new());
    }
}
