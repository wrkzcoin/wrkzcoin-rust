// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `Currency::constructMinerTx` (`src/cryptonotecore/Currency.cpp:242`).

use wrkz_chain::reward::get_block_reward;
use wrkz_primitives::constants::*;
use wrkz_primitives::tx::{build_extra, decompose_amount, Input, Output, Transaction, TransactionPrefix};
use wrkz_primitives::Hash;

/// Everything `constructMinerTx` can `return false` on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MinerTxError {
    /// `addExtraNonceToTransactionExtra` refused a nonce over
    /// `TX_EXTRA_NONCE_MAX_COUNT` (255).
    ExtraNonceTooLong(usize),
    /// `getBlockReward` returned false: "Block is too big"
    /// (`currentBlockSize > 2 * median`).
    BlockTooBig { size: u64, limit: u64 },
    /// `maxOuts` was zero.
    MaxOutsIsZero,
    /// A derivation or a one-time key could not be produced from the given
    /// keys. The C++ logs and returns false.
    BadKeys,
    /// The C++ sanity check `summaryAmounts == blockReward`. Unreachable: the
    /// decomposition and the merge both preserve the total.
    AmountMismatch { summary: u64, reward: u64 },
}

impl std::fmt::Display for MinerTxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MinerTxError::ExtraNonceTooLong(n) => write!(f, "extra nonce is {n} bytes, maximum 255"),
            MinerTxError::BlockTooBig { size, limit } => write!(f, "Block is too big ({size} > {limit})"),
            MinerTxError::MaxOutsIsZero => write!(f, "max_out must be non-zero"),
            MinerTxError::BadKeys => write!(f, "while creating outs: failed to derive the one-time key"),
            MinerTxError::AmountMismatch { summary, reward } => {
                write!(f, "summaryAmounts = {summary} not equal blockReward = {reward}")
            }
        }
    }
}

impl std::error::Error for MinerTxError {}

/// What the coinbase build produced, beside the transaction itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MinerTx {
    /// The reward the coinbase pays: `penalizedBaseReward + penalizedFee`.
    pub reward: u64,
    /// The one-time transaction public key written into `extra`, which the RPC
    /// searches for to place `reserved_offset`.
    pub tx_public_key: Hash,
}

