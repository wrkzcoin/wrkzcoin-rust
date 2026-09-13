// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The transaction pool and block templates (spec/06 "Mempool policy",
//! spec/07 "Block templates", spec/09 `getblocktemplate` / `submitblock`).
//!
//! The C++ splits this work across four files; this crate keeps the same split
//! and cites the line each rule came from:
//!
//! - [`pool`] — `TransactionPool` (`src/cryptonotecore/TransactionPool.cpp`)
//!   and the admission path of `Core::addTransactionToPool` /
//!   `Core::isTransactionValidForPool` (`Core.cpp:2144-2248`), plus
//!   `Core::checkAndRemoveInvalidPoolTransactions` (`Core.cpp:1833`),
//!   `Core::copyTransactionsToPool` (`Core.cpp:567`) and the age limit of
//!   `TransactionPoolCleanWrapper::clean` (`TransactionPoolCleaner.cpp:118`).
//! - [`revalidate`] — `ValidateTransaction::revalidateAfterHeightChange`
//!   (`ValidateTransaction.cpp:120`), the cheap re-check `fillBlockTemplate`
//!   and the cleaner run at a new height. It lives in
//!   [`wrkz_chain::validate`] next to the full validator whose rules it
//!   re-runs, and is re-exported here; this module is the old path to it.
//! - [`chain`] — [`PoolChain`], the view of the chain both halves read, and
//!   [`TemplateContext`], everything a template needs from it in one struct so
//!   that a template can also be built against a foreign daemon's numbers.
//! - [`coinbase`] — `Currency::constructMinerTx` (`Currency.cpp:242`).
//! - [`template_builder`] — `Core::getBlockTemplate` (`Core.cpp:2313`),
//!   `Core::fillBlockTemplate` (`Core.cpp:4353`), the coinbase rebuild loop
//!   (`Core.cpp:2472-2560`), the `reserved_offset` search
//!   (`RpcServer.cpp:1656`) and `Core::submitBlock` (`Core.cpp:1952`).
//!
//! Nothing here is consensus: a node that builds a worse template still
//! follows the chain. It is nonetheless written to produce the *same bytes* as
//! the C++ daemon, because a template that differs is a template a pool can
//! tell apart, and because the coinbase rebuild loop and the reward interact
//! with rules that are consensus.
//!
//! # Example
//!
//! ```
//! use wrkz_chain::{ChainState, Checkpoints, Config};
//! use wrkz_mempool::{TemplateOptions, TransactionPool};
//! use wrkz_storage::MemStore;
//!
//! let mut chain =
//!     ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet())
//!         .expect("genesis applies");
//! chain.set_clock(Some(1_800_000_000));
//! let mut pool = TransactionPool::new(Default::default());
//! let address = "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
//! let t = wrkz_mempool::build_template(&chain, &mut pool, address, 8, None, &TemplateOptions::default())
//!     .expect("template");
//! assert_eq!(t.height, 1);
//! // The reserve is `reserve_size` zero bytes inside the coinbase extra nonce.
//! assert_eq!(&t.blob[t.reserved_offset as usize..][..8], &[0u8; 8]);
//! ```

pub mod chain;
pub mod coinbase;
pub mod pool;
pub mod template_builder;

/// `ValidateTransaction::revalidateAfterHeightChange`, which now lives in
/// `wrkz-chain` beside the rules it shares with `validate_transaction`.
///
/// Kept as a re-export so that `wrkz_mempool::revalidate::*` still resolves.
/// One implementation of each rule: a divergence between this and the full
/// validator is no longer possible, because there is nothing to diverge from.
pub mod revalidate {
    pub use wrkz_chain::validate::{revalidate_after_height_change, RevalidateContext, TxValidation as Revalidated};
}

pub use chain::{PoolChain, TemplateContext, TemplateContextCache};
pub use coinbase::{construct_miner_tx, MinerTxError};
pub use pool::{
    PoolConfig, PoolEntry, PoolRelay, PoolSource, PoolStatus, PoolWithChain, RejectionCategory, TransactionPool,
    TxPriority,
};
pub use template_builder::{
    build_template, build_template_cached, build_template_from_context, submit_block, submit_block_update,
    BlockTemplateResult, SubmitStatus, TemplateError, TemplateOptions, TemplateTransaction,
};
pub use wrkz_chain::validate::{revalidate_after_height_change, RevalidateContext};
