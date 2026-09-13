// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The pool and the template builder against a synthetic chain, offline.
//!
//! The chain is seeded at index 4,400,000 exactly as
//! `crates/wrkz-chain/tests/synthetic.rs` seeds it — above every fork height
//! and above the last checkpoint (4,188,000), so block major version 7, mixin
//! tier V6, the unlock-time rule, the transaction proof of work with its fee
//! escape, the ring signature check and the block proof of work all run for
//! real. The seed helpers are copied rather than shared because `wrkz-chain`'s
//! test binary cannot be depended on; the copy is marked where it starts.
//!
//! Difficulty stays at 1 (61 seeded blocks of difficulty 1 spaced 59 seconds
//! apart), so a template can be "mined" by trying one nonce.
//!
//! Every transaction here pays a fee of at least
//! `TRANSACTION_POW_PASS_WITH_FEE` (10,000) so that the transaction proof of
//! work passes by the fee escape rather than by a 45,000-difficulty search: the
//! escape is the rule the live chain runs on, and a test that mined every
//! transaction would take minutes.

use std::collections::HashSet;
use wrkz_chain::records::{BlockInfo, OutputRecord};
use wrkz_chain::validate::TxRule;
use wrkz_chain::{keys, records, AddStatus, ChainState, Checkpoints, Config};
use wrkz_mempool::pool::{add_block_with_pool, PoolRelay, PoolSource, PoolStatus, PoolWithChain};
use wrkz_mempool::template_builder::{fill_block_template, TemplateTransaction};
use wrkz_mempool::{
    build_template, build_template_cached, build_template_from_context, submit_block, PoolConfig, RejectionCategory,
    SubmitStatus, TemplateContext, TemplateContextCache, TemplateOptions, TransactionPool,
};
use wrkz_pow::cn_fast_hash;
use wrkz_pow::curve;
use wrkz_primitives::block::{BlockTemplate, ParentBlock};
use wrkz_primitives::tx::{
    absolute_to_relative_offsets, append_merge_mining_tag, build_extra, BaseTransaction, Input, MergeMiningTag, Output,
    Transaction, TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_storage::{KvStore, MemStore};

// ---------------------------------------------------------------------------
// the seeded chain (adapted from crates/wrkz-chain/tests/synthetic.rs)
// ---------------------------------------------------------------------------

const TIP: u32 = 4_400_000;
const SEED_LEN: u32 = 128;
const BASE: u32 = TIP - SEED_LEN + 1;
const SPACING: u64 = 59;
const TIP_TIME: u64 = 1_800_000_000;
const NOW: u64 = TIP_TIME + 10_000;
const TIP_CUMULATIVE: u64 = 1_000_000_000_000;
const AMOUNT: u64 = 1_000_000;
/// Clears the fee ladder and the transaction proof-of-work fee escape.
const FEE: u64 = 10_000;

/// The daemon's own address from `crates/wrkz-wallet/tests/fixtures/README.md`.
/// Every key behind it is published in `spec/05` and has never held funds.
pub const FIXTURE_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

fn config() -> Config {
    Config { store_raw_blocks: true, unwind_history: 512, recent_window: SEED_LEN as usize, ..Config::default() }
}

struct Wallet {
    spend_secret: Hash,
    spend_public: Hash,
    view_secret: Hash,
    view_public: Hash,
}

impl Wallet {
    fn from_seed(seed: u8) -> Self {
        let (spend_secret, spend_public) = curve::generate_deterministic_keys(&cn_fast_hash(&[seed, 0xA1]));
        let (view_secret, view_public) = curve::generate_view_from_spend(&spend_secret);
        Self { spend_secret, spend_public, view_secret, view_public }
    }
}

#[derive(Clone, Copy)]
struct Spendable {
    global_index: u32,
    public_key: Hash,
    secret_key: Hash,
}

fn derive_output(to: &Wallet, tx_secret: &Hash, tx_public: &Hash, output_index: u64) -> (Hash, Hash) {
    let sender = curve::generate_key_derivation(&to.view_public, tx_secret).expect("derivation");
    let public = curve::derive_public_key(&sender, output_index, &to.spend_public).expect("output key");
    let receiver = curve::generate_key_derivation(tx_public, &to.view_secret).expect("derivation");
    let secret = curve::derive_secret_key(&receiver, output_index, &to.spend_secret);
    (public, secret)
}

fn tx_keys(tag: &[u8]) -> (Hash, Hash) {
    curve::generate_deterministic_keys(&cn_fast_hash(tag))
}

fn seed_hash(index: u32) -> Hash {
    cn_fast_hash(&[b"wrkz-mempool synthetic seed".as_slice(), &index.to_le_bytes()].concat())
}

struct Harness {
    chain: ChainState<MemStore>,
    pool: TransactionPool,
    outputs: Vec<Spendable>,
    miner: Wallet,
    payee: Wallet,
}

fn harness() -> Harness {
    let mut store = MemStore::default();
    let mut ops = Vec::new();
    for i in BASE..=TIP {
        let back = (TIP - i) as u64;
        let info = BlockInfo {
            block_hash: seed_hash(i),
            timestamp: TIP_TIME - back * SPACING,
            block_size: 300,
            cumulative_difficulty: TIP_CUMULATIVE - back,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: i as u64 + 1,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(i), Some(records::encode_hashes(&[seed_hash(i)]))));
    }

    let owner = Wallet::from_seed(1);
    let (tx_secret, tx_public) = tx_keys(b"seed outputs");
    let mut outputs = Vec::new();
    for i in 0..8u64 {
        let (public_key, secret_key) = derive_output(&owner, &tx_secret, &tx_public, i);
        let record = OutputRecord {
            public_key,
            unlock_time: 0,
            transaction_hash: seed_hash(BASE),
            output_index: i as u16,
            block_index: BASE,
        };
        ops.push((keys::output(AMOUNT, i as u32), Some(record.encode())));
        outputs.push(Spendable { global_index: i as u32, public_key, secret_key });
    }
    ops.push((keys::output_count(AMOUNT), Some(8u32.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(TIP.to_le_bytes().to_vec())));
    store.write_batch(ops).unwrap();

    let mut chain = ChainState::open(store, config(), Checkpoints::mainnet()).expect("seeded state opens");
    chain.set_clock(Some(NOW));
    let mut pool = TransactionPool::new(PoolConfig::default());
    pool.set_clock(Some(NOW));
    Harness { chain, pool, outputs, miner: Wallet::from_seed(2), payee: Wallet::from_seed(3) }
}

/// A second seeded chain, at a height chosen so that a *fee-less* fusion
/// transaction is admissible at all: below `FUSION_FEE_V1_HEIGHT` (864,864) a
/// fusion transaction may pay nothing, and below `TRANSACTION_POW_HEIGHT`
/// (1,123,000) there is no transaction proof of work to search for. Those two
/// windows only overlap below 864,864 — at and above it a fusion transaction
/// must pay 10,000 until 1,123,000, and from there it must find a 60,000- or
/// 320,000-difficulty hash, which no test can do.
///
/// 800,001 is inside the checkpoint zone, so `validateTransactionInputsExpensive`
/// is skipped for pool transactions too (`ValidateTransaction.cpp:657` has no
/// `m_isPoolTransaction` term) and no ring members have to exist.
const FUSION_TIP: u32 = 800_000;

fn fusion_chain() -> ChainState<MemStore> {
    let mut store = MemStore::default();
    let mut ops = Vec::new();
    for i in (FUSION_TIP - SEED_LEN + 1)..=FUSION_TIP {
        let back = (FUSION_TIP - i) as u64;
        let info = BlockInfo {
            block_hash: seed_hash(i),
            timestamp: TIP_TIME - back * SPACING,
            block_size: 300,
            cumulative_difficulty: TIP_CUMULATIVE - back,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: i as u64 + 1,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
    }
    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(FUSION_TIP.to_le_bytes().to_vec())));
    store.write_batch(ops).unwrap();
    let mut chain = ChainState::open(store, config(), Checkpoints::mainnet()).expect("seeded state opens");
    chain.set_clock(Some(NOW));
    assert!(chain.checkpoints().is_in_checkpoint_zone(FUSION_TIP as u64 + 1));
    chain
}

