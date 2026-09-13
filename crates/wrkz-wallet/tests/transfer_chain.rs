// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The acceptance proof for stage 2.4: a transaction this wallet builds is
//! accepted by `wrkz-chain`'s full `ValidateTransaction`, in pool mode, over a
//! synthetic chain state that holds the real ring members.
//!
//! This is the strongest offline stand-in for "the C++ daemon accepts it".
//! `wrkz-chain::validate::validate_transaction` is the line-by-line port of
//! `src/cryptonotecore/ValidateTransaction.cpp` and is pinned by the mainnet
//! replay of stage 3.2, so a transaction that passes it passes every stateless
//! rule and every stateful one the chain state can answer:
//!
//! 1. serialized size against `2 · blockMedianSize − 600`;
//! 2. at least one key input, no duplicate key images, no zero relative
//!    offsets, key images in the prime-order subgroup, no input-sum overflow;
//! 3. non-zero output amounts under `MAX_OUTPUT_SIZE_NODE`, keys that
//!    decompress, no output-sum overflow;
//! 4. `fee = inputs − outputs` at or above `getMinimumTransactionFee(size, H)`;
//! 5. `extra.size() < 1024`;
//! 6. the unlock time at least `H + 15`;
//! 7. at most 90 outputs;
//! 8. the mixin inside the tier at `H`, with the per-input floor of 4,300,000;
//! 9. the transaction proof of work, or its fee escape;
//! 10. every `key_offsets` resolving to an output that exists, is unlocked and
//!     is not already spent, one signature per ring member, and
//!     `checkRingSignature` over the real ring keys.
//!
//! What it does not prove: that the *bytes* are the ones the C++ wallet would
//! have produced from the same seed. Only a C++ build with injectable
//! randomness could show that, and the deployed binary has none; the byte-level
//! evidence is the per-stage pinning in `src/transfer/tests.rs` (denominations,
//! extra layout, prefix hash, offsets, sizes, fee arithmetic) against the C++
//! functions read line by line.

use wrkz_chain::records::{BlockInfo, OutputRecord};
use wrkz_chain::validate::{validate_transaction, TxContext, TxRule, ValidatorState};
use wrkz_chain::{keys, records, ChainState, Checkpoints, Config};
use wrkz_pow::{cn_fast_hash, curve};
use wrkz_primitives::constants::TRANSACTION_POW_PASS_WITH_FEE;
use wrkz_primitives::tx::Input;
use wrkz_storage::{KvStore, MemStore};
use wrkz_wallet::daemon::{self, RandomOut, RandomOuts, RandomOutsForAmount, SendResult};
use wrkz_wallet::file::{Hex32, SecretKey, TransactionInput, Wallet};
use wrkz_wallet::transfer::{
    prepare_transaction, FeeType, PreparedTransaction, SeededRandom, SendParams, TransferDaemon,
};

/// Above every fork height and above the last mainnet checkpoint (4,188,000),
/// so the mixin tier is V6, `cn_upx` is the proof-of-work function, and neither
/// the transaction proof of work nor the signature checks are skipped.
const TIP: u32 = 4_400_000;
const SEED_LEN: u32 = 128;
const BASE: u32 = TIP - SEED_LEN + 1;
const SPACING: u64 = 59;
const TIP_TIME: u64 = 1_800_000_000;
const NOW: u64 = TIP_TIME + 10_000;
const TIP_CUMULATIVE: u64 = 1_000_000_000_000;

/// The denomination every seeded output carries, so one ring can be drawn from
/// them. A single "pretty" amount, as a wallet would create.
const AMOUNT: u64 = 1_000_000;
/// How many outputs of [`AMOUNT`] the chain holds. The first two are the test
/// wallet's; the rest are decoys.
const OUTPUT_COUNT: u32 = 8;
const OURS: u32 = 2;

fn config() -> Config {
    Config { store_raw_blocks: false, unwind_history: 512, recent_window: SEED_LEN as usize, ..Config::default() }
}

