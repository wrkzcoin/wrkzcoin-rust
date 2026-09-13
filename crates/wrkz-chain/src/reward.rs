// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The block reward and the size penalty (`Currency::getBlockReward`,
//! `Currency.cpp:189`; `getPenalizedAmount`, `CryptoNoteBasicImpl.cpp:25`;
//! spec/07 "Reward").

use wrkz_primitives::constants::*;

/// `Common::medianValue` (`src/common/Math.h:14`).
///
/// Sorts in place, returns `T()` (zero) for an empty vector, the middle element
/// for an odd count and the **mean of the two middle elements** for an even
/// one — note the even case, which a "take the upper middle" median would get
/// wrong for every block whose window holds an even number of sizes.
pub fn median_value(v: &mut [u64]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    if v.len() == 1 {
        return v[0];
    }
    let n = v.len() / 2;
    v.sort_unstable();
    if v.len() % 2 == 1 {
        v[n]
    } else {
        // The C++ adds in uint64_t and would wrap; sizes are bounded by the
        // block size cap, so the sum cannot come close, and u128 keeps the
        // arithmetic honest either way.
        ((v[n - 1] as u128 + v[n] as u128) / 2) as u64
    }
}

/// `getPenalizedAmount(amount, medianSize, currentBlockSize)`.
///
/// `amount` unchanged while `size <= median`; otherwise
/// `amount · size · (2·median − size) / median²`, floor.
///
/// The C++ computes `mul128` then `div128_32` **twice** with `medianSize`
/// narrowed to `uint32_t`. Doing the whole thing in `u128` is the same value
/// for every median the chain can produce (a median is at most the block size
/// cap, far below `u32::MAX`): two successive floor divisions by `m` and one
/// floor division by `m²` agree because `m` divides `m²`.
pub fn get_penalized_amount(amount: u64, median_size: u64, current_block_size: u64) -> u64 {
    if amount == 0 {
        return 0;
    }
    if current_block_size <= median_size {
        return amount;
    }
    // The caller has already rejected `size > 2 * median`, so `2m - s` is
    // positive; saturating here keeps a mis-ordered call at zero rather than
    // wrapping into a huge multiplier.
    let factor = current_block_size as u128 * (2u128 * median_size as u128).saturating_sub(current_block_size as u128);
    let product = amount as u128 * factor;
    let m = median_size as u128;
    (product / m / m) as u64
}

/// What `Currency::getBlockReward` produces: the reward the coinbase must equal
/// and the signed change to `alreadyGeneratedCoins`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reward {
    pub reward: u64,
    /// `penalizedBaseReward − (fee − penalizedFee)`; added to the parent's
    /// `alreadyGeneratedCoins`.
    pub emission_change: i64,
}