/// Twelve key inputs of `amount` and the given outputs, at the fusion chain's
/// height.
///
/// The signatures are placeholders and the ring members do not exist: 800,001
/// is inside the checkpoint zone, so nothing reads either. The mixin tier there
/// is min 1 max 3, so every input carries a ring of two.
fn twelve_input_tx(seed: u16, tag: &[u8], amount: u64, output_amounts: &[u64]) -> (Vec<u8>, Hash) {
    let mut inputs = Vec::new();
    for i in 0..12u16 {
        let (secret, public) = curve::generate_deterministic_keys(&cn_fast_hash(
            &[tag, b"input", &seed.to_le_bytes(), &i.to_le_bytes()].concat(),
        ));
        inputs.push(Input::Key {
            amount,
            key_offsets: vec![i as u64 + 1, 1],
            key_image: curve::generate_key_image(&public, &secret),
        });
    }
    let (_, tx_public) = tx_keys(&[tag, b"key", &seed.to_le_bytes()].concat());
    let outputs = output_amounts
        .iter()
        .enumerate()
        .map(|(i, amount)| {
            let (_, key) = curve::generate_deterministic_keys(&cn_fast_hash(
                &[tag, b"output", &seed.to_le_bytes(), &(i as u16).to_le_bytes()].concat(),
            ));
            Output { amount: *amount, key }
        })
        .collect();
    let tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            // The unlock-time rule starts above `UNLOCK_TIME_HEIGHT` (1,200,000).
            unlock_time: 0,
            inputs,
            outputs,
            extra: build_extra(&tx_public, None, None).expect("extra"),
        },
        // Placeholders: the serializer needs one signature per ring member, and
        // inside the checkpoint zone nothing reads them.
        signatures: vec![vec![[0u8; 64]; 2]; 12],
    };
    (tx.to_bytes().expect("serializes"), tx.hash().expect("hash"))
}

/// A fee-less fusion transaction: twelve inputs of 100, whose total decomposes
/// to exactly `[200, 1000]` at dust threshold 0
/// (`Currency::isFusionTransaction`), and no fee, which is valid below
/// `FUSION_FEE_V1_HEIGHT`.
fn fusion_tx(seed: u16) -> (Vec<u8>, Hash) {
    twelve_input_tx(seed, b"fusion", 100, &[200, 1000])
}

/// A signed spend of `real`, ringed with `decoy`.
fn build_spend(real: &Spendable, decoy: &Spendable, to: &Wallet, fee: u64, tag: &[u8]) -> (Transaction, Vec<u8>, Hash) {
    let (lower, higher, secret_index) =
        if real.global_index < decoy.global_index { (real, decoy, 0usize) } else { (decoy, real, 1usize) };
    let ring = [lower.public_key, higher.public_key];
    let absolute = [lower.global_index as u64, higher.global_index as u64];
    let key_offsets = absolute_to_relative_offsets(&absolute).expect("ascending");
    let key_image = curve::generate_key_image(&real.public_key, &real.secret_key);

    let (tx_secret, tx_public) = tx_keys(tag);
    let (out_key, _) = derive_output(to, &tx_secret, &tx_public, 0);
    let mut tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: TIP as u64 + 20,
            inputs: vec![Input::Key { amount: AMOUNT, key_offsets, key_image }],
            outputs: vec![Output { amount: AMOUNT - fee, key: out_key }],
            extra: build_extra(&tx_public, None, None).expect("extra"),
        },
        signatures: Vec::new(),
    };
    let prefix_hash = tx.prefix.hash();
    let signatures =
        curve::generate_ring_signature(&prefix_hash, &key_image, &ring, &real.secret_key, secret_index).expect("sign");
    tx.signatures.push(signatures);
    let blob = tx.to_bytes().expect("serializes");
    let hash = tx.hash().expect("hash");
    (tx, blob, hash)
}

/// What a miner does to a `getblocktemplate` answer: set the merge-mining tag
/// of the parent coinbase to the block's own auxiliary header hash (depth 0,
/// so the blockchain branch stays empty), then search a nonce.
///
/// The auxiliary header hash is taken over the v2+ header hashing blob, which
/// carries neither the timestamp nor the nonce, so fixing the tag once and then
/// varying the nonce is sound.
fn mine(template: &BlockTemplate, difficulty: u64) -> (BlockTemplate, Vec<u8>) {
    let mut block = template.clone();
    let aux = block.auxiliary_header_hash().expect("aux hash");
    let mut extra = Vec::new();
    append_merge_mining_tag(&mut extra, &MergeMiningTag { depth: 0, merkle_root: aux });
    let coinbase = BaseTransaction {
        prefix: TransactionPrefix { version: 0, unlock_time: 0, inputs: vec![], outputs: vec![], extra },
    };
    block.parent_block = Some(ParentBlock::new(0, 0, [0u8; 32], 1, Vec::new(), coinbase, Vec::new()));
    for nonce in 0..1_000_000u32 {
        block.nonce = nonce;
        if block.check_proof_of_work(difficulty).expect("pow input") {
            let blob = block.to_bytes().expect("serializes");
            return (block, blob);
        }
    }
    panic!("no nonce satisfied difficulty {difficulty}");
}

fn options(now: u64, tag: &[u8]) -> TemplateOptions {
    TemplateOptions { tx_key: Some(tx_keys(tag)), now: Some(now) }
}

// ---------------------------------------------------------------------------
// the pool
// ---------------------------------------------------------------------------

#[test]
fn a_valid_transaction_is_accepted_and_relayable() {
    let mut h = harness();
    let (_, blob, hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1");
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(h.pool.len(), 1);
    assert_eq!(h.pool.size_bytes(), blob.len() as u64);
    assert!(h.pool.contains(&hash));
    assert_eq!(h.pool.get(&hash).unwrap().fee, FEE);
    assert!(!h.pool.get(&hash).unwrap().is_fusion);

    // Offering it again is "already exists in pool", not a re-validation.
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::AlreadyInPool);

    // The relay view the P2P layer gets.
    let relay = PoolWithChain::new(&mut h.pool, &h.chain);
    assert!(relay.has_transaction(&hash));
    assert_eq!(relay.transactions_for_relay(&[hash, [9; 32]]), vec![blob]);
    assert_eq!(relay.pool_transaction_hashes(), vec![hash]);
}

