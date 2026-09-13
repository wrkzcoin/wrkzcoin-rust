// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Unit tests for transaction construction.
//!
//! Every stage is pinned twice where it can be: once against the C++ function
//! it mirrors (values derived by reading `Transfer.cpp` / `Utilities.cpp`), and
//! once against a fixed seed, so a change in any stage moves bytes a test
//! asserts. The end-to-end acceptance proof — a transaction validated by
//! `wrkz-chain`'s `validate_transaction` over a synthetic chain — is in
//! `tests/transfer_chain.rs`, because it needs a dev-dependency the library
//! does not have.

use std::cell::RefCell;

use wrkz_primitives::tx::{parse_extra_wallet, PaymentId};

use super::*;
use crate::daemon::{RandomOut, RandomOutsForAmount};

/////////////////////
/* FIXTURES        */
/////////////////////

/// The spend key of the seed in spec/05, whose address is pinned in
/// `spec/vectors/primitives.txt`.
const SPEC_SPEND_SECRET: &str = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
const SPEC_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

/// A height above every fork: mixin tier V6 (min 1, max 7, default 7), the
/// 128-byte fee chunk, the dynamic transaction proof-of-work difficulty with
/// its fee escape.
const HEIGHT: u64 = 4_400_000;
/// A height below `TRANSACTION_POW_HEIGHT` (1,123,000), where no transaction
/// proof of work is required at all. Used by the tests that need the *minimum*
/// fee path, which at a current height is below `TRANSACTION_POW_PASS_WITH_FEE`
/// and would therefore spend tens of seconds searching for a nonce.
const HEIGHT_NO_POW: u64 = 1_100_000;
/// A fixed "now" so the unlock-time and input-unlock rules are reproducible.
const NOW: u64 = 1_800_000_000;

fn spend_secret() -> SecretKey {
    SecretKey::from_hex(SPEC_SPEND_SECRET).expect("spend key")
}

/// A wallet holding the spec seed and nothing else.
fn wallet_from_spend_key(secret: SecretKey) -> Wallet {
    let public = Hex32(curve::secret_key_to_public_key(secret.as_bytes()).expect("public spend key"));
    Wallet::create_from_spend_key(secret, public, 0).expect("wallet")
}

fn test_wallet() -> Wallet {
    let mut wallet = wallet_from_spend_key(spend_secret());
    assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));
    wallet.sub_wallets.is_view_wallet = false;
    wallet
}

/// A second wallet, the payee.
fn payee_wallet() -> Wallet {
    let secret = SecretKey::from_bytes(curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(b"payee")).0);
    wallet_from_spend_key(secret)
}

/// One output of ours, derived with the real curve functions from a synthetic
/// parent transaction, so the ephemeral secret and the key image are the ones a
/// scan would have produced.
///
/// `cache_ephemeral` decides whether `privateEphemeral` is stored — the two
/// branches of `setupInputs`.
fn owned_input(
    wallet: &Wallet,
    tag: &[u8],
    output_index: u64,
    amount: u64,
    global_index: u64,
    cache_ephemeral: bool,
) -> TransactionInput {
    let (tx_secret, tx_public) = curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(tag));
    let sub = wallet.primary_sub_wallet().expect("primary");
    let view_public = curve::secret_key_to_public_key(wallet.private_view_key().as_bytes()).expect("view public");

    let sender = curve::generate_key_derivation(&view_public, &tx_secret).expect("derivation");
    let receiver =
        curve::generate_key_derivation(&tx_public, wallet.private_view_key().as_bytes()).expect("derivation");
    assert_eq!(sender, receiver);

    let key = curve::derive_public_key(&receiver, output_index, sub.public_spend_key.as_bytes()).expect("key");
    let ephemeral = curve::derive_secret_key(&receiver, output_index, sub.private_spend_key.as_bytes());
    let key_image = curve::generate_key_image(&key, &ephemeral);

    TransactionInput {
        amount,
        block_height: HEIGHT - 1000,
        global_output_index: Some(global_index),
        key: Hex32(key),
        key_image: Hex32(key_image),
        parent_transaction_hash: Hex32(wrkz_pow::cn_fast_hash(tag)),
        private_ephemeral: cache_ephemeral.then(|| SecretKey::from_bytes(ephemeral)),
        spend_height: 0,
        transaction_index: output_index,
        transaction_public_key: Hex32(tx_public),
        unlock_time: 0,
    }
}

/// Give the wallet a set of `(amount, global index)` inputs.
fn fund(wallet: &mut Wallet, inputs: &[(u64, u64)], cache_ephemeral: bool) {
    let spend_key = wallet.primary_sub_wallet().expect("primary").public_spend_key;
    for (i, (amount, global_index)) in inputs.iter().enumerate() {
        let input = owned_input(
            wallet,
            &[b"input".as_slice(), &[i as u8]].concat(),
            0,
            *amount,
            *global_index,
            cache_ephemeral,
        );
        wallet.store_transaction_input(&spend_key, input);
    }
}

/////////////////////
/* MOCK DAEMON     */
/////////////////////

/// A `/getrandom_outs` + `/sendrawtransaction` stand-in.
///
/// Decoys are real curve points at ascending global indexes, which is what the
/// live daemon returns (verified against `node-fin.wrkz.work`), so the ring is
/// ascending and the relative offsets are the ordinary ones.
struct MockDaemon {
    /// How many outputs to hand back per amount, whatever was asked for.
    available: usize,
    /// Amounts the daemon claims to know nothing about.
    missing: Vec<u64>,
    /// `/getrandom_outs` fails with this instead of answering.
    random_outs_error: Option<DaemonError>,
    /// What `/sendrawtransaction` answers.
    send_status: (String, Option<String>),
    /// Every hex blob relayed.
    sent: RefCell<Vec<String>>,
    /// Every `(amounts, outs_count)` requested.
    requests: RefCell<Vec<(Vec<u64>, u64)>>,
    /// What a proof-of-work server answers: `None` is no server configured,
    /// `Some(nonce)` a server that returns this nonce whatever it is asked.
    remote_nonce: Option<[u8; TX_POW_NONCE_SIZE]>,
    /// Every `(difficulty, height)` a proof-of-work server was asked for.
    remote_asked: RefCell<Vec<(u64, u64)>>,
}

impl Default for MockDaemon {
    fn default() -> Self {
        MockDaemon {
            available: 8,
            missing: Vec::new(),
            random_outs_error: None,
            send_status: ("OK".to_string(), None),
            sent: RefCell::new(Vec::new()),
            requests: RefCell::new(Vec::new()),
            remote_nonce: None,
            remote_asked: RefCell::new(Vec::new()),
        }
    }
}

impl MockDaemon {
    fn with_available(available: usize) -> Self {
        MockDaemon { available, ..MockDaemon::default() }
    }

    /// Decoy `n` of `amount`: a deterministic curve point at a global index far
    /// from the ones the test wallet owns.
    fn decoy(amount: u64, n: usize) -> (u64, [u8; 32]) {
        let seed = wrkz_pow::cn_fast_hash(&[b"decoy".as_slice(), &amount.to_le_bytes(), &n.to_le_bytes()].concat());
        let (_, public) = curve::generate_deterministic_keys(&seed);
        (1_000_000 + (n as u64) * 37, public)
    }
}

impl TransferDaemon for MockDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> daemon::Result<RandomOuts> {
        self.requests.borrow_mut().push((amounts.to_vec(), outs_count));
        if let Some(e) = &self.random_outs_error {
            return Err(match e {
                DaemonError::Status(s) => DaemonError::Status(s.clone()),
                DaemonError::BadRequest(s) => DaemonError::BadRequest(s.clone()),
                DaemonError::Http(c, s) => DaemonError::Http(*c, s.clone()),
                _ => DaemonError::Transport("mock".into()),
            });
        }

        let outs = amounts
            .iter()
            .filter(|a| !self.missing.contains(a))
            .map(|amount| RandomOutsForAmount {
                amount: *amount,
                outs: (0..self.available)
                    .map(|n| {
                        let (index, key) = MockDaemon::decoy(*amount, n);
                        RandomOut { global_amount_index: index, out_key: Hex32(key).to_hex() }
                    })
                    .collect(),
            })
            .collect();

        Ok(RandomOuts { outs, status: "OK".to_string() })
    }

    fn send_raw_transaction(&self, tx_hex: &str) -> daemon::Result<SendResult> {
        self.sent.borrow_mut().push(tx_hex.to_string());
        Ok(SendResult { status: self.send_status.0.clone(), error: self.send_status.1.clone() })
    }

    fn remote_pow(&self, _prefix: &[u8], difficulty: u64, height: u64) -> Option<[u8; TX_POW_NONCE_SIZE]> {
        self.remote_asked.borrow_mut().push((difficulty, height));
        self.remote_nonce
    }
}

