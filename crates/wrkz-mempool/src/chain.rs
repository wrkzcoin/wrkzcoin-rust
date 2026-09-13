// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The chain view the pool and the template builder read, and the numbers a
//! template needs gathered into one struct.
//!
//! The C++ reads all of this straight off `chainsLeaves[0]` inside `Core`.
//! Keeping it behind a trait costs nothing and buys two things: the offline
//! tests can drive a hand-built state, and the byte-comparison test against
//! the live daemon can build a template from *its* height, parent hash and
//! difficulty without a 4.2-million-block database.

use wrkz_chain::records::BlockInfo;
use wrkz_chain::validate::ChainAccess;
use wrkz_chain::{ChainState, Checkpoints};
use wrkz_primitives::block::{BLOCK_MAJOR_VERSION_1, BLOCK_MAJOR_VERSION_2};
use wrkz_primitives::constants::*;
use wrkz_primitives::Hash;
use wrkz_storage::KvStore;

/// `BLOCK_MINOR_VERSION_0` (`CryptoNoteBasic.h`).
pub const BLOCK_MINOR_VERSION_0: u8 = 0;
/// `BLOCK_MINOR_VERSION_1`.
pub const BLOCK_MINOR_VERSION_1: u8 = 1;

/// What both halves of this crate need from the main chain.
///
/// [`ChainAccess`] already carries the reads `ValidateTransaction` performs
/// (ring members, spent key images, the top timestamp, the clock); this adds
/// the handful of values `Core` reads directly off the main chain leaf.
pub trait PoolChain: ChainAccess {
    /// `Core::getTopBlockIndex()`.
    fn top_index(&self) -> u64;

    /// The block info at `index` on the main chain, or `None` above the tip.
    fn block_info_at(&self, index: u64) -> wrkz_chain::Result<Option<BlockInfo>>;

    /// `Core::blockMedianSize` (`Core.cpp:5097`), which is exactly
    /// `calculateCumulativeBlocksizeLimit(top + 1) / 2` (`Core.cpp:4309`):
    /// `max(median of the last 100 block sizes, granted full reward zone of
    /// the next block's major version)`.
    fn block_median_size(&self) -> u64;

    /// The compiled-in checkpoint table, which `ValidateTransaction` consults
    /// to decide whether the expensive input checks run.
    fn checkpoint_table(&self) -> &Checkpoints;

    /// `Core::isTransactionInChain` (`Core.cpp:1895`).
    ///
    /// [`wrkz_chain::ChainState`] answers it from its transaction-hash index in
    /// one point lookup. The default is `false` for the sake of a foreign chain
    /// view that has no index — the live-daemon template test builds one — and
    /// costs such a view only the *code* the rejection comes back as: a
    /// re-submitted transaction is still refused, by the key-image check, as
    /// `INPUT_KEYIMAGE_ALREADY_SPENT` rather than as "already in the
    /// blockchain".
    fn transaction_in_chain(&self, _hash: &Hash) -> wrkz_chain::Result<bool> {
        Ok(false)
    }

    /// `Core::getTopBlockHash()`.
    fn top_block_hash(&self) -> wrkz_chain::Result<Hash> {
        Ok(self.block_info_at(self.top_index())?.map(|i| i.block_hash).unwrap_or_default())
    }

    /// `IBlockchainCache::getAlreadyGeneratedCoins()` at the tip.
    fn already_generated_coins(&self) -> wrkz_chain::Result<u64> {
        Ok(self.block_info_at(self.top_index())?.map(|i| i.already_generated_coins).unwrap_or_default())
    }
}

impl<S: KvStore> PoolChain for ChainState<S> {
    fn top_index(&self) -> u64 {
        self.tip_index().unwrap_or(0) as u64
    }

    fn block_info_at(&self, index: u64) -> wrkz_chain::Result<Option<BlockInfo>> {
        let Ok(index) = u32::try_from(index) else { return Ok(None) };
        self.block_info(index)
    }

    fn block_median_size(&self) -> u64 {
        ChainState::block_median_size(self)
    }

    fn checkpoint_table(&self) -> &Checkpoints {
        self.checkpoints()
    }

    fn transaction_in_chain(&self, hash: &Hash) -> wrkz_chain::Result<bool> {
        self.has_transaction(hash)
    }
}

