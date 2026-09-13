// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Synthetic chains: one test per stateful rule, and a fork that reorganises
//! to the heavier branch and back.
//!
//! Real mainnet blocks prove the rules that fire on the chain as it is; they
//! cannot prove the rules that fire on a block the network rejected, because no
//! such block was ever published. These build them.
//!
//! The chain is seeded at index 4,400,000, above every fork height: block major
//! version 7 (`cn_upx`), mixin tier V6 (ring 2 to 8), the unlock-time rule, the
//! output-amount cap, the transaction proof of work with its fee escape, and no
//! checkpoint (the last one is at 4,188,000), so the proof of work, the ring
//! signatures and the transaction proof of work all run.
//!
//! Blocks are mined at difficulty 1, which any hash satisfies
//! (`check_hash(h, 1)` cannot overflow), so no work is needed. Difficulty stays
//! at 1 because the seeded window is 61 blocks of difficulty 1 spaced 59
//! seconds apart: LWMA-2 returns `floor(59.4 · D / s)`, which is `D` for
//! `D = 1, s = 59` — the test asserts it rather than assuming it.
//!
//! Every key, derivation, key image and ring signature is produced by the real
//! curve functions of `wrkz-pow`.

use wrkz_chain::records::{BlockInfo, OutputRecord};
use wrkz_chain::replay::{replay_windows, ReplayOptions};
use wrkz_chain::validate::TxRule;
use wrkz_chain::Window;
use wrkz_chain::{keys, records, AddStatus, ChainState, Checkpoints, Config, Rule};
use wrkz_pow::cn_fast_hash;
use wrkz_pow::curve;
use wrkz_primitives::block::{BlockTemplate, ParentBlock};
use wrkz_primitives::tx::{
    absolute_to_relative_offsets, append_merge_mining_tag, build_extra, BaseTransaction, Input, MergeMiningTag, Output,
    Transaction, TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_storage::reader::ChainReader;
use wrkz_storage::{KvStore, MemStore};

/// The seeded top block index. Above 4,300,000 (mixin tier V6) and above the
/// last checkpoint (4,188,000).
const TIP: u32 = 4_400_000;
/// How many block infos the seed writes. Must cover the reward window (100),
/// the difficulty window (61) and the state's `recent_window`, and — because
/// one test exports this chain as a C++ database and replays a window out of it
/// — `wrkz_chain::replay::SEED_DEPTH`, the history a windowed replay reads
/// below its first block.
const SEED_LEN: u32 = wrkz_chain::replay::SEED_DEPTH;
const BASE: u32 = TIP - SEED_LEN + 1;
/// The solvetime that keeps LWMA-2 at the same difficulty.
const SPACING: u64 = 59;
const TIP_TIME: u64 = 1_800_000_000;
/// The validating node's clock. Every block timestamp must be at or below this
/// plus 360 seconds (`CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4`).
const NOW: u64 = TIP_TIME + 10_000;
/// The cumulative difficulty of the seeded tip. Large enough that a seed of
/// [`SEED_LEN`] blocks at a difficulty of 10^9 each still climbs from a
/// positive value.
const TIP_CUMULATIVE: u64 = 10_000_000_000_000;
/// The amount of every seeded spendable output.
const AMOUNT: u64 = 1_000_000;
/// A fee that clears the ladder (10 per 128-byte chunk) and the transaction
/// proof-of-work fee escape (`TRANSACTION_POW_PASS_WITH_FEE`).
const FEE: u64 = 10_000;

fn config() -> Config {
    Config { store_raw_blocks: true, unwind_history: 512, recent_window: 256, ..Config::default() }
}

// ---------------------------------------------------------------------------
// keys and outputs
// ---------------------------------------------------------------------------

/// A wallet: a spend key pair and a view key pair, as `05-addresses` defines
/// them.
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

/// One output this test can spend: its one-time key pair and where it sits.
#[derive(Clone, Copy)]
struct Spendable {
    global_index: u32,
    public_key: Hash,
    secret_key: Hash,
}

/// `constructMinerTx` / the wallet's output derivation: `R = rG`, the shared
/// derivation `D`, the one-time key `P = H_s(D, i)G + B` and its secret
/// `x = H_s(D, i) + b`.
fn derive_output(to: &Wallet, tx_secret: &Hash, tx_public: &Hash, output_index: u64) -> (Hash, Hash) {
    let sender = curve::generate_key_derivation(&to.view_public, tx_secret).expect("derivation");
    let public = curve::derive_public_key(&sender, output_index, &to.spend_public).expect("output key");
    let receiver = curve::generate_key_derivation(tx_public, &to.view_secret).expect("derivation");
    assert_eq!(sender, receiver, "both sides derive the same secret");
    let secret = curve::derive_secret_key(&receiver, output_index, &to.spend_secret);
    assert_eq!(curve::secret_key_to_public_key(&secret), Some(public), "the one-time key pair matches");
    (public, secret)
}

/// A deterministic transaction key pair, so a test run is reproducible.
fn tx_keys(tag: &[u8]) -> (Hash, Hash) {
    curve::generate_deterministic_keys(&cn_fast_hash(tag))
}

// ---------------------------------------------------------------------------
// the seeded chain
// ---------------------------------------------------------------------------

struct Harness {
    chain: ChainState<MemStore>,
    /// Spendable outputs of [`AMOUNT`], unlocked.
    outputs: Vec<Spendable>,
    /// A spendable output whose transaction unlock time is far in the future.
    locked: Spendable,
    miner: Wallet,
    payee: Wallet,
}

fn seed_hash(index: u32) -> Hash {
    cn_fast_hash(&[b"wrkz-chain synthetic seed".as_slice(), &index.to_le_bytes()].concat())
}

/// Write a plausible chain of [`SEED_LEN`] blocks ending at [`TIP`] straight
/// into the store, then open a [`ChainState`] on it.
///
/// The records are the ones [`ChainState`] itself writes, so this is the state
/// a replay would have reached at 4,400,000 — a real chain of 4.4 million
/// blocks cannot be built in a test, and no smaller height reaches the rules
/// these tests are about.
fn harness(difficulty_step: u64) -> Harness {
    let mut store = MemStore::default();
    let mut ops = Vec::new();
    for i in BASE..=TIP {
        let back = (TIP - i) as u64;
        let info = BlockInfo {
            block_hash: seed_hash(i),
            timestamp: TIP_TIME - back * SPACING,
            block_size: 300,
            cumulative_difficulty: TIP_CUMULATIVE - back * difficulty_step,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: i as u64 + 1,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(i), Some(records::encode_hashes(&[seed_hash(i)]))));
    }

    // Spendable outputs, all of the same amount so that one ring can be drawn
    // from them, plus one whose unlock time is a block index far ahead.
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
    let (locked_key, locked_secret) = derive_output(&owner, &tx_secret, &tx_public, 8);
    let locked_record = OutputRecord {
        public_key: locked_key,
        // A block index far above the tip: `blockIndex + 1 >= unlockTime` fails.
        unlock_time: TIP as u64 + 1_000_000,
        transaction_hash: seed_hash(BASE),
        output_index: 8,
        block_index: BASE,
    };
    ops.push((keys::output(AMOUNT, 8), Some(locked_record.encode())));
    ops.push((keys::output_count(AMOUNT), Some(9u32.to_le_bytes().to_vec())));

    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(TIP.to_le_bytes().to_vec())));
    store.write_batch(ops).unwrap();

    let mut chain = ChainState::open(store, config(), Checkpoints::mainnet()).expect("seeded state opens");
    chain.set_clock(Some(NOW));
    assert_eq!(chain.tip_index(), Some(TIP));
    assert!(!chain.checkpoints().is_in_checkpoint_zone(TIP as u64 + 1), "4,400,000 is outside the checkpoint zone");
    Harness {
        chain,
        outputs,
        locked: Spendable { global_index: 8, public_key: locked_key, secret_key: locked_secret },
        miner: Wallet::from_seed(2),
        payee: Wallet::from_seed(3),
    }
}