fn seed_hash(index: u32) -> [u8; 32] {
    cn_fast_hash(&[b"wrkz-wallet transfer acceptance".as_slice(), &index.to_le_bytes()].concat())
}

/// The spend key of the seed in spec/05.
fn spec_wallet() -> Wallet {
    let secret =
        SecretKey::from_hex("243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101").expect("spend key");
    let public = Hex32(curve::secret_key_to_public_key(secret.as_bytes()).expect("public"));
    let mut wallet = Wallet::create_from_spend_key(secret, public, 0).expect("wallet");
    wallet.sub_wallets.is_view_wallet = false;
    wallet
}

fn payee_address() -> String {
    let secret = SecretKey::from_bytes(curve::generate_deterministic_keys(&cn_fast_hash(b"acceptance payee")).0);
    let public = Hex32(curve::secret_key_to_public_key(secret.as_bytes()).expect("public"));
    Wallet::create_from_spend_key(secret, public, 0).expect("payee").primary_address().expect("address").to_string()
}

/// One output of [`AMOUNT`] belonging to `wallet`, derived from a synthetic
/// parent transaction with the real curve functions.
struct Seeded {
    input: TransactionInput,
    record: OutputRecord,
}

fn owned_output(wallet: &Wallet, global_index: u32) -> Seeded {
    let tag = [b"parent".as_slice(), &global_index.to_le_bytes()].concat();
    let (tx_secret, tx_public) = curve::generate_deterministic_keys(&cn_fast_hash(&tag));
    let sub = wallet.primary_sub_wallet().expect("primary");
    let view_public = curve::secret_key_to_public_key(wallet.private_view_key().as_bytes()).expect("view public");

    let sender = curve::generate_key_derivation(&view_public, &tx_secret).expect("derivation");
    let receiver =
        curve::generate_key_derivation(&tx_public, wallet.private_view_key().as_bytes()).expect("derivation");
    assert_eq!(sender, receiver, "both sides derive the same shared secret");

    let key = curve::derive_public_key(&receiver, 0, sub.public_spend_key.as_bytes()).expect("one-time key");
    let ephemeral = curve::derive_secret_key(&receiver, 0, sub.private_spend_key.as_bytes());
    assert_eq!(curve::secret_key_to_public_key(&ephemeral), Some(key), "the one-time key pair matches");
    let key_image = curve::generate_key_image(&key, &ephemeral);

    Seeded {
        input: TransactionInput {
            amount: AMOUNT,
            block_height: BASE as u64,
            global_output_index: Some(u64::from(global_index)),
            key: Hex32(key),
            key_image: Hex32(key_image),
            parent_transaction_hash: Hex32(seed_hash(BASE)),
            // Left unset on purpose, so the send exercises the re-derivation
            // branch of `setupInputs` as well as the chain.
            private_ephemeral: None,
            spend_height: 0,
            transaction_index: 0,
            transaction_public_key: Hex32(tx_public),
            unlock_time: 0,
        },
        record: OutputRecord {
            public_key: key,
            unlock_time: 0,
            transaction_hash: seed_hash(BASE),
            output_index: 0,
            block_index: BASE,
        },
    }
}

/// A decoy output: a real curve point nobody holds the key to, which is all a
/// ring member has to be.
fn decoy_output(global_index: u32) -> OutputRecord {
    let (_, public) = curve::generate_deterministic_keys(&cn_fast_hash(
        &[b"acceptance decoy".as_slice(), &global_index.to_le_bytes()].concat(),
    ));
    OutputRecord {
        public_key: public,
        unlock_time: 0,
        transaction_hash: seed_hash(BASE + 1),
        output_index: global_index as u16,
        block_index: BASE,
    }
}

struct Harness {
    chain: ChainState<MemStore>,
    wallet: Wallet,
    /// Every output of [`AMOUNT`] on the chain, in global index order.
    outputs: Vec<OutputRecord>,
}

