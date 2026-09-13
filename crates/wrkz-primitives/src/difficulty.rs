// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Difficulty algorithms (spec/07-blocks-consensus.md "Difficulty";
//! `src/cryptonotecore/Difficulty.cpp`, `Currency::nextDifficulty`).
//!
//! `timestamps` and `cumulative_difficulties` are the oldest-first windows the
//! cache returns, `difficulty_blocks_count` entries ending at the parent.
//!
//! # `None` means "no difficulty exists for this input"
//!
//! The C++ functions return `uint64_t` and have no failure path, because two
//! shapes of input are simply undefined there:
//!
//! - `nextDifficultyV5`/`V4`/`V3` check `timestamps.size()` and then index
//!   `cumulativeDifficulties[N]` and `[N - 1]` unchecked
//!   (`Difficulty.cpp:41-44`). A shorter cumulative vector is an out-of-bounds
//!   `std::vector::operator[]`, i.e. undefined behaviour.
//! - the same three divide by `100 * 2 * L`. `L` is zero when all
//!   `DIFFICULTY_WINDOW_V3 + 1` timestamps in the window are equal (every
//!   solvetime clamps to 0), and integer division by zero raises `SIGFPE`: the
//!   C++ daemon dies rather than returning a value.
//!
//! Neither can occur on the majority chain — the second would have killed every
//! node that saw such a block — so there is no consensus behaviour to
//! reproduce, and this port returns `None` for both. A caller must treat `None`
//! as "reject this block", never as a difficulty of zero (which
//! [`is_valid_difficulty`] also rejects) and never as a value to substitute.

use crate::constants::*;

/// `Currency::getNextDifficulty(version, parentIndex, ...)` (`Currency.cpp:540`).
///
/// `None` when the windows are inconsistent or degenerate; see the module docs.
pub fn next_difficulty(
    next_version: u8,
    parent_index: u64,
    timestamps: &[u64],
    cumulative_difficulties: &[u64],
) -> Option<u64> {
    if parent_index >= LWMA_2_DIFFICULTY_BLOCK_INDEX_V3 {
        next_difficulty_v5(timestamps, cumulative_difficulties)
    } else if parent_index >= LWMA_2_DIFFICULTY_BLOCK_INDEX_V2 {
        // Live for parent 100,000 - 128,799.
        next_difficulty_v4(timestamps, cumulative_difficulties)
    } else if parent_index >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
        // Unreachable: `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` is defined as
        // `LWMA_2_DIFFICULTY_BLOCK_INDEX` (`CryptoNoteConfig.h:76`), so the arm
        // above already caught every index this one could. Kept because the
        // C++ has it, and because a future config could separate the two.
        next_difficulty_v3(timestamps, cumulative_difficulties)
    } else {
        next_difficulty_legacy(next_version, parent_index, timestamps, cumulative_difficulties)
    }
}

/// The window both LWMA-2 variants share. `None` when `L == 0` (see the module
/// docs: the C++ takes `SIGFPE` there).
///
/// The caller has already checked that both slices hold at least
/// `DIFFICULTY_WINDOW_V3 + 1` entries.
fn lwma_common(
    timestamps: &[u64],
    cum: &[u64],
    clamp_low: i64,
    clamp_high: i64,
    band_low: i64,
    band_high: i64,
) -> Option<(i64, i64, i64)> {
    let t = DIFFICULTY_TARGET as i64;
    let n = DIFFICULTY_WINDOW_V3 as i64;
    let mut l: i64 = 0;
    let mut sum_3_st: i64 = 0;
    for i in 1..=n as usize {
        let mut st = timestamps[i] as i64 - timestamps[i - 1] as i64;
        st = st.clamp(clamp_low, clamp_high);
        l += st * i as i64;
        if i as i64 > n - 3 {
            sum_3_st += st;
        }
    }
    if l == 0 {
        return None;
    }
    let total = (cum[n as usize].wrapping_sub(cum[0])) as i64;
    // Signed overflow is undefined in C++ but wraps on every compiler this is
    // built with, and a debug Rust build would panic instead; `wrapping_mul`
    // makes both profiles agree with the deployed binary. Reachable only with a
    // cumulative difficulty jump above ~2.5e13 across the window.
    let scaled = total.wrapping_mul(t).wrapping_mul(n + 1).wrapping_mul(99);
    // C integer division truncates toward zero, as Rust's `/` does.
    let mut next_d = scaled / (100 * 2 * l);
    let prev_d = cum[n as usize].wrapping_sub(cum[n as usize - 1]) as i64;
    let low = prev_d.wrapping_mul(band_low) / 100;
    let high = prev_d.wrapping_mul(band_high) / 100;
    next_d = low.max(next_d.min(high));
    Some((next_d, prev_d, sum_3_st))
}

/// Both windows must hold `DIFFICULTY_WINDOW_V3 + 1` entries. The C++ only ever
/// checks `timestamps`; see the module docs for why the second check exists.
fn windows_long_enough(timestamps: &[u64], cum: &[u64]) -> bool {
    let need = DIFFICULTY_WINDOW_V3 + 1;
    timestamps.len() >= need && cum.len() >= need
}