// ---------------------------------------------------------------------------
// building blocks and transactions
// ---------------------------------------------------------------------------

/// A key input spending `real`, with `decoy` as the other ring member, signed
/// with the real curve functions.
fn build_spend(
    previous_index: u32,
    real: &Spendable,
    decoy: &Spendable,
    to: &Wallet,
    fee: u64,
    tag: &[u8],
    outputs_amount: Option<u64>,
) -> Transaction {
    assert_ne!(real.global_index, decoy.global_index);
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
            // `UNLOCK_TIME_HEIGHT`: at least `H + MINIMUM_UNLOCK_TIME_BLOCKS`.
            unlock_time: previous_index as u64 + 20,
            inputs: vec![Input::Key { amount: AMOUNT, key_offsets, key_image }],
            outputs: vec![Output { amount: outputs_amount.unwrap_or(AMOUNT - fee), key: out_key }],
            extra: build_extra(&tx_public, None, None).expect("extra"),
        },
        signatures: Vec::new(),
    };
    let prefix_hash = tx.prefix.hash();
    let signatures =
        curve::generate_ring_signature(&prefix_hash, &key_image, &ring, &real.secret_key, secret_index).expect("sign");
    assert!(curve::check_ring_signature(&prefix_hash, &key_image, &ring, &signatures));
    tx.signatures.push(signatures);
    tx
}

/// The block a miner would submit at `index`: a v7 block with the daemon
/// template's parent block (major 0, minor 0, a default-constructed coinbase
/// whose extra is the merge-mining tag) and the tag adjusted the way
/// `MinerManager::adjustMergeMiningTag` does — depth 0, root = the block's own
/// auxiliary header hash.
struct Built {
    block: BlockTemplate,
    blob: Vec<u8>,
    tx_blobs: Vec<Vec<u8>>,
    hash: Hash,
}

fn build_block(
    previous_hash: Hash,
    index: u32,
    timestamp: u64,
    nonce: u32,
    txs: &[Transaction],
    miner: &Wallet,
    tag: &[u8],
    reward_delta: i64,
    coinbase_extra_padding: usize,
) -> Built {
    let fee: u64 = txs.iter().map(|t| t.fee().expect("outputs do not exceed inputs")).sum();
    // Above FIXED_REWARD_V1_HEIGHT with a block far inside the penalty-free
    // zone: the reward is the flat base plus the fees.
    let reward = (1_000_000u64 + fee).wrapping_add(reward_delta as u64);

    let (tx_secret, tx_public) = tx_keys(&[tag, b"coinbase"].concat());
    let (out_key, _) = derive_output(miner, &tx_secret, &tx_public, 0);
    let mut extra = build_extra(&tx_public, None, None).expect("extra");
    extra.extend(std::iter::repeat_n(0xffu8, coinbase_extra_padding));
    let coinbase = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: index as u64 + 40,
            inputs: vec![Input::Base { block_index: index as u64 }],
            outputs: vec![Output { amount: reward, key: out_key }],
            extra,
        },
        signatures: Vec::new(),
    };

    let mut block = BlockTemplate {
        major_version: 7,
        minor_version: 0,
        timestamp,
        previous_block_hash: previous_hash,
        nonce,
        parent_block: None,
        base_transaction: coinbase,
        transaction_hashes: txs.iter().map(|t| t.hash().expect("tx hash")).collect(),
    };
    // The commitment the parent coinbase carries. It is computed from the
    // header hashing blob, which for v2+ holds no timestamp and no nonce, so it
    // does not change when those do.
    let aux = block.auxiliary_header_hash().expect("aux hash");
    let mut parent_extra = Vec::new();
    append_merge_mining_tag(&mut parent_extra, &MergeMiningTag { depth: 0, merkle_root: aux });
    let parent_coinbase = BaseTransaction {
        prefix: TransactionPrefix { version: 0, unlock_time: 0, inputs: vec![], outputs: vec![], extra: parent_extra },
    };
    block.parent_block = Some(ParentBlock::new(0, 0, previous_hash, 1, Vec::new(), parent_coinbase, Vec::new()));

    let blob = block.to_bytes().expect("block serializes");
    let hash = block.hash().expect("block hash");
    Built { block, blob, tx_blobs: txs.iter().map(|t| t.to_bytes().expect("tx serializes")).collect(), hash }
}

/// A block with nothing unusual about it.
fn plain_block(
    previous_hash: Hash,
    index: u32,
    timestamp: u64,
    nonce: u32,
    txs: &[Transaction],
    miner: &Wallet,
    tag: &[u8],
) -> Built {
    build_block(previous_hash, index, timestamp, nonce, txs, miner, tag, 0, 0)
}

fn tip_hash(chain: &ChainState<MemStore>) -> Hash {
    chain.tip_info().unwrap().block_hash
}

/// The rule a rejected block failed on.
fn rule(e: wrkz_chain::ChainError) -> Rule {
    e.rule().cloned().unwrap_or_else(|| panic!("expected a consensus rule, got {e}"))
}

/// The transaction rule a rejected block failed on.
fn tx_rule(e: wrkz_chain::ChainError) -> TxRule {
    match rule(e) {
        Rule::Transaction { rule, .. } => rule,
        other => panic!("expected a transaction rule, got {other}"),
    }
}

/// The rule a rejected block failed on, with the transaction it was reported
/// against: its index in the block and its hash.
fn tx_failure(e: wrkz_chain::ChainError) -> (usize, Hash, TxRule) {
    match rule(e) {
        Rule::Transaction { hash, index, rule } => (index, hash, rule),
        other => panic!("expected a transaction rule, got {other}"),
    }
}

// ---------------------------------------------------------------------------
// (e) one test per stateful rule
// ---------------------------------------------------------------------------