/// Write a plausible chain of [`SEED_LEN`] blocks ending at [`TIP`], with
/// [`OUTPUT_COUNT`] spendable outputs of [`AMOUNT`], and a wallet that owns the
/// first [`OURS`] of them.
fn harness() -> Harness {
    let mut wallet = spec_wallet();
    let spend_key = wallet.primary_sub_wallet().expect("primary").public_spend_key;

    let mut store = MemStore::default();
    let mut ops = Vec::new();

    for i in BASE..=TIP {
        let back = u64::from(TIP - i);
        let info = BlockInfo {
            block_hash: seed_hash(i),
            timestamp: TIP_TIME - back * SPACING,
            // A block size at or above the granted full reward zone is not
            // needed: `blockMedianSize` is `max(median, reward zone)` and the
            // zone alone leaves room for any transaction this test builds.
            block_size: 300,
            cumulative_difficulty: TIP_CUMULATIVE - back,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: u64::from(i) + 1,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(i), Some(records::encode_hashes(&[seed_hash(i)]))));
    }

    let mut outputs = Vec::new();
    for global_index in 0..OUTPUT_COUNT {
        let record = if global_index < OURS {
            let seeded = owned_output(&wallet, global_index);
            wallet.store_transaction_input(&spend_key, seeded.input);
            seeded.record
        } else {
            decoy_output(global_index)
        };
        ops.push((keys::output(AMOUNT, global_index), Some(record.encode())));
        outputs.push(record);
    }
    ops.push((keys::output_count(AMOUNT), Some(OUTPUT_COUNT.to_le_bytes().to_vec())));

    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(TIP.to_le_bytes().to_vec())));
    store.write_batch(ops).expect("seed writes");

    let mut chain = ChainState::open(store, config(), Checkpoints::mainnet()).expect("state opens");
    chain.set_clock(Some(NOW));
    assert_eq!(chain.tip_index(), Some(TIP));
    assert!(
        !chain.checkpoints().is_in_checkpoint_zone(u64::from(TIP) + 1),
        "4,400,000 is outside the checkpoint zone, so signatures and the tx proof of work are checked"
    );

    Harness { chain, wallet, outputs }
}

/// A `/getrandom_outs` stand-in serving exactly the outputs the chain holds, in
/// ascending global index order — the shape the live daemon returns.
struct ChainDaemon {
    outputs: Vec<OutputRecord>,
}

impl TransferDaemon for ChainDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> daemon::Result<RandomOuts> {
        let outs = amounts
            .iter()
            .map(|amount| RandomOutsForAmount {
                amount: *amount,
                outs: self
                    .outputs
                    .iter()
                    .enumerate()
                    .take(outs_count as usize)
                    .map(|(i, o)| RandomOut { global_amount_index: i as u64, out_key: Hex32(o.public_key).to_hex() })
                    .collect(),
            })
            .collect();
        Ok(RandomOuts { outs, status: "OK".to_string() })
    }

    fn send_raw_transaction(&self, _tx_hex: &str) -> daemon::Result<SendResult> {
        panic!("the acceptance test never relays");
    }
}

fn tx_context<'a>(chain: &ChainState<MemStore>, checkpoints: &'a Checkpoints) -> TxContext<'a> {
    TxContext {
        // Pool admission: the top index and the top block's timestamp
        // (`Core.cpp:2229`).
        block_height: u64::from(TIP),
        block_median_size: chain.block_median_size(),
        block_timestamp: TIP_TIME,
        is_pool_transaction: true,
        checkpoints,
    }
}

fn build(harness: &Harness, fee: FeeType, mixin: u64, pow_threads: usize) -> PreparedTransaction {
    let daemon = ChainDaemon { outputs: harness.outputs.clone() };
    let params = SendParams {
        mixin,
        fee,
        pow_threads,
        // The whole balance minus the fee, so both inputs are spent and there
        // is change: two rings, several outputs.
        ..SendParams::basic(&payee_address(), AMOUNT, "", u64::from(TIP), u64::from(TIP))
    };
    prepare_transaction(&harness.wallet, &daemon, &params, &mut SeededRandom::from_label(b"acceptance"))
        .expect("the wallet builds a transaction")
}