/// `Currency::getBlockReward(...)`. `None` is the C++ `return false`:
/// `currentBlockSize > 2 · median`, reported by the caller as
/// `CUMULATIVE_BLOCK_SIZE_TOO_BIG`.
///
/// `median_size` is the median of the previous `CRYPTONOTE_REWARD_BLOCKS_WINDOW`
/// block sizes **including genesis** when fewer exist
/// (`Core.cpp:1611`, `UseGenesis(true)`); it is raised to the granted full
/// reward zone of the block's major version before anything else.
pub fn get_block_reward(
    block_major_version: u8,
    median_size: u64,
    current_block_size: u64,
    already_generated_coins: u64,
    fee: u64,
    block_height: u64,
) -> Option<Reward> {
    // `Currency.cpp:201`: flat from FIXED_REWARD_V1_HEIGHT, otherwise the
    // emission curve. `alreadyGeneratedCoins` above the supply would make the
    // C++ subtraction wrap; saturating gives a base reward of 0 instead, and
    // the assert in the C++ says such a state is not supposed to exist.
    let base_reward = if block_height >= FIXED_REWARD_V1_HEIGHT {
        FIXED_REWARD_V1
    } else {
        MONEY_SUPPLY.saturating_sub(already_generated_coins) >> EMISSION_SPEED_FACTOR
    };

    let median_size = median_size.max(full_reward_zone(block_major_version) as u64);
    if current_block_size > 2 * median_size {
        return None;
    }

    let penalized_base_reward = get_penalized_amount(base_reward, median_size, current_block_size);
    // `Currency.cpp:224`: the fee is only penalized from block major version 2.
    let penalized_fee = if block_major_version >= wrkz_primitives::block::BLOCK_MAJOR_VERSION_2 {
        get_penalized_amount(fee, median_size, current_block_size)
    } else {
        fee
    };

    // The C++ computes both in 64-bit and lets them wrap; `penalizedFee <= fee`
    // and `penalizedBaseReward <= baseReward <= MONEY_SUPPLY`, so neither the
    // subtraction nor the addition can leave the range here.
    let emission_change = penalized_base_reward as i128 - (fee as i128 - penalized_fee as i128);
    Some(Reward { reward: penalized_base_reward + penalized_fee, emission_change: emission_change as i64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_matches_the_cpp() {
        assert_eq!(median_value(&mut []), 0);
        assert_eq!(median_value(&mut [7]), 7);
        // Even: the mean of the two middle values, floored.
        assert_eq!(median_value(&mut [1, 2, 3, 4]), 2);
        assert_eq!(median_value(&mut [1, 2, 4, 5]), 3);
        assert_eq!(median_value(&mut [5, 4, 2, 1]), 3);
        // Odd: the upper middle.
        assert_eq!(median_value(&mut [1, 2, 3]), 2);
        assert_eq!(median_value(&mut [3, 1, 2, 100, 4]), 3);
    }

    #[test]
    fn penalty_is_flat_below_the_median_and_quadratic_above() {
        assert_eq!(get_penalized_amount(0, 100, 200), 0);
        assert_eq!(get_penalized_amount(1000, 100, 100), 1000);
        assert_eq!(get_penalized_amount(1000, 100, 50), 1000);
        // size = 1.5 * median: 1000 * 150 * (200-150) / 100^2 = 750
        assert_eq!(get_penalized_amount(1000, 100, 150), 750);
        // size = 2 * median: the factor is zero.
        assert_eq!(get_penalized_amount(1000, 100, 200), 0);
    }

    #[test]
    fn spec_07_reference_rewards() {
        // Block 1: (5e13 - 1.5e12) >> 22 = 11563301, no penalty.
        let r = get_block_reward(1, 0, 300, 1_500_000_000_000, 0, 1).unwrap();
        assert_eq!(r.reward, 11_563_301);
        assert_eq!(r.emission_change, 11_563_301);
        // Block 4,213,000: the flat reward.
        let r = get_block_reward(7, 0, 211, 5_000_000_000_000, 0, 4_213_000).unwrap();
        assert_eq!(r.reward, 1_000_000);
        // The size ceiling: 2 * max(median, zone).
        assert!(get_block_reward(7, 0, 200_001, 0, 0, 4_213_000).is_none());
        assert!(get_block_reward(7, 0, 200_000, 0, 0, 4_213_000).is_some());
        // v1's zone is 10000, so its ceiling is 20000.
        assert!(get_block_reward(1, 0, 20_001, 0, 0, 1).is_none());
    }

    #[test]
    fn fee_is_only_penalized_from_version_2() {
        let base = MONEY_SUPPLY >> EMISSION_SPEED_FACTOR;
        // v1's zone is 10000, so a 15000-byte block is 1.5x: the base reward is
        // penalized, the fee is not (`Currency.cpp:224`).
        let v1 = get_block_reward(1, 0, 15_000, 0, 1000, 1).unwrap();
        assert_eq!(v1.reward, get_penalized_amount(base, 10_000, 15_000) + 1000);
        assert_eq!(v1.emission_change, get_penalized_amount(base, 10_000, 15_000) as i64);
        // v2's zone is 20000, so 30000 is 1.5x and the fee is penalized to 750.
        let v2 = get_block_reward(2, 0, 30_000, 0, 1000, 1).unwrap();
        assert_eq!(v2.reward, get_penalized_amount(base, 20_000, 30_000) + 750);
        assert_eq!(v2.emission_change, get_penalized_amount(base, 20_000, 30_000) as i64 - 250);
    }
}
