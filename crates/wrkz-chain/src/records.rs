// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The values stored under the keys of [`crate::keys`].
//!
//! Fixed-layout little-endian structs. Every decoder checks the exact length,
//! so a truncated or foreign value is a decode error and never a field read out
//! of the next record.

use crate::{ChainError, Result};
use wrkz_primitives::Hash;

fn corrupt(what: &str, len: usize) -> ChainError {
    ChainError::Corrupt(format!("{what}: {len} bytes is not the record length"))
}

fn hash_at(b: &[u8], off: usize) -> Hash {
    let mut h = [0u8; 32];
    h.copy_from_slice(&b[off..off + 32]);
    h
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().expect("2 bytes"))
}

/// The per-block consensus state, the same six fields as the C++
/// `CachedBlockInfo` (`IBlockchainCache.h:85`, serialized at
/// `BlockchainCache.cpp:108`), in our own encoding.
///
/// `block_size` is the *cumulative* block size the reward rule uses
/// (`coinbase size + sum of transaction sizes`), not the size of the block
/// blob. `already_generated_coins` is the running emission **after** this
/// block; `already_generated_transactions` counts the coinbase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BlockInfo {
    pub block_hash: Hash,
    pub timestamp: u64,
    pub block_size: u32,
    pub cumulative_difficulty: u64,
    pub already_generated_coins: u64,
    pub already_generated_transactions: u64,
}

impl BlockInfo {
    pub const LEN: usize = 32 + 8 + 4 + 8 + 8 + 8;

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(Self::LEN);
        v.extend_from_slice(&self.block_hash);
        v.extend_from_slice(&self.timestamp.to_le_bytes());
        v.extend_from_slice(&self.block_size.to_le_bytes());
        v.extend_from_slice(&self.cumulative_difficulty.to_le_bytes());
        v.extend_from_slice(&self.already_generated_coins.to_le_bytes());
        v.extend_from_slice(&self.already_generated_transactions.to_le_bytes());
        v
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != Self::LEN {
            return Err(corrupt("block info", b.len()));
        }
        Ok(Self {
            block_hash: hash_at(b, 0),
            timestamp: u64_at(b, 32),
            block_size: u32_at(b, 40),
            cumulative_difficulty: u64_at(b, 44),
            already_generated_coins: u64_at(b, 52),
            already_generated_transactions: u64_at(b, 60),
        })
    }
}

/// One key output, the same fields as the C++ `KeyOutputInfo`
/// (`DatabaseCacheData.cpp:20`). This is what a ring member resolves to and
/// what the unlock check reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct OutputRecord {
    pub public_key: Hash,
    /// The *transaction's* unlock time, which is what
    /// `isTransactionSpendTimeUnlocked` is applied to.
    pub unlock_time: u64,
    pub transaction_hash: Hash,
    pub output_index: u16,
    pub block_index: u32,
}

impl OutputRecord {
    pub const LEN: usize = 32 + 8 + 32 + 2 + 4;

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(Self::LEN);
        v.extend_from_slice(&self.public_key);
        v.extend_from_slice(&self.unlock_time.to_le_bytes());
        v.extend_from_slice(&self.transaction_hash);
        v.extend_from_slice(&self.output_index.to_le_bytes());
        v.extend_from_slice(&self.block_index.to_le_bytes());
        v
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != Self::LEN {
            return Err(corrupt("output", b.len()));
        }
        Ok(Self {
            public_key: hash_at(b, 0),
            unlock_time: u64_at(b, 32),
            transaction_hash: hash_at(b, 40),
            output_index: u16_at(b, 72),
            block_index: u32_at(b, 74),
        })
    }
}

/// A list of 32-byte hashes (block transaction hashes, or the key images a
/// block spent).
pub fn encode_hashes(hashes: &[Hash]) -> Vec<u8> {
    let mut v = Vec::with_capacity(hashes.len() * 32);
    for h in hashes {
        v.extend_from_slice(h);
    }
    v
}

pub fn decode_hashes(b: &[u8]) -> Result<Vec<Hash>> {
    if !b.len().is_multiple_of(32) {
        return Err(corrupt("hash list", b.len()));
    }
    Ok(b.as_chunks::<32>().0.to_vec())
}