#[test]
fn a_wallet_built_transaction_passes_the_full_chain_validator() {
    let harness = harness();
    let checkpoints = Checkpoints::mainnet();

    // A fee at or above `TRANSACTION_POW_PASS_WITH_FEE` takes the fee escape of
    // spec/06 rule 9, so no nonce search is needed and the test is fast. The
    // proof-of-work path has its own test below.
    let prepared = build(&harness, FeeType::FixedFee(TRANSACTION_POW_PASS_WITH_FEE), 3, 1);

    assert_eq!(prepared.fee, TRANSACTION_POW_PASS_WITH_FEE);
    assert_eq!(prepared.transaction.prefix.inputs.len(), 2, "both seeded inputs are spent");
    assert_eq!(prepared.pow_nonce, None, "the fee escape applied");
    assert!(prepared.verify_signatures());

    let blob = prepared.transaction.to_bytes().expect("serializes");
    assert_eq!(blob.len(), prepared.size);

    let validation = validate_transaction(
        &prepared.transaction,
        &blob,
        &mut ValidatorState::new(),
        &harness.chain,
        &tx_context(&harness.chain, &checkpoints),
    )
    .expect("the chain validator accepts the transaction");

    assert_eq!(validation.fee, prepared.fee);
    assert!(!validation.is_fusion);

    // The rings really did resolve against the chain: every offset names an
    // output the state holds, and one of them is ours.
    for (i, input) in prepared.transaction.prefix.inputs.iter().enumerate() {
        let Input::Key { key_offsets, amount, .. } = input else { panic!("key input") };
        assert_eq!(*amount, AMOUNT);
        let absolute = wrkz_primitives::tx::relative_offsets_to_absolute(key_offsets).expect("ascending");
        assert_eq!(absolute.len(), 4, "ring size = mixin + 1");
        for index in &absolute {
            assert!(*index < u64::from(OUTPUT_COUNT), "index {index} exists on chain");
        }
        let real = absolute[prepared.rings[i].real_output];
        assert!(real < u64::from(OURS), "the real ring member is one of ours");
    }
}

#[test]
fn the_validator_rejects_the_same_transaction_once_it_is_tampered_with() {
    // The acceptance above is only meaningful if the validator would have said
    // no to a transaction that is wrong, so each mutation is checked to fail on
    // the rule it breaks.
    let harness = harness();
    let checkpoints = Checkpoints::mainnet();
    let prepared = build(&harness, FeeType::FixedFee(TRANSACTION_POW_PASS_WITH_FEE), 3, 1);

    let check = |tx: &wrkz_primitives::tx::Transaction| -> Result<(), TxRule> {
        let blob = tx.to_bytes().expect("serializes");
        match validate_transaction(
            tx,
            &blob,
            &mut ValidatorState::new(),
            &harness.chain,
            &tx_context(&harness.chain, &checkpoints),
        ) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.rule().cloned().expect("a rule, not a state fault")),
        }
    };

    assert_eq!(check(&prepared.transaction), Ok(()));

    // A zeroed signature.
    let mut tx = prepared.transaction.clone();
    tx.signatures[0][0] = [0u8; 64];
    assert!(matches!(check(&tx), Err(TxRule::InputInvalidSignatures { .. })));

    // A ring member swapped for one nobody signed over.
    let mut tx = prepared.transaction.clone();
    let Input::Key { key_offsets, .. } = &mut tx.prefix.inputs[0] else { panic!() };
    key_offsets[1] += 1;
    assert!(matches!(check(&tx), Err(TxRule::InputInvalidSignatures { .. })));

    // A global index the chain does not hold.
    let mut tx = prepared.transaction.clone();
    let Input::Key { key_offsets, .. } = &mut tx.prefix.inputs[0] else { panic!() };
    key_offsets[0] += u64::from(OUTPUT_COUNT);
    assert!(matches!(check(&tx), Err(TxRule::InputInvalidGlobalIndex { .. })));

    // A raised output amount, so the fee no longer covers the minimum.
    let mut tx = prepared.transaction.clone();
    let last = tx.prefix.outputs.len() - 1;
    tx.prefix.outputs[last].amount += TRANSACTION_POW_PASS_WITH_FEE;
    assert!(matches!(check(&tx), Err(TxRule::WrongFee { .. })));

    // An unlock time below `H + 15`.
    let mut tx = prepared.transaction.clone();
    tx.prefix.unlock_time = u64::from(TIP) + 14;
    assert!(matches!(check(&tx), Err(TxRule::UnlockTimeTooSmall { .. })));

    // A key image already spent: feed the validator a state that holds it.
    let mut state = ValidatorState::new();
    let Input::Key { key_image, .. } = &prepared.transaction.prefix.inputs[0] else { panic!() };
    state.spent_key_images.insert(*key_image);
    let blob = prepared.transaction.to_bytes().unwrap();
    let err = validate_transaction(
        &prepared.transaction,
        &blob,
        &mut state,
        &harness.chain,
        &tx_context(&harness.chain, &checkpoints),
    )
    .expect_err("a double spend is refused");
    assert!(matches!(err.rule(), Some(TxRule::InputKeyImageAlreadySpent { .. })));
}