/// LWMA-2 from parent index 128,800 (`Difficulty.cpp:14`).
///
/// Returns the startup guess of 10,000 while either window is short, matching
/// the C++ `timestamps.size() < N + 1` branch.
pub fn next_difficulty_v5(timestamps: &[u64], cum: &[u64]) -> Option<u64> {
    if !windows_long_enough(timestamps, cum) {
        return Some(10_000);
    }
    let t = DIFFICULTY_TARGET as i64;
    let (mut next_d, prev_d, sum_3_st) = lwma_common(timestamps, cum, -4 * t, 6 * t, 67, 150)?;
    if sum_3_st < (8 * t) / 10 {
        next_d = next_d.max(prev_d.wrapping_mul(108) / 100);
    }
    Some(next_d as u64)
}

/// LWMA-2 for parent index 100,000 – 128,799 (`Difficulty.cpp:58`).
///
/// This is the live algorithm for that range, not `next_difficulty_v3`:
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` is *defined as*
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX` (`CryptoNoteConfig.h:76`), so the V4 arm of
/// `getNextDifficulty` catches everything at or above 100,000 and the V3 arm
/// below it is unreachable.
///
/// **Do not fix the clamp.** `Difficulty.cpp:73` reads
/// `clamp(-6 * T, ST, 6 * T)`, which looks like `(low, value, high)`, but the
/// helper is `clamp(n, lower, upper) = max(lower, min(n, upper))`
/// (`Difficulty.h:17`). With the arguments in that order it evaluates to
/// `max(ST, min(-6T, 6T))` = `max(ST, -6T)`: a lower bound only, and **no
/// upper bound at all**. A genuine clamp changes the chain — block 100,001
/// has an interval of 414 seconds, above `6 * T = 360`, and clamping it
/// yields 14,866,321 where the chain records 14,767,992.
pub fn next_difficulty_v4(timestamps: &[u64], cum: &[u64]) -> Option<u64> {
    if !windows_long_enough(timestamps, cum) {
        return Some(1000);
    }
    let t = DIFFICULTY_TARGET as i64;
    let (mut next_d, prev_d, sum_3_st) = lwma_common(timestamps, cum, -6 * t, i64::MAX, 67, 150)?;
    if sum_3_st < (8 * t) / 10 {
        next_d = next_d.max(prev_d.wrapping_mul(110) / 100);
    }
    Some(next_d as u64)
}

/// LWMA-2 for parent index 100,000 – 128,799 (`Difficulty.cpp:101`).
pub fn next_difficulty_v3(timestamps: &[u64], cum: &[u64]) -> Option<u64> {
    if !windows_long_enough(timestamps, cum) {
        return Some(1000);
    }
    let t = DIFFICULTY_TARGET as i64;
    let ftl = CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3 as i64;
    let (mut next_d, prev_d, sum_3_st) = lwma_common(timestamps, cum, -ftl, 6 * t, 70, 107)?;
    if sum_3_st < (8 * t) / 10 {
        next_d = prev_d.wrapping_mul(110) / 100; // assignment, not max
    }
    Some(next_d as u64)
}

/// The Bytecoin algorithm with the "zawy" override (`Currency::nextDifficulty`, `Currency.cpp:564`).
///
/// The C++ takes both vectors by value and `resize`s them, which keeps the
/// **first** `window` entries (`DIFFICULTY_LAG` is expressed by the caller
/// handing over a window that starts earlier); the truncation here is the same
/// operation on borrowed slices. `None` when the two windows disagree in
/// length, which the C++ would index out of bounds on.
pub fn next_difficulty_legacy(version: u8, parent_index: u64, timestamps_in: &[u64], cum_in: &[u64]) -> Option<u64> {
    let window = difficulty_window(version);
    let cut = difficulty_cut(version);
    let length = timestamps_in.len().min(window);
    if length <= 1 {
        return Some(1);
    }
    if cum_in.len() < length {
        return None;
    }
    let timestamps_o = &timestamps_in[..length];
    let cum = &cum_in[..length];
    let mut timestamps = timestamps_o.to_vec();
    timestamps.sort_unstable();
    let (cut_begin, cut_end) = if length <= window - 2 * cut {
        (0, length)
    } else {
        // C++ `(length - (window - 2 * cut) + 1) / 2`
        let b = (length - (window - 2 * cut)).div_ceil(2);
        (b, b + (window - 2 * cut))
    };
    let mut time_span = timestamps[cut_end - 1] - timestamps[cut_begin];
    if time_span == 0 {
        time_span = 1;
    }
    let total_work = cum[cut_end - 1].wrapping_sub(cum[cut_begin]);
    let product = total_work as u128 * DIFFICULTY_TARGET as u128;
    let (low, high) = (product as u64, (product >> 64) as u64);
    if high != 0 || u64::MAX - low < (time_span - 1) {
        return Some(0);
    }
    let zawy_version = if ZAWY_DIFFICULTY_V2 { 2 } else { ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION };
    if version >= zawy_version && zawy_version != 0 {
        return Some(low / time_span);
    }
    if ZAWY_DIFFICULTY_BLOCK_INDEX != 0 && ZAWY_DIFFICULTY_BLOCK_INDEX <= parent_index {
        // Recompute with window 17 and cut 0 over the last 17 (unsorted) entries.
        let t_window = 17.min(timestamps_o.len());
        let mut ts: Vec<u64> = timestamps_o[timestamps_o.len() - t_window..].to_vec();
        let cd = &cum[cum.len() - t_window..];
        ts.sort_unstable();
        let mut span = ts[t_window - 1] - ts[0];
        if span == 0 {
            span = 1;
        }
        let work = cd[t_window - 1].wrapping_sub(cd[0]);
        let p = work as u128 * DIFFICULTY_TARGET as u128;
        if (p >> 64) != 0 {
            return Some(0);
        }
        let d = (p as u64) / span;
        return Some(d.max(100));
    }
    // C++ `(low + timeSpan - 1) / timeSpan`; the guard above has already ruled
    // out the overflow that addition could have.
    Some(low.div_ceil(time_span))
}

