// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Record structures (`BlockchainCache.cpp:86-116`, `DatabaseCacheData.cpp:14-27`,
//! `DBUtils.cpp:20`) in their KV and binary encodings.

use crate::codec::{self, req_hash, req_u16, req_u32, req_u64, u32_array, u64_array};
use crate::{Result, StorageError};
use wrkz_primitives::kv::{Section, Value, TYPE_UINT32, TYPE_UINT64};
use wrkz_primitives::{varint, Error};

/// Prefix `6`: per-block totals.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CachedBlockInfo {
    pub block_hash: [u8; 32],
    pub timestamp: u64,
    /// The cumulative size used by the reward rule.
    pub block_size: u32,
    pub cumulative_difficulty: u64,
    pub already_generated_coins: u64,
    pub already_generated_transactions: u64,
}

impl CachedBlockInfo {
    pub fn to_section(&self) -> Section {
        Section::new()
            .string("block_hash", &self.block_hash)
            .u64("timestamp", self.timestamp)
            .u32("block_size", self.block_size)
            .u64("cumulative_difficulty", self.cumulative_difficulty)
            .u64("already_generated_coins", self.already_generated_coins)
            .u64("already_generated_transaction_count", self.already_generated_transactions)
    }

    pub fn from_section(s: &Section) -> Result<Self> {
        Ok(Self {
            block_hash: req_hash(s, "block_hash")?,
            timestamp: req_u64(s, "timestamp")?,
            block_size: req_u32(s, "block_size")?,
            cumulative_difficulty: req_u64(s, "cumulative_difficulty")?,
            already_generated_coins: req_u64(s, "already_generated_coins")?,
            already_generated_transactions: req_u64(s, "already_generated_transaction_count")?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        codec::value(codec::BLOCK_INDEX_TO_BLOCK_INFO, Value::Object(self.to_section()))
    }

    pub fn decode(doc: &[u8]) -> Result<Self> {
        Self::from_section(&codec::decode_object(codec::BLOCK_INDEX_TO_BLOCK_INFO, doc)?)
    }
}

/// A `KeyInput` in KV form: `{ amount, key_offsets: [u32], k_image }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInputRecord {
    pub amount: u64,
    pub key_offsets: Vec<u32>,
    pub key_image: [u8; 32],
}

/// Prefix `a` (value part `cached_transaction`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CachedTransactionInfo {
    pub block_index: u32,
    /// Position in the block, coinbase 0.
    pub transaction_index: u32,
    pub transaction_hash: [u8; 32],
    pub unlock_time: u64,
    /// Output target keys (`TransactionOutputTarget`: `{ type: 0x02, data: { key } }`).
    pub output_keys: Vec<[u8; 32]>,
    pub output_amounts: Vec<u64>,
    pub global_indexes: Vec<u32>,
    pub key_inputs: Vec<KeyInputRecord>,
    pub transaction_public_key: [u8; 32],
    /// Raw payment id bytes (32 when present), empty otherwise.
    pub payment_id: Vec<u8>,
}

impl CachedTransactionInfo {
    pub fn to_section(&self) -> Section {
        let outputs: Vec<Section> = self
            .output_keys
            .iter()
            .map(|k| Section::new().string("type", &[0x02]).object("data", Section::new().string("key", k)))
            .collect();
        let inputs: Vec<Section> = self
            .key_inputs
            .iter()
            .map(|i| {
                let mut s = Section::new().u64("amount", i.amount);
                if !i.key_offsets.is_empty() {
                    s.entries.push((
                        "key_offsets".into(),
                        Value::Array(i.key_offsets.iter().map(|o| Value::Uint(*o as u64, TYPE_UINT32)).collect()),
                    ));
                }
                s.string("k_image", &i.key_image)
            })
            .collect();
        let mut s = Section::new()
            .u32("block_index", self.block_index)
            .u32("transaction_index", self.transaction_index)
            .string("transaction_hash", &self.transaction_hash)
            .u64("unlock_time", self.unlock_time)
            .object_array("outputs", outputs);
        if !self.output_amounts.is_empty() {
            s.entries.push((
                "output_amounts".into(),
                Value::Array(self.output_amounts.iter().map(|a| Value::Uint(*a, TYPE_UINT64)).collect()),
            ));
        }
        if !self.global_indexes.is_empty() {
            s.entries.push((
                "global_indexes".into(),
                Value::Array(self.global_indexes.iter().map(|g| Value::Uint(*g as u64, TYPE_UINT32)).collect()),
            ));
        }
        s = s.object_array("key_inputs", inputs).string("tx_public_key", &self.transaction_public_key);
        // Pushed directly rather than through `Section::string`, which drops an
        // empty value: `paymentId` is a `std::string` (`BlockchainCache.h:59`)
        // and `KVBinaryOutputStreamSerializer::operator()(std::string&)`
        // (line 207) writes the entry unconditionally. Only `binary()` (line
        // 217) has the `if (size > 0)` suppression, so an empty payment id is
        // present with length zero in the C++ output too. Do not "fix" this.
        s.entries.push(("payment_id".into(), Value::String(self.payment_id.clone())));
        s
    }

