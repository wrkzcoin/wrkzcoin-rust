// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Block structures, the hashing blobs, block identity and the proof-of-work
//! input (spec/04-serialization.md "Block header and block template",
//! "Parent block", "Hashing blobs and block identity"; spec/07).

use crate::constants::{GENESIS_COINBASE_TX_HEX, GENESIS_NONCE};
use crate::ser::{Reader, Writer};
use crate::tx::{self, BaseTransaction, Input, MergeMiningTag, Transaction};
use crate::{varint, Error, Hash, Result};
use std::sync::OnceLock;
use wrkz_pow::{cn_fast_hash, tree_depth, tree_hash, tree_hash_from_branch, tree_hash_from_branch_with_path};

pub const BLOCK_MAJOR_VERSION_1: u8 = 1;
pub const BLOCK_MAJOR_VERSION_2: u8 = 2;
pub const BLOCK_MAJOR_VERSION_7: u8 = 7;
/// `serializeBlockHeader` rejects anything above this on input.
pub const MAX_BLOCK_MAJOR_VERSION: u8 = BLOCK_MAJOR_VERSION_7;

/// The merge-mining parent block carried by every v2+ block.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ParentBlock {
    pub major_version: u8,
    pub minor_version: u8,
    pub previous_block_hash: Hash,
    /// `numberOfTransactions`, MUST be >= 1.
    pub transaction_count: u16,
    pub base_transaction_branch: Vec<Hash>,
    /// The parent coinbase (`BaseTransaction`: prefix only, any version).
    /// Private so that [`ParentBlock::merge_mining_tag`] cannot go stale; read
    /// it with [`ParentBlock::base_transaction`].
    base_transaction: BaseTransaction,
    pub blockchain_branch: Vec<Hash>,
    /// The merge-mining tag parsed out of `base_transaction.prefix.extra` once,
    /// when the parent block is built. Every use of the tag on the hot path
    /// (serialization, the block id, the PoW commitment check) reads it from
    /// here instead of walking `extra` again, which used to happen three to
    /// four times per block.
    merge_mining_tag: Option<MergeMiningTag>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BlockTemplate {
    pub major_version: u8,
    pub minor_version: u8,
    /// v1: in the header; v2+: serialized inside the parent block.
    pub timestamp: u64,
    pub previous_block_hash: Hash,
    /// v1: in the header; v2+: serialized inside the parent block.
    pub nonce: u32,
    pub parent_block: Option<ParentBlock>,
    pub base_transaction: Transaction,
    pub transaction_hashes: Vec<Hash>,
}

impl ParentBlock {
    /// Assemble a parent block, parsing the merge-mining tag out of the
    /// coinbase `extra` once. This is the only way to set `base_transaction`,
    /// which keeps the cached tag and `extra` in step.
    pub fn new(
        major_version: u8,
        minor_version: u8,
        previous_block_hash: Hash,
        transaction_count: u16,
        base_transaction_branch: Vec<Hash>,
        base_transaction: BaseTransaction,
        blockchain_branch: Vec<Hash>,
    ) -> Self {
        let merge_mining_tag = tx::parse_extra(&base_transaction.prefix.extra).merge_mining_tag;
        Self {
            major_version,
            minor_version,
            previous_block_hash,
            transaction_count,
            base_transaction_branch,
            base_transaction,
            blockchain_branch,
            merge_mining_tag,
        }
    }

    /// The parent coinbase.
    pub fn base_transaction(&self) -> &BaseTransaction {
        &self.base_transaction
    }

    /// `ParentBlockSerializer` output with the two flags.
    /// `hashing`: include the merkle root; `header_only`: stop after `numberOfTransactions`.
    pub fn write(
        &self,
        w: &mut Writer,
        block_timestamp: u64,
        block_nonce: u32,
        hashing: bool,
        header_only: bool,
    ) -> Result<()> {
        // Every structural check runs before the first byte is written. The C++
        // interleaves them with the writes and relies on the throw discarding
        // the half-built stream, which is the same outcome; doing it up front
        // also means the merkle root is never computed from a branch of the
        // wrong length, including on the `header_only` PoW path where the C++
        // check sits after the early return. A parent block that came off the
        // wire always satisfies this: `read` sizes the branch from
        // `tree_depth(transaction_count)`, so only a hand-built template can
        // fail it.
        if self.transaction_count < 1 {
            return Err(Error::Malformed("parent numberOfTransactions"));
        }
        if self.base_transaction_branch.len() != tree_depth(self.transaction_count as usize) {
            return Err(Error::Malformed("miner transaction branch size"));
        }
        if !header_only {
            let tag = self.merge_mining_tag.as_ref().ok_or(Error::Malformed("merge mining tag missing"))?;
            if tag.depth > 256 {
                return Err(Error::Malformed("merge mining tag depth"));
            }
            if tag.depth as usize != self.blockchain_branch.len() {
                return Err(Error::Malformed("blockchain branch size != mm depth"));
            }
        }
        w.varint(self.major_version as u64).varint(self.minor_version as u64);
        w.varint(block_timestamp);
        w.raw(&self.previous_block_hash);
        w.u32_le(block_nonce);
        if hashing {
            let miner_tx_hash = self.base_transaction.hash();
            let root = tree_hash_from_branch(&self.base_transaction_branch, &miner_tx_hash);
            w.raw(&root);
        }
        w.varint(self.transaction_count as u64);
        if header_only {
            return Ok(());
        }
        for h in &self.base_transaction_branch {
            w.raw(h);
        }
        self.base_transaction.write(w);
        for h in &self.blockchain_branch {
            w.raw(h);
        }
        Ok(())
    }

