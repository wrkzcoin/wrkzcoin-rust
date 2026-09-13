// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The 4,300,000 fork (mixin tier V6: rings of up to eight) at the block level,
//! where one C++ rule decides everything: a block's transactions are judged at
//! the **previous** block's index (`Core::addBlock`, spec/06). So the block *at*
//! 4,300,000 is still judged under V5 — at most one decoy — and the first block
//! that may carry a larger ring is 4,300,001.
//!
//! The pool judges a transaction at the **top** index (`Core.cpp:2238`), which
//! is exactly the index the next block will be judged at; the second test pins
//! that the two agree at every height across the boundary. A wallet choosing
//! its ring by the network's top index (`Nigel::networkBlockCount`) therefore
//! never builds a transaction the next block would refuse.
//!
//! The chain is the harness's real chain, seeded just below the fork: every key,
//! ring signature and state rule runs.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
mod chainbuild;

use chainbuild::*;
use wrkz_chain::validate::{validate_transaction, TxContext, TxRule, ValidatorState};
use wrkz_chain::{ChainError, Rule};
use wrkz_primitives::tx::Transaction;

/// Two blocks below the fork height, so both V5-judged blocks can be tried.
const SEED_TIP: u32 = 4_299_998;
const FORK: u32 = 4_300_000;

/// A ring of three: mixin 2, which V5 refuses and V6 allows.
fn ring_of_three(chain: &Chain, tag: &[u8]) -> Transaction {
    build_spend_ring(chain.tip(), &chain.outputs[0], &[&chain.outputs[1], &chain.outputs[2]], &chain.payee, FEE, tag)
}

fn is_mixin_refusal(err: &ChainError) -> bool {
    matches!(err, ChainError::Rule(Rule::Transaction { rule: TxRule::InvalidMixin(_), .. }))
}

/// What the pool says of `tx` on this chain: `Core::addTransactionToPool`'s
/// context, the top index and `isPoolTransaction`.
fn pool_verdict(chain: &Chain, tx: &Transaction) -> Result<(), TxRule> {
    let blob = tx.to_bytes().expect("serializes");
    let ctx = TxContext {
        block_height: u64::from(chain.tip()),
        block_median_size: chain.state.block_median_size(),
        block_timestamp: chain.state.tip_info().expect("seeded").timestamp,
        is_pool_transaction: true,
        checkpoints: chain.state.checkpoints(),
    };
    match validate_transaction(tx, &blob, &mut ValidatorState::new(), &chain.state, &ctx) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.rule().cloned().expect("a rule, not a state fault")),
    }
}

#[test]
fn the_first_block_that_may_carry_a_ring_of_three_is_4_300_001() {
    let mut chain = Chain::with_tip(SEED_TIP, 9);
    // Blocks 4,299,999 and 4,300,000 are judged at 4,299,998 and 4,299,999.
    for index in [FORK - 1, FORK] {
        assert_eq!(chain.tip() + 1, index);
        let (blob, txs) = chain.build_only(&[ring_of_three(&chain, b"early")]);
        let err = chain.state.add_block(&blob, &txs).expect_err("a V5-judged block refuses a ring of three");
        assert!(is_mixin_refusal(&err), "block {index}: {err}");
        // A ring of two is fine on either side. Each spends its own output: a
        // key image may only be spent once.
        let (real, decoy) = if index == FORK { (5, 6) } else { (3, 4) };
        let two = build_spend(
            chain.tip(),
            &chain.outputs[real],
            &chain.outputs[decoy],
            &chain.payee,
            FEE,
            &[b'2', real as u8],
        );
        chain.push(&[two]);
    }
    // The tip is the fork height itself: the next block is judged under V6.
    assert_eq!(chain.tip(), FORK);
    chain.push(&[ring_of_three(&chain, b"on time")]);
    assert_eq!(chain.tip(), FORK + 1);
}

#[test]
fn the_pool_agrees_with_the_next_block_on_every_side_of_the_fork() {
    let mut chain = Chain::with_tip(SEED_TIP, 9);
    while chain.tip() <= FORK {
        let tx = ring_of_three(&chain, &chain.tip().to_le_bytes());
        let pool = pool_verdict(&chain, &tx);
        let (blob, txs) = chain.build_only(std::slice::from_ref(&tx));
        let next_block_accepts = {
            // Try the block on a throwaway copy of the state, so the loop can
            // go on building on the real one.
            let mut trial = Chain::with_tip(SEED_TIP, 9);
            for built in &chain.built {
                trial.state.add_block(&built.blob, &built.tx_blobs).expect("replays");
            }
            trial.state.add_block(&blob, &txs).is_ok()
        };
        assert_eq!(pool.is_ok(), next_block_accepts, "tip {}: pool {pool:?}", chain.tip());
        assert_eq!(
            pool.is_ok(),
            chain.tip() >= FORK,
            "tip {}: the pool follows the tier at the top index",
            chain.tip()
        );
        chain.push_empty();
    }
}