    pub fn from_section(s: &Section) -> Result<Self> {
        let mut output_keys = Vec::new();
        for o in s.get_array("outputs") {
            let Value::Object(os) = o else { return Err(StorageError::Decode("outputs entry".into())) };
            let data = os.get_object("data").ok_or(StorageError::Missing("data"))?;
            output_keys.push(req_hash(data, "key")?);
        }
        let mut key_inputs = Vec::new();
        for i in s.get_array("key_inputs") {
            let Value::Object(is) = i else { return Err(StorageError::Decode("key_inputs entry".into())) };
            key_inputs.push(KeyInputRecord {
                amount: req_u64(is, "amount")?,
                key_offsets: u32_array(is, "key_offsets")?,
                key_image: req_hash(is, "k_image")?,
            });
        }
        Ok(Self {
            block_index: req_u32(s, "block_index")?,
            transaction_index: req_u32(s, "transaction_index")?,
            transaction_hash: req_hash(s, "transaction_hash")?,
            unlock_time: req_u64(s, "unlock_time")?,
            output_keys,
            output_amounts: u64_array(s, "output_amounts")?,
            global_indexes: u32_array(s, "global_indexes")?,
            key_inputs,
            transaction_public_key: req_hash(s, "tx_public_key")?,
            payment_id: s.get_bytes("payment_id").map(|b| b.to_vec()).unwrap_or_default(),
        })
    }

    pub fn sum_inputs(&self) -> u64 {
        self.key_inputs.iter().map(|i| i.amount).sum()
    }
    pub fn sum_outputs(&self) -> u64 {
        self.output_amounts.iter().sum()
    }
    pub fn fee(&self) -> u64 {
        self.sum_inputs().saturating_sub(self.sum_outputs())
    }
}

/// Prefix `a`: `{ cached_transaction, key_indexes: [{ key: amount, value: [global indexes] }] }`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ExtendedTransactionInfo {
    pub info: CachedTransactionInfo,
    /// amount → global indexes of this transaction's outputs of that amount (ascending amount)
    pub amount_to_key_indexes: Vec<(u64, Vec<u32>)>,
}

impl ExtendedTransactionInfo {
    pub fn to_section(&self) -> Section {
        let entries: Vec<Section> = self
            .amount_to_key_indexes
            .iter()
            .map(|(amount, idx)| {
                let mut s = Section::new().u64("key", *amount);
                if !idx.is_empty() {
                    s.entries.push((
                        "value".into(),
                        Value::Array(idx.iter().map(|g| Value::Uint(*g as u64, TYPE_UINT32)).collect()),
                    ));
                }
                s
            })
            .collect();
        Section::new().object("cached_transaction", self.info.to_section()).object_array("key_indexes", entries)
    }

    pub fn from_section(s: &Section) -> Result<Self> {
        let info = CachedTransactionInfo::from_section(
            s.get_object("cached_transaction").ok_or(StorageError::Missing("cached_transaction"))?,
        )?;
        let mut amount_to_key_indexes = Vec::new();
        for e in s.get_array("key_indexes") {
            let Value::Object(es) = e else { return Err(StorageError::Decode("key_indexes entry".into())) };
            amount_to_key_indexes.push((req_u64(es, "key")?, u32_array(es, "value")?));
        }
        Ok(Self { info, amount_to_key_indexes })
    }