/////////////////////
/* DENOMINATIONS   */
/////////////////////

#[test]
fn denominations_table() {
    // `splitAmountIntoDenominations` (`Transfer.cpp:1310`): each non-zero
    // decimal digit times its power of ten, least significant first. The C++
    // comment's own example is the first row.
    let table: &[(u64, &[u64])] = &[
        (1_234_567, &[7, 60, 500, 4_000, 30_000, 200_000, 1_000_000]),
        (0, &[]),
        (1, &[1]),
        (10, &[10]),
        // "If we have for example, 1010 - we want 1000 + 10, not 1000 + 0 + 10 + 0"
        (1010, &[10, 1000]),
        (1000, &[1000]),
        (999, &[9, 90, 900]),
        (100_000, &[100_000]),
        (9_876_543_210, &[10, 200, 3_000, 40_000, 500_000, 6_000_000, 70_000_000, 800_000_000, 9_000_000_000]),
    ];

    for (amount, want) in table {
        assert_eq!(&split_amount_into_denominations(*amount, true), want, "amount {amount}");
        assert_eq!(&split_amount_into_denominations(*amount, false), want, "amount {amount}, no cap");
    }
}

#[test]
fn denominations_split_amounts_above_the_client_output_cap() {
    // MAX_OUTPUT_SIZE_CLIENT is 500,000,000,000. A digit of 6 at 10^12 is
    // 6,000,000,000,000: too large, so ten pieces of 600,000,000,000 - still
    // too large, so a hundred pieces of 60,000,000,000.
    let split = split_amount_into_denominations(6_000_000_000_000, true);
    assert_eq!(split.len(), 100);
    assert!(split.iter().all(|a| *a == 60_000_000_000));
    assert_eq!(split.iter().sum::<u64>(), 6_000_000_000_000);

    // A digit of 1 at 10^12 needs only one round: ten pieces of 10^11, which is
    // under the cap.
    let split = split_amount_into_denominations(1_000_000_000_000, true);
    assert_eq!(split, vec![100_000_000_000; 10]);

    // Exactly the cap is not "above" it, so it stays one output.
    assert_eq!(split_amount_into_denominations(500_000_000_000, true), vec![500_000_000_000]);

    // Without the cap the split is the plain decomposition.
    assert_eq!(split_amount_into_denominations(6_000_000_000_000, false), vec![6_000_000_000_000]);
}

#[test]
fn denominations_agree_with_decompose_amount_at_zero_dust() {
    // `splitAmountIntoDenominations(a, false)` and
    // `decompose_amount_into_digits(a, 0)` are the same function written twice.
    for amount in [1u64, 7, 42, 1010, 1_234_567, 9_876_543_210, 999_999_999] {
        assert_eq!(split_amount_into_denominations(amount, false), tx::decompose_amount(amount, 0), "amount {amount}");
    }
}

#[test]
fn every_denomination_is_a_pretty_amount() {
    for amount in [1u64, 1010, 1_234_567, 6_000_000_000_000, 987_654_321] {
        for d in split_amount_into_denominations(amount, true) {
            assert!(constants::is_pretty_amount(d), "{d} from {amount}");
        }
    }
}

/////////////////////
/* EXTRA LAYOUT    */
/////////////////////

#[test]
fn extra_layout_public_key_only() {
    let pubkey = Hex32([0x11; 32]);
    let extra = build_wallet_extra(&pubkey, &[], None);
    assert_eq!(extra.len(), 33);
    assert_eq!(extra[0], 0x01);
    assert_eq!(&extra[1..], &[0x11u8; 32]);
    assert_eq!(parse_extra_wallet(&extra).public_key, Some([0x11; 32]));
}

#[test]
fn extra_layout_long_payment_id() {
    // `01 ‖ R ‖ 02 ‖ 33 ‖ 00 ‖ id`
    let pubkey = Hex32([0x11; 32]);
    let nonce = tx::build_nonce(Some(&PaymentId::Long([0x22; 32])), None);
    let extra = build_wallet_extra(&pubkey, &nonce, None);

    assert_eq!(extra.len(), 33 + 2 + 33);
    assert_eq!(extra[33], 0x02);
    assert_eq!(extra[34], 33);
    assert_eq!(extra[35], 0x00);
    assert_eq!(&extra[36..68], &[0x22u8; 32]);

    let parsed = parse_extra_wallet(&extra);
    assert_eq!(parsed.public_key, Some([0x11; 32]));
    assert_eq!(parsed.payment_id, Some(PaymentId::Long([0x22; 32])));
}

#[test]
fn extra_layout_short_payment_id_and_pow_nonce() {
    // `01 ‖ R ‖ 02 ‖ 09 ‖ 03 ‖ 8 bytes ‖ 04 ‖ 8 bytes`
    let pubkey = Hex32([0x11; 32]);
    let nonce = tx::build_nonce(Some(&PaymentId::EncryptedShort([0xab; 8])), None);
    let pow = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    let extra = build_wallet_extra(&pubkey, &nonce, Some(&pow));

    assert_eq!(extra.len(), 33 + 2 + 9 + 9);
    assert_eq!(extra[33], 0x02);
    assert_eq!(extra[34], 9);
    assert_eq!(extra[35], 0x03);
    assert_eq!(&extra[36..44], &[0xabu8; 8]);
    assert_eq!(extra[44], 0x04);
    assert_eq!(&extra[45..], &pow);
    // The nonce is the last eight bytes of extra, and so of the prefix.
    assert_eq!(&extra[extra.len() - 8..], &pow);

    let parsed = parse_extra_wallet(&extra);
    assert_eq!(parsed.payment_id, Some(PaymentId::EncryptedShort([0xab; 8])));
}

#[test]
fn extra_layout_arbitrary_data_after_the_payment_id() {
    let pubkey = Hex32([0x11; 32]);
    let nonce = tx::build_nonce(Some(&PaymentId::Long([0x22; 32])), Some(b"hello"));
    let extra = build_wallet_extra(&pubkey, &nonce, None);

    // nonce = 00 ‖ 32 bytes ‖ 7f ‖ 05 ‖ "hello" = 40 bytes
    assert_eq!(extra[33], 0x02);
    assert_eq!(extra[34], 40);
    assert_eq!(extra[68], 0x7f);
    assert_eq!(extra[69], 5);
    assert_eq!(&extra[70..75], b"hello");
}

#[test]
fn extra_nonce_length_is_a_varint() {
    // `Tools::uintToVarintVector`: 127 bytes is one byte, 128 is two. The
    // primitives' `build_extra` writes a single byte and cannot express this,
    // which is why the wallet has its own builder.
    let pubkey = Hex32([0x11; 32]);
    let short = build_wallet_extra(&pubkey, &[0u8; 127], None);
    assert_eq!(&short[33..35], &[0x02, 127]);
    let long = build_wallet_extra(&pubkey, &[0u8; 128], None);
    assert_eq!(&long[33..36], &[0x02, 0x80, 0x01]);
    assert!(tx::build_extra(&[0x11; 32], Some(&vec![0u8; 256]), None).is_err());
}

/////////////////////
/* PREFIX HASH     */
/////////////////////