/// Everything `Core::getBlockTemplate` reads off the chain before it starts
/// building, in one value.
///
/// Separating it from [`PoolChain`] is what lets the live comparison test
/// build our template for the daemon's own height: it fills this in from the
/// daemon's `getblocktemplate` answer instead of from a local database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateContext {
    /// `height = getTopBlockIndex() + 1`.
    pub height: u64,
    /// `b.previousBlockHash = getTopBlockHash()`.
    pub previous_block_hash: Hash,
    /// `getDifficultyForNextBlock()`.
    pub difficulty: u64,
    /// `getBlockMajorVersionForHeight(height)`.
    pub major_version: u8,
    /// `Core.cpp:2346-2362`.
    pub minor_version: u8,
    /// `calculateCumulativeBlocksizeLimit(height) / 2`.
    pub median_size: u64,
    /// `currency.maxBlockCumulativeSize(height)`.
    pub max_cumulative_size: u64,
    /// `chainsLeaves[0]->getAlreadyGeneratedCoins()`.
    pub already_generated_coins: u64,
    /// The median of the last `blockchain_timestamp_check_window` timestamps
    /// ending at the tip, or `None` when `height` is below the window and the
    /// C++ skips the clamp entirely (`Core.cpp:2415`).
    pub timestamp_median: Option<u64>,
}