#[test]
fn a_corrupt_blob_is_a_deserialization_failure() {
    let mut h = harness();
    assert_eq!(h.pool.add(&[0xff, 0xff, 0xff], &h.chain, PoolSource::Network), PoolStatus::DeserializationFailed);
}

#[test]
fn a_fee_below_the_ladder_is_rejected() {
    let mut h = harness();
    let (_, blob, _) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, 1, b"cheap");
    let status = h.pool.add(&blob, &h.chain, PoolSource::Rpc);
    match status {
        PoolStatus::Rejected(TxRule::WrongFee { fee, minimum }) => {
            assert_eq!(fee, 1);
            // 10 atomic units per started 128-byte chunk at this height.
            assert_eq!(minimum, wrkz_primitives::fees::required_minimum_fee(blob.len(), TIP as u64));
        }
        other => panic!("expected WRONG_FEE, got {other:?}"),
    }
    assert!(h.pool.is_empty());
}

#[test]
fn a_second_spend_of_the_same_output_is_refused_by_the_pool() {
    let mut h = harness();
    let (_, blob_a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    // The same real output, a different decoy and a different destination key:
    // a different transaction hash with the same key image.
    let (_, blob_b, _) = build_spend(&h.outputs[0], &h.outputs[2], &h.miner, FEE, b"b");
    assert_eq!(h.pool.add(&blob_a, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    match h.pool.add(&blob_b, &h.chain, PoolSource::Rpc) {
        PoolStatus::KeyImageInPool { key_image, holder } => {
            assert_eq!(holder, hash_a);
            assert_eq!(key_image, curve::generate_key_image(&h.outputs[0].public_key, &h.outputs[0].secret_key));
        }
        other => panic!("expected KeyImageInPool, got {other:?}"),
    }
    assert_eq!(h.pool.len(), 1);
}

#[test]
fn a_key_image_already_spent_on_chain_is_refused() {
    let mut h = harness();
    let (_, blob_a, _) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    assert_eq!(h.pool.add(&blob_a, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    // Mine it.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"cb1")).unwrap();
    assert_eq!(t.block.transaction_hashes.len(), 1);
    let (_, blob) = mine(&t.block, t.difficulty);
    assert!(matches!(submit_block(&mut h.pool, &mut h.chain, &blob), SubmitStatus::Added(_)));
    assert!(h.pool.is_empty(), "the mined transaction leaves the pool");

    // A different transaction spending the same output: the chain now holds
    // the key image.
    let (_, blob_b, _) = build_spend(&h.outputs[0], &h.outputs[2], &h.miner, FEE, b"b");
    match h.pool.add(&blob_b, &h.chain, PoolSource::Rpc) {
        PoolStatus::Rejected(TxRule::InputKeyImageAlreadySpent { .. }) => {}
        other => panic!("expected INPUT_KEYIMAGE_ALREADY_SPENT, got {other:?}"),
    }
    // A rule that depends on what the chain holds is never remembered: the
    // second offer is validated again and refused again by the validator.
    match h.pool.add(&blob_b, &h.chain, PoolSource::Rpc) {
        PoolStatus::Rejected(TxRule::InputKeyImageAlreadySpent { .. }) => {}
        other => panic!("expected INPUT_KEYIMAGE_ALREADY_SPENT again, not from the cache: {other:?}"),
    }
    assert_eq!(h.pool.rejection_cache_len(), 0);
    // And the transaction that is now in a block reports as such.
    assert_eq!(h.pool.add(&blob_a, &h.chain, PoolSource::Rpc), PoolStatus::AlreadyInBlockchain);
}

/// The same transaction with input 0's ring signature broken.
fn with_bad_signature(mut tx: Transaction) -> (Vec<u8>, Hash) {
    tx.signatures[0][0][0] ^= 0x01;
    (tx.to_bytes().expect("serializes"), tx.hash().expect("hash"))
}

#[test]
fn a_bad_signature_is_remembered_and_refused_without_validation() {
    let mut h = harness();
    let (tx, _, _) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"forged");
    let (blob, hash) = with_bad_signature(tx);
    let rule = TxRule::InputInvalidSignatures { input: 0 };

    let first = h.pool.add(&blob, &h.chain, PoolSource::Network);
    assert_eq!(first, PoolStatus::Rejected(rule.clone()));
    assert_eq!(first.category(), Some(RejectionCategory::InvalidSignature));
    assert!(first.category().unwrap().is_peer_fault());
    assert_eq!(h.pool.rejection_cache_len(), 1);

    // Offered again: refused on the hash, with the same rule and message.
    let again = h.pool.add(&blob, &h.chain, PoolSource::Network);
    assert_eq!(again, PoolStatus::CachedRejection(rule.clone()));
    assert!(again.is_cached_rejection());
    assert_eq!(again.message(), first.message());
    assert_eq!(again.rule(), Some(&rule));

    // Past the time limit it is validated again (and remembered again).
    h.pool.set_clock(Some(NOW + wrkz_mempool::pool::REJECTION_CACHE_TTL));
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::Rejected(rule.clone()));
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::CachedRejection(rule.clone()));

    // A chain switch forgets it: the ring members may have changed.
    assert!(h.pool.remove_spent_in_chain(&h.chain).is_empty());
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::Rejected(rule.clone()));

    // And so does asking.
    assert!(h.pool.forget_rejection(&hash));
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::Rejected(rule));
    assert!(h.pool.is_empty());
}

#[test]
fn a_height_bound_rejection_is_remembered_only_at_the_tip_it_was_judged_at() {
    let mut h = harness();
    let (_, blob, _) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, 1, b"cheap");
    let first = h.pool.add(&blob, &h.chain, PoolSource::Network);
    let Some(rule @ TxRule::WrongFee { .. }) = first.rule().cloned() else {
        panic!("expected WRONG_FEE, got {first:?}")
    };
    assert_eq!(first.category(), Some(RejectionCategory::InvalidAtHeight));
    assert!(!first.category().unwrap().is_peer_fault());
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::CachedRejection(rule.clone()));

    // A new block moves the context: validated again, then remembered at the
    // new tip.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    let (_, block) = mine(&t.block, t.difficulty);
    assert!(matches!(submit_block(&mut h.pool, &mut h.chain, &block), SubmitStatus::Added(_)));
    let second = h.pool.add(&blob, &h.chain, PoolSource::Network);
    assert!(matches!(second, PoolStatus::Rejected(TxRule::WrongFee { .. })), "{second:?}");
    assert!(h.pool.add(&blob, &h.chain, PoolSource::Network).is_cached_rejection());
}