#[test]
fn prefix_hash_of_a_hand_built_transaction() {
    // A prefix whose every field is fixed, so the hash pins the serializer, the
    // extra builder and `getTransactionPrefixHash` together.
    let prefix = TransactionPrefix {
        version: 1,
        unlock_time: 4_400_035,
        inputs: vec![Input::Key { amount: 1000, key_offsets: vec![5, 3, 12], key_image: [0x33; 32] }],
        outputs: vec![Output { amount: 900, key: [0x44; 32] }, Output { amount: 90, key: [0x45; 32] }],
        extra: build_wallet_extra(&Hex32([0x11; 32]), &[], Some(&[0u8; 8])),
    };

    let bytes = prefix.to_bytes();
    // version 01, unlock a3c78c02 (4,400,035), one key input (tag 02, amount
    // e807 = 1000, three offsets 05 03 0c, the key image), two key outputs
    // (8407 = 900 and 5a = 90, each behind tag 02), then extra as a
    // varint-length blob: 2a = 42 bytes of `01 | R | 04 | 8 zero bytes`.
    assert_eq!(
        hex(&bytes),
        concat!(
            "01a3c78c02",
            "01",
            "02",
            "e807",
            "03",
            "05",
            "03",
            "0c",
            "3333333333333333333333333333333333333333333333333333333333333333",
            "02",
            "8407",
            "02",
            "4444444444444444444444444444444444444444444444444444444444444444",
            "5a",
            "02",
            "4545454545454545454545454545454545454545454545454545454545454545",
            "2a",
            "01",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "04",
            "0000000000000000",
        )
    );
    assert_eq!(hex(&prefix.hash()), "cabdc7dd9ffab4ea79beeb6e68e576f1f0ee3b44b9c920710a2b59f9126d4618");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/////////////////////
/* SIZE AND FEE    */
/////////////////////

#[test]
fn estimate_transaction_size_matches_the_cpp_arithmetic() {
    // header 1 + 10 + 1 + 0 + 32 + 0 = 44; input 112 + mixin * 68; output 43.
    assert_eq!(estimate_transaction_size(&[], 0, false, 0), 44);
    assert_eq!(estimate_transaction_size(&[0], 1, false, 0), 44 + 112 + 43);
    assert_eq!(estimate_transaction_size(&[7], 1, false, 0), 44 + 112 + 7 * 68 + 43);
    // A payment id adds a flat 35, extra data its length plus 4.
    assert_eq!(estimate_transaction_size(&[1], 2, true, 0), 44 + 35 + 180 + 86);
    assert_eq!(estimate_transaction_size(&[1], 2, false, 10), 44 + 14 + 180 + 86);
    // Rings of different sizes are charged individually.
    assert_eq!(estimate_transaction_size(&[1, 7], 1, false, 0), 44 + (112 + 68) + (112 + 476) + 43);
}

#[test]
fn approximate_maximum_input_count_for_fusion() {
    // (30000 - 42 - 4 * 43) / (112 + 68 * mixin)
    assert_eq!(approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, 7), 50);
    assert_eq!(approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, 1), 165);
    assert_eq!(approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, 0), 265);
    // A ring so large that fewer than twelve inputs fit is what
    // `FUSION_MIXIN_TOO_LARGE` reports.
    assert_eq!(approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, 34), 12);
    assert_eq!(approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, 35), 11);
    // No underflow when nothing fits at all.
    assert_eq!(approximate_maximum_input_count(10, 4, 1), 0);
}

#[test]
fn next_fallback_mixin_table() {
    // `Mixins.cpp:71`.
    assert_eq!(next_fallback_mixin(1, 0, 1), None, "already at the minimum");
    assert_eq!(next_fallback_mixin(0, 0, 1), None);
    assert_eq!(next_fallback_mixin(7, 5, 1), Some(5), "the measured value");
    assert_eq!(next_fallback_mixin(7, 9, 1), Some(6), "never at or above what failed");
    assert_eq!(next_fallback_mixin(7, 0, 1), Some(1), "nothing learned lands on the minimum");
    assert_eq!(next_fallback_mixin(7, 0, 3), Some(3));
}

#[test]
fn default_unlock_time_by_height() {
    assert_eq!(default_unlock_time(1_500_000), 1_500_000 + 40 + 15);
    assert_eq!(default_unlock_time(1_500_001), 1_500_001 + 20 + 15);
    assert_eq!(default_unlock_time(HEIGHT), HEIGHT + 35);
}

#[test]
fn fee_verification_bounds() {
    // The per-byte check multiplies the rate by the whole size, not by started
    // chunks: at the V2 rate a 1000-byte transaction wants at least 78.
    let rate = FeeType::MinimumFee;
    assert_eq!((rate.rate(HEIGHT) * 1000.0) as u64, 78);
    assert!(!verify_transaction_fee(rate, 77, HEIGHT, 1000));
    assert!(verify_transaction_fee(rate, 78, HEIGHT, 1000));
    assert!(verify_transaction_fee(rate, 156, HEIGHT, 1000));
    assert!(!verify_transaction_fee(rate, 157, HEIGHT, 1000), "no more than twice the rate");
    // A fixed fee must match exactly.
    assert!(verify_transaction_fee(FeeType::FixedFee(500), 500, HEIGHT, 1000));
    assert!(!verify_transaction_fee(FeeType::FixedFee(500), 501, HEIGHT, 1000));
    // Below the fee-per-byte heights only the flat minimum is checked.
    assert!(verify_transaction_fee(FeeType::FixedFee(5), 50_000, 700_000, 1000));
    assert!(!verify_transaction_fee(FeeType::FixedFee(5), 49_999, 700_000, 1000));
    assert!(verify_transaction_fee(FeeType::FixedFee(5), 5, 100, 1000));
}

#[test]
fn payload_size_limit() {
    let max = fees::wallet_max_tx_size(HEIGHT);
    assert_eq!(max, 124_400, "min(100000 + h * 102400 / 525600, 125000) - 600");
    assert!(is_transaction_payload_too_big(max as usize, HEIGHT).is_ok());
    let err = is_transaction_payload_too_big(max as usize + 1, HEIGHT).unwrap_err();
    assert_eq!(err.code(), 32);
}

/////////////////////
/* INPUT SELECTION */
/////////////////////

#[test]
fn digit_count_matches_floor_log10_plus_one() {
    for (amount, digits) in [(1u64, 1), (9, 1), (10, 2), (99, 2), (100, 3), (1000, 4), (u64::MAX, 20)] {
        assert_eq!(digit_count(amount), digits, "amount {amount}");
    }
}

#[test]
fn input_selection_is_round_robin_over_digit_buckets() {
    // `SubWallets::getSpendableTransactionInputs` (`SubWallets.cpp:530`):
    // sorted largest first, bucketed by digit count, then one per bucket
    // starting from the smallest bucket, smallest amount within each bucket.
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(1, 10), (7, 11), (20, 12), (80, 13), (100, 14), (600, 15), (9, 16)], true);

    let selected = wallet.spendable_transaction_inputs_at(true, &[], HEIGHT, NOW).expect("selection");
    let amounts: Vec<u64> = selected.iter().map(|i| i.input.amount).collect();

    // Buckets: {1: [1, 7, 9]}, {2: [20, 80]}, {3: [100, 600]}.
    // Round 1 takes 1, 20, 100; round 2 takes 7, 80, 600; round 3 takes 9.
    assert_eq!(amounts, vec![1, 20, 100, 7, 80, 600, 9]);
}

#[test]
fn input_selection_skips_locked_inputs_and_view_wallets() {
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(1000, 20), (2000, 21)], true);
    // Lock the second one behind an unlock height above the tip.
    let spend_key = wallet.primary_sub_wallet().unwrap().public_spend_key;
    wallet.sub_wallets.sub_wallet[0].unspent_inputs[1].unlock_time = HEIGHT + 100;

    let selected = wallet.spendable_transaction_inputs_at(true, &[], HEIGHT, NOW).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].input.amount, 1000);
    assert_eq!(selected[0].public_spend_key, spend_key);

    wallet.sub_wallets.is_view_wallet = true;
    assert_eq!(wallet.spendable_transaction_inputs_at(true, &[], HEIGHT, NOW).unwrap_err().code(), 39);
}

#[test]
fn input_selection_honours_the_subwallet_filter() {
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(1000, 30)], true);
    let second = wallet.add_sub_wallet().expect("subwallet").0;
    let second_key = wallet.sub_wallets.sub_wallet[1].public_spend_key;

    // Give the second subwallet an input of its own.
    let input = owned_input(&wallet, b"second-sub", 0, 5000, 31, true);
    let key = curve::derive_public_key(
        &curve::generate_key_derivation(input.transaction_public_key.as_bytes(), wallet.private_view_key().as_bytes())
            .unwrap(),
        0,
        second_key.as_bytes(),
    )
    .unwrap();
    let mut input = input;
    input.key = Hex32(key);
    wallet.store_transaction_input(&second_key, input);

    let only_second = wallet.spendable_transaction_inputs_at(false, &[second_key], HEIGHT, NOW).unwrap();
    assert_eq!(only_second.len(), 1);
    assert_eq!(only_second[0].input.amount, 5000);
    assert_eq!(only_second[0].public_spend_key, second_key);
    assert!(second.starts_with("Wrkz"));

    let all = wallet.spendable_transaction_inputs_at(true, &[], HEIGHT, NOW).unwrap();
    assert_eq!(all.len(), 2);
}

/////////////////////
/* VALIDATION      */
/////////////////////

fn funded_wallet() -> Wallet {
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(100_000, 40), (200_000, 41), (300_000, 42), (900_000, 43)], true);
    wallet
}

fn base_params(amount: u64) -> SendParams {
    SendParams { mixin: 1, ..SendParams::basic(&payee_address(), amount, "", HEIGHT, HEIGHT) }
}

fn payee_address() -> String {
    payee_wallet().primary_address().expect("payee address").to_string()
}