#[test]
fn a_plain_block_applies_at_difficulty_one() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let tx = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None);
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[tx], &h.miner, b"b1");
    let outcome = h.chain.add_block(&b.blob, &b.tx_blobs).expect("the block applies");
    assert_eq!(outcome.status, AddStatus::Main);
    assert_eq!(outcome.index, TIP + 1);
    // LWMA-2 over 61 blocks of difficulty 1 spaced 59 s returns 1.
    assert_eq!(outcome.difficulty, 1);
    assert_eq!(outcome.cumulative_difficulty, TIP_CUMULATIVE + 1);
    // The coinbase pays the flat reward plus the fee, but the *emission* grows
    // only by the base reward: `emissionChange = penalizedBaseReward − (fee −
    // penalizedFee)` and the fee is unpenalized here, so the fee is money that
    // already existed (`Currency.cpp:226`).
    assert_eq!(outcome.already_generated_coins, 30_000_000_000_000 + 1_000_000);
    assert_eq!(h.chain.tip_index(), Some(TIP + 1));
    assert_eq!(h.chain.block_index_by_hash(&b.hash).unwrap(), Some(TIP + 1));
    // The spend is recorded, and the new outputs are resolvable.
    let image = match &b.block.transaction_hashes.first() {
        Some(_) => match &Transaction::from_bytes(&b.tx_blobs[0]).unwrap().prefix.inputs[0] {
            Input::Key { key_image, .. } => *key_image,
            _ => unreachable!(),
        },
        None => unreachable!(),
    };
    assert_eq!(h.chain.key_image_spent_at(&image).unwrap(), Some(TIP + 1));
    assert_eq!(h.chain.output_count_for_amount(AMOUNT - FEE).unwrap(), 1);
    assert_eq!(h.chain.block_transaction_hashes(TIP + 1).unwrap().len(), 2);
}

#[test]
fn a_key_image_spent_by_an_earlier_block_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let tx = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None);
    let b1 = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[tx], &h.miner, b"b1");
    h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap();

    // The same output again, in the next block. A different decoy and a
    // different destination give a different transaction with the same key
    // image, which is exactly what a double spend looks like.
    let again = build_spend(TIP + 1, &h.outputs[0], &h.outputs[2], &h.payee, FEE, b"tx2", None);
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 8, &[again], &h.miner, b"b2");
    let e = h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap_err();
    assert!(matches!(tx_rule(e), TxRule::InputKeyImageAlreadySpent { .. }));
    assert_eq!(h.chain.tip_index(), Some(TIP + 1), "the rejected block did not land");
}

#[test]
fn two_transactions_in_one_block_spending_the_same_output_are_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let a = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None);
    let b = build_spend(TIP, &h.outputs[0], &h.outputs[2], &h.payee, FEE, b"tx2", None);
    let built = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[a, b], &h.miner, b"b1");
    let e = h.chain.add_block(&built.blob, &built.tx_blobs).unwrap_err();
    // Caught by the block's `TransactionValidatorState`, not by the chain.
    assert!(matches!(tx_rule(e), TxRule::InputKeyImageAlreadySpent { .. }));
}

#[test]
fn spending_a_locked_output_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let tx = build_spend(TIP, &h.locked, &h.outputs[0], &h.payee, FEE, b"tx1", None);
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[tx], &h.miner, b"b1");
    let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
    assert_eq!(
        tx_rule(e),
        TxRule::InputSpendLockedOut { amount: AMOUNT, global_index: 8, unlock_time: TIP as u64 + 1_000_000 }
    );
}

#[test]
fn a_ring_member_that_does_not_exist_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let ghost = Spendable { global_index: 10_000, public_key: h.outputs[1].public_key, secret_key: [0; 32] };
    let tx = build_spend(TIP, &h.outputs[0], &ghost, &h.payee, FEE, b"tx1", None);
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[tx], &h.miner, b"b1");
    let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
    assert_eq!(tx_rule(e), TxRule::InputInvalidGlobalIndex { amount: AMOUNT, global_index: 10_000 });
}

#[test]
fn a_tampered_ring_signature_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let mut tx = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None);
    tx.signatures[0][1][0] ^= 1;
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[tx], &h.miner, b"b1");
    let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
    assert_eq!(tx_rule(e), TxRule::InputInvalidSignatures { input: 0 });
}

/// Three transactions in one block, two of them signed wrongly. The whole
/// block's rings are verified as one batch, and the pair reported must be the
/// lowest `(transaction, input)` — where the sequential C++ loop would have
/// stopped — at every thread count, including `1`, which is that loop.
#[test]
fn a_block_of_several_bad_signatures_reports_the_lowest_transaction() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let mut txs = vec![
        build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None),
        build_spend(TIP, &h.outputs[2], &h.outputs[3], &h.payee, FEE, b"tx2", None),
        build_spend(TIP, &h.outputs[4], &h.outputs[5], &h.payee, FEE, b"tx3", None),
    ];
    // Transactions 1 and 2 are signed wrongly. Transaction 1 is the lowest, so
    // transaction 2's signature must never be the one reported.
    txs[1].signatures[0][0][0] ^= 1;
    txs[2].signatures[0][1][0] ^= 1;
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &txs, &h.miner, b"b1");
    let expected = (1usize, b.block.transaction_hashes[1], TxRule::InputInvalidSignatures { input: 0 });
    for threads in [1usize, 2, 3, 4, 8, 16] {
        h.chain.set_validate_threads(threads);
        let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
        assert_eq!(tx_failure(e), expected, "{threads} threads");
    }

    // The same block signed properly is accepted, so the rejection above was
    // not something else about the block.
    let mut h = harness(1);
    let txs = vec![
        build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None),
        build_spend(TIP, &h.outputs[2], &h.outputs[3], &h.payee, FEE, b"tx2", None),
        build_spend(TIP, &h.outputs[4], &h.outputs[5], &h.payee, FEE, b"tx3", None),
    ];
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &txs, &h.miner, b"b1");
    assert_eq!(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap().index, TIP + 1);
}

/// A chain failure in one transaction of a block against a bad signature in an
/// earlier and in a later transaction. The C++ validates transaction 0 to the
/// end before it starts transaction 1, so the lower of the two wins both times
/// — and a block-wide batch must not let a later transaction's signature jump
/// the queue in front of an earlier transaction's chain failure.
#[test]
fn a_block_mixing_a_chain_failure_and_a_bad_signature_keeps_the_cpp_order() {
    let ghost =
        |h: &Harness| Spendable { global_index: 10_000, public_key: h.outputs[1].public_key, secret_key: [0; 32] };

    // Transaction 1 names a ring member that does not exist; transaction 0 is
    // signed wrongly. The signature is lower, so it is reported.
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let mut txs = vec![
        build_spend(TIP, &h.outputs[0], &h.outputs[2], &h.payee, FEE, b"tx1", None),
        build_spend(TIP, &h.outputs[3], &ghost(&h), &h.payee, FEE, b"tx2", None),
        build_spend(TIP, &h.outputs[4], &h.outputs[5], &h.payee, FEE, b"tx3", None),
    ];
    txs[0].signatures[0][0][0] ^= 1;
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &txs, &h.miner, b"b1");
    let expected = (0usize, b.block.transaction_hashes[0], TxRule::InputInvalidSignatures { input: 0 });
    for threads in [1usize, 2, 4, 8] {
        h.chain.set_validate_threads(threads);
        let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
        assert_eq!(tx_failure(e), expected, "{threads} threads");
    }

    // The other way round: the missing ring member is still in transaction 1,
    // but the bad signature has moved to transaction 2, above it. The C++ stops
    // at transaction 1 and never looks at transaction 2, so the missing member
    // is reported and transaction 2's signature is never even gathered.
    let mut h = harness(1);
    let mut txs = vec![
        build_spend(TIP, &h.outputs[0], &h.outputs[2], &h.payee, FEE, b"tx1", None),
        build_spend(TIP, &h.outputs[3], &ghost(&h), &h.payee, FEE, b"tx2", None),
        build_spend(TIP, &h.outputs[4], &h.outputs[5], &h.payee, FEE, b"tx3", None),
    ];
    txs[2].signatures[0][0][0] ^= 1;
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &txs, &h.miner, b"b1");
    let expected = (
        1usize,
        b.block.transaction_hashes[1],
        TxRule::InputInvalidGlobalIndex { amount: AMOUNT, global_index: 10_000 },
    );
    for threads in [1usize, 2, 4, 8] {
        h.chain.set_validate_threads(threads);
        let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
        assert_eq!(tx_failure(e), expected, "{threads} threads");
    }
}

