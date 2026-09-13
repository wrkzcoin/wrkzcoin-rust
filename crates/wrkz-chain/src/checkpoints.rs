// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Checkpoints (`src/cryptonotecore/Checkpoints.cpp`,
//! `src/config/CryptoNoteCheckpoints.h`; spec/07 "Checkpoints").
//!
//! The compiled-in table is `CHECKPOINTS` from the C++ header, 4,198 entries
//! from index 0 to 4,188,000, stored as `data/checkpoints.bin`: for each entry a
//! little-endian `uint32` index and its 32-byte block hash, ascending. Keeping
//! the bytes out of the source keeps a 4,200-line generated file out of the
//! crate and makes the table one `include_bytes!` instead of 4,198 hex decodes
//! at startup.
//!
//! Zone semantics, from `Core::addBlock` and `ValidateTransaction`:
//!
//! - the zone is every index up to and including the highest checkpointed index;
//! - inside the zone, a block at a checkpointed index must match the checkpoint
//!   (`CHECKPOINT_BLOCK_HASH_MISMATCH`) and **no proof of work is checked**;
//! - inside the zone, `validateTransactionInputsExpensive` (key image spent
//!   check, ring member resolution, unlock check, signature count and ring
//!   signatures) and `validateTransactionPoW` are skipped for block
//!   transactions — the transaction hash is committed by the checkpointed block
//!   hash, so altering it would break the checkpoint;
//! - outside the zone everything is verified.
//!
//! [`Checkpoints::disable_from`] is the switch the replay uses to turn the zone
//! off from a chosen height, so that the proof of work, the ring signatures and
//! the transaction proof of work are exercised against real data below the last
//! checkpoint too. It is not a C++ feature; the C++ equivalent is
//! `--load-checkpoints` with a shorter file, which spec/07 calls safe (more
//! verification, never less).

use std::collections::BTreeMap;
use wrkz_primitives::Hash;

/// `CHECKPOINTS` from `src/config/CryptoNoteCheckpoints.h` at commit 8d89d7bf.
static MAINNET_CHECKPOINTS: &[u8] = include_bytes!("../data/checkpoints.bin");

/// One record of [`MAINNET_CHECKPOINTS`]: `uint32` index then a 32-byte hash.
const ENTRY_LEN: usize = 4 + 32;

/// The table's own length check: a truncated `checkpoints.bin` would otherwise
/// lose its last entry silently, which is a shorter checkpoint set and so a
/// slower but still correct sync — better to notice at the first call.
const _: () = assert!(MAINNET_CHECKPOINTS.len().is_multiple_of(ENTRY_LEN));

/// `CryptoNote::Checkpoints`.
#[derive(Clone, Debug, Default)]
pub struct Checkpoints {
    points: BTreeMap<u32, Hash>,
    /// Indexes at or above this are treated as if no checkpoint existed;
    /// `u64::MAX` (the default) leaves the table alone.
    disabled_from: u64,
}

impl Checkpoints {
    /// An empty set: every block is verified in full.
    pub fn none() -> Self {
        Self { points: BTreeMap::new(), disabled_from: u64::MAX }
    }