/// Block acceptance rule: `checkProofOfWork` needs a non-zero difficulty.
pub fn is_valid_difficulty(d: u64) -> bool {
    d != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v5_steady_state() {
        // 07 acceptance 3: 61 equal 60 s solvetimes, equal per-block difficulty D.
        let d = 1_000_000u64;
        let timestamps: Vec<u64> = (0..61).map(|i| 1_000_000 + 60 * i).collect();
        let cum: Vec<u64> = (0..61).map(|i| 5_000_000_000 + d * i).collect();
        // L = sum(60*i, i=1..60) = 60*1830 = 109800
        // next = (60*D * 60 * 61 * 99) / (200 * 109800) = D * 0.99 (integer truncated)
        let got = next_difficulty_v5(&timestamps, &cum);
        assert_eq!(got, Some((d as i64 * 60 * 60 * 61 * 99 / (200 * 109_800)) as u64));
        assert_eq!(got, Some(990_000));
        assert_eq!(next_difficulty_v5(&timestamps[..60], &cum[..60]), Some(10_000));
    }

    #[test]
    fn short_cumulative_window_is_not_a_difficulty() {
        // The C++ checks only `timestamps.size()` and then reads
        // `cumulativeDifficulties[60]` and `[59]`; with a 10-entry vector that
        // is an out-of-bounds read. Here it is the startup value, not a panic.
        let timestamps: Vec<u64> = (0..61).map(|i| 1_000_000 + 60 * i).collect();
        let cum = vec![1u64; 10];
        assert_eq!(next_difficulty_v5(&timestamps, &cum), Some(10_000));
        assert_eq!(next_difficulty_v4(&timestamps, &cum), Some(1000));
        assert_eq!(next_difficulty_v3(&timestamps, &cum), Some(1000));
        assert_eq!(next_difficulty(7, 4_213_649, &timestamps, &cum), Some(10_000));
        // one entry short of the window on the cumulative side only
        let cum: Vec<u64> = (0..60).map(|i| 5_000_000_000 + 1_000_000 * i).collect();
        assert_eq!(next_difficulty_v5(&timestamps, &cum), Some(10_000));
    }

    #[test]
    fn sixty_one_equal_timestamps_have_no_difficulty() {
        // Every solvetime is 0, so L is 0 and the C++ divides by zero: SIGFPE,
        // i.e. no such block can be on the majority chain. `None`, never 0.
        let timestamps = vec![1_600_000_000u64; 61];
        let cum: Vec<u64> = (0..61).map(|i| 5_000_000_000 + 1_000_000 * i).collect();
        assert_eq!(next_difficulty_v5(&timestamps, &cum), None);
        assert_eq!(next_difficulty_v4(&timestamps, &cum), None);
        assert_eq!(next_difficulty_v3(&timestamps, &cum), None);
        assert_eq!(next_difficulty(7, 4_213_649, &timestamps, &cum), None);
        // L is also 0 when the positive and negative clamped terms cancel, and
        // one differing timestamp is enough to make it non-zero again.
        let mut timestamps = timestamps;
        timestamps[60] += 60;
        assert!(next_difficulty_v5(&timestamps, &cum).is_some());
    }

    #[test]
    fn legacy_rejects_a_short_cumulative_window() {
        assert_eq!(next_difficulty_legacy(3, 2, &[1529831318, 1529831318], &[1]), None);
        assert_eq!(next_difficulty_legacy(3, 2, &[1529831318, 1529831318], &[]), None);
    }

    #[test]
    fn legacy_first_blocks() {
        // Blocks 1 and 2 have difficulty 1 (fewer than two entries); block 3 is 60 (07 "Legacy algorithm").
        assert_eq!(next_difficulty_legacy(1, 0, &[], &[]), Some(1));
        assert_eq!(next_difficulty_legacy(2, 1, &[1529831318], &[1]), Some(1));
        // block 3: v3 branch, parent index 2, window entries blocks 1..2 (genesis excluded)
        assert_eq!(next_difficulty_legacy(3, 2, &[1529831318, 1529831318], &[1, 2]), Some(60));
    }
}