#[test]
fn a_block_whose_proof_of_work_misses_the_difficulty_is_rejected() {
    // A window whose cumulative difficulty climbs by 10^9 per block makes the
    // next difficulty about 10^9, which no unmined block satisfies.
    let mut h = harness(1_000_000_000);
    let prev = tip_hash(&h.chain);
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &h.miner, b"b1");
    let e = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err();
    match rule(e) {
        Rule::ProofOfWorkTooWeak { difficulty } => {
            assert!(difficulty > 900_000_000, "the derived difficulty was {difficulty}");
        }
        other => panic!("expected PROOF_OF_WORK_TOO_WEAK, got {other}"),
    }
    // The same block on the difficulty-1 chain is accepted, so the only
    // difference is the difficulty the state derived.
    let mut easy = harness(1);
    let prev = tip_hash(&easy.chain);
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &easy.miner, b"b1");
    assert_eq!(easy.chain.add_block(&b.blob, &b.tx_blobs).unwrap().difficulty, 1);
}

#[test]
fn a_timestamp_below_the_window_median_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    // The window is 11 from 128,800; its median is the 6th newest timestamp.
    let median = TIP_TIME - 5 * SPACING;
    let b = plain_block(prev, TIP + 1, median - 1, 7, &[], &h.miner, b"b1");
    assert_eq!(
        rule(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err()),
        Rule::TimestampTooFarInPast { timestamp: median - 1, median }
    );
    // Exactly the median passes: the rule is `<`.
    let b = plain_block(prev, TIP + 1, median, 7, &[], &h.miner, b"b1");
    h.chain.add_block(&b.blob, &b.tx_blobs).expect("a timestamp equal to the median is accepted");
}

#[test]
fn a_timestamp_too_far_in_the_future_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let limit = NOW + 360;
    let b = plain_block(prev, TIP + 1, limit + 1, 7, &[], &h.miner, b"b1");
    assert_eq!(
        rule(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err()),
        Rule::TimestampTooFarInFuture { timestamp: limit + 1, limit }
    );
}

#[test]
fn an_oversize_block_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    // `maxBlockCumulativeSize(4,400,000)` is about 957 KB. The coinbase's extra
    // is not size-limited by any rule a coinbase goes through, so padding it is
    // the cheapest way to build an oversize block.
    let limit = wrkz_primitives::constants::max_block_cumulative_size(TIP as u64 + 1);
    let b = build_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &h.miner, b"b1", 0, limit as usize);
    match rule(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err()) {
        Rule::CumulativeBlockSizeTooBig { size, limit: got } => {
            assert!(size > got);
            assert_eq!(got, limit);
        }
        other => panic!("expected CUMULATIVE_BLOCK_SIZE_TOO_BIG, got {other}"),
    }

    // A block that is under the hard cap but over `2 · median` is rejected by
    // the reward rule instead (`Currency::getBlockReward` returns false).
    let median = h.chain.block_median_size();
    let b = build_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &h.miner, b"b1", 0, (2 * median) as usize);
    match rule(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err()) {
        Rule::CumulativeBlockSizeTooBig { size, limit: got } => {
            assert_eq!(got, 2 * median);
            assert!(size > got && size < limit);
        }
        other => panic!("expected CUMULATIVE_BLOCK_SIZE_TOO_BIG, got {other}"),
    }
}

#[test]
fn a_wrong_coinbase_reward_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let tx = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None);
    for delta in [1i64, -1] {
        let b = build_block(prev, TIP + 1, TIP_TIME + SPACING, 7, std::slice::from_ref(&tx), &h.miner, b"b1", delta, 0);
        assert_eq!(
            rule(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err()),
            Rule::BlockRewardMismatch {
                expected: 1_000_000 + FEE,
                got: (1_000_000u64 + FEE).wrapping_add(delta as u64)
            }
        );
    }
}

#[test]
fn a_block_with_the_wrong_major_version_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let mut b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &h.miner, b"b1");
    b.block.major_version = 6;
    let blob = b.block.to_bytes().unwrap();
    assert_eq!(rule(h.chain.add_block(&blob, &[]).unwrap_err()), Rule::WrongVersion { expected: 7, got: 6 });
}

#[test]
fn a_coinbase_naming_the_wrong_height_or_unlock_time_is_rejected() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let index = TIP as u64 + 1;

    // The unlock time must be exactly `index + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW`.
    let mut b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &h.miner, b"b1");
    b.block.base_transaction.prefix.unlock_time = index + 41;
    let blob = b.block.to_bytes().unwrap();
    assert_eq!(
        rule(h.chain.add_block(&blob, &[]).unwrap_err()),
        Rule::CoinbaseWrongUnlockTime { expected: index + 40, got: index + 41 }
    );

    // The `BaseInput` must name the block's own index. Note that the version
    // rule ran first against this *claimed* index (`CachedBlock::getBlockIndex`),
    // and 4,400,002 is still a v7 height, so the height rule is what fires.
    let mut b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[], &h.miner, b"b1");
    b.block.base_transaction.prefix.inputs = vec![Input::Base { block_index: index + 1 }];
    let blob = b.block.to_bytes().unwrap();
    assert_eq!(
        rule(h.chain.add_block(&blob, &[]).unwrap_err()),
        Rule::BaseInputWrongBlockIndex { expected: index, got: index + 1 }
    );
}

// ---------------------------------------------------------------------------
// (f) a fork that reorganises to the heavier branch and back
// ---------------------------------------------------------------------------