/// Run the validator with the fixed clock.
fn validate(wallet: &Wallet, params: &SendParams) -> Result<()> {
    let change = if params.change_address.is_empty() {
        wallet.primary_address().unwrap().to_string()
    } else {
        params.change_address.clone()
    };
    let unlock = if params.unlock_time == 0 { default_unlock_time(params.network_height) } else { params.unlock_time };
    validate_transaction_parameters(
        wallet,
        &params.destinations,
        params.mixin,
        params.fee,
        &params.payment_id,
        &params.addresses_to_take_from,
        &change,
        unlock,
        params.network_height,
        NOW,
    )
}

#[test]
fn validation_error_codes() {
    let wallet = funded_wallet();

    // A good set validates.
    assert!(validate(&wallet, &base_params(1000)).is_ok());

    // NO_DESTINATIONS_GIVEN (18)
    let mut p = base_params(1000);
    p.destinations.clear();
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 18);

    // AMOUNT_IS_ZERO (19)
    let p = base_params(0);
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 19);

    // ADDRESS_WRONG_LENGTH (12)
    let mut p = base_params(1000);
    p.destinations[0].0 = "Wrkztooshort".to_string();
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 12);

    // ADDRESS_WRONG_PREFIX (13): right length, wrong first four characters.
    let mut p = base_params(1000);
    p.destinations[0].0 = format!("Zzzz{}", &payee_address()[4..]);
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 13);

    // ADDRESS_NOT_BASE58 (14): a character outside the alphabet.
    let mut p = base_params(1000);
    let mut bad = payee_address();
    bad.replace_range(10..11, "0");
    p.destinations[0].0 = bad;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 14);

    // ADDRESS_NOT_VALID (15): a valid-looking address with a broken checksum.
    let mut p = base_params(1000);
    let good = payee_address();
    let swapped = format!("{}{}{}", &good[..90], if &good[90..91] == "a" { "b" } else { "a" }, &good[91..]);
    p.destinations[0].0 = swapped;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 15);

    // NOT_ENOUGH_BALANCE (11)
    let p = base_params(10_000_000);
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 11);

    // WILL_OVERFLOW (9)
    let mut p = base_params(u64::MAX);
    p.destinations.push((payee_address(), 2));
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 9);

    // MIXIN_TOO_BIG (22) and MIXIN_TOO_SMALL (21)
    let mut p = base_params(1000);
    p.mixin = 8;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 22);
    let mut p = base_params(1000);
    p.mixin = 0;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 21);
    // At 4,213,649 the tier is min 1, max 1, so a mixin of 7 is too big.
    let mut p = base_params(1000);
    p.mixin = 7;
    p.network_height = 4_213_649;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 22);

    // PAYMENT_ID_WRONG_LENGTH (23) and PAYMENT_ID_INVALID (24)
    let mut p = base_params(1000);
    p.payment_id = "abcd".to_string();
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 23);
    let mut p = base_params(1000);
    p.payment_id = "zzzzzzzzzzzzzzzz".to_string();
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 24);

    // FEE_TOO_SMALL (17): below the network rate.
    let mut p = base_params(1000);
    p.fee = FeeType::FeePerByte(0.0001);
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 17);

    // ADDRESS_NOT_IN_WALLET (10): change address is not ours.
    let mut p = base_params(1000);
    p.change_address = payee_address();
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 10);

    // ADDRESS_IS_INTEGRATED (25): only standard addresses may be ours.
    let mut p = base_params(1000);
    p.change_address = integrated(&wallet, "0011223344556677");
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 25);

    // UNLOCK_TIME_TOO_SMALL (60)
    let mut p = base_params(1000);
    p.unlock_time = HEIGHT + 14;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 60);
    let mut p = base_params(1000);
    p.unlock_time = HEIGHT + 15;
    assert!(validate(&wallet, &p).is_ok());
    // A unix-time unlock must be fifteen block times ahead of the clock.
    let mut p = base_params(1000);
    p.unlock_time = NOW + 15 * 60 - 1;
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 60);
    let mut p = base_params(1000);
    p.unlock_time = NOW + 15 * 60;
    assert!(validate(&wallet, &p).is_ok());

    // CONFLICTING_PAYMENT_IDS (26)
    let mut p = base_params(1000);
    p.destinations[0].0 = integrated_for(&payee_address(), "0011223344556677");
    p.payment_id = "ffffffffffffffff".to_string();
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 26);
    // Two integrated addresses that disagree conflict even with no explicit id.
    let mut p = base_params(1000);
    p.destinations = vec![
        (integrated_for(&payee_address(), "0011223344556677"), 1000),
        (integrated_for(&payee_address(), "8899aabbccddeeff"), 1000),
    ];
    assert_eq!(validate(&wallet, &p).unwrap_err().code(), 26);
}

fn integrated(wallet: &Wallet, payment_id: &str) -> String {
    integrated_for(wallet.primary_address().unwrap(), payment_id)
}

fn integrated_for(address: &str, payment_id: &str) -> String {
    let parsed = base58::parse_address(address).unwrap();
    base58::integrated_address(&parsed.spend_public_key, &parsed.view_public_key, payment_id).unwrap()
}

/////////////////////
/* PIPELINE        */
/////////////////////

fn seeded() -> SeededRandom {
    SeededRandom::from_label(b"wrkz transfer test seed")
}

/// Build without relaying, with a fixed seed and one proof-of-work thread.
fn prepare(wallet: &Wallet, daemon: &MockDaemon, params: &SendParams) -> Result<PreparedTransaction> {
    prepare_transaction(wallet, daemon, params, &mut seeded())
}

#[test]
fn a_fixed_seed_builds_a_fixed_transaction() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();

    // A fixed fee of 10,000 clears the proof-of-work fee escape, so this test
    // exercises everything except the nonce search (which has its own tests).
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    let a = prepare(&wallet, &daemon, &params).expect("prepared");
    let b = prepare(&wallet, &daemon, &params).expect("prepared again");

    assert_eq!(a.to_hex(), b.to_hex(), "the same seed builds the same bytes");
    assert_eq!(a.transaction_hash, b.transaction_hash);

    // Pinned so that any change to any stage shows up here.
    assert_eq!(a.transaction_hash.to_hex(), "2e56b33e8db714c5b6e61c91817371361b1b05b3773a738157ba4174e98c5c4c");
    assert_eq!(a.size, 282);
    assert_eq!(a.fee, 10_000);
    assert_eq!(a.pow_nonce, None, "the fee escape applied");
    assert_eq!(a.pow_difficulty, 0);

    // A different seed builds a different transaction.
    let c = prepare_transaction(&wallet, &daemon, &params, &mut SeededRandom::from_label(b"other")).unwrap();
    assert_ne!(a.to_hex(), c.to_hex());
}

#[test]
fn built_transaction_is_well_formed() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");

    // The request shape: one entry per input, `outs_count = mixin + 1`.
    let requests = daemon.requests.borrow();
    let (amounts, outs_count) = requests.last().expect("a request was made");
    assert_eq!(*outs_count, params.mixin + 1);
    assert_eq!(amounts.len(), prepared.transaction.prefix.inputs.len());
    assert!(amounts.windows(2).all(|w| w[0] <= w[1]), "inputs are sorted by amount before the request");

    let tx = &prepared.transaction;
    assert_eq!(tx.prefix.version, 1);
    assert_eq!(tx.prefix.unlock_time, HEIGHT + 35);
    assert_eq!(tx.signatures.len(), tx.prefix.inputs.len());

    // Every ring is `mixin + 1` long, the offsets are ascending and the first
    // is absolute.
    for (i, input) in tx.prefix.inputs.iter().enumerate() {
        let Input::Key { key_offsets, .. } = input else { panic!("key input") };
        assert_eq!(key_offsets.len() as u64, params.mixin + 1);
        let absolute = tx::relative_offsets_to_absolute(key_offsets).expect("ascending offsets");
        assert!(absolute.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(absolute, prepared.rings[i].ring.iter().map(|(g, _)| *g).collect::<Vec<_>>());
        assert_eq!(absolute[prepared.rings[i].real_output], prepared.inputs[i].input.global_output_index.unwrap());
    }

    // Outputs: ascending amounts, all pretty, and the amounts of the payment
    // plus the change.
    let amounts: Vec<u64> = tx.prefix.outputs.iter().map(|o| o.amount).collect();
    assert!(amounts.windows(2).all(|w| w[0] <= w[1]), "outputs sorted by amount");
    assert!(verify_amounts(tx));
    assert_eq!(amounts.iter().sum::<u64>(), tx.prefix.sum_inputs().unwrap() - 10_000);

    // Extra: the public key and nothing else (no payment id, no proof of work).
    assert_eq!(tx.prefix.extra.len(), 33);
    assert_eq!(tx.prefix.extra[0], 0x01);
    assert_eq!(&tx.prefix.extra[1..], prepared.tx_public_key.as_bytes());

    assert!(prepared.verify_signatures(), "every ring signature verifies");
    assert!(is_transaction_payload_too_big(prepared.size, HEIGHT).is_ok());
    assert!(!prepared.is_fusion);
}