#[test]
fn a_ring_of_one_is_below_the_tier_floor_from_4_300_000() {
    // The per-input floor of spec/06 rule 8: from 4,300,000 the minimum is 1,
    // so a mixin of 0 (ring size 1) is refused by the wallet before it is ever
    // built, and would be refused by the validator too.
    let harness = harness();
    let daemon = ChainDaemon { outputs: harness.outputs.clone() };
    let params =
        SendParams { mixin: 0, ..SendParams::basic(&payee_address(), AMOUNT, "", u64::from(TIP), u64::from(TIP)) };
    let err = prepare_transaction(&harness.wallet, &daemon, &params, &mut SeededRandom::from_label(b"floor"))
        .expect_err("mixin 0 is below the tier");
    assert_eq!(err.code(), 21, "MIXIN_TOO_SMALL");
}

#[test]
#[ignore = "searches a real transaction proof of work; minutes on one core, tens of seconds on many"]
fn a_low_fee_transaction_passes_the_validator_including_its_proof_of_work() {
    // The other half of spec/06 rule 9: below `TRANSACTION_POW_PASS_WITH_FEE` a
    // nonce must be found, and the validator hashes the prefix with `cn_upx`
    // and checks it against `40000 + (inputs + 4 · outputs) · 1000`.
    let harness = harness();
    let checkpoints = Checkpoints::mainnet();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

    let prepared = build(&harness, FeeType::MinimumFee, 3, threads);

    assert!(prepared.fee < TRANSACTION_POW_PASS_WITH_FEE, "the escape does not apply");
    let nonce = prepared.pow_nonce.expect("a nonce was searched for");
    let prefix = prepared.transaction.prefix.to_bytes();
    assert_eq!(&prefix[prefix.len() - 8..], &nonce, "the nonce is the last eight bytes of the prefix");

    let blob = prepared.transaction.to_bytes().expect("serializes");
    validate_transaction(
        &prepared.transaction,
        &blob,
        &mut ValidatorState::new(),
        &harness.chain,
        &tx_context(&harness.chain, &checkpoints),
    )
    .expect("the chain validator accepts it, proof of work included");

    // And the validator really was checking: a different nonce fails.
    let mut tampered = prepared.transaction.clone();
    let at = tampered.prefix.extra.len() - 8;
    tampered.prefix.extra[at] ^= 0xff;
    let blob = tampered.to_bytes().unwrap();
    let err = validate_transaction(
        &tampered,
        &blob,
        &mut ValidatorState::new(),
        &harness.chain,
        &tx_context(&harness.chain, &checkpoints),
    )
    .expect_err("a broken nonce is refused");
    assert!(matches!(err.rule(), Some(TxRule::PowInvalid { .. })));
}