#[test]
fn a_heavier_branch_takes_over_and_the_original_takes_it_back() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);

    // Branch A: two blocks on top of the seeded tip.
    let a1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 11, &[], &h.miner, b"a1");
    assert_eq!(h.chain.add_block(&a1.blob, &a1.tx_blobs).unwrap().status, AddStatus::Main);
    let a2 = plain_block(a1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 12, &[], &h.miner, b"a2");
    assert_eq!(h.chain.add_block(&a2.blob, &a2.tx_blobs).unwrap().status, AddStatus::Main);
    assert_eq!(h.chain.tip_index(), Some(TIP + 2));
    assert_eq!(h.chain.tip_info().unwrap().cumulative_difficulty, TIP_CUMULATIVE + 2);

    // Branch B forks at the same point. Its blocks differ from A's only in the
    // nonce and the miner's transaction key, so they are different blocks at
    // the same heights.
    let b1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 21, &[], &h.miner, b"b1");
    let out = h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap();
    assert_eq!(out.status, AddStatus::Alternative, "one block cannot outweigh two");
    assert_eq!(h.chain.tip_index(), Some(TIP + 2));

    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 22, &[], &h.miner, b"b2");
    let out = h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap();
    assert_eq!(out.status, AddStatus::Alternative, "a tie keeps the current main chain");
    assert_eq!(h.chain.tip_info().unwrap().block_hash, a2.hash);

    // The third block makes B heavier: the chain switches.
    let b3 = plain_block(b2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 23, &[], &h.miner, b"b3");
    let out = h.chain.add_block(&b3.blob, &b3.tx_blobs).unwrap();
    assert_eq!(out.status, AddStatus::AlternativeAndSwitched);
    assert_eq!(h.chain.tip_index(), Some(TIP + 3));
    assert_eq!(h.chain.tip_info().unwrap().block_hash, b3.hash);
    assert_eq!(h.chain.tip_info().unwrap().cumulative_difficulty, TIP_CUMULATIVE + 3);
    // The B blocks are the main chain now and the A blocks are alternatives.
    for (i, b) in [(TIP + 1, b1.hash), (TIP + 2, b2.hash), (TIP + 3, b3.hash)] {
        assert_eq!(h.chain.block_index_by_hash(&b).unwrap(), Some(i), "B block at {i} is on the main chain");
    }
    for a in [a1.hash, a2.hash] {
        assert_eq!(h.chain.block_index_by_hash(&a).unwrap(), None, "an A block is no longer on the main chain");
        assert!(h.chain.alternative_cumulative_difficulty(&a).is_some(), "an A block is kept as an alternative");
    }

    // And back: extending A past B switches again.
    let a3 = plain_block(a2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 13, &[], &h.miner, b"a3");
    let out = h.chain.add_block(&a3.blob, &a3.tx_blobs).unwrap();
    assert_eq!(out.status, AddStatus::Alternative, "a tie again keeps the current main chain");
    assert_eq!(h.chain.tip_info().unwrap().block_hash, b3.hash);

    let a4 = plain_block(a3.hash, TIP + 4, TIP_TIME + 4 * SPACING, 14, &[], &h.miner, b"a4");
    let out = h.chain.add_block(&a4.blob, &a4.tx_blobs).unwrap();
    assert_eq!(out.status, AddStatus::AlternativeAndSwitched);
    assert_eq!(h.chain.tip_index(), Some(TIP + 4));
    assert_eq!(h.chain.tip_info().unwrap().block_hash, a4.hash);
    assert_eq!(h.chain.tip_info().unwrap().cumulative_difficulty, TIP_CUMULATIVE + 4);
    for (i, a) in [(TIP + 1, a1.hash), (TIP + 2, a2.hash), (TIP + 3, a3.hash), (TIP + 4, a4.hash)] {
        assert_eq!(h.chain.block_index_by_hash(&a).unwrap(), Some(i), "A block at {i} is back on the main chain");
    }
    for b in [b1.hash, b2.hash, b3.hash] {
        assert_eq!(h.chain.block_index_by_hash(&b).unwrap(), None);
    }
}

#[test]
fn a_reorganisation_moves_the_spent_key_images_and_the_outputs_with_it() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);
    let outputs_before = h.chain.output_count_for_amount(AMOUNT).unwrap();

    // A spends output 0 on branch A.
    let spend = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"txA", None);
    let image = match &spend.prefix.inputs[0] {
        Input::Key { key_image, .. } => *key_image,
        _ => unreachable!(),
    };
    let a1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 11, &[spend], &h.miner, b"a1");
    h.chain.add_block(&a1.blob, &a1.tx_blobs).unwrap();
    assert_eq!(h.chain.key_image_spent_at(&image).unwrap(), Some(TIP + 1));
    assert_eq!(h.chain.output_count_for_amount(AMOUNT - FEE).unwrap(), 1);

    // Branch B is empty of transactions and overtakes A.
    let b1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 21, &[], &h.miner, b"b1");
    h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap();
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 22, &[], &h.miner, b"b2");
    assert_eq!(h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap().status, AddStatus::AlternativeAndSwitched);

    // The spend is undone: the key image is free and the output it created is gone.
    assert_eq!(h.chain.key_image_spent_at(&image).unwrap(), None);
    assert_eq!(h.chain.output_count_for_amount(AMOUNT - FEE).unwrap(), 0);
    // The coinbase outputs of the two chains are of amount 1,000,000 (branch B,
    // no fees) and the seeded outputs are untouched.
    assert_eq!(h.chain.output_count_for_amount(AMOUNT).unwrap(), outputs_before + 2);

    // The same output can now be spent again on the new chain.
    let respend = build_spend(TIP + 2, &h.outputs[0], &h.outputs[2], &h.payee, FEE, b"txB", None);
    let b3 = plain_block(b2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 23, &[respend], &h.miner, b"b3");
    h.chain.add_block(&b3.blob, &b3.tx_blobs).expect("the output is unspent on this chain");
    assert_eq!(h.chain.key_image_spent_at(&image).unwrap(), Some(TIP + 3));
}

// ---------------------------------------------------------------------------
// (g) the transaction index and what a chain switch reports
// ---------------------------------------------------------------------------

#[test]
fn the_transaction_index_follows_the_main_chain() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);

    let spend = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"txA", None);
    let spend_hash = spend.hash().unwrap();
    let a1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 11, &[spend], &h.miner, b"a1");
    let coinbase_hash = a1.block.base_transaction.hash().unwrap();
    assert_eq!(h.chain.transaction_block_index(&spend_hash).unwrap(), None, "not mined yet");

    h.chain.add_block(&a1.blob, &a1.tx_blobs).unwrap();
    // `Core::isTransactionInChain`: the block's transactions and its coinbase.
    assert_eq!(h.chain.transaction_block_index(&spend_hash).unwrap(), Some(TIP + 1));
    assert_eq!(h.chain.transaction_block_index(&coinbase_hash).unwrap(), Some(TIP + 1));
    assert!(h.chain.has_transaction(&spend_hash).unwrap());
    assert!(!h.chain.has_transaction(&[0xab; 32]).unwrap());
    // The batched answer `/get_transactions_status` asks for, in order.
    assert_eq!(
        h.chain.transaction_block_indexes(&[spend_hash, [0xab; 32], coinbase_hash]).unwrap(),
        vec![Some(TIP + 1), None, Some(TIP + 1)]
    );

    // A heavier branch that does not carry the transaction takes the chain
    // over: the index goes with the block.
    let b1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 21, &[], &h.miner, b"b1");
    h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap();
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 22, &[], &h.miner, b"b2");
    assert_eq!(h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap().status, AddStatus::AlternativeAndSwitched);
    assert_eq!(h.chain.transaction_block_index(&spend_hash).unwrap(), None, "unwound with its block");
    assert_eq!(h.chain.transaction_block_index(&coinbase_hash).unwrap(), None);
    assert_eq!(
        h.chain.transaction_block_index(&b1.block.base_transaction.hash().unwrap()).unwrap(),
        Some(TIP + 1),
        "the new chain's coinbase is indexed"
    );

    // And it comes back when the first branch wins again.
    let a2 = plain_block(a1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 12, &[], &h.miner, b"a2");
    h.chain.add_block(&a2.blob, &a2.tx_blobs).unwrap();
    let a3 = plain_block(a2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 13, &[], &h.miner, b"a3");
    assert_eq!(h.chain.add_block(&a3.blob, &a3.tx_blobs).unwrap().status, AddStatus::AlternativeAndSwitched);
    assert_eq!(h.chain.transaction_block_index(&spend_hash).unwrap(), Some(TIP + 1));
}

