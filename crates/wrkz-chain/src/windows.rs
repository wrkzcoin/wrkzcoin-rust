// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Which slices of the chain a windowed replay covers, and how they are
//! chosen.
//!
//! A full linear replay from genesis reads the whole database and applies every
//! block; on a laptop against a 40 GB database that is an overnight job, and
//! most of it is blocks that exercise no rule the block before them did not.
//! The interesting blocks are the ones on either side of a **rule change**:
//! every height where a version, a fee, a mixin tier, a difficulty algorithm, a
//! size or unlock limit, or the transaction proof of work starts or stops
//! applying. [`fork_heights`] is that list, taken from
//! `wrkz_primitives::constants` rather than written out again, so a new fork
//! height added to the constants is covered here the day it lands.
//!
//! A window around height `H` is `[H − W + 1, H + W]`: the `W` blocks ending at
//! `H` and the `W` blocks starting after it. Rules are judged at the *previous*
//! block index (spec/06 "The height a transaction is judged at"), so a rule
//! "from `H`" first fires in the block at `H + 1`; covering both sides means
//! the last block under the old rule and the first block under the new one are
//! both validated.

use wrkz_primitives::constants::*;

/// A half-open-free, inclusive range of block indexes to replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Window {
    pub start: u32,
    pub end: u32,
}

impl Window {
    pub fn len(&self) -> u32 {
        self.end + 1 - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..={}", self.start, self.end)
    }
}

/// Every height at which a consensus rule starts or stops applying.
///
/// Sorted and deduplicated. Sources, in the order they appear below:
///
/// - `UPGRADE_HEIGHTS` — the block major version table (`UpgradeManager.cpp:25`);
/// - `FORK_HEIGHTS` — the advisory list `/info` reports. It gates nothing by
///   itself, but every height in it was a release boundary and several of them
///   are rule changes under another name, so they are all covered;
/// - the mixin tiers (`Mixins.cpp:18`), including 4,300,000 where the floor
///   changes from "the largest ring" to "every ring";
/// - the fee ladder (`Utilities.cpp:293-343`), including `MINIMUM_FEE_V1_HEIGHT
///   + 1`, which is where the `<=` in the ladder actually switches;
/// - the difficulty algorithm switches (`Currency.cpp:540`) and the "zawy"
///   override at 20,160, plus the future-time-limit and timestamp-window
///   changes that share those heights (`Currency.h:51-77`);
/// - the block and transaction limits: `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT`
///   (543,000), the extra-size limit at 543,000 + 40, the output count at
///   777,777, the output amount cap at 800,000, `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT`
///   and the block-time unlock branch at 600,000;
/// - the fusion fee window (864,864 and 1,123,000) and the dust thresholds;
/// - the transaction proof of work: activation at 1,123,000, the dynamic
///   difficulty at 1,200,000, the fee escape at 1,500,000 — which is also
///   where the block reward goes flat and the fee chunk size halves;
/// - the unlock-time rule at 1,200,000;
/// - the end of the checkpoint zone at 4,188,000, the one height where the
///   checkpoint semantics themselves change.
pub fn fork_heights() -> Vec<u64> {
    let mut heights: Vec<u64> = Vec::new();
    heights.extend(UPGRADE_HEIGHTS.iter().map(|(_, h)| *h));
    heights.extend_from_slice(FORK_HEIGHTS);
    heights.extend_from_slice(&[
        // mixin tiers
        MIXIN_LIMITS_V1_HEIGHT,
        MIXIN_LIMITS_V2_HEIGHT,
        MIXIN_LIMITS_V3_HEIGHT,
        MIXIN_LIMITS_V4_HEIGHT,
        MIXIN_LIMITS_V5_HEIGHT,
        MIXIN_LIMITS_V6_HEIGHT,
        // fee ladder
        MINIMUM_FEE_V1_HEIGHT,
        MINIMUM_FEE_V1_HEIGHT + 1,
        MINIMUM_FEE_PER_BYTE_V1_HEIGHT,
        MINIMUM_FEE_PER_BYTE_V2_HEIGHT,
        // difficulty algorithms, future time limits, timestamp windows
        ZAWY_DIFFICULTY_BLOCK_INDEX,
        LWMA_2_DIFFICULTY_BLOCK_INDEX,
        LWMA_2_DIFFICULTY_BLOCK_INDEX_V3,
        // block and transaction limits
        TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT,
        MAX_EXTRA_SIZE_V2_HEIGHT,
        MAX_EXTRA_SIZE_V2_HEIGHT + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW,
        BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT,
        TRANSACTION_INPUT_BLOCKTIME_VALIDATION_HEIGHT,
        NORMAL_TX_MAX_OUTPUT_COUNT_V1_HEIGHT,
        MAX_OUTPUT_SIZE_HEIGHT,
        // dust and fusion
        DUST_THRESHOLD_V2_HEIGHT,
        FUSION_DUST_THRESHOLD_HEIGHT_V2,
        FUSION_FEE_V1_HEIGHT,
        FUSION_ZERO_FEE_V2_HEIGHT,
        // transaction proof of work
        TRANSACTION_POW_HEIGHT,
        TRANSACTION_POW_HEIGHT_DYN_V1,
        TRANSACTION_POW_PASS_WITH_FEE_HEIGHT,
        // flat reward
        FIXED_REWARD_V1_HEIGHT,
        // unlock time
        UNLOCK_TIME_HEIGHT,
        UNLOCK_TIME_HEIGHT_V2,
        // the end of the checkpoint zone
        LAST_CHECKPOINT_HEIGHT,
    ]);
    heights.sort_unstable();
    heights.dedup();
    heights
}

