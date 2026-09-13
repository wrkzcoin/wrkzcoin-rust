// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Fee rules (spec/06-transactions.md "Fee"; `src/utilities/Utilities.cpp:293-343`).
//!
//! `h` is the height a transaction is judged at: the previous block's index
//! inside a block, the top index at pool admission.

use crate::constants::*;

/// `Utilities::getTransactionFee(size, height, feePerByte)`.
/// The C++ does the chunk arithmetic in `double`; with the two rates in use
/// every product is an exact integer, so integer math is identical.
pub fn transaction_fee(size: usize, h: u64, fee_per_byte: f64) -> u64 {
    if h <= MINIMUM_FEE_V1_HEIGHT + 1 {
        MINIMUM_FEE
    } else if h < MINIMUM_FEE_PER_BYTE_V1_HEIGHT {
        MINIMUM_FEE_V1
    } else if h < MINIMUM_FEE_PER_BYTE_V2_HEIGHT {
        let chunks = (size as u64).div_ceil(FEE_PER_BYTE_CHUNK_SIZE);
        (chunks as f64 * fee_per_byte * FEE_PER_BYTE_CHUNK_SIZE as f64) as u64
    } else {
        let chunks = (size as u64).div_ceil(FEE_PER_BYTE_CHUNK_SIZE_V2);
        (chunks as f64 * fee_per_byte * FEE_PER_BYTE_CHUNK_SIZE_V2 as f64) as u64
    }
}

/// `Utilities::getMinimumTransactionFee(size, height)` (`Utilities.cpp:328`).
///
/// **Do not fix**: the C++ compares the height against the *rate*
/// `MINIMUM_FEE_PER_BYTE_V1` (1.953125), so the V2 rate has applied since
/// height 2. Blocks 832,000 – 1,499,999 were validated at 20 atomic per
/// started 256-byte chunk (spec/12-roadmap.md, "Things that look like bugs").
pub fn minimum_transaction_fee(size: usize, h: u64) -> u64 {
    let mut rate = MINIMUM_FEE_PER_BYTE_V1;
    if (h as f64) > MINIMUM_FEE_PER_BYTE_V1 {
        rate = MINIMUM_FEE_PER_BYTE_V2;
    }
    transaction_fee(size, h, rate)
}

/// The minimum a *normal* (non-fusion) transaction must pay at `h`
/// (`ValidateTransaction::validateTransactionFee`). The fee must also be non-zero.
pub fn required_minimum_fee(size: usize, h: u64) -> u64 {
    if h >= MINIMUM_FEE_PER_BYTE_V1_HEIGHT {
        minimum_transaction_fee(size, h)
    } else if h > MINIMUM_FEE_V1_HEIGHT + 1 {
        MINIMUM_FEE_V1
    } else {
        MINIMUM_FEE
    }
}

/// Fee rule for a normal transaction. `fee` must be non-zero and at least the minimum.
pub fn is_valid_normal_fee(fee: u64, size: usize, h: u64) -> bool {
    fee != 0 && fee >= required_minimum_fee(size, h)
}

/// Fee rule for a fusion transaction (any fee, except a 10000 floor in one window).
pub fn is_valid_fusion_fee(fee: u64, h: u64) -> bool {
    if (FUSION_FEE_V1_HEIGHT..FUSION_ZERO_FEE_V2_HEIGHT).contains(&h) {
        fee >= FUSION_FEE_V1
    } else {
        true
    }
}

/// The wallet's `getMaxTxSize(height)` = `min(maxBlockCumulativeSize, 125000) - 600` (10, step 13).
pub fn wallet_max_tx_size(height: u64) -> u64 {
    max_block_cumulative_size(height).min(125_000) - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_vectors_from_spec() {
        // 06 acceptance 2: sizes 100, 256, 257, 1000 at H = 700000, 900000, 1600000
        let sizes = [100, 256, 257, 1000];
        let expect: [(u64, [u64; 4]); 3] =
            [(700_000, [50000, 50000, 50000, 50000]), (900_000, [20, 20, 40, 80]), (1_600_000, [10, 20, 30, 80])];
        for (h, want) in expect {
            for (s, w) in sizes.iter().zip(want) {
                assert_eq!(required_minimum_fee(*s, h), w, "size {s} at {h}");
            }
        }
        assert_eq!(required_minimum_fee(500, 678_501), 5);
        assert_eq!(required_minimum_fee(500, 678_502), 50_000);
        assert!(!is_valid_normal_fee(0, 100, 4_213_649));
        assert!(is_valid_fusion_fee(0, 4_213_649));
        assert!(!is_valid_fusion_fee(0, 900_000));
    }

    #[test]
    fn live_tip_transaction_fee() {
        // Block 4,213,650 carries one tx with fee 70 => 7 chunks of 128 => 769..896 bytes.
        assert_eq!(required_minimum_fee(769, 4_213_649), 70);
        assert_eq!(required_minimum_fee(896, 4_213_649), 70);
        assert_eq!(required_minimum_fee(897, 4_213_649), 80);
    }
}