#[test]
fn a_chain_switch_reports_the_blocks_it_unwound() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);

    // Two main-chain blocks, each carrying one transaction.
    let tx1 = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"tx1", None);
    let tx1_hash = tx1.hash().unwrap();
    let a1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 11, &[tx1], &h.miner, b"a1");
    h.chain.add_block(&a1.blob, &a1.tx_blobs).unwrap();
    let tx2 = build_spend(TIP + 1, &h.outputs[2], &h.outputs[3], &h.payee, FEE, b"tx2", None);
    let tx2_hash = tx2.hash().unwrap();
    let a2 = plain_block(a1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 12, &[tx2], &h.miner, b"a2");
    h.chain.add_block(&a2.blob, &a2.tx_blobs).unwrap();

    // A branch of three empty blocks from the fork point.
    let b1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 21, &[], &h.miner, b"b1");
    let report = h.chain.add_block_detailed(&b1.blob, &b1.tx_blobs).unwrap();
    assert_eq!(report.outcome.status, AddStatus::Alternative);
    assert!(report.unwound.is_empty(), "nothing has left the main chain yet");
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 22, &[], &h.miner, b"b2");
    assert!(h.chain.add_block_detailed(&b2.blob, &b2.tx_blobs).unwrap().unwound.is_empty(), "a tie does not switch");

    let b3 = plain_block(b2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 23, &[], &h.miner, b"b3");
    let report = h.chain.add_block_detailed(&b3.blob, &b3.tx_blobs).unwrap();
    assert_eq!(report.outcome.status, AddStatus::AlternativeAndSwitched);

    // Exactly the two blocks that left, tip first, with their transactions.
    assert_eq!(report.unwound.len(), 2);
    assert_eq!(report.unwound[0].index, TIP + 2);
    assert_eq!(report.unwound[0].hash, a2.hash);
    assert_eq!(report.unwound[0].transaction_hashes, vec![tx2_hash]);
    assert_eq!(report.unwound[0].transactions, a2.tx_blobs);
    assert_eq!(report.unwound[1].index, TIP + 1);
    assert_eq!(report.unwound[1].hash, a1.hash);
    assert_eq!(report.unwound[1].transaction_hashes, vec![tx1_hash]);
    assert_eq!(report.unwound[1].transactions, a1.tx_blobs);
    // They are alternative blocks now, and their transactions have left the
    // chain with them.
    for b in &report.unwound {
        assert!(h.chain.alternative_cumulative_difficulty(&b.hash).is_some());
        assert_eq!(h.chain.block_index_by_hash(&b.hash).unwrap(), None);
        for hash in &b.transaction_hashes {
            assert_eq!(h.chain.transaction_block_index(hash).unwrap(), None);
        }
    }
    // A block that merely extends the tip reports nothing.
    let b4 = plain_block(b3.hash, TIP + 4, TIP_TIME + 4 * SPACING, 24, &[], &h.miner, b"b4");
    let report = h.chain.add_block_detailed(&b4.blob, &b4.tx_blobs).unwrap();
    assert_eq!(report.outcome.status, AddStatus::Main);
    assert!(report.unwound.is_empty());
}

// ---------------------------------------------------------------------------
// (h) alternative chains: pruning and fork choice as `pruneStaleAlternativeChains`
// ---------------------------------------------------------------------------

/// `n` empty blocks on top of `from` (at `from_index`), each added to the
/// chain. `tag` keeps them apart from any other run at the same heights.
/// Returns the blocks and the status of the last one.
fn extend(h: &mut Harness, from: Hash, from_index: u32, n: u32, tag: &str) -> (Vec<Built>, AddStatus) {
    // A few hundred blocks at SPACING run past the harness clock's future-time
    // limit; move "now" far enough ahead for any run these tests build.
    h.chain.set_clock(Some(TIP_TIME + 1_000_000));
    let mut built = Vec::new();
    let mut prev = from;
    let mut status = AddStatus::Main;
    for i in 1..=n {
        let index = from_index + i;
        let timestamp = TIP_TIME + u64::from(index - TIP) * SPACING;
        let b = plain_block(prev, index, timestamp, i, &[], &h.miner, format!("{tag}{i}").as_bytes());
        status = h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_or_else(|e| panic!("{tag}{i}: {e}")).status;
        prev = b.hash;
        built.push(b);
    }
    (built, status)
}

/// The fault this replaced: pruning dropped single blocks by their own height,
/// so a branch lost its base while its upper blocks stayed, and the next block
/// on one of those came back as a *local* error — which the node reads as its
/// own corruption and stops on. A branch now goes whole, judged by its tip.
#[test]
fn a_stale_branch_is_pruned_whole_so_a_block_on_it_is_an_orphan_not_a_fault() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);
    // a1, a2 on the main chain; b1..b3 take over and leave them as a branch.
    let (a, _) = extend(&mut h, fork, TIP, 2, "a");
    let (b, status) = extend(&mut h, fork, TIP, 3, "b");
    assert_eq!(status, AddStatus::AlternativeAndSwitched);

    // Main grows until the branch tip, a2 at TIP + 2, is exactly
    // CRYPTONOTE_MAX_ALT_BLOCK_DEPTH (180) behind. a1 is 181 behind: the old
    // pass dropped it here and kept a2.
    extend(&mut h, b[2].hash, TIP + 3, 179, "m");
    assert_eq!(h.chain.tip_index(), Some(TIP + 182));
    for x in &a {
        assert!(h.chain.alternative_cumulative_difficulty(&x.hash).is_some(), "the branch is kept whole");
    }

    // A block on a2 is an alternative block; the branch's tip is now TIP + 3.
    let c = plain_block(a[1].hash, TIP + 3, TIP_TIME + 3 * SPACING, 99, &[], &h.miner, b"c3");
    assert_eq!(h.chain.add_block(&c.blob, &c.tx_blobs).unwrap().status, AddStatus::Alternative);
    assert_eq!(h.chain.alternative_block_count(), 3);

    // 180 behind keeps it; 181 behind takes all three at once.
    let top = tip_hash(&h.chain);
    extend(&mut h, top, TIP + 182, 1, "n");
    assert_eq!(h.chain.alternative_block_count(), 3);
    let top = tip_hash(&h.chain);
    extend(&mut h, top, TIP + 183, 1, "o");
    assert_eq!(h.chain.alternative_block_count(), 0);

    // A block on the pruned branch is an orphan — a rule, the peer's problem.
    let d = plain_block(c.hash, TIP + 4, TIP_TIME + 4 * SPACING, 100, &[], &h.miner, b"d4");
    assert_eq!(rule(h.chain.add_block(&d.blob, &d.tx_blobs).unwrap_err()), Rule::RejectedAsOrphaned);
}

/// The C++ never prunes the branch a block just went into, and judges a branch
/// by its tip, so while a competing chain keeps arriving block after block it
/// is followed however deep it forks and however long it is. A node that capped
/// either would stay on the old chain while the C++ nodes moved: two networks.
#[test]
fn a_branch_that_keeps_arriving_is_followed_past_the_depth_and_count_limits() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);
    extend(&mut h, fork, TIP, 200, "a");
    // Forks 200 below the tip (> 180) and holds 200 blocks (> 100) before
    // its 201st outweighs the main chain.
    let (b, status) = extend(&mut h, fork, TIP, 201, "b");
    assert_eq!(status, AddStatus::AlternativeAndSwitched);
    assert_eq!(h.chain.tip_index(), Some(TIP + 201));
    assert_eq!(h.chain.tip_info().unwrap().block_hash, b[200].hash);
    // The 200 blocks that left are one branch over the budget of 100 and not
    // the one being extended: they go whole.
    assert_eq!(h.chain.alternative_block_count(), 0);
}