    /// Read the in-block form (`hashing = false, header_only = false`).
    /// Returns the parent block plus the block's timestamp and nonce.
    pub fn read(r: &mut Reader<'_>) -> Result<(Self, u64, u32)> {
        let major_version = r.varint_bits(8)? as u8;
        let minor_version = r.varint_bits(8)? as u8;
        let timestamp = r.varint()?;
        let previous_block_hash = r.hash()?;
        let nonce = r.u32_le()?;
        let tx_num = r.varint_bits(16)?;
        if tx_num < 1 {
            return Err(Error::Malformed("parent numberOfTransactions"));
        }
        let transaction_count = tx_num as u16;
        // `tree_depth` of a u16 count is at most 16, and the tag depth at most
        // 256, so neither reservation is attacker-scaled.
        let depth = tree_depth(transaction_count as usize);
        let mut base_transaction_branch = Vec::with_capacity(depth);
        for _ in 0..depth {
            base_transaction_branch.push(r.hash()?);
        }
        let base_transaction = BaseTransaction::read(r)?;
        let merge_mining_tag = tx::parse_extra(&base_transaction.prefix.extra).merge_mining_tag;
        let tag = merge_mining_tag.as_ref().ok_or(Error::Malformed("merge mining tag missing"))?;
        if tag.depth > 256 {
            return Err(Error::Malformed("merge mining tag depth"));
        }
        let mut blockchain_branch = Vec::with_capacity(tag.depth as usize);
        for _ in 0..tag.depth {
            blockchain_branch.push(r.hash()?);
        }
        Ok((
            Self {
                major_version,
                minor_version,
                previous_block_hash,
                transaction_count,
                base_transaction_branch,
                base_transaction,
                blockchain_branch,
                merge_mining_tag,
            },
            timestamp,
            nonce,
        ))
    }

    /// The merge-mining tag of the parent coinbase, parsed once when this
    /// parent block was built.
    pub fn merge_mining_tag(&self) -> Option<&MergeMiningTag> {
        self.merge_mining_tag.as_ref()
    }
}

impl BlockTemplate {
    fn write_header(&self, w: &mut Writer) {
        w.varint(self.major_version as u64).varint(self.minor_version as u64);
        if self.major_version == BLOCK_MAJOR_VERSION_1 {
            w.varint(self.timestamp);
            w.raw(&self.previous_block_hash);
            w.u32_le(self.nonce);
        } else {
            w.raw(&self.previous_block_hash);
        }
    }