/// The chain state could not be read, or has no defined difficulty.
#[derive(Debug)]
pub enum ContextError {
    /// `getDifficultyForNextBlock()` returned 0 or was undefined — the C++
    /// refuses to build a template at all (`Core.cpp:2334`).
    DifficultyIsZero,
    /// A block info the windows need is missing.
    Missing(u64),
    Chain(wrkz_chain::ChainError),
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextError::DifficultyIsZero => {
                write!(f, "Cannot create block template, difficulty is zero. Oh shit, you fucked up hard!")
            }
            ContextError::Missing(i) => write!(f, "block info for index {i} is missing"),
            ContextError::Chain(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ContextError {}

impl From<wrkz_chain::ChainError> for ContextError {
    fn from(e: wrkz_chain::ChainError) -> Self {
        ContextError::Chain(e)
    }
}

impl TemplateContext {
    /// The head of `Core::getBlockTemplate` (`Core.cpp:2331-2437`).
    pub fn from_chain<C: PoolChain + ?Sized>(chain: &C) -> Result<Self, ContextError> {
        let top = chain.top_index();
        let height = top + 1;
        let difficulty = next_block_difficulty(chain, top)?.ok_or(ContextError::DifficultyIsZero)?;
        let major_version = block_major_version_for_index(height);
        Ok(Self {
            height,
            previous_block_hash: chain.top_block_hash()?,
            difficulty,
            major_version,
            minor_version: minor_version_for(major_version),
            median_size: chain.block_median_size(),
            max_cumulative_size: max_block_cumulative_size(height),
            already_generated_coins: chain.already_generated_coins()?,
            timestamp_median: timestamp_median(chain, height)?,
        })
    }

    /// `Core.cpp:2427`: the template's timestamp is `time(nullptr)`, raised to
    /// the median of the last window of timestamps when it is below it.
    pub fn clamp_timestamp(&self, now: u64) -> u64 {
        match self.timestamp_median {
            Some(median) if now < median => median,
            _ => now,
        }
    }
}

/// One [`TemplateContext`], kept for as long as the chain tip it was read from.
///
/// Not in the C++, and it changes no byte of any answer. Every field of a
/// context is a function of the tip and nothing else — the difficulty is
/// LWMA-2 over the 61 headers below it, the timestamp median over the last 11
/// or 60 timestamps, the median size over the last 100 block sizes, and the
/// rest are single reads — so two contexts read at the same tip are equal by
/// construction. A pool asking `getblocktemplate` once a second per worker
/// otherwise pays for those ~170 reads and both medians on every call, for a
/// value that cannot have changed until a block arrives.
///
/// What is deliberately *not* cached is everything downstream of this:
/// `fillBlockTemplate` still walks the pool on every call (and still drops
/// what no longer validates), and the coinbase is still built with a fresh
/// `generateKeyPair()`. Handing two miners a coinbase with the same
/// transaction key would give the same one-time output key to the same
/// address, so the template itself is never reused — only the reading of the
/// chain in front of it.
#[derive(Default)]
pub struct TemplateContextCache {
    /// `(top_index, top_block_hash, context)`.
    held: std::sync::Mutex<Option<(u64, Hash, TemplateContext)>>,
}

impl TemplateContextCache {
    pub fn new() -> TemplateContextCache {
        TemplateContextCache::default()
    }

    /// [`TemplateContext::from_chain`], reading the chain only when the tip
    /// has moved since the last call.
    pub fn context<C: PoolChain + ?Sized>(&self, chain: &C) -> Result<TemplateContext, ContextError> {
        let top_index = chain.top_index();
        let top_hash = chain.top_block_hash()?;
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((index, hash, ctx)) = held.as_ref() {
            if *index == top_index && *hash == top_hash {
                return Ok(ctx.clone());
            }
        }
        let ctx = TemplateContext::from_chain(chain)?;
        *held = Some((top_index, top_hash, ctx.clone()));
        Ok(ctx)
    }

    /// Forget what is held. Nothing needs to call this — a tip that moves is
    /// noticed by [`TemplateContextCache::context`] itself — but a test that
    /// wants a cold read says so with it.
    pub fn clear(&self) {
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// `Core.cpp:2346-2362`.
///
/// Both arms yield 0 on this network: `UPGRADE_HEIGHT_V2` and
/// `UPGRADE_HEIGHT_V3` are both defined (1 and 2), so neither
/// `upgradeHeight(...) == UNDEF_HEIGHT` test can fire. The shape is kept so
/// that a network that leaves one undefined still gets the C++ answer.
pub fn minor_version_for(major_version: u8) -> u8 {
    const V2_UNDEFINED: bool = false;
    const V3_UNDEFINED: bool = false;
    if major_version == BLOCK_MAJOR_VERSION_1 {
        if V2_UNDEFINED {
            BLOCK_MINOR_VERSION_1
        } else {
            BLOCK_MINOR_VERSION_0
        }
    } else if V3_UNDEFINED && major_version == BLOCK_MAJOR_VERSION_2 {
        BLOCK_MINOR_VERSION_1
    } else {
        BLOCK_MINOR_VERSION_0
    }
}

/// `IBlockchainCache::getDifficultyForNextBlock(parentIndex)`
/// (`DatabaseBlockchainCache.cpp:1933`).
///
/// The rule itself is [`wrkz_chain::difficulty_for_next_block_from`], the same
/// one [`wrkz_chain::ChainState::difficulty_for_next_block`] applies to a block
/// it is about to accept, over the window
/// [`wrkz_chain::difficulty_window_indexes`] names. Only the *reading* of the
/// window is this crate's own, because a [`PoolChain`] may be a foreign
/// daemon's numbers rather than a local state.
///
/// `None` is the C++ `DIFFICULTY_OVERHEAD` case, which
/// `Core::getBlockTemplate` turns into "difficulty is zero".
pub fn next_block_difficulty<C: PoolChain + ?Sized>(chain: &C, parent_index: u64) -> Result<Option<u64>, ContextError> {
    let Ok(parent) = u32::try_from(parent_index) else { return Ok(None) };
    let mut window = Vec::new();
    for i in wrkz_chain::difficulty_window_indexes(parent) {
        window.push(chain.block_info_at(i as u64)?.ok_or(ContextError::Missing(i as u64))?);
    }
    Ok(wrkz_chain::difficulty_for_next_block_from(parent, &window))
}

/// The window `Core::getBlockTemplate` uses for its timestamp clamp
/// (`Core.cpp:2404`).
///
/// **Not** `Currency::timestampCheckWindow` (`Currency.h:51`), which the
/// validator uses: the template switches to the 11-block window at
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX` (100,000) while the validator switches at
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX_V3` (128,800). Between those two heights the
/// daemon clamped its own templates against an 11-block median while judging
/// blocks against a 60-block one. Local policy either way, but the bytes only
/// match if the quirk is copied.
pub fn template_timestamp_check_window(height: u64) -> usize {
    if height >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
        BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3
    } else {
        BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW
    }
}

/// `Core.cpp:2402-2431`: the median of the timestamps of indexes
/// `height - window ..= height - 1`, and only when `height >= window`.
pub fn timestamp_median<C: PoolChain + ?Sized>(chain: &C, height: u64) -> Result<Option<u64>, ContextError> {
    let window = template_timestamp_check_window(height) as u64;
    if height < window {
        return Ok(None);
    }
    let mut timestamps = Vec::with_capacity(window as usize);
    for i in (height - window)..height {
        timestamps.push(chain.block_info_at(i)?.ok_or(ContextError::Missing(i))?.timestamp);
    }
    Ok(Some(wrkz_chain::reward::median_value(&mut timestamps)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minor_version_is_zero_on_this_network() {
        for major in 1..=7u8 {
            assert_eq!(minor_version_for(major), 0);
        }
    }
}