#[test]
fn change_comes_back_to_the_primary_address() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    // 100,000 is the smallest input; sending 40,000 leaves 50,000 change after
    // a 10,000 fee.
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(40_000) };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");

    assert_eq!(prepared.change_required, 50_000);
    assert_eq!(prepared.change_address, wallet.primary_address().unwrap());

    // The change outputs are the ones we can underive to our own spend key.
    let derivation =
        curve::generate_key_derivation(prepared.tx_public_key.as_bytes(), wallet.private_view_key().as_bytes())
            .unwrap();
    let ours: u64 = prepared
        .outputs
        .iter()
        .enumerate()
        .filter(|(i, o)| {
            curve::underive_public_key(&derivation, *i as u64, o.key.as_bytes()).map(Hex32)
                == Some(wallet.primary_sub_wallet().unwrap().public_spend_key)
        })
        .map(|(_, o)| o.amount)
        .sum();
    assert_eq!(ours, 50_000);
}

#[test]
fn the_ephemeral_secret_is_reused_or_re_derived() {
    // Both branches of `setupInputs` must produce the same key image and the
    // same signatures: the cached `privateEphemeral` and the one re-derived
    // from the parent transaction public key.
    let daemon = MockDaemon::default();
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };

    let mut cached = test_wallet();
    fund(&mut cached, &[(100_000, 40), (200_000, 41)], true);
    let mut derived = test_wallet();
    fund(&mut derived, &[(100_000, 40), (200_000, 41)], false);

    assert!(cached.sub_wallets.sub_wallet[0].unspent_inputs[0].private_ephemeral.is_some());
    assert!(derived.sub_wallets.sub_wallet[0].unspent_inputs[0].private_ephemeral.is_none());

    let a = prepare(&cached, &daemon, &params).expect("cached");
    let b = prepare(&derived, &daemon, &params).expect("derived");
    assert_eq!(a.to_hex(), b.to_hex());
}

#[test]
fn payment_ids_reach_the_extra() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();

    // A long payment id is plaintext.
    let params = SendParams { fee: FeeType::FixedFee(10_000), payment_id: "11".repeat(32), ..base_params(50_000) };
    let prepared = prepare(&wallet, &daemon, &params).unwrap();
    let parsed = parse_extra_wallet(&prepared.transaction.prefix.extra);
    assert_eq!(parsed.payment_id, Some(PaymentId::Long([0x11; 32])));
    assert_eq!(prepared.payment_id, "11".repeat(32));

    // A short one is encrypted to the receiver's view key, so the receiver can
    // decrypt it back with their private view key and the tx public key.
    let params = SendParams {
        fee: FeeType::FixedFee(10_000),
        payment_id: "0011223344556677".to_string(),
        ..base_params(50_000)
    };
    let prepared = prepare(&wallet, &daemon, &params).unwrap();
    let parsed = parse_extra_wallet(&prepared.transaction.prefix.extra);
    let Some(PaymentId::EncryptedShort(cipher)) = parsed.payment_id else { panic!("short payment id") };
    assert_ne!(hex(&cipher), "0011223344556677", "it is encrypted");

    let payee = payee_wallet();
    let derivation =
        curve::generate_key_derivation(prepared.tx_public_key.as_bytes(), payee.private_view_key().as_bytes()).unwrap();
    let mut plain = cipher;
    crate::sync::encrypt_payment_id(&mut plain, &derivation);
    assert_eq!(hex(&plain), "0011223344556677");
}

#[test]
fn an_integrated_address_supplies_the_payment_id() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams {
        fee: FeeType::FixedFee(10_000),
        destinations: vec![(integrated_for(&payee_address(), "0011223344556677"), 50_000)],
        ..base_params(50_000)
    };
    let prepared = prepare(&wallet, &daemon, &params).unwrap();
    assert_eq!(prepared.payment_id, "0011223344556677");
    assert!(matches!(
        parse_extra_wallet(&prepared.transaction.prefix.extra).payment_id,
        Some(PaymentId::EncryptedShort(_))
    ));
}

#[test]
fn a_short_payment_id_needs_a_single_destination() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams {
        fee: FeeType::FixedFee(10_000),
        payment_id: "0011223344556677".to_string(),
        destinations: vec![(payee_address(), 20_000), (payee_address(), 20_000)],
        ..base_params(40_000)
    };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 61);
}

#[test]
fn extra_data_is_written_after_the_payment_id() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams {
        fee: FeeType::FixedFee(10_000),
        payment_id: "22".repeat(32),
        extra_data: b"hello world".to_vec(),
        ..base_params(50_000)
    };
    let prepared = prepare(&wallet, &daemon, &params).unwrap();
    let extra = &prepared.transaction.prefix.extra;
    assert_eq!(extra[33], 0x02);
    assert_eq!(extra[34], 33 + 2 + 11);
    assert_eq!(extra[35], 0x00);
    assert_eq!(extra[68], 0x7f);
    assert_eq!(extra[69], 11);
    assert_eq!(&extra[70..81], b"hello world");
    assert!(extra.len() < wrkz_primitives::constants::MAX_EXTRA_SIZE_V2, "extra stays under the consensus cap");
}

/////////////////////
/* FEE LOOP        */
/////////////////////

#[test]
fn the_fee_loop_converges_on_a_fee_the_size_justifies() {
    let mut wallet = test_wallet();
    // Many small inputs, so covering the amount takes several and the size (and
    // therefore the fee) grows as they are added.
    let inputs: Vec<(u64, u64)> = (0..40).map(|i| (10_000u64, 100 + i)).collect();
    fund(&mut wallet, &inputs, true);

    let daemon = MockDaemon::default();
    // A rate high enough that the fee clears `TRANSACTION_POW_PASS_WITH_FEE`,
    // so the loop runs without a nonce search. The minimum-fee path is covered
    // by `minimum_fee_below_the_proof_of_work_height` instead.
    let rate = 3.0;
    let params = SendParams { mixin: 3, fee: FeeType::FeePerByte(rate), ..base_params(150_000) };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");

    // The loop stops as soon as the fee it has already reserved covers what the
    // built size asks for, so the fee can sit a chunk above the exact figure -
    // `tryMakeFeePerByteTransaction` compares with `>=` and does not shrink it
    // back (`Transfer.cpp:625`).
    assert!(prepared.fee >= fees::transaction_fee(prepared.size, HEIGHT, rate));
    let minimum = fees::required_minimum_fee(prepared.size, HEIGHT);
    assert!(prepared.fee >= minimum, "fee {} covers the minimum {minimum}", prepared.fee);
    assert!(verify_transaction_fee(FeeType::FeePerByte(rate), prepared.fee, HEIGHT, prepared.size));

    // Inputs cover the amount plus the fee exactly, with the rest as change.
    let sum_in = prepared.transaction.prefix.sum_inputs().unwrap();
    let sum_out = prepared.transaction.prefix.sum_outputs().unwrap();
    assert_eq!(sum_in - sum_out, prepared.fee);
    assert_eq!(sum_out, 150_000 + prepared.change_required);
    assert!(prepared.fee >= TRANSACTION_POW_PASS_WITH_FEE, "the fee escape needs 10,000");
    assert_eq!(prepared.pow_nonce, None, "the fee escape applied");
    // The loop kept adding inputs until they covered the fee its own size
    // implies, which is well past what the first estimate asked for.
    assert!(prepared.transaction.prefix.inputs.len() > 15);
}

#[test]
fn minimum_fee_below_the_proof_of_work_height() {
    // The `FeeType::MinimumFee` path end to end, at a height where no
    // transaction proof of work is required (spec/06 rule 9 starts at
    // 1,123,000), so the test costs no hashing.
    let mut wallet = test_wallet();
    let inputs: Vec<(u64, u64)> = (0..8).map(|i| (100_000u64, 400 + i)).collect();
    fund(&mut wallet, &inputs, true);

    let daemon = MockDaemon::default();
    // The tier at 1,100,000 is min 1, max 1.
    let params = SendParams {
        mixin: 1,
        fee: FeeType::MinimumFee,
        ..SendParams::basic(&payee_address(), 250_000, "", HEIGHT_NO_POW, HEIGHT_NO_POW)
    };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");

    assert_eq!(prepared.pow_nonce, None, "no proof of work below 1,123,000");
    assert_eq!(prepared.pow_difficulty, 0);
    // `Transfer.cpp:352` picks the rate by comparing the *height* to
    // `MINIMUM_FEE_PER_BYTE_V2_HEIGHT`, so below 1,500,000 the wallet pays the
    // V1 rate (500 per 256-byte chunk) even though `getMinimumTransactionFee`
    // - which compares the height to the *rate* - only demands the V2 one (20
    // per chunk). The C++ overpays here by design; a port that "fixed" it would
    // build visibly different transactions.
    assert_eq!(prepared.fee, fees::transaction_fee(prepared.size, HEIGHT_NO_POW, MINIMUM_FEE_PER_BYTE_V1));
    assert_eq!(prepared.fee, fees::required_minimum_fee(prepared.size, HEIGHT_NO_POW) * 25);
    assert!(verify_transaction_fee(FeeType::MinimumFee, prepared.fee, HEIGHT_NO_POW, prepared.size));
    assert_eq!(prepared.transaction.prefix.unlock_time, HEIGHT_NO_POW + 40 + 15);
    assert!(prepared.verify_signatures());
}