    /// The compiled-in mainnet table.
    pub fn mainnet() -> Self {
        let mut points = BTreeMap::new();
        for entry in MAINNET_CHECKPOINTS.as_chunks::<ENTRY_LEN>().0 {
            let index = u32::from_le_bytes(entry[..4].try_into().expect("4 bytes"));
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&entry[4..]);
            points.insert(index, hash);
        }
        Self { points, disabled_from: u64::MAX }
    }

    /// `Checkpoints::addCheckpoint`: an index may only be named once, and a
    /// second, different entry for the same index is an error.
    pub fn add(&mut self, index: u32, hash: Hash) -> bool {
        match self.points.entry(index) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(hash);
                true
            }
            std::collections::btree_map::Entry::Occupied(_) => false,
        }
    }

    /// Parse the `index,hash` CSV of `--load-checkpoints`
    /// (`Checkpoints::loadCheckpointsFromFile`). Blank lines are skipped; the
    /// C++ `getline` loop stops at the first line without a comma, this is
    /// stricter and says so.
    pub fn from_csv(text: &str) -> std::result::Result<Self, String> {
        let mut cp = Self::none();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (index, hash) = line.split_once(',').ok_or_else(|| format!("line {}: no comma", n + 1))?;
            let index: u32 = index.trim().parse().map_err(|_| format!("line {}: bad index", n + 1))?;
            let bytes = hex::decode(hash.trim()).map_err(|_| format!("line {}: bad hash", n + 1))?;
            let hash: Hash = bytes.try_into().map_err(|_| format!("line {}: hash is not 32 bytes", n + 1))?;
            if !cp.add(index, hash) {
                return Err(format!("line {}: checkpoint for {index} already exists", n + 1));
            }
        }
        Ok(cp)
    }

    /// Treat every index at or above `height` as uncheckpointed, so that the
    /// proof of work, ring signatures and transaction proof of work of those
    /// blocks are verified. `None` restores the whole table.
    pub fn disable_from(&mut self, height: Option<u64>) {
        self.disabled_from = height.unwrap_or(u64::MAX);
    }

    /// The height from which checkpoints are ignored, if one was set.
    pub fn disabled_from(&self) -> Option<u64> {
        (self.disabled_from != u64::MAX).then_some(self.disabled_from)
    }

    /// The highest checkpointed index still in force, `None` when the set is
    /// empty or entirely disabled.
    pub fn top_index(&self) -> Option<u32> {
        self.points.range(..self.clamp_disabled()).next_back().map(|(i, _)| *i)
    }

    /// Every checkpoint still in force, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Hash)> + '_ {
        self.points.range(..self.clamp_disabled()).map(|(i, h)| (*i, h))
    }

    /// The number of checkpoints still in force.
    pub fn len(&self) -> usize {
        self.points.range(..self.clamp_disabled()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.top_index().is_none()
    }

    fn clamp_disabled(&self) -> u32 {
        u32::try_from(self.disabled_from).unwrap_or(u32::MAX)
    }

    /// `Checkpoints::isInCheckpointZone(index)`: the set is non-empty and
    /// `index` is at or below its highest entry.
    ///
    /// The C++ takes a `uint32_t`; a `uint64_t` height above `u32::MAX` cannot
    /// be in any zone, which is what the saturating compare below gives.
    pub fn is_in_checkpoint_zone(&self, index: u64) -> bool {
        if index >= self.disabled_from {
            return false;
        }
        match self.top_index() {
            Some(top) => index <= top as u64,
            None => false,
        }
    }

    /// The checkpoint at `index`, if this index is checkpointed and in force.
    pub fn get(&self, index: u32) -> Option<&Hash> {
        if (index as u64) >= self.disabled_from {
            return None;
        }
        self.points.get(&index)
    }

    /// `Checkpoints::checkBlock(index, hash)`: true when there is no checkpoint
    /// at this index, or there is one and it matches.
    pub fn check_block(&self, index: u32, hash: &Hash) -> bool {
        match self.get(index) {
            Some(expected) => expected == hash,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_table_matches_the_cpp_header() {
        let cp = Checkpoints::mainnet();
        assert_eq!(cp.len(), 4198);
        assert_eq!(cp.top_index(), Some(4_188_000));
        assert_eq!(hex::encode(cp.get(0).unwrap()), "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce");
        assert_eq!(
            hex::encode(cp.get(4_188_000).unwrap()),
            "771dac9bc565de01b51f541654fd8fe9da063bc8462272efd76f8b881d47b659"
        );
        // Every 100 up to 1000, every 1000 after that.
        assert!(cp.get(100).is_some());
        assert!(cp.get(150).is_none());
        assert!(cp.get(2000).is_some());
        assert!(cp.get(2100).is_none());
        // Zone semantics.
        assert!(cp.is_in_checkpoint_zone(0));
        assert!(cp.is_in_checkpoint_zone(4_188_000));
        assert!(!cp.is_in_checkpoint_zone(4_188_001));
        // A non-checkpointed index inside the zone passes check_block.
        assert!(cp.check_block(150, &[9u8; 32]));
        assert!(!cp.check_block(100, &[9u8; 32]));
    }

    #[test]
    fn disable_from_turns_the_zone_off() {
        let mut cp = Checkpoints::mainnet();
        cp.disable_from(Some(0));
        assert!(cp.is_empty());
        assert!(!cp.is_in_checkpoint_zone(0));
        assert!(cp.check_block(0, &[9u8; 32]), "a disabled checkpoint cannot reject a block");
        cp.disable_from(Some(1000));
        assert_eq!(cp.top_index(), Some(900));
        assert!(cp.is_in_checkpoint_zone(900));
        assert!(!cp.is_in_checkpoint_zone(1000));
        assert!(cp.get(1000).is_none());
        assert!(cp.get(900).is_some());
        cp.disable_from(None);
        assert_eq!(cp.top_index(), Some(4_188_000));
    }

    #[test]
    fn csv_round_trip() {
        let text = "0,877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce\n\n100,ac79baed856a44531af0da18b64c9c77ae4acd707962fb6f15905bd54804e3f3\n";
        let cp = Checkpoints::from_csv(text).unwrap();
        assert_eq!(cp.len(), 2);
        assert_eq!(cp.top_index(), Some(100));
        assert!(Checkpoints::from_csv("0,ff").is_err());
        assert!(Checkpoints::from_csv("nope").is_err());
    }
}