#[test]
fn the_rejection_cache_holds_at_most_its_capacity() {
    let h = harness();
    let mut pool = TransactionPool::new(PoolConfig { rejection_cache_capacity: 1, ..PoolConfig::default() });
    pool.set_clock(Some(NOW));
    let (a, _) = with_bad_signature(build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a").0);
    let (b, _) = with_bad_signature(build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE, b"b").0);
    assert!(matches!(pool.add(&a, &h.chain, PoolSource::Network), PoolStatus::Rejected(_)));
    assert!(matches!(pool.add(&b, &h.chain, PoolSource::Network), PoolStatus::Rejected(_)));
    assert_eq!(pool.rejection_cache_len(), 1);
    // `a` was pushed out by `b`, so it is validated again.
    assert!(matches!(pool.add(&a, &h.chain, PoolSource::Network), PoolStatus::Rejected(_)));
    assert!(pool.add(&a, &h.chain, PoolSource::Network).is_cached_rejection());
    assert_eq!(pool.rejection_cache_len(), 1);
}

#[test]
fn a_conflicting_spend_is_refused_before_it_is_validated() {
    let mut h = harness();
    let (_, pooled, pooled_hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    assert_eq!(h.pool.add(&pooled, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    // The same key image with a forged signature: the pool refuses it for the
    // key image, without verifying the signature, and remembers nothing.
    let (forged, _) = with_bad_signature(build_spend(&h.outputs[0], &h.outputs[2], &h.miner, FEE, b"b").0);
    let status = h.pool.add(&forged, &h.chain, PoolSource::Network);
    assert!(matches!(status, PoolStatus::KeyImageInPool { holder, .. } if holder == pooled_hash), "{status:?}");
    assert_eq!(status.category(), Some(RejectionCategory::Policy));
    assert_eq!(h.pool.rejection_cache_len(), 0);

    // Once the holder is gone it reaches the validator, which refuses it.
    assert!(h.pool.remove(&pooled_hash));
    assert_eq!(
        h.pool.add(&forged, &h.chain, PoolSource::Network),
        PoolStatus::Rejected(TxRule::InputInvalidSignatures { input: 0 })
    );
}

#[test]
fn a_full_pool_refuses_the_least_profitable_before_validating_it() {
    let h = harness();
    let (_, rich, _) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE * 9, b"rich");
    let (_, mid, _) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE * 3, b"mid");
    let (cheap, _) = with_bad_signature(build_spend(&h.outputs[4], &h.outputs[5], &h.payee, FEE, b"cheap").0);
    assert_eq!(cheap.len(), rich.len());
    let mut pool = TransactionPool::new(PoolConfig { max_size_bytes: 2 * rich.len() as u64, ..PoolConfig::default() });
    pool.set_clock(Some(NOW));
    assert_eq!(pool.add(&rich, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(pool.add(&mid, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    // Forged, but it could not have stayed anyway: refused as `PoolFull`
    // without a signature being checked.
    assert_eq!(pool.add(&cheap, &h.chain, PoolSource::Network), PoolStatus::PoolFull);
    assert_eq!(pool.rejection_cache_len(), 0);
}

// ---------------------------------------------------------------------------
// a chain switch whose earlier block spends a pooled key image
// ---------------------------------------------------------------------------

/// Pool a spend of output 0; put an empty block on the main chain; build a
/// two-block branch from the same parent whose **first** block spends output
/// 0 through a different transaction. Returns the pooled hash and the second
/// branch block, which switches the chain when added — and which is empty, so
/// the sweep against the switching block's own key images cannot see the
/// conflict.
fn prepare_a_switch_onto_a_spending_branch(h: &mut Harness) -> (Hash, Vec<u8>) {
    let (_, pooled, pooled_hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"pooled");
    assert_eq!(h.pool.add(&pooled, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    let (_, rival, rival_hash) = build_spend(&h.outputs[0], &h.outputs[2], &h.miner, FEE, b"rival");

    let ctx = TemplateContext::from_chain(&h.chain).unwrap();
    let main =
        build_template_from_context(&ctx, &[], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"main")).unwrap();
    let (_, main_blob) = mine(&main.block, 1);
    let update = add_block_with_pool(&mut h.pool, &mut h.chain, &main_blob, &[]).unwrap();
    assert_eq!(update.outcome.status, AddStatus::Main);
    assert!(h.pool.contains(&pooled_hash));

    let candidate = TemplateTransaction { hash: rival_hash, blob: rival.clone(), fee: FEE };
    let alt1 =
        build_template_from_context(&ctx, &[candidate], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 61, b"alt1"))
            .unwrap();
    let (alt1_block, alt1_blob) = mine(&alt1.block, 1);
    let update = add_block_with_pool(&mut h.pool, &mut h.chain, &alt1_blob, &[rival]).unwrap();
    assert_eq!(update.outcome.status, AddStatus::Alternative);
    assert!(h.pool.contains(&pooled_hash), "an alternative block does not touch the pool");

    let mut ctx2 = ctx.clone();
    ctx2.height = TIP as u64 + 2;
    ctx2.previous_block_hash = alt1_block.hash().unwrap();
    let alt2 =
        build_template_from_context(&ctx2, &[], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 62, b"alt2")).unwrap();
    let (_, alt2_blob) = mine(&alt2.block, 1);
    (pooled_hash, alt2_blob)
}

#[test]
fn after_a_switch_remove_spent_in_chain_drops_what_an_earlier_branch_block_spent() {
    let mut h = harness();
    let (pooled_hash, alt2_blob) = prepare_a_switch_onto_a_spending_branch(&mut h);

    // What the node does today: the chain switches, and the pool is swept
    // against the switching block's own (empty) key-image set.
    let report = h.chain.add_block_detailed(&alt2_blob, &[]).unwrap();
    assert_eq!(report.outcome.status, AddStatus::AlternativeAndSwitched);
    assert!(h.pool.on_block_added(&h.chain, report.outcome.index, &[], &HashSet::new()).is_empty());
    assert!(h.pool.contains(&pooled_hash), "the gap: the per-block sweep cannot see an earlier block");

    // The sweep the node must add after a switch.
    assert_eq!(h.pool.remove_spent_in_chain(&h.chain), vec![pooled_hash]);
    assert!(h.pool.is_empty());
    assert!(h.pool.remove_spent_in_chain(&h.chain).is_empty());
}

#[test]
fn the_template_skips_a_transaction_whose_key_image_the_chain_spent() {
    let mut h = harness();
    let (pooled_hash, alt2_blob) = prepare_a_switch_onto_a_spending_branch(&mut h);
    let report = h.chain.add_block_detailed(&alt2_blob, &[]).unwrap();
    assert_eq!(report.outcome.status, AddStatus::AlternativeAndSwitched);
    assert!(h.pool.contains(&pooled_hash));

    // No sweep ran, and the revalidation reads no chain state; the key-image
    // lookup in the builder keeps the transaction out of the template and
    // drops it from the pool.
    let ctx = TemplateContext::from_chain(&h.chain).unwrap();
    assert!(fill_block_template(&mut h.pool, &h.chain, &ctx).is_empty());
    assert!(!h.pool.contains(&pooled_hash));
}

#[test]
fn add_block_with_pool_sweeps_the_whole_new_chain_on_a_switch() {
    let mut h = harness();
    let (pooled_hash, alt2_blob) = prepare_a_switch_onto_a_spending_branch(&mut h);
    let update = add_block_with_pool(&mut h.pool, &mut h.chain, &alt2_blob, &[]).unwrap();
    assert_eq!(update.outcome.status, AddStatus::AlternativeAndSwitched);
    assert_eq!(update.removed, vec![pooled_hash]);
    assert!(h.pool.is_empty());
}

#[test]
fn the_cleaner_drops_transactions_past_their_live_time() {
    let mut h = harness();
    let (_, blob, hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"old");
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    // One second short of the live time: kept.
    h.pool.set_clock(Some(NOW + wrkz_primitives::constants::CRYPTONOTE_MEMPOOL_TX_LIVETIME - 1));
    assert!(h.pool.clean(TIP as u64).is_empty());
    assert_eq!(h.pool.len(), 1);

    // At the live time: dropped, and remembered as recently deleted.
    h.pool.set_clock(Some(NOW + wrkz_primitives::constants::CRYPTONOTE_MEMPOOL_TX_LIVETIME));
    assert_eq!(h.pool.clean(TIP as u64), vec![hash]);
    assert!(h.pool.is_empty());
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::RecentlyDeleted);

    // Once the memory of the deletion expires it may be offered again.
    h.pool.set_clock(Some(NOW + 3 * wrkz_primitives::constants::CRYPTONOTE_MEMPOOL_TX_LIVETIME));
    h.pool.clean(TIP as u64);
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Network), PoolStatus::Added);
}

#[test]
fn transactions_are_ordered_by_fee_per_byte() {
    let mut h = harness();
    // Three transactions of the same size, so fee per byte is decided by the
    // fee alone, offered cheapest first.
    let (_, cheap, cheap_hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"cheap");
    let (_, mid, mid_hash) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE * 3, b"mid");
    let (_, rich, rich_hash) = build_spend(&h.outputs[4], &h.outputs[5], &h.payee, FEE * 9, b"rich");
    for blob in [&cheap, &mid, &rich] {
        assert_eq!(h.pool.add(blob, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    }
    assert_eq!(cheap.len(), mid.len());
    assert_eq!(mid.len(), rich.len());
    assert_eq!(h.pool.hashes(), vec![rich_hash, mid_hash, cheap_hash]);

    // And the template split puts all three in the fee-paying list.
    let (regular, fusion) = h.pool.for_block_template();
    assert_eq!(regular.iter().map(|e| e.hash).collect::<Vec<_>>(), vec![rich_hash, mid_hash, cheap_hash]);
    assert!(fusion.is_empty());
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

#[test]
fn a_template_over_an_empty_pool_has_the_daemons_shape() {
    let mut h = harness();
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();

    assert_eq!(t.height, TIP as u64 + 1);
    assert_eq!(t.difficulty, 1);
    assert_eq!(t.block.major_version, 7);
    assert_eq!(t.block.minor_version, 0);
    assert_eq!(t.block.previous_block_hash, seed_hash(TIP));
    assert_eq!(t.block.nonce, 0);
    assert!(t.block.transaction_hashes.is_empty());

    let parent = t.block.parent_block.as_ref().expect("v7 carries a parent block");
    assert_eq!(parent.major_version, 0, "Core.cpp:2365");
    assert_eq!(parent.minor_version, 0);
    assert_eq!(parent.previous_block_hash, [0u8; 32]);
    assert_eq!(parent.transaction_count, 1);
    assert_eq!(hex::encode(&parent.base_transaction().prefix.extra[..3]), "032100");

    // The coinbase: one flat-reward output, unlock at height + 40.
    let cb = &t.block.base_transaction;
    assert_eq!(cb.prefix.inputs, vec![Input::Base { block_index: TIP as u64 + 1 }]);
    assert_eq!(cb.prefix.unlock_time, TIP as u64 + 1 + 40);
    assert_eq!(cb.prefix.outputs.len(), 1);
    assert_eq!(cb.prefix.outputs[0].amount, 1_000_000);
    assert_eq!(t.reward, 1_000_000);
    // `01 ‖ R ‖ 02 ‖ 08 ‖ eight reserved bytes`, and nothing else: with an
    // empty pool the rebuild loop settles on the first try and adds no padding.
    assert_eq!(cb.prefix.extra.len(), 33 + 2 + 8);
    assert_eq!(t.cumulative_size, cb.to_bytes().unwrap().len() as u64);
}

#[test]
fn the_reserved_offset_points_at_the_reserved_bytes() {
    let mut h = harness();
    for reserve in [1usize, 8, 60, 255] {
        let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, reserve, None, &options(TIP_TIME + 60, b"cb"))
            .unwrap();
        let at = t.reserved_offset as usize;
        assert_eq!(&t.blob[at..at + reserve], vec![0u8; reserve], "reserve {reserve}");
        // The two bytes before the reserve are the extra-nonce tag and length,
        // and the 32 before those are the transaction public key.
        assert_eq!(t.blob[at - 2], wrkz_primitives::tx::TX_EXTRA_NONCE);
        assert_eq!(t.blob[at - 1] as usize, reserve);
        assert_eq!(&t.blob[at - 34..at - 2], &t.tx_public_key[..]);
        assert_eq!(t.blob[at - 35], wrkz_primitives::tx::TX_EXTRA_TAG_PUBKEY);
    }

    // An explicit extra nonce takes the place of the zero reserve.
    let nonce = b"mining!!".to_vec();
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, Some(&nonce), &options(TIP_TIME + 60, b"cb"))
        .unwrap();
    let at = t.reserved_offset as usize;
    assert_eq!(&t.blob[at..at + nonce.len()], &nonce[..]);

    // No reserve at all: the C++ leaves the offset at zero and does not search.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    assert_eq!(t.reserved_offset, 0);
}

#[test]
fn a_template_is_deterministic_given_a_fixed_timestamp_and_transaction_key() {
    let mut h = harness();
    let a = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"fixed")).unwrap();
    let b = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"fixed")).unwrap();
    assert_eq!(a.blob, b.blob);
    assert_eq!(a.reserved_offset, b.reserved_offset);

    // A different transaction key changes only the key and the derived output.
    let c = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"other")).unwrap();
    assert_ne!(c.blob, a.blob);
    assert_eq!(c.blob.len(), a.blob.len());
    assert_eq!(c.reserved_offset, a.reserved_offset);
}