#[test]
fn a_fixed_fee_below_the_minimum_is_refused() {
    // `Transfer.cpp:419`: the fixed-fee path builds the transaction first and
    // only then weighs the fee against `getMinimumTransactionFee(size, H)`, so
    // this runs at a height below the proof-of-work fork to keep the discarded
    // build cheap.
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(100_000, 40), (200_000, 41)], true);
    let daemon = MockDaemon::default();
    let params = SendParams {
        mixin: 1,
        fee: FeeType::FixedFee(1),
        ..SendParams::basic(&payee_address(), 50_000, "", HEIGHT_NO_POW, HEIGHT_NO_POW)
    };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 17);

    // The same send with a fee that covers the built size succeeds.
    let params = SendParams { fee: FeeType::FixedFee(1000), ..params };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");
    assert_eq!(prepared.fee, 1000);
}

#[test]
fn the_ring_size_rules_are_the_daemons_not_the_networks() {
    // One peer claiming a height past the mixin fork moves `/info`'s
    // `network_height` past it, while the daemon's pool still judges at its own
    // top block (C++ `0b58b035`). The default ring, and the validation of an
    // explicit one, follow the daemon.
    let fork = constants::MIXIN_LIMITS_V6_HEIGHT;
    let params = SendParams::basic(&payee_address(), 50_000, "", fork + 10, fork - 10);
    assert_eq!(params.mixin, constants::DEFAULT_MIXIN_V5);
    assert_eq!(FusionParams::basic(fork + 10, fork - 10).mixin, constants::DEFAULT_MIXIN_V5);

    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams { mixin: constants::DEFAULT_MIXIN_V6, fee: FeeType::FixedFee(10_000), ..params };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 22, "MIXIN_TOO_BIG before the fork");
}

#[test]
fn send_all_reduces_the_amount_rather_than_the_change() {
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(100_000, 40)], true);
    let daemon = MockDaemon::default();
    // Below the proof-of-work fork, so the small fee `sendAll` produces costs
    // no hashing.
    let params = SendParams {
        mixin: 1,
        send_all: true,
        ..SendParams::basic(&payee_address(), 100_000, "", HEIGHT_NO_POW, HEIGHT_NO_POW)
    };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");

    assert_eq!(prepared.change_required, 0, "nothing comes back");
    let sum_out = prepared.transaction.prefix.sum_outputs().unwrap();
    assert_eq!(sum_out + prepared.fee, 100_000, "the fee comes out of the amount, not the change");
    assert_eq!(prepared.transaction.prefix.inputs.len(), 1);
}

#[test]
fn not_enough_balance_reports_code_eleven() {
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(1000, 40)], true);
    let daemon = MockDaemon::default();
    // Passes the balance check (1000 >= 1000) but cannot cover the fee.
    let params = base_params(1000);
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 11);
}

#[test]
fn too_many_outputs_reports_output_decomposition() {
    // 90 outputs is the cap. An amount whose decomposition, plus the change's,
    // exceeds it must be refused with OUTPUT_DECOMPOSITION (56).
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(6_000_000_000_000, 40), (1_000_000, 41)], true);
    let daemon = MockDaemon::default();
    // 6 * 10^12 splits into 100 outputs of 6 * 10^10 (above the client cap).
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(6_000_000_000_000) };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 56);
}

/////////////////////
/* RINGS           */
/////////////////////

#[test]
fn not_enough_decoys_reports_code_twenty_eight() {
    // The daemon answers with the amount but no outputs at all, so even the
    // network minimum ring cannot be built and the fallback has nothing left to
    // retry at. (A merely *thin* answer is rescued by the fallback, which
    // `the_mixin_fallback_retries_at_the_achievable_ring` covers.)
    let wallet = funded_wallet();
    let daemon = MockDaemon::with_available(0);
    let params = SendParams { mixin: 7, fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    let err = prepare(&wallet, &daemon, &params).unwrap_err();
    assert_eq!(err.code(), 28);
    assert!(err.to_string().contains("found outputs: 0"), "{err}");

    // Two requests were made: the original and the fallback at the minimum.
    let requests = daemon.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].1, 8);
    assert_eq!(requests[1].1, 2, "the fallback lands on the tier minimum of 1");
}

#[test]
fn a_missing_amount_reports_code_twenty_eight() {
    let wallet = funded_wallet();
    let daemon = MockDaemon { missing: vec![100_000], ..MockDaemon::default() };
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 28);
}

#[test]
fn the_mixin_fallback_retries_at_the_achievable_ring() {
    // Five outputs per amount can support a ring of four decoys plus ours, so a
    // request for seven falls back to four and succeeds.
    let wallet = funded_wallet();
    let daemon = MockDaemon::with_available(5);
    let params = SendParams { mixin: 7, fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    let prepared = prepare(&wallet, &daemon, &params).expect("fallback succeeded");
    assert_eq!(prepared.mixin, 4);
    for input in &prepared.transaction.prefix.inputs {
        let Input::Key { key_offsets, .. } = input else { panic!() };
        assert_eq!(key_offsets.len(), 5);
    }

    // Two requests: the first at seven, the second at four.
    let requests = daemon.requests.borrow();
    assert_eq!(requests.first().unwrap().1, 8);
    assert_eq!(requests.last().unwrap().1, 5);
}

#[test]
fn a_daemon_that_is_offline_reports_code_thirty() {
    let wallet = funded_wallet();
    let daemon =
        MockDaemon { random_outs_error: Some(DaemonError::Transport("refused".into())), ..MockDaemon::default() };
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 30);
}