/// `Currency::constructMinerTx(...)`.
///
/// The C++ calls `generateKeyPair()` itself; the pair is a parameter here so
/// that a caller can make a template reproducible (the byte-comparison tests)
/// without changing a single rule. `tx_keys` is `(secret, public)`.
///
/// The output amounts are the reward decomposed into its non-zero decimal
/// digits at the height's dust threshold, then merged from the top while more
/// than `max_outs` remain — `outAmounts[n-2] += outAmounts.back()`, so the
/// merged amount is folded into the *second largest* and the list stays sorted
/// only by construction, not by a re-sort.
pub fn construct_miner_tx(
    block_major_version: u8,
    height: u64,
    median_size: u64,
    already_generated_coins: u64,
    current_block_size: u64,
    fee: u64,
    public_view_key: &Hash,
    public_spend_key: &Hash,
    extra_nonce: &[u8],
    max_outs: usize,
    tx_keys: (Hash, Hash),
) -> Result<(Transaction, MinerTx), MinerTxError> {
    let (tx_secret_key, tx_public_key) = tx_keys;

    // `tx.extra.clear()` then the public key, then the nonce if there is one.
    let nonce = (!extra_nonce.is_empty()).then_some(extra_nonce);
    if extra_nonce.len() > 255 {
        return Err(MinerTxError::ExtraNonceTooLong(extra_nonce.len()));
    }
    let extra = build_extra(&tx_public_key, nonce, None)
        .map_err(|_| MinerTxError::ExtraNonceTooLong(nonce.map_or(0, |n| n.len())))?;

    let reward =
        get_block_reward(block_major_version, median_size, current_block_size, already_generated_coins, fee, height)
            .ok_or(MinerTxError::BlockTooBig {
                size: current_block_size,
                limit: 2 * median_size.max(full_reward_zone(block_major_version) as u64),
            })?
            .reward;

    let mut out_amounts = decompose_amount(reward, default_dust_threshold(height));
    if max_outs == 0 {
        return Err(MinerTxError::MaxOutsIsZero);
    }
    while max_outs < out_amounts.len() {
        let last = out_amounts.pop().expect("more than max_outs >= 1 entries");
        let n = out_amounts.len();
        out_amounts[n - 1] = out_amounts[n - 1].wrapping_add(last);
    }

    let derivation =
        wrkz_pow::curve::generate_key_derivation(public_view_key, &tx_secret_key).ok_or(MinerTxError::BadKeys)?;
    let mut outputs = Vec::with_capacity(out_amounts.len());
    let mut summary: u64 = 0;
    for (index, amount) in out_amounts.iter().enumerate() {
        let key = wrkz_pow::curve::derive_public_key(&derivation, index as u64, public_spend_key)
            .ok_or(MinerTxError::BadKeys)?;
        summary = summary.wrapping_add(*amount);
        outputs.push(Output { amount: *amount, key });
    }
    if summary != reward {
        return Err(MinerTxError::AmountMismatch { summary, reward });
    }

    let tx = Transaction {
        prefix: TransactionPrefix {
            version: wrkz_primitives::tx::CURRENT_TRANSACTION_VERSION,
            unlock_time: height + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW,
            inputs: vec![Input::Base { block_index: height }],
            outputs,
            extra,
        },
        signatures: Vec::new(),
    };
    Ok((tx, MinerTx { reward, tx_public_key }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> ((Hash, Hash), Hash, Hash) {
        let tx = wrkz_pow::curve::generate_deterministic_keys(&[7u8; 32]);
        let (spend_secret, spend_public) = wrkz_pow::curve::generate_deterministic_keys(&[9u8; 32]);
        let (_, view_public) = wrkz_pow::curve::generate_view_from_spend(&spend_secret);
        (tx, spend_public, view_public)
    }

    #[test]
    fn a_flat_reward_decomposes_to_one_output() {
        let (tx_keys, spend, view) = keys();
        // Above FIXED_REWARD_V1_HEIGHT the base reward is 1,000,000 and the
        // dust threshold is 0, so the decomposition is a single chunk.
        let (tx, info) =
            construct_miner_tx(7, 4_214_042, 100_000, 30_000_000_000_000, 300, 0, &view, &spend, &[], 11, tx_keys)
                .unwrap();
        assert_eq!(info.reward, 1_000_000);
        assert_eq!(tx.prefix.outputs.len(), 1);
        assert_eq!(tx.prefix.outputs[0].amount, 1_000_000);
        assert_eq!(tx.prefix.unlock_time, 4_214_042 + 40);
        assert_eq!(tx.prefix.inputs, vec![Input::Base { block_index: 4_214_042 }]);
        // `01 <32 bytes>` and nothing else without an extra nonce.
        assert_eq!(tx.prefix.extra.len(), 33);
        assert_eq!(tx.prefix.extra[0], wrkz_primitives::tx::TX_EXTRA_TAG_PUBKEY);
        assert_eq!(&tx.prefix.extra[1..], &info.tx_public_key[..]);
    }

    #[test]
    fn the_extra_nonce_follows_the_public_key() {
        let (tx_keys, spend, view) = keys();
        let (tx, _) = construct_miner_tx(
            7,
            4_214_042,
            100_000,
            30_000_000_000_000,
            300,
            0,
            &view,
            &spend,
            &[0u8; 8],
            11,
            tx_keys,
        )
        .unwrap();
        assert_eq!(tx.prefix.extra.len(), 33 + 2 + 8);
        assert_eq!(tx.prefix.extra[33], wrkz_primitives::tx::TX_EXTRA_NONCE);
        assert_eq!(tx.prefix.extra[34], 8);
    }

    #[test]
    fn outputs_are_merged_from_the_top_down_to_max_outs() {
        let (tx_keys, spend, view) = keys();
        // Height 1: the emission curve with a dust threshold of 10 gives a
        // reward of 11,563,301, which decomposes into more than two chunks.
        let full =
            construct_miner_tx(1, 1, 10_000, 1_500_000_000_000, 300, 0, &view, &spend, &[], 11, tx_keys).unwrap().0;
        assert!(full.prefix.outputs.len() > 2);
        let merged =
            construct_miner_tx(1, 1, 10_000, 1_500_000_000_000, 300, 0, &view, &spend, &[], 2, tx_keys).unwrap().0;
        assert_eq!(merged.prefix.outputs.len(), 2);
        let total: u64 = merged.prefix.outputs.iter().map(|o| o.amount).sum();
        assert_eq!(total, 11_563_301);
        // The merge folds the tail into the entry before it, so the first
        // output is untouched and the second carries everything above it.
        assert_eq!(merged.prefix.outputs[0].amount, full.prefix.outputs[0].amount);
    }

    #[test]
    fn an_oversized_block_is_block_too_big() {
        let (tx_keys, spend, view) = keys();
        let e = construct_miner_tx(7, 4_214_042, 100_000, 0, 200_001, 0, &view, &spend, &[], 11, tx_keys).unwrap_err();
        assert_eq!(e, MinerTxError::BlockTooBig { size: 200_001, limit: 200_000 });
    }
}