    /// Full `BlockTemplate` serialization (what `/getrawblocks` and P2P carry).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        // `serializeBlockHeader` (`CryptoNoteSerialization.cpp:455-478`) is one
        // function for both directions: it throws above `BLOCK_MAJOR_VERSION_7`
        // and, after the `== 1` / `>= 2` arms, falls into an `else` that throws
        // as well — so major version 0 is rejected by the C++ serializer
        // itself, in both directions, and never reaches `validateBlock`. The
        // two checks here are that same pair.
        if self.major_version == 0 || self.major_version > MAX_BLOCK_MAJOR_VERSION {
            return Err(Error::Malformed("block major version"));
        }
        let mut w = Writer::new();
        self.write_header(&mut w);
        if self.major_version >= BLOCK_MAJOR_VERSION_2 {
            let pb = self.parent_block.as_ref().ok_or(Error::Malformed("parent block missing"))?;
            pb.write(&mut w, self.timestamp, self.nonce, false, false)?;
        }
        self.base_transaction.write(&mut w)?;
        w.varint(self.transaction_hashes.len() as u64);
        for h in &self.transaction_hashes {
            w.raw(h);
        }
        Ok(w.into_inner())
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let b = Self::read(&mut r)?;
        r.finish()?;
        Ok(b)
    }

    pub fn read(r: &mut Reader<'_>) -> Result<Self> {
        let major_version = r.varint_bits(8)? as u8;
        // Both halves of the `serializeBlockHeader` guard; see `to_bytes`.
        // Version 0 is a deserialization failure in the C++ too, not a block
        // that parses and is rejected later by `Core::validateBlock`.
        if major_version == 0 || major_version > MAX_BLOCK_MAJOR_VERSION {
            return Err(Error::Malformed("block major version"));
        }
        let minor_version = r.varint_bits(8)? as u8;
        let (timestamp, previous_block_hash, nonce, parent_block) = if major_version == BLOCK_MAJOR_VERSION_1 {
            let ts = r.varint()?;
            let prev = r.hash()?;
            let nonce = r.u32_le()?;
            (ts, prev, nonce, None)
        } else {
            let prev = r.hash()?;
            let (pb, ts, nonce) = ParentBlock::read(r)?;
            (ts, prev, nonce, Some(pb))
        };
        // The coinbase inside a block is a full Transaction; a coinbase has no
        // signature bytes, so this reads exactly the prefix.
        let base_transaction = Transaction::read(r)?;
        let n = r.count(32)?;
        // `count` bounds `n` by the bytes that remain, and a hash costs 32
        // bytes on the wire and 32 in memory, so the reservation cannot
        // outgrow the input; cap it anyway so the rule is uniform.
        let mut transaction_hashes = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            transaction_hashes.push(r.hash()?);
        }
        Ok(Self {
            major_version,
            minor_version,
            timestamp,
            previous_block_hash,
            nonce,
            parent_block,
            base_transaction,
            transaction_hashes,
        })
    }

    /// Coinbase hash + tx hashes, in block order.
    pub fn all_transaction_hashes(&self) -> Result<Vec<Hash>> {
        let mut v = Vec::with_capacity(1 + self.transaction_hashes.len());
        v.push(self.base_transaction.hash()?);
        v.extend_from_slice(&self.transaction_hashes);
        Ok(v)
    }

    /// `CachedBlock::getTransactionTreeHash`.
    pub fn transaction_tree_hash(&self) -> Result<Hash> {
        Ok(tree_hash(&self.all_transaction_hashes()?))
    }

    /// `getBlockHashingBinaryArray`: header ‖ tree hash ‖ varint(tx count + 1).
    pub fn header_hashing_blob(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        self.write_header(&mut w);
        w.raw(&self.transaction_tree_hash()?);
        w.varint(self.transaction_hashes.len() as u64 + 1);
        Ok(w.into_inner())
    }

    /// `getParentBlockHashingBinaryArray(headerOnly)`; v2+ only.
    pub fn parent_hashing_blob(&self, header_only: bool) -> Result<Vec<u8>> {
        let pb = self.parent_block.as_ref().ok_or(Error::Malformed("parent block missing"))?;
        let mut w = Writer::new();
        pb.write(&mut w, self.timestamp, self.nonce, true, header_only)?;
        Ok(w.into_inner())
    }

    /// `getAuxiliaryBlockHeaderHash`: what the merge-mining tag commits to.
    pub fn auxiliary_header_hash(&self) -> Result<Hash> {
        Ok(hash_as_object(&self.header_hashing_blob()?))
    }

    /// `CachedBlock::getBlockHash`: keccak over the varint-length-prefixed blob.
    pub fn hash(&self) -> Result<Hash> {
        let mut blob = self.header_hashing_blob()?;
        if self.major_version >= BLOCK_MAJOR_VERSION_2 {
            blob.extend_from_slice(&self.parent_hashing_blob(false)?);
        }
        Ok(hash_as_object(&blob))
    }

    /// The bytes the proof-of-work function hashes (no length prefix).
    pub fn pow_input(&self) -> Result<Vec<u8>> {
        if self.major_version == BLOCK_MAJOR_VERSION_1 {
            self.header_hashing_blob()
        } else {
            self.parent_hashing_blob(true)
        }
    }

    /// `CachedBlock::getBlockLongHash`.
    pub fn pow_hash(&self) -> Result<Hash> {
        wrkz_pow::pow_hash_for_block_version(self.major_version, &self.pow_input()?)
            .ok_or(Error::Malformed("no PoW for this block version"))
    }

    /// `Currency::checkProofOfWork` (07 "Proof of work check"): the difficulty
    /// test plus, for v2+, the merge-mining tag commitment.
    pub fn check_proof_of_work(&self, difficulty: u64) -> Result<bool> {
        self.check_proof_of_work_with(&self.pow_hash()?, difficulty)
    }

    /// [`BlockTemplate::check_proof_of_work`] with the proof-of-work hash
    /// already computed by [`BlockTemplate::pow_hash`] on this very block —
    /// typically on another thread, ahead of time. The hash is the expensive
    /// half; the difficulty test and the merge-mining commitment below are
    /// microseconds.
    pub fn check_proof_of_work_with(&self, pow_hash: &Hash, difficulty: u64) -> Result<bool> {
        if !wrkz_pow::check_hash(pow_hash, difficulty) {
            return Ok(false);
        }
        if self.major_version == BLOCK_MAJOR_VERSION_1 {
            return Ok(true);
        }
        let pb = self.parent_block.as_ref().ok_or(Error::Malformed("parent block missing"))?;
        let Some(tag) = pb.merge_mining_tag() else { return Ok(false) };
        if pb.blockchain_branch.len() > 256 {
            return Ok(false);
        }
        let aux = self.auxiliary_header_hash()?;
        let root = tree_hash_from_branch_with_path(&pb.blockchain_branch, &aux, Some(&genesis_block_hash()));
        Ok(root == tag.merkle_root)
    }

    /// The parent-block rules of `Core::validateBlock` (`Core.cpp:2714`): for a
    /// **v2** block only, the parent major version must not exceed 1 (0 is
    /// accepted: the daemon's own templates carry 0, `Core.cpp:2365`), and the
    /// serialized parent block must be at most 2048 bytes.
    pub fn validate_parent_block(&self) -> Result<bool> {
        if self.major_version < BLOCK_MAJOR_VERSION_2 {
            return Ok(true);
        }
        let pb = self.parent_block.as_ref().ok_or(Error::Malformed("parent block missing"))?;
        if self.major_version == BLOCK_MAJOR_VERSION_2 && pb.major_version > BLOCK_MAJOR_VERSION_1 {
            return Ok(false);
        }
        let mut w = Writer::new();
        pb.write(&mut w, self.timestamp, self.nonce, false, false)?;
        Ok(w.len() <= 2048)
    }

    /// Coinbase output total (the amount the reward rule must equal).
    pub fn coinbase_output_total(&self) -> Option<u64> {
        self.base_transaction.prefix.sum_outputs()
    }

    /// The block index claimed by the coinbase `BaseInput`, if well-formed.
    pub fn coinbase_height(&self) -> Option<u64> {
        match self.base_transaction.prefix.inputs.as_slice() {
            [Input::Base { block_index }] => Some(*block_index),
            _ => None,
        }
    }
}