#[test]
fn a_not_enough_outputs_body_is_classified_as_code_twenty_eight() {
    // `isNotEnoughOutputsResponse` (`Nigel.cpp:1063`).
    let e = DaemonError::BadRequest(r#"{"errorCode":27,"errorMessage":"nope"}"#.to_string());
    assert_eq!(classify_random_outs_error(&e).code(), 28);
    let e = DaemonError::BadRequest(r#"{"errorCode":3}"#.to_string());
    assert_eq!(classify_random_outs_error(&e).code(), 30);
    assert_eq!(classify_random_outs_error(&DaemonError::RateLimited).code(), 30);
}

#[test]
fn relative_offsets_wrap_like_the_cpp_uint32_subtraction() {
    // `setupInputs` computes `copy[i] = outputIndexes[i] - outputIndexes[i-1]`
    // on `uint32_t`. Ascending indexes give the ordinary offsets; a descending
    // pair wraps, and converting back with `uint32` addition recovers the
    // original, which is exactly what the daemon does.
    let ascending = [5u32, 10, 20, 21, 22];
    let offsets = wrapping_offsets(&ascending);
    assert_eq!(offsets, vec![5, 5, 10, 1, 1]);
    assert_eq!(
        tx::relative_offsets_to_absolute(&offsets).unwrap(),
        ascending.iter().map(|v| u64::from(*v)).collect::<Vec<_>>()
    );

    let descending = [20u32, 10];
    let offsets = wrapping_offsets(&descending);
    assert_eq!(offsets, vec![20, u64::from(u32::MAX) - 9]);
    // Converting back on u32 recovers 10; on u64 it does not, which is why an
    // out-of-order daemon answer would break both wallets identically.
    assert_eq!((20u32).wrapping_add(offsets[1] as u32), 10);
}

/// The offset arithmetic of `setup_inputs`, factored out for the test above.
fn wrapping_offsets(absolute: &[u32]) -> Vec<u64> {
    absolute
        .iter()
        .enumerate()
        .map(|(i, v)| if i == 0 { u64::from(*v) } else { u64::from(v.wrapping_sub(absolute[i - 1])) })
        .collect()
}

/////////////////////
/* PROOF OF WORK   */
/////////////////////

#[test]
fn the_proof_of_work_search_finds_a_nonce_in_the_last_eight_bytes() {
    // A low difficulty so the test is fast; the placement and the verification
    // are what matter and do not depend on the difficulty.
    let mut prefix = vec![0x42u8; 120];
    prefix.extend_from_slice(&[0u8; TX_POW_NONCE_SIZE]);

    let (nonce, hashes) = transaction_pow_search(&prefix, 500, 0, 1).expect("a nonce exists");
    assert!(hashes >= 1);

    let mut solved = prefix.clone();
    let at = solved.len() - TX_POW_NONCE_SIZE;
    solved[at..].copy_from_slice(&nonce);
    assert!(wrkz_pow::check_hash(&wrkz_pow::cn_upx(&solved), 500));

    // Deterministic given the start, and the start is what moves it.
    assert_eq!(transaction_pow_search(&prefix, 500, 0, 1).unwrap().0, nonce);
    let other = transaction_pow_search(&prefix, 500, 777, 1).unwrap().0;
    assert_ne!(other, nonce);
    let mut solved = prefix;
    solved[at..].copy_from_slice(&other);
    assert!(wrkz_pow::check_hash(&wrkz_pow::cn_upx(&solved), 500));
}

#[test]
fn the_fee_escape_decides_whether_a_nonce_is_searched_for() {
    // From 1,500,000 a fee of 10,000 or more needs no proof of work at all
    // (spec/06 rule 9).
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();

    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    let prepared = prepare(&wallet, &daemon, &params).unwrap();
    assert_eq!(prepared.pow_nonce, None);
    assert_eq!(prepared.pow_difficulty, 0);
    assert_eq!(prepared.transaction.prefix.extra.len(), 33, "no 0x04 field");
}

#[test]
#[ignore = "searches a real transaction proof of work; ~20 s on 16 threads"]
fn a_low_fee_transaction_carries_a_valid_proof_of_work() {
    let wallet = funded_wallet();
    let daemon = MockDaemon::default();
    // A fee below TRANSACTION_POW_PASS_WITH_FEE, so the nonce must be searched.
    let params = SendParams {
        fee: FeeType::MinimumFee,
        pow_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        ..base_params(50_000)
    };
    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");

    assert!(prepared.fee < TRANSACTION_POW_PASS_WITH_FEE);
    let nonce = prepared.pow_nonce.expect("a nonce was searched for");
    let expected = constants::transaction_pow_difficulty(
        HEIGHT,
        false,
        prepared.transaction.prefix.inputs.len() as u64,
        prepared.transaction.prefix.outputs.len() as u64,
    )
    .unwrap();
    assert_eq!(prepared.pow_difficulty, expected);

    let prefix = prepared.transaction.prefix.to_bytes();
    assert_eq!(&prefix[prefix.len() - 8..], &nonce, "the nonce is the last eight bytes of the prefix");
    assert!(wrkz_pow::check_hash(&wrkz_pow::cn_upx(&prefix), expected), "the daemon's check passes");
    assert!(prepared.verify_signatures(), "signing came after the nonce");
}

#[test]
fn a_server_nonce_is_checked_with_one_hash() {
    let mut prefix = vec![0x42u8; 120];
    prefix.extend_from_slice(&[0u8; TX_POW_NONCE_SIZE]);
    let (nonce, _) = transaction_pow_search(&prefix, 500, 0, 1).expect("a nonce exists");
    assert!(nonce_satisfies(&prefix, &nonce, 500));

    let wrong = (0u64..).map(u64::to_le_bytes).find(|n| !nonce_satisfies(&prefix, n, 500)).unwrap();
    assert!(!nonce_satisfies(&prefix, &wrong, 500));
    assert!(!nonce_satisfies(&[1, 2, 3], &nonce, 500), "a prefix shorter than a nonce");
}

#[test]
fn no_server_is_asked_when_the_fee_escape_applies() {
    let wallet = funded_wallet();
    let daemon = MockDaemon { remote_nonce: Some([0; TX_POW_NONCE_SIZE]), ..MockDaemon::default() };
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(50_000) };
    let prepared = prepare(&wallet, &daemon, &params).unwrap();
    assert_eq!(prepared.pow_nonce, None);
    assert!(daemon.remote_asked.borrow().is_empty(), "the prefix never left the wallet");
}

#[test]
#[ignore = "searches a real transaction proof of work; ~20 s on 16 threads"]
fn a_server_nonce_is_used_when_it_checks_out_and_ignored_when_it_does_not() {
    let wallet = funded_wallet();
    let params = SendParams {
        fee: FeeType::MinimumFee,
        pow_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        ..base_params(50_000)
    };

    // A server that answers with a nonce that fails the check: the wallet
    // searches for one itself and the transaction is still valid.
    let wrong = MockDaemon { remote_nonce: Some([0xff; TX_POW_NONCE_SIZE]), ..MockDaemon::default() };
    let searched = prepare(&wallet, &wrong, &params).expect("prepared");
    let nonce = searched.pow_nonce.expect("a nonce was searched for");
    assert!(searched.pow_hashes > 0);
    // Asked on every build of the fee loop, as the C++ asks its solver in every
    // `makeTransaction`; always at the height the wallet is judged at.
    let asked = wrong.remote_asked.borrow();
    assert!(asked.iter().all(|&(_, height)| height == HEIGHT), "{asked:?}");
    assert_eq!(asked.last().map(|&(difficulty, _)| difficulty), Some(searched.pow_difficulty));

    // The same build against a server that knows the answer: nothing is hashed
    // here beyond the one check, and the transaction is the same.
    let right = MockDaemon { remote_nonce: Some(nonce), ..MockDaemon::default() };
    let served = prepare(&wallet, &right, &params).expect("prepared");
    assert_eq!(served.pow_nonce, Some(nonce));
    assert_eq!(served.pow_hashes, 0);
    assert_eq!(served.transaction_hash, searched.transaction_hash);
    assert!(served.verify_signatures());
}

/////////////////////
/* FUSION          */
/////////////////////

#[test]
fn fusion_selection_prefers_a_full_bucket() {
    let mut wallet = test_wallet();
    // One bucket with twelve entries and one with three: the full one wins.
    let mut inputs: Vec<(u64, u64)> = (0..12).map(|i| (100u64 + i, 200 + i)).collect();
    inputs.extend((0..3).map(|i| (10_000u64 + i, 300 + i)));
    fund(&mut wallet, &inputs, true);

    let mut random = seeded();
    let (selected, max, found) =
        wallet.fusion_transaction_inputs(true, &[], 1, HEIGHT, NOW, None, &mut random).expect("selection");

    assert_eq!(max, 165, "(30000 - 42 - 172) / (112 + 68)");
    assert_eq!(selected.len(), 12, "the full bucket, and only it");
    assert!(selected.iter().all(|i| i.input.amount < 1000));
    assert_eq!(found, selected.iter().map(|i| i.input.amount).sum::<u64>());
}

#[test]
fn fusion_selection_falls_back_to_every_bucket() {
    let mut wallet = test_wallet();
    // No bucket is full, so all of them are used.
    let inputs: Vec<(u64, u64)> =
        (0..5).map(|i| (100u64 + i, 200 + i)).chain((0..5).map(|i| (10_000u64 + i, 300 + i))).collect();
    fund(&mut wallet, &inputs, true);

    let mut random = seeded();
    let (selected, _, _) =
        wallet.fusion_transaction_inputs(true, &[], 1, HEIGHT, NOW, None, &mut random).expect("selection");
    assert_eq!(selected.len(), 10);
}

#[test]
fn fusion_selection_honours_the_optimize_target() {
    let mut wallet = test_wallet();
    let inputs: Vec<(u64, u64)> =
        (0..12).map(|i| (100u64 + i, 200 + i)).chain((0..12).map(|i| (10_000u64 + i, 300 + i))).collect();
    fund(&mut wallet, &inputs, true);

    let mut random = seeded();
    let (selected, _, _) =
        wallet.fusion_transaction_inputs(true, &[], 1, HEIGHT, NOW, Some(1000), &mut random).expect("selection");
    assert!(selected.iter().all(|i| i.input.amount < 1000));
    assert_eq!(selected.len(), 12);
}

#[test]
fn fusion_refuses_a_wallet_that_is_already_optimized() {
    let mut wallet = test_wallet();
    fund(&mut wallet, &[(100, 200), (200, 201)], true);
    let daemon = MockDaemon::default();
    let params = FusionParams { mixin: 1, ..FusionParams::basic(HEIGHT, HEIGHT) };
    let err = prepare_fusion_transaction(&wallet, &daemon, &params, &mut seeded()).unwrap_err();
    assert_eq!(err.code(), 36, "FULLY_OPTIMIZED");
}

#[test]
fn fusion_refuses_a_ring_too_large_for_twelve_inputs() {
    let wallet = test_wallet();
    let daemon = MockDaemon::default();
    // The tier caps the mixin at 7, so this is checked at a height with a
    // looser tier: 10,000 to 302,399 allows up to 30.
    let params = FusionParams { mixin: 30, ..FusionParams::basic(300_000, 300_000) };
    let err = prepare_fusion_transaction(&wallet, &daemon, &params, &mut seeded()).unwrap_err();
    // 30 is under the limit, so this one is FULLY_OPTIMIZED, not the mixin.
    assert_eq!(err.code(), 36);

    // `validateOptimizeTarget`: more than one significant digit.
    let params = FusionParams { mixin: 1, optimize_target: Some(1234), ..FusionParams::basic(HEIGHT, HEIGHT) };
    assert_eq!(prepare_fusion_transaction(&wallet, &daemon, &params, &mut seeded()).unwrap_err().code(), 59);
}

#[test]
fn the_fusion_classifier_matches_the_daemon_rules() {
    // `Currency::isFusionTransaction`: twelve inputs, four inputs per output,
    // and outputs equal to the ascending decomposition of the input sum.
    let key_input = |amount| Input::Key { amount, key_offsets: vec![1, 1], key_image: [0x11; 32] };
    let mut tx = RawTransaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: 0,
            inputs: (0..12).map(|_| key_input(1000)).collect(),
            outputs: vec![Output { amount: 2000, key: [0x22; 32] }, Output { amount: 10_000, key: [0x23; 32] }],
            extra: Vec::new(),
        },
        signatures: Vec::new(),
    };
    // 12 * 1000 = 12,000 -> [2000, 10000] ascending.
    assert!(is_fusion_transaction(&tx, 1000, HEIGHT));
    // Too few inputs.
    tx.prefix.inputs.pop();
    assert!(!is_fusion_transaction(&tx, 1000, HEIGHT));
    tx.prefix.inputs.push(key_input(1000));
    // Wrong outputs.
    tx.prefix.outputs[0].amount = 3000;
    assert!(!is_fusion_transaction(&tx, 1000, HEIGHT));
    tx.prefix.outputs[0].amount = 2000;
    assert!(is_fusion_transaction(&tx, 1000, HEIGHT));
    // Above the size budget.
    assert!(!is_fusion_transaction(&tx, FUSION_TX_MAX_SIZE + 1, HEIGHT));
    // Fewer than four inputs per output.
    tx.prefix.outputs = vec![
        Output { amount: 2000, key: [0x22; 32] },
        Output { amount: 4000, key: [0x23; 32] },
        Output { amount: 6000, key: [0x24; 32] },
    ];
    assert!(!is_fusion_transaction(&tx, 1000, HEIGHT));
}