/// ...but once a main-chain block arrives nothing is excluded, and a branch
/// over the budget that nobody is extending goes whole (`Core.cpp:1688`).
#[test]
fn a_main_block_prunes_a_long_branch_nobody_is_extending() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);
    let (a, _) = extend(&mut h, fork, TIP, 200, "a");
    let (b, status) = extend(&mut h, fork, TIP, 150, "b");
    assert_eq!(status, AddStatus::Alternative);
    assert_eq!(h.chain.alternative_block_count(), 150, "the branch being extended is never pruned");

    extend(&mut h, a[199].hash, TIP + 200, 1, "m");
    assert_eq!(h.chain.alternative_block_count(), 0, "the weakest leaf's segment goes whole");
    let next = plain_block(b[149].hash, TIP + 151, TIP_TIME + 151 * SPACING, 7, &[], &h.miner, b"b151");
    assert_eq!(rule(h.chain.add_block(&next.blob, &next.tx_blobs).unwrap_err()), Rule::RejectedAsOrphaned);
}

/// An alternative block's transactions are checked when its branch is applied.
/// A branch that fails there is forgotten with everything on it, so a peer
/// mining on it gets orphans rather than another unwind and re-apply per block.
#[test]
fn a_branch_that_fails_its_switch_is_forgotten() {
    let mut h = harness(1);
    let fork = tip_hash(&h.chain);
    let (a, _) = extend(&mut h, fork, TIP, 2, "a");

    let mut bad = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"bad", None);
    bad.signatures[0][0][0] ^= 1;
    let b1 = plain_block(fork, TIP + 1, TIP_TIME + SPACING, 21, &[bad], &h.miner, b"b1");
    assert_eq!(h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap().status, AddStatus::Alternative);
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 22, &[], &h.miner, b"b2");
    assert_eq!(h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap().status, AddStatus::Alternative);

    // b3 makes the branch heavier; applying it fails at b1 and is rolled back.
    let b3 = plain_block(b2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 23, &[], &h.miner, b"b3");
    let e = h.chain.add_block(&b3.blob, &b3.tx_blobs).unwrap_err();
    assert!(matches!(rule(e), Rule::Transaction { index: 0, .. }));
    assert_eq!(h.chain.tip_info().unwrap().block_hash, a[1].hash, "the old chain is back");

    for x in [&b1, &b2, &b3] {
        assert!(h.chain.alternative_cumulative_difficulty(&x.hash).is_none(), "the failed branch is forgotten");
    }
    let b4 = plain_block(b3.hash, TIP + 4, TIP_TIME + 4 * SPACING, 24, &[], &h.miner, b"b4");
    assert_eq!(rule(h.chain.add_block(&b4.blob, &b4.tx_blobs).unwrap_err()), Rule::RejectedAsOrphaned);
}

/// In a block that carries transactions, proof of work is checked before them:
/// a block without the work costs one hash, never a block's worth of ring
/// signatures. Only the rule a block failing both names has changed; nothing
/// that was accepted is refused. (An empty block keeps the C++ order.)
#[test]
fn a_block_without_the_work_is_refused_before_its_transactions_are_checked() {
    let mut h = harness(1_000_000_000);
    let prev = tip_hash(&h.chain);
    let mut bad = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"bad", None);
    bad.signatures[0][0][0] ^= 1;
    let b = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 7, &[bad], &h.miner, b"b1");
    assert!(matches!(
        rule(h.chain.add_block(&b.blob, &b.tx_blobs).unwrap_err()),
        Rule::ProofOfWorkTooWeak { .. }
    ));
}

#[test]
fn the_public_difficulty_rule_is_the_one_the_chain_applies() {
    let mut h = harness(1);
    // What `add_block` used, straight from the same call the template builder
    // now makes.
    assert_eq!(h.chain.main_chain_difficulty_for_next_block(TIP).unwrap(), Some(1));
    let a1 = plain_block(tip_hash(&h.chain), TIP + 1, TIP_TIME + SPACING, 11, &[], &h.miner, b"a1");
    let outcome = h.chain.add_block(&a1.blob, &a1.tx_blobs).unwrap();
    assert_eq!(Some(outcome.difficulty), h.chain.main_chain_difficulty_for_next_block(TIP).unwrap());
    // The window is the last `difficultyBlocksCount` indexes ending at the
    // parent, genesis excluded.
    let window = wrkz_chain::difficulty_window_indexes(TIP);
    assert_eq!(*window.end(), TIP);
    assert_eq!(window.count(), 61, "DIFFICULTY_BLOCKS_COUNT_V3");
    assert!(wrkz_chain::difficulty_window_indexes(0).is_empty(), "genesis is never in a window");
}

// ---------------------------------------------------------------------------
// the windowed replay against a chain that actually has transactions
// ---------------------------------------------------------------------------

/// Export this synthetic chain in the **C++** record layout of spec/11, so the
/// windowed replay can be pointed at it exactly as it is pointed at a real
/// node's database.
///
/// Only what a windowed replay reads: `6` block infos, `5` hash → index, `4`
/// raw blocks for the window, `j` key outputs and `b` counters for ring
/// resolution, `7` spent key images, `8` last_block_index and the schema
/// version.
fn export_as_cpp_database(h: &Harness, applied: &[&Built]) -> MemStore {
    use wrkz_storage::codec::{self, KeyPart};
    use wrkz_storage::records::{CachedBlockInfo, RawBlockRecord};

    let mut store = MemStore::default();
    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    ops.push((codec::DB_VERSION_KEY.to_vec(), Some(b"4".to_vec())));

    let top = TIP + applied.len() as u32;
    for index in BASE..=top {
        let ours = h.chain.block_info(index).unwrap().expect("seeded or applied");
        let theirs = CachedBlockInfo {
            block_hash: ours.block_hash,
            timestamp: ours.timestamp,
            block_size: ours.block_size,
            cumulative_difficulty: ours.cumulative_difficulty,
            already_generated_coins: ours.already_generated_coins,
            already_generated_transactions: ours.already_generated_transactions,
        };
        ops.push((codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(index)), Some(theirs.encode())));
        ops.push((
            codec::key(codec::BLOCK_HASH_TO_BLOCK_INDEX, KeyPart::Hash(ours.block_hash)),
            Some(codec::value_u32("5", index)),
        ));
    }
    for (n, b) in applied.iter().enumerate() {
        let index = TIP + 1 + n as u32;
        ops.push((
            codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(index)),
            Some(RawBlockRecord { block: b.blob.clone(), transactions: b.tx_blobs.clone() }.encode()),
        ));
        // The `7` records: every key image a block spends, at that block.
        for blob in &b.tx_blobs {
            for input in &Transaction::from_bytes(blob).unwrap().prefix.inputs {
                if let Input::Key { key_image, .. } = input {
                    ops.push((
                        codec::key(codec::KEY_IMAGE_TO_BLOCK_INDEX, KeyPart::Hash(*key_image)),
                        Some(codec::value_u32("7", index)),
                    ));
                }
            }
        }
    }
    // The `j` table and the `b` counter for the outputs the rings name.
    for out in h.outputs.iter().chain(std::iter::once(&h.locked)) {
        let unlock_time = if out.global_index == h.locked.global_index { TIP as u64 + 1_000_000 } else { 0 };
        let info = wrkz_storage::records::KeyOutputInfo {
            public_key: out.public_key,
            transaction_hash: seed_hash(BASE),
            unlock_time,
            output_index: out.global_index as u16,
            block_index: BASE,
        };
        ops.push((
            codec::key(codec::KEY_OUTPUT_KEY, KeyPart::AmountIndex(AMOUNT, out.global_index)),
            Some(info.encode()),
        ));
    }
    ops.push((codec::key(codec::KEY_OUTPUT_AMOUNT, KeyPart::U64(AMOUNT)), Some(codec::value_u32("b", 9))));
    ops.push((
        codec::key(codec::BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(codec::LAST_BLOCK_INDEX_KEY)),
        Some(codec::value_u32("8", top)),
    ));
    store.write_batch(ops).unwrap();
    store
}