#[test]
fn the_cached_context_is_the_one_the_chain_gives() {
    let h = harness();
    let cache = TemplateContextCache::new();
    let cold = TemplateContext::from_chain(&h.chain).unwrap();
    // First call fills it, second is the hit; both must equal a fresh read.
    assert_eq!(cache.context(&h.chain).unwrap(), cold);
    assert_eq!(cache.context(&h.chain).unwrap(), cold);
    cache.clear();
    assert_eq!(cache.context(&h.chain).unwrap(), cold);
}

#[test]
fn a_cached_template_is_the_template_build_template_gives() {
    let mut h = harness();
    let (_, a, _) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    let cache = TemplateContextCache::new();
    let opts = options(TIP_TIME + 60, b"k");
    let plain = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &opts).unwrap();
    // Cold, then warm: the same bytes as the uncached builder, both times.
    for _ in 0..2 {
        let cached = build_template_cached(&h.chain, &mut h.pool, &cache, FIXTURE_ADDRESS, 8, None, &opts).unwrap();
        assert_eq!(cached.blob, plain.blob);
        assert_eq!(cached.reserved_offset, plain.reserved_offset);
        assert_eq!(cached.difficulty, plain.difficulty);
        assert_eq!(cached.height, plain.height);
        assert_eq!(cached.reward, plain.reward);
    }
}

