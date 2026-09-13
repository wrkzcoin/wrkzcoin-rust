// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `getblockheaderbyhash` for a block on an alternative chain. The C++ looks a
//! hash up in every segment (`Core::getBlockByHash`), so a block that lost a
//! race still has a header, marked `orphan_status: true`.
//!
//! The block has to be real — `ChainState::add_block` validates it before it
//! is held — so this borrows `wrkz-chain`'s chain harness, which mines valid
//! blocks at difficulty 1 above the last checkpoint.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
#[path = "../../wrkz-chain/tests/chainbuild/mod.rs"]
mod chainbuild;

use chainbuild::{Chain, AMOUNT, TIP};
use wrkz_chain::{AddStatus, Config};
use wrkz_mempool::TransactionPool;
use wrkz_rpc::api::NodeApi;
use wrkz_rpc::node::ChainNode;

#[test]
fn an_alternative_block_has_a_header_marked_as_orphaned() {
    // Bodies kept: a header reads the block back.
    let cfg = Config { store_raw_blocks: true, unwind_history: u32::MAX, recent_window: 256, ..Config::default() };
    let mut chain = Chain::with_config(9, cfg);
    let fork = chain.tip_hash();
    chain.push_empty();
    chain.push_empty();
    let (blob, alt_hash) = chain.branch_block(fork, TIP + 1, b"alternative");
    let status = chain.state.add_block(&blob, &[]).expect("a valid block on a shorter branch").status;
    assert_eq!(status, AddStatus::Alternative);

    let node = ChainNode::standalone(chain.state, TransactionPool::new(Default::default()));
    let main = node.block_header_by_index(u64::from(TIP + 1)).unwrap().expect("the main block at that height");
    assert!(!main.orphan_status);
    assert_ne!(main.hash, alt_hash);

    let h = node.block_header_by_hash(&alt_hash).unwrap().expect("held as an alternative");
    assert!(h.orphan_status, "`extraDetails.isAlternative`");
    assert_eq!(h.hash, alt_hash);
    assert_eq!(h.height, u64::from(TIP + 1));
    assert_eq!(h.prev_hash, fork);
    assert_eq!(h.num_txes, 1, "the coinbase counts");
    assert_eq!(h.reward, AMOUNT);
    assert_eq!(h.block_size, main.block_size, "the same shape of block, so the same size");
    assert_eq!(h.difficulty, main.difficulty, "the C++ reads the main chain's difficulty at that height");

    // A hash held nowhere is still "does not exist".
    assert_eq!(node.block_header_by_hash(&[0x42; 32]).unwrap(), None);
}