/// The windowed replay end to end on a chain with real spends: the ring members
/// are resolved out of the source's `j` table, the key images out of its `7`
/// table, and every rule runs because a window has checkpoints off.
#[test]
fn a_windowed_replay_seeds_ring_members_and_key_images_from_the_source() {
    let mut h = harness(1);

    let prev = tip_hash(&h.chain);
    let tx1 = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"w1", None);
    let b1 = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 31, &[tx1], &h.miner, b"wb1");
    h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap();
    let tx2 = build_spend(TIP + 1, &h.outputs[2], &h.outputs[3], &h.payee, FEE, b"w2", None);
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 32, &[tx2], &h.miner, b"wb2");
    h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap();
    let b3 = plain_block(b2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 33, &[], &h.miner, b"wb3");
    h.chain.add_block(&b3.blob, &b3.tx_blobs).unwrap();

    let source = ChainReader::new(export_as_cpp_database(&h, &[&b1, &b2, &b3]));

    // A fresh state that knows nothing: no genesis of this chain, no outputs.
    let mut chain = ChainState::open_or_genesis(MemStore::default(), config(), Checkpoints::mainnet()).expect("opens");
    chain.set_clock(Some(NOW));
    let mut lines = Vec::new();
    let report = replay_windows(
        &source,
        &mut chain,
        &[Window { start: TIP + 1, end: TIP + 3 }],
        &ReplayOptions::default(),
        &mut |l| lines.push(l.to_string()),
    )
    .unwrap_or_else(|e| panic!("windowed replay: {e}\n{}", lines.join("\n")));

    assert_eq!(report.blocks(), 3);
    assert_eq!(report.transactions(), 2);
    assert_eq!(report.rings(), 2, "one ring signature verified per key input");
    assert!(report.key_image_history, "the source answered the spent-key-image lookups");
    assert_eq!(chain.tip_index(), Some(TIP + 3));
    assert_eq!(chain.tip_info().unwrap().cumulative_difficulty, TIP_CUMULATIVE + 3);
    // The window really did run with checkpoints off, so the proof of work of
    // every one of those blocks was computed.
    assert!(chain.checkpoints().is_empty(), "checkpoints are off inside a window");

    // A double spend against history the window did not replay is still caught,
    // because the key image heights came out of the source's `7` table: block
    // TIP+3 respends the output block TIP+1 already spent.
    let respend = build_spend(TIP + 2, &h.outputs[0], &h.outputs[4], &h.payee, FEE, b"w3", None);
    let bad = plain_block(b2.hash, TIP + 3, TIP_TIME + 3 * SPACING, 34, &[respend], &h.miner, b"wb4");
    let mut source = export_as_cpp_database(&h, &[&b1, &b2, &b3]);
    source
        .put(
            wrkz_storage::codec::key(
                wrkz_storage::codec::BLOCK_INDEX_TO_RAW_BLOCK,
                wrkz_storage::codec::KeyPart::U32(TIP + 3),
            ),
            wrkz_storage::records::RawBlockRecord { block: bad.blob.clone(), transactions: bad.tx_blobs.clone() }
                .encode(),
        )
        .unwrap();
    let source = ChainReader::new(source);
    let mut chain = ChainState::open_or_genesis(MemStore::default(), config(), Checkpoints::mainnet()).expect("opens");
    chain.set_clock(Some(NOW));
    let e = replay_windows(
        &source,
        &mut chain,
        // The window starts at TIP+3, so the spend in TIP+1 is *not* replayed:
        // only the seeded key-image record can catch this.
        &[Window { start: TIP + 3, end: TIP + 3 }],
        &ReplayOptions::default(),
        &mut quiet(),
    )
    .unwrap_err();
    assert!(e.contains("INPUT_KEYIMAGE_ALREADY_SPENT"), "{e}");
}

/// Without the source's `7` records the run still works, says so once, and
/// reports that double-spend history was not available.
#[test]
fn a_source_without_key_image_records_is_reported_once() {
    let mut h = harness(1);
    let prev = tip_hash(&h.chain);
    let tx1 = build_spend(TIP, &h.outputs[0], &h.outputs[1], &h.payee, FEE, b"w1", None);
    let b1 = plain_block(prev, TIP + 1, TIP_TIME + SPACING, 31, &[tx1], &h.miner, b"wb1");
    h.chain.add_block(&b1.blob, &b1.tx_blobs).unwrap();
    let tx2 = build_spend(TIP + 1, &h.outputs[2], &h.outputs[3], &h.payee, FEE, b"w2", None);
    let b2 = plain_block(b1.hash, TIP + 2, TIP_TIME + 2 * SPACING, 32, &[tx2], &h.miner, b"wb2");
    h.chain.add_block(&b2.blob, &b2.tx_blobs).unwrap();

    // A lite database: no `7` records at all.
    let mut store = export_as_cpp_database(&h, &[&b1, &b2]);
    for blob in b1.tx_blobs.iter().chain(&b2.tx_blobs) {
        for input in &Transaction::from_bytes(blob).unwrap().prefix.inputs {
            if let Input::Key { key_image, .. } = input {
                store
                    .delete(wrkz_storage::codec::key(
                        wrkz_storage::codec::KEY_IMAGE_TO_BLOCK_INDEX,
                        wrkz_storage::codec::KeyPart::Hash(*key_image),
                    ))
                    .unwrap();
            }
        }
    }
    let source = ChainReader::new(store);
    let mut chain = ChainState::open_or_genesis(MemStore::default(), config(), Checkpoints::mainnet()).expect("opens");
    chain.set_clock(Some(NOW));
    let mut lines = Vec::new();
    let report = replay_windows(
        &source,
        &mut chain,
        &[Window { start: TIP + 1, end: TIP + 2 }],
        &ReplayOptions::default(),
        &mut |l| lines.push(l.to_string()),
    )
    .expect("the blocks are still valid");
    assert_eq!(report.blocks(), 2);
    assert!(!report.key_image_history);
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("warning: the source database has no spent-key-image")).count(),
        1,
        "said once, not once per input: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("only detected when both halves were inside one window")));
}

fn quiet() -> impl FnMut(&str) {
    |_: &str| {}
}