#[test]
fn a_new_block_invalidates_the_cached_context() {
    let mut h = harness();
    let cache = TemplateContextCache::new();
    let before = cache.context(&h.chain).unwrap();

    // Mine the template the cache just described, and add it.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"mine")).unwrap();
    let (_, mined) = mine(&t.block, t.difficulty);
    assert!(matches!(submit_block(&mut h.pool, &mut h.chain, &mined), SubmitStatus::Added(_)));

    let after = cache.context(&h.chain).unwrap();
    assert_ne!(after.height, before.height);
    assert_ne!(after.previous_block_hash, before.previous_block_hash);
    // And it is what a cold read of the new tip gives.
    assert_eq!(after, TemplateContext::from_chain(&h.chain).unwrap());
}

#[test]
fn the_timestamp_is_raised_to_the_median() {
    let mut h = harness();
    // The median of the last 11 seeded timestamps, which the C++ clamps up to.
    let median = TIP_TIME - 5 * SPACING;
    let early = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(median - 500, b"cb")).unwrap();
    assert_eq!(early.block.timestamp, median);
    let late = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(median + 500, b"cb")).unwrap();
    assert_eq!(late.block.timestamp, median + 500);
}

#[test]
fn a_template_carries_the_pool_and_pays_the_fees() {
    let mut h = harness();
    let (_, a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    let (_, b, hash_b) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE * 3, b"b");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(h.pool.add(&b, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    // Priority order: the richer transaction first.
    assert_eq!(t.block.transaction_hashes, vec![hash_b, hash_a]);
    assert_eq!(t.fee, FEE * 4);
    // Far below the granted full reward zone, so no penalty: base + fees.
    assert_eq!(t.reward, 1_000_000 + FEE * 4);
    assert_eq!(t.block.base_transaction.prefix.outputs.iter().map(|o| o.amount).sum::<u64>(), t.reward);
    assert_eq!(
        t.cumulative_size,
        a.len() as u64 + b.len() as u64 + t.block.base_transaction.to_bytes().unwrap().len() as u64
    );
}

#[test]
fn the_size_limit_stops_the_fill() {
    let mut h = harness();
    let (_, a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE * 9, b"a");
    let (_, b, _) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE, b"b");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(h.pool.add(&b, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    // `maxTotalSize = min(1.25 * median, maxCumulative) - 600`. A median that
    // leaves room for one transaction and not two.
    let mut ctx = TemplateContext::from_chain(&h.chain).unwrap();
    let one = a.len() as u64;
    ctx.median_size = (one + 600 + 10) * 100 / 125;
    let selected = fill_block_template(&mut h.pool, &h.chain, &ctx);
    assert_eq!(selected.len(), 1, "only the more profitable transaction fits");
    assert_eq!(selected[0].hash, hash_a);
    // Neither was invalid, so the pool still holds both.
    assert_eq!(h.pool.len(), 2);

    // With the real median both fit.
    let ctx = TemplateContext::from_chain(&h.chain).unwrap();
    assert_eq!(fill_block_template(&mut h.pool, &h.chain, &ctx).len(), 2);
}

#[test]
fn the_reward_is_penalised_once_the_block_passes_the_median() {
    // The granted full reward zone for a v7 block is 100,000 bytes, and
    // `getBlockReward` raises the median to it, so the penalty needs a block
    // above that. `build_template_from_context` only reads a candidate's
    // length, hash and fee, so a stand-in blob is enough to reach the zone.
    let ctx = TemplateContext {
        height: TIP as u64 + 1,
        previous_block_hash: seed_hash(TIP),
        difficulty: 1,
        major_version: 7,
        minor_version: 0,
        median_size: 100_000,
        max_cumulative_size: wrkz_primitives::constants::max_block_cumulative_size(TIP as u64 + 1),
        already_generated_coins: 30_000_000_000_000,
        timestamp_median: None,
    };
    let big = TemplateTransaction { hash: [1; 32], blob: vec![0u8; 150_000], fee: 40_000 };
    let t =
        build_template_from_context(&ctx, &[big], FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    let expected =
        wrkz_chain::reward::get_block_reward(7, 100_000, t.cumulative_size, 30_000_000_000_000, 40_000, TIP as u64 + 1)
            .unwrap();
    assert_eq!(t.reward, expected.reward);
    assert!(t.reward < 1_000_000 + 40_000, "the penalty bit: {} < {}", t.reward, 1_000_000 + 40_000);
    assert_eq!(t.block.base_transaction.prefix.outputs.iter().map(|o| o.amount).sum::<u64>(), t.reward);
    // The unpenalised template of the same shape pays more.
    let small = TemplateTransaction { hash: [1; 32], blob: vec![0u8; 1_000], fee: 40_000 };
    let s =
        build_template_from_context(&ctx, &[small], FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    assert_eq!(s.reward, 1_000_000 + 40_000);
}

// ---------------------------------------------------------------------------
// mining, submission and reorganisation
// ---------------------------------------------------------------------------

#[test]
fn a_mined_template_submits_and_empties_the_pool() {
    let mut h = harness();
    let (_, a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    let (_, b, hash_b) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE * 3, b"b");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(h.pool.add(&b, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    let (block, blob) = mine(&t.block, t.difficulty);

    match submit_block(&mut h.pool, &mut h.chain, &blob) {
        SubmitStatus::Added(outcome) => {
            assert_eq!(outcome.index, TIP + 1);
            assert_eq!(outcome.status, AddStatus::Main);
            assert_eq!(outcome.difficulty, 1);
        }
        other => panic!("expected the block to be added, got {other:?}"),
    }
    assert_eq!(h.chain.tip_index(), Some(TIP + 1));
    assert_eq!(h.chain.tip_info().unwrap().block_hash, block.hash().unwrap());
    assert!(h.pool.is_empty(), "both transactions left the pool with the block");
    assert!(!h.pool.contains(&hash_a) && !h.pool.contains(&hash_b));

    // Submitting the same block again cannot get as far as `ALREADY_EXISTS`:
    // `Core::submitBlock` reassembles the body from the pool, and the pool no
    // longer holds the transactions it just mined.
    match submit_block(&mut h.pool, &mut h.chain, &blob) {
        SubmitStatus::TransactionAbsentInPool(hash) => assert!(hash == hash_a || hash == hash_b),
        other => panic!("expected TRANSACTION_ABSENT_IN_POOL, got {other:?}"),
    }
}

#[test]
fn resubmitting_an_empty_block_is_already_exists_and_still_answers_ok() {
    let mut h = harness();
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    let (_, blob) = mine(&t.block, t.difficulty);
    let first = submit_block(&mut h.pool, &mut h.chain, &blob);
    assert!(matches!(first, SubmitStatus::Added(_)));
    assert!(first.should_relay());

    // `ALREADY_EXISTS` is inside the C++ `BLOCK_ADDED` error condition
    // (`AddBlockErrorCondition.h:73`), so the RPC answers OK - but nothing is
    // relayed a second time.
    let again = submit_block(&mut h.pool, &mut h.chain, &blob);
    assert!(matches!(again, SubmitStatus::AlreadyExists));
    assert!(again.is_ok());
    assert!(!again.should_relay());
}

#[test]
fn submitting_a_template_whose_transactions_the_pool_lost_is_refused() {
    let mut h = harness();
    let (_, a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"cb")).unwrap();
    let (_, blob) = mine(&t.block, t.difficulty);
    h.pool.remove(&hash_a);
    match submit_block(&mut h.pool, &mut h.chain, &blob) {
        SubmitStatus::TransactionAbsentInPool(hash) => assert_eq!(hash, hash_a),
        other => panic!("expected TRANSACTION_ABSENT_IN_POOL, got {other:?}"),
    }
}

#[test]
fn a_reorganisation_returns_the_unwound_transactions_to_the_pool() {
    let mut h = harness();
    let (_, a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    // Main chain: one block at TIP + 1 carrying the transaction.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 60, b"main")).unwrap();
    assert_eq!(t.block.transaction_hashes, vec![hash_a]);
    let (main_block, main_blob) = mine(&t.block, t.difficulty);
    assert!(matches!(submit_block(&mut h.pool, &mut h.chain, &main_blob), SubmitStatus::Added(_)));
    assert!(h.pool.is_empty());

    // A competing empty block at TIP + 1, built from the same parent.
    let mut ctx = TemplateContext::from_chain(&h.chain).unwrap();
    ctx.height = TIP as u64 + 1;
    ctx.previous_block_hash = seed_hash(TIP);
    let alt1 =
        build_template_from_context(&ctx, &[], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 61, b"alt1")).unwrap();
    let (alt1_block, alt1_blob) = mine(&alt1.block, 1);
    let update = add_block_with_pool(&mut h.pool, &mut h.chain, &alt1_blob, &[]).unwrap();
    assert_eq!(update.outcome.status, AddStatus::Alternative);
    assert_eq!(h.chain.tip_info().unwrap().block_hash, main_block.hash().unwrap());

    // A second block on the alternative branch makes it heavier and forces the
    // switch, which puts the main chain's transaction back in the pool.
    ctx.height = TIP as u64 + 2;
    ctx.previous_block_hash = alt1_block.hash().unwrap();
    let alt2 =
        build_template_from_context(&ctx, &[], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 62, b"alt2")).unwrap();
    let (alt2_block, alt2_blob) = mine(&alt2.block, 1);
    let update = add_block_with_pool(&mut h.pool, &mut h.chain, &alt2_blob, &[]).unwrap();

    assert_eq!(update.outcome.status, AddStatus::AlternativeAndSwitched);
    assert_eq!(h.chain.tip_index(), Some(TIP + 2));
    assert_eq!(h.chain.tip_info().unwrap().block_hash, alt2_block.hash().unwrap());
    assert_eq!(update.restored, vec![hash_a], "the unwound block's transaction is back in the pool");
    assert!(h.pool.contains(&hash_a));
    assert_eq!(h.pool.get(&hash_a).unwrap().source, PoolSource::AlternativeBlock);

    // And it is spendable again: the next template picks it up.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 8, None, &options(TIP_TIME + 63, b"cb2")).unwrap();
    assert_eq!(t.block.transaction_hashes, vec![hash_a]);
}

#[test]
fn a_two_block_reorganisation_returns_exactly_the_unwound_transactions() {
    let mut h = harness();
    let (_, a, hash_a) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    let (_, b, hash_b) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE, b"b");
    assert_eq!(h.pool.add(&a, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(h.pool.add(&b, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    let fork = h.chain.tip_info().unwrap().block_hash;

    // Two main-chain blocks, one transaction each: the template takes the
    // first, the next template takes what is left.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"m1")).unwrap();
    let (_, main1_blob) = mine(&t.block, t.difficulty);
    let one = t.block.transaction_hashes.clone();
    assert!(matches!(submit_block(&mut h.pool, &mut h.chain, &main1_blob), SubmitStatus::Added(_)));
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 61, b"m2")).unwrap();
    let (_, main2_blob) = mine(&t.block, t.difficulty);
    let two = t.block.transaction_hashes.clone();
    assert!(matches!(submit_block(&mut h.pool, &mut h.chain, &main2_blob), SubmitStatus::Added(_)));
    assert_eq!(h.chain.tip_index(), Some(TIP + 2));
    assert!(h.pool.is_empty(), "both transactions are mined");
    assert_eq!([one.as_slice(), two.as_slice()].concat().len(), 2);
    // Both are in the chain now, at the two different heights.
    assert!(h.chain.transaction_block_index(&hash_a).unwrap().is_some());
    assert!(h.chain.transaction_block_index(&hash_b).unwrap().is_some());

    // A three-block branch from the fork point outweighs them.
    let mut ctx = TemplateContext::from_chain(&h.chain).unwrap();
    let mut previous = fork;
    let mut restored = Vec::new();
    for (n, tag) in [(1u64, b"alt1".as_slice()), (2, b"alt2"), (3, b"alt3")] {
        ctx.height = TIP as u64 + n;
        ctx.previous_block_hash = previous;
        let alt =
            build_template_from_context(&ctx, &[], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60 + n, tag)).unwrap();
        let (block, blob) = mine(&alt.block, 1);
        previous = block.hash().unwrap();
        let update = add_block_with_pool(&mut h.pool, &mut h.chain, &blob, &[]).unwrap();
        if n < 3 {
            assert_eq!(update.outcome.status, AddStatus::Alternative, "block {n} cannot outweigh two yet");
            assert!(update.restored.is_empty());
        } else {
            assert_eq!(update.outcome.status, AddStatus::AlternativeAndSwitched);
            restored = update.restored;
        }
    }

    // Exactly the two transactions of the two unwound blocks, and nothing else:
    // the coinbases are not pooled and the branch's own blocks are empty.
    let mut got = restored.clone();
    got.sort();
    let mut want = vec![hash_a, hash_b];
    want.sort();
    assert_eq!(got, want, "both unwound transactions, and only those, went back to the pool");
    assert_eq!(h.pool.len(), 2);
    for hash in [hash_a, hash_b] {
        assert!(h.pool.contains(&hash));
        assert_eq!(h.pool.get(&hash).unwrap().source, PoolSource::AlternativeBlock);
        assert_eq!(h.chain.transaction_block_index(&hash).unwrap(), None, "and out of the chain index");
    }
    // The deepest block was unwound last, so its transaction is offered back
    // last: the order is the chain state's unwind order, tip first.
    assert_eq!(restored.len(), 2);

    // And the next template can mine them again.
    let t = build_template(&h.chain, &mut h.pool, FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 70, b"m3")).unwrap();
    assert_eq!(t.block.transaction_hashes.len(), 2);
}

#[test]
fn the_pool_and_the_template_work_through_a_dyn_pool_chain() {
    // What the RPC layer wants: one `&dyn PoolChain` behind which any chain
    // state can sit. Everything the pool needs from the chain is object safe.
    let mut h = harness();
    let (_, blob, hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"dyn");
    let chain: &dyn wrkz_mempool::PoolChain = &h.chain;

    assert_eq!(chain.top_index(), TIP as u64);
    assert_eq!(wrkz_mempool::chain::next_block_difficulty(chain, TIP as u64).unwrap(), Some(1));
    let ctx = TemplateContext::from_chain(chain).unwrap();
    assert_eq!(ctx.height, TIP as u64 + 1);

    assert_eq!(h.pool.add(&blob, chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(fill_block_template(&mut h.pool, chain, &ctx).len(), 1);
    let relay = PoolWithChain::new(&mut h.pool, chain);
    assert!(relay.has_transaction(&hash));
}

#[test]
fn the_pool_holds_at_most_sixty_fee_less_transactions() {
    // `Core.cpp:2220`: the cap counts fee-*less* transactions, not
    // `isFusionTransaction` ones, and it is checked before validation.
    let chain = fusion_chain();
    let mut pool = TransactionPool::new(PoolConfig::default());
    pool.set_clock(Some(NOW));
    assert_eq!(pool.config().fusion_max_pool_count, 60);

    let mut hashes = Vec::new();
    for seed in 0..60u16 {
        let (blob, hash) = fusion_tx(seed);
        assert_eq!(pool.add(&blob, &chain, PoolSource::Rpc), PoolStatus::Added, "fusion transaction {seed}");
        hashes.push(hash);
    }
    assert_eq!(pool.len(), 60);
    assert_eq!(pool.fusion_transaction_count(), 60);
    // All of them are fee-less, so a template takes them in the second list.
    let (regular, fusion) = pool.for_block_template();
    assert!(regular.is_empty());
    assert_eq!(fusion.len(), 60);

    // The sixty-first is refused before it is even validated.
    let (blob, _) = fusion_tx(60);
    assert_eq!(pool.add(&blob, &chain, PoolSource::Rpc), PoolStatus::FusionPoolFull);
    assert_eq!(pool.len(), 60);

    // A fee-paying transaction is unaffected by the fusion cap: twelve inputs
    // of 1,000,000 paying one output of 11,500,000 is not a fusion shape
    // (the decomposition of 12,000,000 is [2,000,000, 10,000,000]) and pays a
    // fee well clear of the ladder.
    let (paying, _) = twelve_input_tx(61, b"paying", 1_000_000, &[11_500_000]);
    assert!(500_000 >= wrkz_primitives::fees::required_minimum_fee(paying.len(), FUSION_TIP as u64));
    assert_eq!(pool.add(&paying, &chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(pool.len(), 61);
    assert_eq!(pool.fusion_transaction_count(), 60);

    // Room again once one leaves.
    assert!(pool.remove(&hashes[0]));
    let (blob, _) = fusion_tx(62);
    assert_eq!(pool.add(&blob, &chain, PoolSource::Rpc), PoolStatus::Added);
}

#[test]
fn a_block_that_spends_a_pooled_key_image_evicts_it() {
    let mut h = harness();
    // The pool holds a transaction; a block arrives spending the same output
    // through a different transaction, so the pooled one can never be mined.
    let (_, pooled, pooled_hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"pooled");
    let (_, mined_blob, mined_hash) = build_spend(&h.outputs[0], &h.outputs[2], &h.miner, FEE, b"mined");
    assert_eq!(h.pool.add(&pooled, &h.chain, PoolSource::Rpc), PoolStatus::Added);

    // Build a template that carries the *other* transaction. It is not in the
    // pool, so the context path is used directly.
    let ctx = TemplateContext::from_chain(&h.chain).unwrap();
    let candidate = TemplateTransaction { hash: mined_hash, blob: mined_blob.clone(), fee: FEE };
    let t = build_template_from_context(&ctx, &[candidate], FIXTURE_ADDRESS, 0, None, &options(TIP_TIME + 60, b"cb"))
        .unwrap();
    let (_, blob) = mine(&t.block, t.difficulty);

    let update = add_block_with_pool(&mut h.pool, &mut h.chain, &blob, &[mined_blob]).unwrap();
    assert_eq!(update.outcome.status, AddStatus::Main);
    assert_eq!(update.removed, vec![pooled_hash], "the pooled double spend is dropped");
    assert!(h.pool.is_empty());
}

#[test]
fn the_pool_state_and_the_key_image_index_stay_in_step() {
    let mut h = harness();
    let (tx, blob, hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE, b"a");
    let image = match &tx.prefix.inputs[0] {
        Input::Key { key_image, .. } => *key_image,
        _ => unreachable!(),
    };
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(h.pool.spent_key_images().copied().collect::<HashSet<_>>(), HashSet::from([image]));
    assert!(h.pool.remove(&hash));
    assert!(h.pool.spent_key_images().next().is_none());
    assert_eq!(h.pool.size_bytes(), 0);
    // Removal frees the key image, so the same spend can be offered again.
    assert_eq!(h.pool.add(&blob, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    h.pool.flush();
    assert!(h.pool.is_empty());
    assert_eq!(h.pool.size_bytes(), 0);
}

#[test]
fn a_pool_at_its_budget_refuses_the_least_profitable_offer() {
    let h = harness();
    // Four transactions of exactly the same size, so the fee alone orders them.
    let (_, rich, rich_hash) = build_spend(&h.outputs[0], &h.outputs[1], &h.payee, FEE * 9, b"rich");
    let (_, mid, mid_hash) = build_spend(&h.outputs[2], &h.outputs[3], &h.payee, FEE * 3, b"mid");
    let (_, cheap, _) = build_spend(&h.outputs[4], &h.outputs[5], &h.payee, FEE, b"cheap");
    let (_, richest, richest_hash) = build_spend(&h.outputs[6], &h.outputs[7], &h.payee, FEE * 20, b"richest");
    let one = rich.len() as u64;
    assert!([mid.len(), cheap.len(), richest.len()].iter().all(|n| *n as u64 == one));

    let cfg = PoolConfig { max_size_bytes: 2 * one, ..PoolConfig::default() };
    let mut pool = TransactionPool::new(cfg.clone());
    pool.set_clock(Some(NOW));
    assert_eq!(pool.add(&rich, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(pool.add(&mid, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(pool.size_bytes(), 2 * one);

    // At the budget and worse than everything held: refused up front, so a
    // flood of low fee spam cannot churn the pool (`TransactionPool.cpp:180`).
    assert_eq!(pool.add(&cheap, &h.chain, PoolSource::Rpc), PoolStatus::PoolFull);
    assert_eq!(pool.hashes(), vec![rich_hash, mid_hash]);
    assert!(pool.take_evicted().is_empty());

    // Better than something held: admitted, then `evictToFitLocked` sheds the
    // least profitable down to 90% of the budget - which for two transactions
    // of one budget-half each means only the newcomer survives.
    assert_eq!(pool.add(&richest, &h.chain, PoolSource::Rpc), PoolStatus::Added);
    assert_eq!(pool.hashes(), vec![richest_hash]);
    assert_eq!(pool.size_bytes(), one);
    assert_eq!(pool.take_evicted(), vec![mid_hash, rich_hash]);
    assert!(pool.take_evicted().is_empty());
}