/// `getObjectHash(BinaryArray)`: keccak over `varint(len) ‖ bytes`.
pub fn hash_as_object(blob: &[u8]) -> Hash {
    let mut v = varint::encode(blob.len() as u64);
    v.extend_from_slice(blob);
    cn_fast_hash(&v)
}

/// `Currency::generateGenesisBlock` (07 "Genesis").
pub fn genesis_block() -> BlockTemplate {
    let coinbase = hex::decode(GENESIS_COINBASE_TX_HEX).expect("genesis hex");
    let base_transaction = Transaction::from_bytes(&coinbase).expect("genesis coinbase parses");
    BlockTemplate {
        major_version: BLOCK_MAJOR_VERSION_1,
        minor_version: 0,
        timestamp: 0,
        previous_block_hash: [0u8; 32],
        nonce: GENESIS_NONCE,
        parent_block: None,
        base_transaction,
        transaction_hashes: Vec::new(),
    }
}

/// `877e55b4…a6ce`, computed from the genesis template.
///
/// `checkProofOfWork` needs this for every v2+ block, and rebuilding it means a
/// hex decode, a transaction parse and three keccaks, so it is computed once
/// per process. The value is a constant of the network, not of any block.
pub fn genesis_block_hash() -> Hash {
    static GENESIS_HASH: OnceLock<Hash> = OnceLock::new();
    *GENESIS_HASH.get_or_init(|| genesis_block().hash().expect("genesis hashes"))
}

/// `RawBlock` as carried by RPC/P2P: the block blob and its transaction blobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    pub block: Vec<u8>,
    pub transactions: Vec<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_reconstructs() {
        let g = genesis_block();
        assert_eq!(hex::encode(g.hash().unwrap()), "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce");
        let blob = g.header_hashing_blob().unwrap();
        assert_eq!(blob.len(), 72);
        assert_eq!(hex::encode(&g.to_bytes().unwrap()[..4]), "01000000");
        assert_eq!(g.base_transaction.prefix.outputs.len(), 3);
        assert_eq!(g.coinbase_output_total(), Some(1_500_000_000_000));
        assert_eq!(
            hex::encode(g.pow_hash().unwrap()),
            "3cb9405522d3c32293ce88d1c7700f2f27b86fbb3a11663be7f244199dbb5423"
        );
        assert!(g.check_proof_of_work(1).unwrap());
    }
}
