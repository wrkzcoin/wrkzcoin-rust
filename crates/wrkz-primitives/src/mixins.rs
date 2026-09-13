// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Mixin tiers and the ring-size rule (spec/06-transactions.md rule 8;
//! `src/utilities/Mixins.cpp`, `src/cryptonotecore/Mixins.h`).

use crate::constants::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixinRange {
    pub min: u64,
    pub max: u64,
    pub default: u64,
}

/// `Utilities::getMixinAllowableRange(height)`.
pub fn mixin_allowable_range(height: u64) -> MixinRange {
    if height >= MIXIN_LIMITS_V6_HEIGHT {
        MixinRange { min: MINIMUM_MIXIN_V6, max: MAXIMUM_MIXIN_V6, default: DEFAULT_MIXIN_V6 }
    } else if height >= MIXIN_LIMITS_V5_HEIGHT {
        MixinRange { min: MINIMUM_MIXIN_V5, max: MAXIMUM_MIXIN_V5, default: DEFAULT_MIXIN_V5 }
    } else if height >= MIXIN_LIMITS_V4_HEIGHT {
        MixinRange { min: MINIMUM_MIXIN_V4, max: MAXIMUM_MIXIN_V4, default: DEFAULT_MIXIN_V4 }
    } else if height >= MIXIN_LIMITS_V3_HEIGHT {
        MixinRange { min: MINIMUM_MIXIN_V3, max: MAXIMUM_MIXIN_V3, default: DEFAULT_MIXIN_V3 }
    } else if height >= MIXIN_LIMITS_V2_HEIGHT {
        MixinRange { min: MINIMUM_MIXIN_V2, max: MAXIMUM_MIXIN_V2, default: DEFAULT_MIXIN_V2 }
    } else if height >= MIXIN_LIMITS_V1_HEIGHT {
        MixinRange { min: MINIMUM_MIXIN_V1, max: MAXIMUM_MIXIN_V1, default: DEFAULT_MIXIN_V1 }
    } else {
        MixinRange { min: 0, max: u64::MAX, default: DEFAULT_MIXIN_V0 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MixinError {
    TooLarge { largest_mixin: u64, max: u64 },
    TooSmall { smallest_mixin: u64, min: u64 },
}

/// `Mixins::validate(transaction, minMixin, maxMixin, height)` given the ring
/// size of every key input. Below `MIXIN_LIMITS_V6_HEIGHT` the floor is
/// judged on the *largest* ring (historical behaviour blocks depend on);
/// from it, on the smallest.
pub fn validate_ring_sizes(ring_sizes: &[usize], height: u64) -> Result<(), MixinError> {
    let range = mixin_allowable_range(height);
    let mut largest_ring: u64 = 1;
    let mut smallest_ring: u64 = u64::MAX;
    let have_key_input = !ring_sizes.is_empty();
    for &r in ring_sizes {
        let r = r as u64;
        if r > largest_ring {
            largest_ring = r;
        }
        if r < smallest_ring {
            smallest_ring = r;
        }
    }
    if !have_key_input || smallest_ring < 1 {
        smallest_ring = if have_key_input { 1 } else { largest_ring };
    }
    let largest_mixin = largest_ring - 1;
    let smallest_mixin = if height >= MIXIN_LIMITS_V6_HEIGHT { smallest_ring - 1 } else { largest_mixin };
    if largest_mixin > range.max {
        return Err(MixinError::TooLarge { largest_mixin, max: range.max });
    }
    if smallest_mixin < range.min {
        return Err(MixinError::TooSmall { smallest_mixin, min: range.min });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers() {
        assert_eq!(mixin_allowable_range(0), MixinRange { min: 0, max: u64::MAX, default: 3 });
        assert_eq!(mixin_allowable_range(302_400), MixinRange { min: 3, max: 7, default: 3 });
        assert_eq!(mixin_allowable_range(4_213_649), MixinRange { min: 1, max: 1, default: 1 });
        assert_eq!(mixin_allowable_range(4_300_000), MixinRange { min: 1, max: 7, default: 7 });
    }

    #[test]
    fn floor_semantics_change_at_v6() {
        // one full ring and one ring of 1: passes below 4.3M (floor judged on largest), fails from it.
        assert_eq!(validate_ring_sizes(&[2, 1], 4_213_649), Ok(()));
        assert_eq!(validate_ring_sizes(&[8, 1], 4_299_999), Err(MixinError::TooLarge { largest_mixin: 7, max: 1 }));
        assert_eq!(validate_ring_sizes(&[8, 1], 4_300_000), Err(MixinError::TooSmall { smallest_mixin: 0, min: 1 }));
        assert_eq!(validate_ring_sizes(&[8, 2], 4_300_000), Ok(()));
        assert_eq!(validate_ring_sizes(&[9], 4_300_000), Err(MixinError::TooLarge { largest_mixin: 8, max: 7 }));
        // no key inputs: smallest ring clamps to 1, so a min of 1 fails (such a tx is rejected by the input checks first)
        assert_eq!(validate_ring_sizes(&[], 4_300_000), Err(MixinError::TooSmall { smallest_mixin: 0, min: 1 }));
        assert_eq!(validate_ring_sizes(&[], 302_400), Err(MixinError::TooSmall { smallest_mixin: 0, min: 3 }));
        assert_eq!(validate_ring_sizes(&[], 9_999), Ok(()));
        // tier at 302,400: min 3 -> ring 4 ok, ring 3 too small
        assert_eq!(validate_ring_sizes(&[4, 4, 4, 4], 302_400), Ok(()));
        assert!(validate_ring_sizes(&[3], 302_400).is_err());
    }
}