/// The `(payment id, transaction hash)` pairs a block contributed to the
/// payment-id index, in block order, so an unwind can take exactly those back
/// out again. 64 bytes each, both halves raw.
pub fn encode_payment_id_refs(refs: &[(Hash, Hash)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(refs.len() * 64);
    for (id, hash) in refs {
        v.extend_from_slice(id);
        v.extend_from_slice(hash);
    }
    v
}

pub fn decode_payment_id_refs(b: &[u8]) -> Result<Vec<(Hash, Hash)>> {
    if !b.len().is_multiple_of(64) {
        return Err(corrupt("payment id reference list", b.len()));
    }
    Ok(b.as_chunks::<64>()
        .0
        .iter()
        .map(|c| {
            let (id, hash) = c.split_at(32);
            (Hash::try_from(id).expect("32 bytes"), Hash::try_from(hash).expect("32 bytes"))
        })
        .collect())
}

/// The `(amount, global index)` pairs a block created, in creation order, so an
/// unwind can delete exactly those outputs and step the per-amount counters
/// back.
pub fn encode_output_refs(refs: &[(u64, u32)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(refs.len() * 12);
    for (amount, index) in refs {
        v.extend_from_slice(&amount.to_le_bytes());
        v.extend_from_slice(&index.to_le_bytes());
    }
    v
}

pub fn decode_output_refs(b: &[u8]) -> Result<Vec<(u64, u32)>> {
    if !b.len().is_multiple_of(12) {
        return Err(corrupt("output reference list", b.len()));
    }
    Ok(b.as_chunks::<12>().0.iter().map(|c| (u64_at(c, 0), u32_at(c, 8))).collect())
}

/// The block blob and its transaction blobs, as `/getrawblocks` and P2P carry
/// them. Length-prefixed with 32-bit lengths; unlike the C++ `4` record
/// (spec/11, a varint per byte) this is the plain bytes, because nothing but
/// this crate reads it.
pub fn encode_raw_block(block: &[u8], transactions: &[Vec<u8>]) -> Vec<u8> {
    let total = 8 + block.len() + transactions.iter().map(|t| 4 + t.len()).sum::<usize>();
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&(block.len() as u32).to_le_bytes());
    v.extend_from_slice(block);
    v.extend_from_slice(&(transactions.len() as u32).to_le_bytes());
    for t in transactions {
        v.extend_from_slice(&(t.len() as u32).to_le_bytes());
        v.extend_from_slice(t);
    }
    v
}

pub fn decode_raw_block(b: &[u8]) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let bad = || ChainError::Corrupt("raw block record: truncated".into());
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        let end = at.checked_add(n).ok_or_else(bad)?;
        let s = b.get(at..end).ok_or_else(bad)?;
        at = end;
        Ok(s)
    };
    let block_len = u32::from_le_bytes(take(4)?.try_into().expect("4 bytes")) as usize;
    let block = take(block_len)?.to_vec();
    let count = u32::from_le_bytes(take(4)?.try_into().expect("4 bytes")) as usize;
    // The count is our own, but a corrupt record must not size an allocation.
    let mut transactions = Vec::new();
    for _ in 0..count {
        let n = u32::from_le_bytes(take(4)?.try_into().expect("4 bytes")) as usize;
        transactions.push(take(n)?.to_vec());
    }
    if at != b.len() {
        return Err(ChainError::Corrupt(format!("raw block record: {} trailing bytes", b.len() - at)));
    }
    Ok((block, transactions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let info = BlockInfo {
            block_hash: [7; 32],
            timestamp: 1_788_929_495,
            block_size: 211,
            cumulative_difficulty: u64::MAX,
            already_generated_coins: 1_500_000_000_000,
            already_generated_transactions: 3,
        };
        assert_eq!(BlockInfo::decode(&info.encode()).unwrap(), info);
        assert_eq!(info.encode().len(), BlockInfo::LEN);
        let out = OutputRecord {
            public_key: [1; 32],
            unlock_time: 40,
            transaction_hash: [2; 32],
            output_index: 65535,
            block_index: 4_213_650,
        };
        assert_eq!(OutputRecord::decode(&out.encode()).unwrap(), out);
        assert_eq!(decode_hashes(&encode_hashes(&[[1; 32], [2; 32]])).unwrap(), vec![[1u8; 32], [2u8; 32]]);
        assert_eq!(decode_output_refs(&encode_output_refs(&[(10, 1), (0, 0)])).unwrap(), vec![(10, 1), (0, 0)]);
        let raw = encode_raw_block(b"block", &[b"tx1".to_vec(), b"".to_vec()]);
        assert_eq!(decode_raw_block(&raw).unwrap(), (b"block".to_vec(), vec![b"tx1".to_vec(), b"".to_vec()]));
    }

    #[test]
    fn short_records_are_decode_errors() {
        assert!(BlockInfo::decode(&[0; 10]).is_err());
        assert!(OutputRecord::decode(&[0; 79]).is_err());
        assert!(decode_hashes(&[0; 33]).is_err());
        assert!(decode_output_refs(&[0; 13]).is_err());
        assert!(decode_raw_block(&[0; 3]).is_err());
        let mut raw = encode_raw_block(b"block", &[]);
        raw.push(0);
        assert!(decode_raw_block(&raw).is_err());
    }
}
