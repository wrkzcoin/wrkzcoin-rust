// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Fusion transaction detection (`Currency::isFusionTransaction`,
//! `Currency.cpp:353`; spec/06 "Fusion transactions").
//!
//! A fusion transaction pays no fee (outside one window) and needs a different,
//! much harder transaction proof of work, so this predicate decides two
//! consensus rules and has to agree with the C++ on every transaction on chain.

use wrkz_primitives::constants::*;
use wrkz_primitives::tx::{decompose_amount, TransactionPrefix};

/// `Currency::isFusionTransaction(transaction, size, height)`.
///
/// `height` is the previous block's index, the same `blockHeight` the whole
/// validator runs at. The C++ signature takes a `uint32_t`, so a height above
/// `u32::MAX` truncates there; no such height exists, and the tier lookups here
/// take the value as given.
pub fn is_fusion_transaction(prefix: &TransactionPrefix, size: usize, height: u64) -> bool {
    if size > FUSION_TX_MAX_SIZE {
        return false;
    }
    let inputs = &prefix.inputs;
    if inputs.len() < FUSION_TX_MIN_INPUT_COUNT {
        return false;
    }
    if inputs.len() < prefix.outputs.len() * FUSION_TX_MIN_IN_OUT_COUNT_RATIO {
        return false;
    }

    let threshold = default_fusion_dust_threshold(height);
    let mut input_amount: u64 = 0;
    for input in inputs {
        // `getInputsAmounts` takes the amount of every input; a `BaseInput`
        // contributes nothing, and a coinbase never reaches this code.
        let amount = match input {
            wrkz_primitives::tx::Input::Key { amount, .. } => *amount,
            wrkz_primitives::tx::Input::Base { .. } => 0,
        };
        if amount < threshold {
            return false;
        }
        // `inputAmount += amount` in uint64_t: wraps in the C++, wraps here.
        input_amount = input_amount.wrapping_add(amount);
    }

    // `Currency.cpp:385`: the fusion fee window subtracts a flat 10000 from the
    // total before the decomposition. The C++ subtraction is unsigned and would
    // wrap for a total below 10000; such a transaction cannot then match any
    // decomposition, so wrapping and rejecting are the same outcome, but the
    // wrap is what the C++ does.
    if (FUSION_FEE_V1_HEIGHT..FUSION_ZERO_FEE_V2_HEIGHT).contains(&height) {
        input_amount = input_amount.wrapping_sub(FUSION_FEE_V1);
    }

    let mut expected = decompose_amount(input_amount, threshold);
    expected.sort_unstable();
    let actual: Vec<u64> = prefix.outputs.iter().map(|o| o.amount).collect();
    expected == actual
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_primitives::tx::{Input, Output};

    fn tx(input_amounts: &[u64], output_amounts: &[u64]) -> TransactionPrefix {
        TransactionPrefix {
            version: 1,
            unlock_time: 0,
            inputs: input_amounts
                .iter()
                .enumerate()
                .map(|(i, a)| Input::Key { amount: *a, key_offsets: vec![i as u64], key_image: [i as u8; 32] })
                .collect(),
            outputs: output_amounts.iter().map(|a| Output { amount: *a, key: [0; 32] }).collect(),
            extra: Vec::new(),
        }
    }

    #[test]
    fn the_four_shape_rules() {
        // 12 inputs of 100 = 1200 -> decomposed at threshold 0: [200, 1000] sorted.
        let ins = [100u64; 12];
        assert!(is_fusion_transaction(&tx(&ins, &[200, 1000]), 500, 4_213_649));
        // Outputs in the wrong order fail: the comparison is against the sorted list.
        assert!(!is_fusion_transaction(&tx(&ins, &[1000, 200]), 500, 4_213_649));
        // Too few inputs.
        assert!(!is_fusion_transaction(&tx(&[100; 11], &[100, 1000]), 500, 4_213_649));
        // Ratio: 12 inputs allow at most 3 outputs.
        assert!(!is_fusion_transaction(&tx(&ins, &[200, 400, 300, 300]), 500, 4_213_649));
        // Too large.
        assert!(!is_fusion_transaction(&tx(&ins, &[200, 1000]), FUSION_TX_MAX_SIZE + 1, 4_213_649));
    }

    #[test]
    fn the_fee_window_changes_the_expected_decomposition() {
        // 12 inputs of 10000 = 120000; inside [864864, 1123000) the check
        // decomposes 120000 - 10000 = 110000 -> [10000, 100000].
        let ins = [10_000u64; 12];
        assert!(is_fusion_transaction(&tx(&ins, &[10_000, 100_000]), 500, 900_000));
        assert!(!is_fusion_transaction(&tx(&ins, &[20_000, 100_000]), 500, 900_000));
        // Outside the window the full total is decomposed: 120000 -> [20000, 100000].
        assert!(is_fusion_transaction(&tx(&ins, &[20_000, 100_000]), 500, 1_123_000));
    }

    #[test]
    fn the_dust_threshold_moves_at_400000() {
        // Below 400,000 the threshold is 10, so inputs under 10 are not fusion
        // inputs and the low digits fold into one dust amount.
        assert!(!is_fusion_transaction(&tx(&[9; 12], &[108]), 500, 399_999));
        // 12 inputs of 10 = 120: chunks 0 then 100+20 -> dust fold of 0 leaves [20, 100].
        assert!(is_fusion_transaction(&tx(&[10; 12], &[20, 100]), 500, 399_999));
    }
}