/// The highest checkpointed index in `CryptoNoteCheckpoints.h`; above it the
/// C++ verifies everything, below it a block at a non-checkpointed index is
/// accepted without proof of work or signatures (spec/07 "Checkpoints").
pub const LAST_CHECKPOINT_HEIGHT: u64 = 4_188_000;

/// The windows around every [`fork_heights`] entry, clamped to `1..=top`,
/// sorted and merged.
///
/// Block 0 is never in a window: genesis is constructed, not replayed.
pub fn fork_windows(window: u32, top: u32) -> Vec<Window> {
    let raw = fork_heights().into_iter().filter_map(|h| around(h, window, top));
    merge(raw.collect())
}

/// `[h − window + 1, h + window]`, clamped. `None` when it falls entirely
/// outside `1..=top`.
fn around(height: u64, window: u32, top: u32) -> Option<Window> {
    let w = window.max(1) as u64;
    let start = height.saturating_sub(w - 1).max(1);
    let end = height.saturating_add(w).min(top as u64);
    if start > end || start > top as u64 {
        return None;
    }
    Some(Window { start: start as u32, end: end as u32 })
}

/// Sort and merge overlapping or touching windows, so no block is replayed
/// twice and the state is seeded once per contiguous run.
pub fn merge(mut windows: Vec<Window>) -> Vec<Window> {
    windows.sort_unstable();
    let mut out: Vec<Window> = Vec::with_capacity(windows.len());
    for w in windows {
        match out.last_mut() {
            // `+ 1` so that two windows that merely touch become one run: the
            // second would otherwise re-seed a state it already has.
            Some(last) if w.start <= last.end.saturating_add(1) => last.end = last.end.max(w.end),
            _ => out.push(w),
        }
    }
    out
}

/// `count` random windows of `window` blocks in `1..=top`, from `seed`.
///
/// The generator is SplitMix64, written out here so a run is reproducible from
/// the printed seed on any machine and any build, with no dependency on a
/// random-number crate whose stream could change between versions.
pub fn sample_windows(count: u32, window: u32, seed: u64, top: u32) -> Vec<Window> {
    let window = window.max(1);
    if top < 1 {
        return Vec::new();
    }
    // The last start that still leaves a whole window; a chain shorter than one
    // window gives a single window over all of it.
    let highest_start = (top as u64 + 1).saturating_sub(window as u64).max(1);
    let span = highest_start; // starts are 1..=highest_start
    let mut state = seed;
    let mut picked = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let start = 1 + (splitmix64(&mut state) % span) as u32;
        let end = ((start as u64 + window as u64 - 1).min(top as u64)) as u32;
        picked.push(Window { start, end });
    }
    merge(picked)
}