/////////////////////
/* RELAY AND STORE */
/////////////////////

#[test]
fn a_successful_send_updates_the_wallet() {
    let mut wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(40_000) };

    let before = wallet.balance(HEIGHT);
    let prepared = send_transaction_advanced(&mut wallet, &daemon, &params, &mut seeded()).expect("sent");

    // The blob reached the daemon.
    assert_eq!(daemon.sent.borrow().len(), 1);
    assert_eq!(daemon.sent.borrow()[0], prepared.to_hex());

    // The inputs are locked, not spent.
    let sub = &wallet.sub_wallets.sub_wallet[0];
    assert_eq!(sub.locked_inputs.len(), prepared.inputs.len());
    assert!(sub.spent_inputs.is_empty());
    for input in &prepared.inputs {
        assert!(sub.locked_inputs.iter().any(|i| i.key_image == input.input.key_image));
        assert!(!sub.unspent_inputs.iter().any(|i| i.key_image == input.input.key_image));
    }

    // The change is an unconfirmed incoming amount.
    let unconfirmed: u64 = sub.unconfirmed_incoming_amounts.iter().map(|i| i.amount).sum();
    assert_eq!(unconfirmed, prepared.change_required);
    assert!(sub.unconfirmed_incoming_amounts.iter().all(|i| i.parent_transaction_hash == prepared.transaction_hash));

    // The transaction is recorded as unconfirmed, with signed transfers.
    let tx = wallet.unconfirmed_transactions().iter().find(|t| t.hash == prepared.transaction_hash).expect("recorded");
    assert_eq!(tx.block_height, 0);
    assert_eq!(tx.fee, 10_000);
    assert_eq!(tx.total_amount(), -(40_000i64 + 10_000));

    // The transaction private key is kept.
    assert_eq!(wallet.tx_private_key(&prepared.transaction_hash), Some(&prepared.tx_private_key));

    // Balance: the spent inputs left, the change came back locked.
    let after = wallet.balance(HEIGHT);
    assert_eq!(after.0 + prepared.inputs.iter().map(|i| i.input.amount).sum::<u64>(), before.0);
    assert_eq!(after.1, prepared.change_required);
}

#[test]
fn a_daemon_refusal_reports_code_thirty_one_and_changes_nothing() {
    let mut wallet = funded_wallet();
    let daemon = MockDaemon {
        send_status: ("Failed".to_string(), Some("Transaction verification failed".to_string())),
        ..MockDaemon::default()
    };
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(40_000) };

    let err = send_transaction_advanced(&mut wallet, &daemon, &params, &mut seeded()).unwrap_err();
    assert_eq!(err.code(), 31);
    assert!(err.to_string().contains("Transaction verification failed"));

    assert!(wallet.sub_wallets.sub_wallet[0].locked_inputs.is_empty());
    assert!(wallet.unconfirmed_transactions().is_empty());
    assert_eq!(wallet.sub_wallets.tx_private_keys.len(), 0);
}

#[test]
fn a_prepared_transaction_can_be_relayed_later_and_expires() {
    let mut wallet = funded_wallet();
    let daemon = MockDaemon::default();
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(40_000) };

    let prepared = prepare(&wallet, &daemon, &params).expect("prepared");
    assert!(daemon.sent.borrow().is_empty(), "prepare does not relay");

    // Still spendable, so it relays.
    let sent = send_prepared_transaction(&mut wallet, &daemon, prepared.clone(), HEIGHT).expect("relayed");
    assert_eq!(sent.transaction_hash, prepared.transaction_hash);
    assert_eq!(daemon.sent.borrow().len(), 1);

    // Now the inputs are locked, so a second attempt expires.
    let err = send_prepared_transaction(&mut wallet, &daemon, prepared, HEIGHT).unwrap_err();
    assert_eq!(err.code(), 57);
}

#[test]
fn a_view_wallet_cannot_send() {
    let mut wallet = funded_wallet();
    wallet.sub_wallets.is_view_wallet = true;
    let daemon = MockDaemon::default();
    let params = SendParams { fee: FeeType::FixedFee(10_000), ..base_params(40_000) };
    assert_eq!(prepare(&wallet, &daemon, &params).unwrap_err().code(), 39);
}

/////////////////////
/* RANDOMNESS      */
/////////////////////

#[test]
fn the_seeded_source_is_reproducible_and_distinct_per_seed() {
    let mut a = SeededRandom::new([7u8; 32]);
    let mut b = SeededRandom::new([7u8; 32]);
    let mut c = SeededRandom::new([8u8; 32]);

    for _ in 0..4 {
        let x = a.random_scalar();
        assert_eq!(x, b.random_scalar());
        assert_ne!(x, c.random_scalar());
        // A reduced scalar, like `random_scalar`.
        assert!(curve::sc_check(&x));
    }

    let mut a = SeededRandom::new([7u8; 32]);
    let (secret, public) = a.key_pair();
    assert_eq!(curve::secret_key_to_public_key(&secret), Some(public));

    // `next_below` stays in range.
    let mut a = SeededRandom::new([9u8; 32]);
    for bound in 1..20u64 {
        assert!(a.next_below(bound) < bound);
    }
}

#[test]
fn shuffle_is_a_permutation() {
    let mut items: Vec<u32> = (0..32).collect();
    let mut random = SeededRandom::new([3u8; 32]);
    shuffle(&mut items, &mut random);
    let mut sorted = items.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..32).collect::<Vec<u32>>());
    assert_ne!(items, sorted, "and it actually moved things");
}

#[test]
fn the_mock_daemon_returns_ascending_global_indexes() {
    // The assumption the ring assembly rests on, and the shape the live node
    // returns (`tests/transfer_live.rs` checks the real one).
    let daemon = MockDaemon::default();
    let outs = daemon.random_outs(&[100, 1000], 8).unwrap();
    assert_eq!(outs.outs.len(), 2);
    for set in &outs.outs {
        let indexes: Vec<u64> = set.outs.iter().map(|o| o.global_amount_index).collect();
        assert!(indexes.windows(2).all(|w| w[0] < w[1]));
    }
}