    pub fn encode(&self) -> Vec<u8> {
        codec::value(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, Value::Object(self.to_section()))
    }

    pub fn decode(doc: &[u8]) -> Result<Self> {
        Self::from_section(&codec::decode_object(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, doc)?)
    }
}

/// Prefix `j`: the output at (amount, global index).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct KeyOutputInfo {
    pub public_key: [u8; 32],
    pub transaction_hash: [u8; 32],
    pub unlock_time: u64,
    pub output_index: u16,
    pub block_index: u32,
}

impl KeyOutputInfo {
    pub fn to_section(&self) -> Section {
        Section::new()
            .string("public_key", &self.public_key)
            .string("transaction_hash", &self.transaction_hash)
            .u64("unlock_time", self.unlock_time)
            .u16("output_index", self.output_index)
            .u32("block_index", self.block_index)
    }

    pub fn from_section(s: &Section) -> Result<Self> {
        Ok(Self {
            public_key: req_hash(s, "public_key")?,
            transaction_hash: req_hash(s, "transaction_hash")?,
            unlock_time: req_u64(s, "unlock_time")?,
            output_index: req_u16(s, "output_index")?,
            block_index: req_u32(s, "block_index")?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        codec::value(codec::KEY_OUTPUT_KEY, Value::Object(self.to_section()))
    }

    pub fn decode(doc: &[u8]) -> Result<Self> {
        Self::from_section(&codec::decode_object(codec::KEY_OUTPUT_KEY, doc)?)
    }
}

/// Prefix `4`: the raw block record (`DB::serialize(RawBlock)`, `DBUtils.cpp:20`).
/// **Not** a KV document: the binary serializer's generic container path, so
/// every byte is written as a varint (bytes >= 0x80 take two bytes).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RawBlockRecord {
    pub block: Vec<u8>,
    pub transactions: Vec<Vec<u8>>,
}

fn write_byte_vec(out: &mut Vec<u8>, v: &[u8]) {
    varint::write(out, v.len() as u64);
    for &b in v {
        varint::write(out, b as u64);
    }
}

fn read_byte_vec(data: &[u8], pos: &mut usize) -> std::result::Result<Vec<u8>, Error> {
    let (n, k) = varint::read(&data[*pos..])?;
    *pos += k;
    if n > (data.len() - *pos) as u64 {
        return Err(Error::Truncated);
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let (b, k) = varint::read_bits(&data[*pos..], 8)?;
        *pos += k;
        out.push(b as u8);
    }
    Ok(out)
}

impl RawBlockRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.block.len() * 2);
        write_byte_vec(&mut out, &self.block);
        varint::write(&mut out, self.transactions.len() as u64);
        for t in &self.transactions {
            write_byte_vec(&mut out, t);
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let block = read_byte_vec(data, &mut pos)?;
        let (n, k) = varint::read(&data[pos..])?;
        pos += k;
        let mut transactions = Vec::with_capacity(n.min(10_000) as usize);
        for _ in 0..n {
            transactions.push(read_byte_vec(data, &mut pos)?);
        }
        if pos != data.len() {
            return Err(Error::TrailingBytes(data.len() - pos).into());
        }
        Ok(Self { block, transactions })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_info_round_trip() {
        let b = CachedBlockInfo {
            block_hash: [9; 32],
            timestamp: 1788932901,
            block_size: 1032,
            cumulative_difficulty: 1 << 40,
            already_generated_coins: 123,
            already_generated_transactions: 3_510_849,
        };
        let doc = b.encode();
        assert_eq!(CachedBlockInfo::decode(&doc).unwrap(), b);
        // block_size is uint32 (type 6)
        let s = codec::decode_object("6", &doc).unwrap();
        assert!(matches!(s.get("block_size"), Some(Value::Uint(1032, 6))));
        assert!(matches!(s.get("timestamp"), Some(Value::Uint(_, 5))));
    }

    #[test]
    fn transaction_info_round_trip() {
        let t = ExtendedTransactionInfo {
            info: CachedTransactionInfo {
                block_index: 4213650,
                transaction_index: 1,
                transaction_hash: [1; 32],
                unlock_time: 4213685,
                output_keys: vec![[2; 32], [3; 32]],
                output_amounts: vec![10000, 50000],
                global_indexes: vec![1787444, 726939],
                key_inputs: vec![KeyInputRecord { amount: 60070, key_offsets: vec![5, 7], key_image: [4; 32] }],
                transaction_public_key: [5; 32],
                payment_id: vec![],
            },
            amount_to_key_indexes: vec![(10000, vec![1787444]), (50000, vec![726939])],
        };
        let doc = t.encode();
        assert_eq!(ExtendedTransactionInfo::decode(&doc).unwrap(), t);
        assert_eq!(t.info.fee(), 70);
        let s = codec::decode_object("a", &doc).unwrap();
        let ct = s.get_object("cached_transaction").unwrap();
        // outputs: [{ type: "\x02", data: { key } }]
        let Value::Object(o) = &ct.get_array("outputs")[0] else { panic!() };
        assert_eq!(o.get_bytes("type"), Some(&[2u8][..]));
        // key_offsets are uint32
        let Value::Object(ki) = &ct.get_array("key_inputs")[0] else { panic!() };
        assert!(matches!(ki.get_array("key_offsets")[0], Value::Uint(5, 6)));
    }

    /// A malformed array must be an error, not a short array that then misaligns
    /// against `outputs` / `output_amounts`.
    #[test]
    fn rejects_a_malformed_integer_array() {
        let mut info = CachedTransactionInfo {
            transaction_hash: [1; 32],
            transaction_public_key: [2; 32],
            output_keys: vec![[3; 32]],
            output_amounts: vec![7],
            global_indexes: vec![9],
            ..Default::default()
        };
        let mut s = info.to_section();
        for (name, v) in s.entries.iter_mut() {
            if name == "global_indexes" {
                *v = Value::Array(vec![Value::String(b"nine".to_vec())]);
            }
        }
        let doc = codec::value("a", Value::Object(Section::new().object("cached_transaction", s)));
        assert!(ExtendedTransactionInfo::decode(&doc).is_err());
        // A global index that does not fit uint32 is rejected rather than truncated.
        info.global_indexes = vec![1];
        let mut s = info.to_section();
        for (name, v) in s.entries.iter_mut() {
            if name == "global_indexes" {
                *v = Value::Array(vec![Value::Uint(1 << 33, TYPE_UINT64)]);
            }
        }
        let doc = codec::value("a", Value::Object(Section::new().object("cached_transaction", s)));
        assert!(ExtendedTransactionInfo::decode(&doc).is_err());
    }

    #[test]
    fn key_output_round_trip() {
        let k = KeyOutputInfo {
            public_key: [7; 32],
            transaction_hash: [8; 32],
            unlock_time: 4213690,
            output_index: 3,
            block_index: 4213650,
        };
        let doc = k.encode();
        assert_eq!(KeyOutputInfo::decode(&doc).unwrap(), k);
        let s = codec::decode_object("j", &doc).unwrap();
        assert!(matches!(s.get("output_index"), Some(Value::Uint(3, 7))));
    }

    #[test]
    fn raw_block_record_varint_per_byte() {
        let r = RawBlockRecord { block: vec![0x01, 0x7f, 0x80, 0xff], transactions: vec![vec![0x90], vec![]] };
        let enc = r.encode();
        // 4 bytes: 01 7f, then 0x80 -> 80 01, 0xff -> ff 01 ; tx count 2; tx1: len 1, 0x90 -> 90 01; tx2: len 0
        assert_eq!(enc, vec![0x04, 0x01, 0x7f, 0x80, 0x01, 0xff, 0x01, 0x02, 0x01, 0x90, 0x01, 0x00]);
        assert_eq!(RawBlockRecord::decode(&enc).unwrap(), r);
        let mut bad = enc.clone();
        bad.push(0);
        assert!(RawBlockRecord::decode(&bad).is_err());
    }
}