/// SplitMix64 (Steele, Lea and Flood 2014), the reference constants.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A seed from the clock, for a run that was not given one. It is printed, so
/// the run can be repeated with `--seed`.
pub fn seed_from_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x243F_6A88_85A3_08D3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every height a rule changes at, spelled out. If a constant moves or a
    /// fork is added, this fails and says so — which is the point: the windowed
    /// replay is only as good as this list.
    #[test]
    fn the_fork_height_list_is_the_rule_changes() {
        let heights = fork_heights();
        let expected: Vec<u64> = vec![
            1,         // UPGRADE_HEIGHT_V2, FORK_HEIGHTS[0]
            2,         // UPGRADE_HEIGHT_V3
            3,         // UPGRADE_HEIGHT_V4
            10_000,    // mixin tier V1
            20_160,    // ZAWY_DIFFICULTY_BLOCK_INDEX
            40_000,    // FORK_HEIGHTS
            100_000,   // LWMA-2 V3, future time limit 180
            128_800,   // LWMA-2 V5, future time limit 360, timestamp window 11
            302_400,   // UPGRADE_HEIGHT_V5, mixin tier V2, dust threshold V2
            400_000,   // fusion dust threshold V2
            430_000,   // mixin tier V3
            543_000,   // signature count validation, MAX_EXTRA_SIZE_V2_HEIGHT
            543_040,   // extra size enforced in blocks (543,000 + 40)
            600_000,   // UPGRADE_HEIGHT_V6, blob shuffle check, block-time unlock
            658_500,   // mixin tier V4
            678_500,   // MINIMUM_FEE_V1_HEIGHT
            678_501,   // where the fee ladder's `<=` switches
            777_777,   // output count limit
            800_000,   // output amount cap
            832_000,   // fee per byte V1 height
            864_864,   // fusion fee window opens
            1_000_000, // UPGRADE_HEIGHT_V7, mixin tier V5
            1_123_000, // transaction PoW, fusion fee window closes
            1_200_000, // dynamic tx PoW difficulty, unlock time rule
            1_500_000, // flat reward, fee chunk 128, tx PoW fee escape
            1_800_000, // FORK_HEIGHTS
            2_500_000, // FORK_HEIGHTS
            2_800_000, // FORK_HEIGHTS
            3_500_000, // FORK_HEIGHTS
            3_800_000, // FORK_HEIGHTS
            4_188_000, // the end of the checkpoint zone
            4_300_000, // mixin tier V6, per-input mixin floor
            4_500_000, // prune capability
        ];
        assert_eq!(heights, expected);
    }

    #[test]
    fn a_window_covers_both_sides_of_the_height() {
        // W blocks ending at H, and W blocks starting after it.
        assert_eq!(around(1_000_000, 3, 5_000_000), Some(Window { start: 999_998, end: 1_000_003 }));
        assert_eq!(around(1_000_000, 3, 5_000_000).unwrap().len(), 6);
        // Clamped at the bottom: block 0 is genesis and is never replayed.
        assert_eq!(around(2, 10, 5_000_000), Some(Window { start: 1, end: 12 }));
        // Clamped at the top, and dropped when it starts above the tip.
        assert_eq!(around(4_500_000, 10, 1_000_000), None);
        assert_eq!(around(1_000_000, 10, 1_000_005), Some(Window { start: 999_991, end: 1_000_005 }));
    }

    #[test]
    fn windows_are_sorted_and_merged() {
        // 1, 2 and 3 are one run at any window size; 543,000 and 543,040 merge
        // at W = 2000 and stay apart at W = 5.
        let merged = fork_windows(2000, 5_000_000);
        assert!(merged.windows(2).all(|p| p[0].end + 1 < p[1].start), "windows are disjoint and not touching");
        assert!(merged.windows(2).all(|p| p[0].start < p[1].start), "windows are sorted");
        assert!(merged.iter().any(|w| w.start <= 543_000 && w.end >= 543_040), "543,000 and 543,040 merged");
        assert_eq!(merged.first().unwrap().start, 1, "block 0 is genesis, never replayed");

        let small = fork_windows(5, 5_000_000);
        assert!(small.iter().any(|w| w.start == 542_996 && w.end == 543_005));
        assert!(small.iter().any(|w| w.start == 543_036 && w.end == 543_045));
        // Every fork height is inside some window at every window size.
        for w in [1u32, 5, 500, 2000] {
            let ws = fork_windows(w, 5_000_000);
            for h in fork_heights() {
                assert!(ws.iter().any(|x| (x.start as u64) <= h && h <= x.end as u64), "height {h} at window {w}");
            }
        }
        // A short chain drops the windows above its tip.
        let short = fork_windows(2000, 50_000);
        assert!(short.iter().all(|w| w.end <= 50_000));
        assert!(short.iter().any(|w| w.start <= 20_160 && w.end >= 20_160));
    }

    #[test]
    fn merge_joins_touching_runs() {
        let m = merge(vec![Window { start: 10, end: 20 }, Window { start: 21, end: 30 }]);
        assert_eq!(m, vec![Window { start: 10, end: 30 }]);
        let m = merge(vec![Window { start: 21, end: 30 }, Window { start: 10, end: 19 }]);
        assert_eq!(m, vec![Window { start: 10, end: 19 }, Window { start: 21, end: 30 }]);
        let m = merge(vec![Window { start: 10, end: 40 }, Window { start: 15, end: 20 }]);
        assert_eq!(m, vec![Window { start: 10, end: 40 }]);
    }

    #[test]
    fn sampling_is_reproducible_from_its_seed() {
        let a = sample_windows(8, 1000, 12345, 4_213_650);
        let b = sample_windows(8, 1000, 12345, 4_213_650);
        assert_eq!(a, b, "the same seed gives the same windows");
        assert_ne!(a, sample_windows(8, 1000, 12346, 4_213_650));
        assert!(a.iter().all(|w| w.start >= 1 && w.end <= 4_213_650));
        assert!(a.iter().all(|w| w.len() <= 1000 || w.start == w.end));
        assert!(a.windows(2).all(|p| p[0].end + 1 < p[1].start), "merged and disjoint");
        // A chain shorter than one window still yields one usable window.
        let tiny = sample_windows(3, 1000, 1, 5);
        assert!(tiny.iter().all(|w| w.start >= 1 && w.end <= 5));
    }
}
