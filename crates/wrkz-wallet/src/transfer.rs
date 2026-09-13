// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Transaction construction: input selection, denominations, rings, extra,
//! transaction proof of work, ring signatures, the fee loop and relaying
//! (spec/10-wallet.md "Transaction construction", spec/06-transactions.md).
//!
//! Mirrors, function by function:
//!
//! | C++ | here |
//! | --- | --- |
//! | `SendTransaction::sendTransactionBasic` (`Transfer.cpp:39`) | [`send_transaction_basic`] |
//! | `SendTransaction::sendTransactionAdvanced` (line 92, the mixin fallback) | [`send_transaction_advanced`] |
//! | `sendTransactionAdvancedWithMixin` (line 182) | `send_transaction_advanced_with_mixin` |
//! | `SendTransaction::sendPreparedTransaction` (line 519) | [`send_prepared_transaction`] |
//! | `SendTransaction::tryMakeFeePerByteTransaction` (line 566) | `try_make_fee_per_byte_transaction` |
//! | `SendTransaction::makeTransaction` (line 1386) | `make_transaction` |
//! | `SendTransaction::setupDestinations` (line 786) | [`setup_destinations`] |
//! | `SendTransaction::splitAmountIntoDenominations` (line 1310) | [`split_amount_into_denominations`] |
//! | `SendTransaction::getRingParticipants` (line 823) | `get_ring_participants` |
//! | `SendTransaction::prepareRingParticipants` (line 973) | `prepare_ring_participants` |
//! | `SendTransaction::setupInputs` (line 1073) | `setup_inputs` |
//! | `SendTransaction::setupOutputs` (line 1167) | `setup_outputs` |
//! | `SendTransaction::generateRingSignatures` (line 1216) | `generate_ring_signatures` |
//! | `SendTransaction::isTransactionPayloadTooBig` (line 673) | [`is_transaction_payload_too_big`] |
//! | `SendTransaction::verifyAmounts` (line 1620) | [`verify_amounts`] |
//! | `SendTransaction::sumTransactionFee` (line 1650) | [`sum_transaction_fee`] |
//! | `SendTransaction::verifyTransactionFee` (line 1670) | [`verify_transaction_fee`] |
//! | `SendTransaction::relayTransaction` (line 764) | `relay_transaction` |
//! | `SendTransaction::storeSentTransaction` (line 723) | [`Wallet::store_sent_transaction`] |
//! | `SendTransaction::storeUnconfirmedIncomingInputs` (line 690) | [`Wallet::store_unconfirmed_incoming_inputs`] |
//! | `CryptoNote::generateTransactionPoWHeight` (`TransactionPoW.cpp:85`) | [`transaction_pow_search`] |
//! | `validateTransaction` (`ValidateParameters.cpp:60`) | [`validate_transaction_parameters`] |
//! | `validateFusionTransaction` (`ValidateParameters.cpp:27`) | `validate_fusion_parameters` |
//! | `SubWallets::getSpendableTransactionInputs` (`SubWallets.cpp:530`) | [`Wallet::spendable_transaction_inputs`] |
//! | `SubWallets::getFusionTransactionInputs` (`SubWallets.cpp:615`) | [`Wallet::fusion_transaction_inputs`] |
//! | `Utilities::estimateTransactionSize` (`Utilities.cpp:346`) | [`estimate_transaction_size`] |
//! | `Utilities::getApproximateMaximumInputCount` (`Utilities.cpp:424`) | [`approximate_maximum_input_count`] |
//! | `Utilities::nextFallbackMixin` (`Mixins.cpp:71`) | [`next_fallback_mixin`] |
//!
//! # Determinism
//!
//! Everything random in the C++ pipeline arrives here through
//! [`TransferRandom`]: the transaction key pair, the signer nonce and decoy
//! scalars of every ring signature, the starting value of the proof-of-work
//! nonce search, and the shuffles of fusion input selection. With
//! [`SeededRandom`] and one proof-of-work thread, one seed gives one
//! transaction, byte for byte, which is what makes the transaction testable at
//! all. [`SystemRandom`] is the production implementation.
//!
//! Decoy *selection* needs no randomness: the C++ takes the daemon's outputs in
//! the order they arrived, skipping its own (`Transfer.cpp:1030`).
//!
//! # Two C++ quirks reproduced here
//!
//! 1. **The ring sort is a no-op.** `getRingParticipants` sorts each amount's
//!    outputs by global index inside `for (auto fakeOut : fakeOuts)` — by
//!    value, so the sort applies to a loop copy and the returned vector keeps
//!    the daemon's order (`Transfer.cpp:961-968`). The ring is nonetheless
//!    ascending in practice because `/getrandom_outs` returns ascending global
//!    indexes; this port takes the same order rather than sorting, so both
//!    wallets emit the same bytes. Relative offsets are therefore computed with
//!    **wrapping `u32`** subtraction, exactly as `setupInputs` does
//!    (`Transfer.cpp:1141`), so an out-of-order daemon answer produces the same
//!    (broken) transaction here as there instead of a different one.
//! 2. **`generateTransactionPoWHeight` has no fusion branch**
//!    (`TransactionPoW.h:79`): it always uses the non-fusion difficulty. The
//!    deployed C++ wallet has no fusion send, so nothing exercises it; the
//!    fusion path here uses the difficulty the *daemon* demands
//!    (`FUSION_TRANSACTION_POW_DIFFICULTY_V2`, spec/06 rule 9), because a
//!    transaction built with the other one would simply be rejected.

use std::collections::BTreeMap;

use wrkz_pow::curve;
use wrkz_primitives::constants::{
    self, FUSION_TX_MAX_SIZE, FUSION_TX_MIN_INPUT_COUNT, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, INTEGRATED_ADDRESS_LENGTH,
    INTEGRATED_ADDRESS_LENGTH_LONG, LONG_PAYMENT_ID_LENGTH, MAX_OUTPUT_SIZE_CLIENT, MINIMUM_FEE_PER_BYTE_V1,
    MINIMUM_FEE_PER_BYTE_V2, MINIMUM_FEE_PER_BYTE_V2_HEIGHT, MINIMUM_FEE_V1, MINIMUM_FEE_V1_HEIGHT,
    MINIMUM_UNLOCK_TIME_BLOCKS, NORMAL_TX_MAX_OUTPUT_COUNT_V1, SHORT_PAYMENT_ID_LENGTH, STANDARD_ADDRESS_LENGTH,
    TRANSACTION_POW_PASS_WITH_FEE, TRANSACTION_POW_PASS_WITH_FEE_HEIGHT, TX_POW_NONCE_SIZE, UNLOCK_TIME_HEIGHT_V2,
    UNLOCK_TIME_TRANSACTION_POOL_WINDOW, UNLOCK_TIME_TRANSACTION_POOL_WINDOW_V2,
};
use wrkz_primitives::tx::{
    self, Input, Output, PaymentId, Transaction as RawTransaction, TransactionPrefix, TX_EXTRA_NONCE,
    TX_EXTRA_TAG_PUBKEY, TX_EXTRA_TRANSACTION_POW_NONCE,
};
use wrkz_primitives::{base58, fees, mixins, varint};

#[cfg(feature = "native")]
use crate::daemon::Daemon;
use crate::daemon::{self, DaemonError, RandomOuts, SendResult};
use crate::file::{
    Hash, Hex32, KeyImage, PublicKey, Result, SecretKey, Transaction, TransactionInput, Transfer, UnconfirmedInput,
    Wallet, WalletError,
};
use crate::sync::{encrypt_payment_id_hex, is_input_unlocked_at};

////////////////////
/* RANDOMNESS     */
////////////////////

/// The signer nonce `k` and the `(c, r)` pair of every ring member, which is
/// all the randomness one ring signature consumes.
pub type RingRandomness = ([u8; 32], Vec<([u8; 32], [u8; 32])>);

/// Every random value the C++ pipeline draws, in one place.
///
/// The C++ calls `Crypto::random_scalar()` (a 64-byte CSPRNG read reduced mod
/// `l`) for the transaction secret key and for every ring-signature scalar, and
/// `std::thread` indices for the proof-of-work nonce start. This trait is the
/// seam a test replaces to get a reproducible transaction.
pub trait TransferRandom {
    /// `Crypto::random_scalar()` (`crypto.cpp:110`).
    fn random_scalar(&mut self) -> [u8; 32];

    /// Where the proof-of-work nonce search starts. The C++ starts thread `i`
    /// at `i` and steps by the thread count (`TransactionPoW.cpp:150`), so zero
    /// is the faithful single-threaded value; a wallet that wants distinct
    /// search paths across sends may return anything.
    fn pow_nonce_start(&mut self) -> u64;

    /// A uniform value in `0..bound` (`bound > 0`), used by the Fisher-Yates
    /// shuffles of fusion input selection (`std::shuffle`, `SubWallets.cpp:652`).
    fn next_below(&mut self, bound: u64) -> u64;

    /// `Crypto::generate_keys()`: a random scalar and its public key.
    ///
    /// `secret_key_to_public_key` cannot fail for a reduced scalar, but the
    /// loop is here rather than an `expect` so a broken implementation of
    /// [`TransferRandom::random_scalar`] cannot panic the wallet.
    fn key_pair(&mut self) -> ([u8; 32], [u8; 32]) {
        loop {
            let secret = self.random_scalar();
            if let Some(public) = curve::secret_key_to_public_key(&secret) {
                return (secret, public);
            }
        }
    }

    /// The randomness one ring signature needs: the signer nonce `k` and a
    /// `(c, r)` pair per ring member (`generateRingSignatures`, `crypto.cpp:560`).
    fn ring_randomness(&mut self, ring_size: usize) -> RingRandomness {
        let k = self.random_scalar();
        let decoys = (0..ring_size).map(|_| (self.random_scalar(), self.random_scalar())).collect();
        (k, decoys)
    }
}

/// The production source: the operating system CSPRNG, like `Crypto::rand`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRandom;

impl TransferRandom for SystemRandom {
    fn random_scalar(&mut self) -> [u8; 32] {
        curve::random_scalar()
    }

    fn pow_nonce_start(&mut self) -> u64 {
        0
    }

    fn next_below(&mut self, bound: u64) -> u64 {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).expect("system randomness");
        u64::from_le_bytes(buf) % bound.max(1)
    }
}

/// A reproducible source: keccak in counter mode over a 32-byte seed.
///
/// Not a replacement for the system CSPRNG in production — it exists so that
/// "the same seed builds the same transaction" is a testable statement, which
/// is how every stage of this module is pinned.
#[derive(Clone, Debug)]
pub struct SeededRandom {
    seed: [u8; 32],
    counter: u64,
}

impl SeededRandom {
    /// A stream from a 32-byte seed.
    pub fn new(seed: [u8; 32]) -> Self {
        Self { seed, counter: 0 }
    }

    /// A stream from any label, hashed into the seed.
    pub fn from_label(label: &[u8]) -> Self {
        Self::new(wrkz_pow::cn_fast_hash(label))
    }

    fn block(&mut self) -> [u8; 32] {
        let mut buf = [0u8; 40];
        buf[..32].copy_from_slice(&self.seed);
        buf[32..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter = self.counter.wrapping_add(1);
        wrkz_pow::cn_fast_hash(&buf)
    }
}

impl TransferRandom for SeededRandom {
    fn random_scalar(&mut self) -> [u8; 32] {
        // `random_scalar` reduces 64 random bytes, so this does too: a value
        // drawn here has the same distribution as one drawn there.
        let mut wide = [0u8; 64];
        wide[..32].copy_from_slice(&self.block());
        wide[32..].copy_from_slice(&self.block());
        curve::scalar_from_64_bytes(&wide)
    }

    fn pow_nonce_start(&mut self) -> u64 {
        u64::from_le_bytes(self.block()[..8].try_into().expect("8 bytes"))
    }

    fn next_below(&mut self, bound: u64) -> u64 {
        u64::from_le_bytes(self.block()[..8].try_into().expect("8 bytes")) % bound.max(1)
    }
}

////////////////////
/* DAEMON         */
////////////////////

/// The two daemon calls transaction construction makes (`Nigel`).
///
/// An implementation must not retry: the mixin fallback of
/// [`send_transaction_advanced`] is the only retry policy there is.
pub trait TransferDaemon {
    /// `Nigel::getRandomOutsByAmounts` — `POST /getrandom_outs` with
    /// `outs_count = mixin + 1`.
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> daemon::Result<RandomOuts>;

    /// `Nigel::sendTransaction` — `POST /sendrawtransaction`.
    fn send_raw_transaction(&self, tx_hex: &str) -> daemon::Result<SendResult>;

    /// An external transaction proof-of-work server (the C++ `TxPowClient`;
    /// [`crate::txpow::TxPowServer`] here): the nonce for `prefix`, or `None`
    /// to search on this machine. The default asks nobody. A nonce that comes
    /// back is checked with one hash before it is used and ignored if it
    /// fails, as `generateTransactionPoWHeight` does.
    fn remote_pow(&self, prefix: &[u8], difficulty: u64, height: u64) -> Option<[u8; TX_POW_NONCE_SIZE]> {
        let _ = (prefix, difficulty, height);
        None
    }
}

#[cfg(feature = "native")]
impl TransferDaemon for Daemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> daemon::Result<RandomOuts> {
        Daemon::random_outs(self, amounts, outs_count)
    }

    fn send_raw_transaction(&self, tx_hex: &str) -> daemon::Result<SendResult> {
        Daemon::send_raw_transaction(self, tx_hex)
    }
}

/// `isNotEnoughOutputsResponse` (`Nigel.cpp:1063`): a non-200 answer whose body
/// carries `errorCode == CANT_GET_FAKE_OUTPUTS` (27) means the chain does not
/// hold enough outputs, not that the daemon is unreachable.
fn classify_random_outs_error(e: &DaemonError) -> WalletError {
    let body = match e {
        DaemonError::BadRequest(b) | DaemonError::Http(_, b) => b.as_str(),
        DaemonError::Status(s) => s.as_str(),
        _ => return WalletError::DaemonOffline(e.to_string()),
    };
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) if v.get("errorCode").and_then(serde_json::Value::as_u64) == Some(27) => {
            let msg = v
                .get("errorMessage")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("The daemon does not have enough outputs to mix with")
                .to_string();
            WalletError::NotEnoughFakeOutputs(msg)
        }
        _ => WalletError::DaemonOffline(e.to_string()),
    }
}

////////////////////
/* PARAMETERS     */
////////////////////

/// `WalletTypes::FeeType` (`WalletTypes.h:520`): the three ways to ask for a fee.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FeeType {
    /// `FeeType::MinimumFee()`: whatever the network minimum is for the built
    /// size at the current height.
    MinimumFee,
    /// `FeeType::FeePerByte(rate)`: atomic units per byte. Must be at least the
    /// network minimum rate or the send fails with `FEE_TOO_SMALL`.
    FeePerByte(f64),
    /// `FeeType::FixedFee(fee)`: exactly this many atomic units.
    FixedFee(u64),
}

impl FeeType {
    fn is_fixed(self) -> bool {
        matches!(self, FeeType::FixedFee(_))
    }

    fn fixed_fee(self) -> u64 {
        match self {
            FeeType::FixedFee(f) => f,
            _ => 0,
        }
    }

    /// The rate the fee loop weighs the built transaction against: the caller's
    /// when given, otherwise the network minimum for the height
    /// (`Transfer.cpp:352-366`; note this branch compares the *height* to
    /// `MINIMUM_FEE_PER_BYTE_V2_HEIGHT`, unlike `getMinimumTransactionFee`,
    /// which compares it to the rate — both land on V2 at every live height).
    fn rate(self, network_height: u64) -> f64 {
        match self {
            FeeType::FeePerByte(r) => r,
            _ if network_height > MINIMUM_FEE_PER_BYTE_V2_HEIGHT => MINIMUM_FEE_PER_BYTE_V2,
            _ => MINIMUM_FEE_PER_BYTE_V1,
        }
    }
}

/// Everything `sendTransactionAdvanced` takes.
#[derive(Clone, Debug)]
pub struct SendParams {
    /// `(address, amount)` pairs. Integrated addresses are allowed and are
    /// split into address + payment id.
    pub destinations: Vec<(String, u64)>,
    /// Ring size minus one. Must be inside the tier at `network_height`.
    pub mixin: u64,
    pub fee: FeeType,
    /// 16 or 64 hex characters, or empty.
    pub payment_id: String,
    /// Addresses of this container to spend from; empty means all of them.
    pub addresses_to_take_from: Vec<String>,
    /// Where change goes; empty means the primary address.
    pub change_address: String,
    /// `0` means `networkHeight + 20 (or 40) + 15` (`Transfer.cpp:196`).
    pub unlock_time: u64,
    /// Arbitrary bytes written into the `0x7f` sub-field of the extra nonce.
    pub extra_data: Vec<u8>,
    /// Reduce the destination amount by the fee rather than the change.
    pub send_all: bool,
    /// The height every height-dependent rule is judged at:
    /// `daemon->networkBlockCount()`.
    pub network_height: u64,
    /// Threads the proof-of-work search may use. `1` keeps the search — and so
    /// the whole transaction — deterministic for a given seed.
    pub pow_threads: usize,
}

impl SendParams {
    /// The defaults of `sendTransactionBasic` (`Transfer.cpp:39`) for one
    /// destination: the tier default mixin at `network_height`, the minimum
    /// fee, change to the primary address, and the derived unlock time.
    pub fn basic(destination: &str, amount: u64, payment_id: &str, network_height: u64) -> SendParams {
        SendParams {
            destinations: vec![(destination.to_string(), amount)],
            mixin: mixins::mixin_allowable_range(network_height).default,
            fee: FeeType::MinimumFee,
            payment_id: payment_id.to_string(),
            addresses_to_take_from: Vec::new(),
            change_address: String::new(),
            unlock_time: 0,
            extra_data: Vec::new(),
            send_all: false,
            network_height,
            pow_threads: 1,
        }
    }
}

/// Everything the fusion path takes.
#[derive(Clone, Debug)]
pub struct FusionParams {
    pub mixin: u64,
    /// Addresses of this container to take inputs from; empty means all.
    pub addresses_to_take_from: Vec<String>,
    /// Where the fused outputs go. Empty means the primary address.
    pub destination_address: String,
    /// Do not consume inputs at or above this amount, and do not build outputs
    /// larger than it. Must have a single significant digit (`AMOUNT_UGLY`).
    pub optimize_target: Option<u64>,
    pub network_height: u64,
    pub pow_threads: usize,
}

impl FusionParams {
    /// `sendFusionTransactionBasic`: the tier default mixin, every subwallet,
    /// the primary address, no optimize target.
    pub fn basic(network_height: u64) -> FusionParams {
        FusionParams {
            mixin: mixins::mixin_allowable_range(network_height).default,
            addresses_to_take_from: Vec::new(),
            destination_address: String::new(),
            optimize_target: None,
            network_height,
            pow_threads: 1,
        }
    }
}

////////////////////
/* RESULTS        */
////////////////////

/// An input of ours with the keys needed to spend it
/// (`WalletTypes::TxInputAndOwner`, `WalletTypes.h:243`).
#[derive(Clone, Debug)]
pub struct OwnedSpendableInput {
    pub public_spend_key: PublicKey,
    pub private_spend_key: SecretKey,
    pub input: TransactionInput,
}

/// One output the transaction creates (`WalletTypes::KeyOutput`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyOutput {
    pub amount: u64,
    pub key: PublicKey,
}

/// A destination after integrated addresses are resolved and amounts split
/// (`WalletTypes::TransactionDestination`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionDestination {
    pub amount: u64,
    pub receiver_public_spend_key: PublicKey,
    pub receiver_public_view_key: PublicKey,
}

/// One assembled ring: the real output at its position among the decoys
/// (`WalletTypes::ObscuredInput`).
#[derive(Clone, Debug)]
pub struct ObscuredInput {
    pub amount: u64,
    pub key_image: KeyImage,
    /// `(global index, one-time public key)` in the order they go on the wire.
    pub ring: Vec<(u64, PublicKey)>,
    /// Index of our own output inside [`ObscuredInput::ring`].
    pub real_output: usize,
    pub private_ephemeral: SecretKey,
    pub owner_public_spend_key: PublicKey,
}

/// A built transaction and everything the wallet needs to relay it and record
/// it afterwards (`WalletTypes::PreparedTransactionInfo`, `WalletTypes.h:560`).
#[derive(Clone, Debug)]
pub struct PreparedTransaction {
    pub transaction: RawTransaction,
    /// `getTransactionHash(tx)`: keccak over the whole serialized transaction.
    pub transaction_hash: Hash,
    /// Serialized size in bytes; what the fee and the size limit are judged on.
    pub size: usize,
    pub fee: u64,
    pub mixin: u64,
    pub payment_id: String,
    pub change_address: String,
    pub change_required: u64,
    pub inputs: Vec<OwnedSpendableInput>,
    pub outputs: Vec<KeyOutput>,
    pub tx_private_key: SecretKey,
    pub tx_public_key: PublicKey,
    /// The rings, kept so a caller can re-verify the signatures.
    pub rings: Vec<ObscuredInput>,
    /// `None` when the fee escape of spec/06 rule 9 applied and no nonce was
    /// searched for.
    pub pow_nonce: Option<[u8; 8]>,
    /// The difficulty the nonce satisfies, `0` when no proof of work was needed.
    pub pow_difficulty: u64,
    /// How many hashes the search took, for the operator's benefit.
    pub pow_hashes: u64,
    /// Whether the finished transaction satisfies `Currency::isFusionTransaction`.
    pub is_fusion: bool,
}

impl PreparedTransaction {
    /// The bytes `/sendrawtransaction` takes.
    pub fn to_hex(&self) -> String {
        let blob = self.transaction.to_bytes().unwrap_or_default();
        blob.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Re-verify every ring signature against the final prefix hash, the check
    /// the daemon will run (`checkRingSignature`, spec/06 rule 10).
    pub fn verify_signatures(&self) -> bool {
        let prefix_hash = self.transaction.prefix.hash();
        self.rings.len() == self.transaction.signatures.len()
            && self.rings.iter().enumerate().all(|(i, ring)| {
                let keys: Vec<[u8; 32]> = ring.ring.iter().map(|(_, k)| *k.as_bytes()).collect();
                curve::check_ring_signature(
                    &prefix_hash,
                    ring.key_image.as_bytes(),
                    &keys,
                    &self.transaction.signatures[i],
                )
            })
    }
}

/// A failure of the inner pipeline, carrying what the mixin fallback needs.
struct Failure {
    error: WalletError,
    /// `TransactionResult::achievableMixin`: the largest ring the chain can
    /// actually build for these denominations, `0` when nothing was learned.
    achievable_mixin: u64,
}

impl From<WalletError> for Failure {
    fn from(error: WalletError) -> Self {
        Failure { error, achievable_mixin: 0 }
    }
}

type Inner<T> = std::result::Result<T, Failure>;

////////////////////
/* SIZE AND FEE   */
////////////////////

const KEY_IMAGE_SIZE: usize = 32;
const OUTPUT_KEY_SIZE: usize = 32;
/// `sizeof(uint64_t) + 2`, the C++ allowance for a varint amount.
const AMOUNT_SIZE: usize = 10;
const GLOBAL_INDEXES_VECTOR_SIZE_SIZE: usize = 1;
const GLOBAL_INDEXES_INITIAL_VALUE_SIZE: usize = 4;
const GLOBAL_INDEXES_DIFFERENCE_SIZE: usize = 4;
const SIGNATURE_SIZE: usize = 64;
const EXTRA_TAG_SIZE: usize = 1;
const INPUT_TAG_SIZE: usize = 1;
const OUTPUT_TAG_SIZE: usize = 1;
const PUBLIC_KEY_SIZE: usize = 32;
const TRANSACTION_VERSION_SIZE: usize = 1;
/// `sizeof(uint64_t) + 2` in `estimateTransactionSize`, and plain
/// `sizeof(uint64_t)` in `getApproximateMaximumInputCount` — the two C++
/// functions disagree, and both are reproduced.
const TRANSACTION_UNLOCK_TIME_SIZE: usize = 10;
/// `35` bytes: the nonce tag, its length, the sub-tag and 32 bytes.
const PAYMENT_ID_ALLOWANCE: usize = 35;

/// `Utilities::estimateTransactionSize(mixins, numOutputs, havePaymentID, extraDataSize)`
/// (`Utilities.cpp:356`): an upper-bound guess used to pick the first fee.
///
/// One entry of `mixins` per input. The estimate deliberately sits above the
/// truth — the real relative offsets are smaller than the four bytes charged
/// per ring member — because the fee loop pays for an extra rebuild when it
/// guesses low and nothing when it guesses high.
pub fn estimate_transaction_size(
    mixins: &[u64],
    num_outputs: usize,
    have_payment_id: bool,
    extra_data: usize,
) -> usize {
    let extra_data_size = if extra_data > 0 { extra_data + 4 } else { 0 };
    let payment_id_size = if have_payment_id { PAYMENT_ID_ALLOWANCE } else { 0 };

    let header_size = TRANSACTION_VERSION_SIZE
        + TRANSACTION_UNLOCK_TIME_SIZE
        + EXTRA_TAG_SIZE
        + extra_data_size
        + PUBLIC_KEY_SIZE
        + payment_id_size;

    let input_size = INPUT_TAG_SIZE
        + AMOUNT_SIZE
        + KEY_IMAGE_SIZE
        + SIGNATURE_SIZE
        + GLOBAL_INDEXES_VECTOR_SIZE_SIZE
        + GLOBAL_INDEXES_INITIAL_VALUE_SIZE;

    let inputs_size: usize =
        mixins.iter().map(|m| input_size + (*m as usize) * (GLOBAL_INDEXES_DIFFERENCE_SIZE + SIGNATURE_SIZE)).sum();

    let outputs_size = (OUTPUT_TAG_SIZE + OUTPUT_KEY_SIZE + AMOUNT_SIZE) * num_outputs;

    header_size + inputs_size + outputs_size
}

/// `Utilities::getApproximateMaximumInputCount(transactionSize, outputCount, mixinCount)`
/// (`Utilities.cpp:424`): how many inputs of this ring size fit in a budget.
///
/// Note the header allowance here uses `sizeof(uint64_t)` for the unlock time
/// where [`estimate_transaction_size`] uses `sizeof(uint64_t) + 2`; both are
/// copied as they are. Returns `0` rather than underflowing when the outputs
/// alone do not fit, which the C++ would do with a wrapped `size_t`.
pub fn approximate_maximum_input_count(transaction_size: usize, output_count: usize, mixin: u64) -> usize {
    let outputs_size = output_count * (OUTPUT_TAG_SIZE + OUTPUT_KEY_SIZE + AMOUNT_SIZE);
    let header_size = TRANSACTION_VERSION_SIZE + 8 + EXTRA_TAG_SIZE + PUBLIC_KEY_SIZE;
    let input_size = INPUT_TAG_SIZE
        + AMOUNT_SIZE
        + KEY_IMAGE_SIZE
        + SIGNATURE_SIZE
        + GLOBAL_INDEXES_VECTOR_SIZE_SIZE
        + GLOBAL_INDEXES_INITIAL_VALUE_SIZE
        + (mixin as usize) * (GLOBAL_INDEXES_DIFFERENCE_SIZE + SIGNATURE_SIZE);

    transaction_size.saturating_sub(header_size).saturating_sub(outputs_size) / input_size
}

/// `Utilities::nextFallbackMixin(triedMixin, achievableMixin, minMixin)`
/// (`Mixins.cpp:71`): what to retry at after `NOT_ENOUGH_FAKE_OUTPUTS`.
///
/// Never at or above what just failed, never below what the network accepts,
/// and `None` when the attempt was already at the network minimum.
pub fn next_fallback_mixin(tried: u64, achievable: u64, min: u64) -> Option<u64> {
    if tried <= min {
        return None;
    }
    let mut next = achievable;
    if next > tried - 1 {
        next = tried - 1;
    }
    if next < min {
        next = min;
    }
    Some(next)
}

/// `SendTransaction::sumTransactionFee` (`Transfer.cpp:1650`): inputs minus
/// outputs. `None` when the outputs exceed the inputs, which cannot happen for
/// a transaction this module built.
pub fn sum_transaction_fee(tx: &RawTransaction) -> Option<u64> {
    tx.prefix.sum_inputs()?.checked_sub(tx.prefix.sum_outputs()?)
}

/// `SendTransaction::verifyAmounts` (`Transfer.cpp:1620`): every output amount
/// is a member of `Constants::PRETTY_AMOUNTS`. Inputs are deliberately not
/// checked — they may have come from a wallet that does not enforce this.
pub fn verify_amounts(tx: &RawTransaction) -> bool {
    tx.prefix.outputs.iter().all(|o| constants::is_pretty_amount(o.amount))
}

/// `SendTransaction::verifyTransactionFee` (`Transfer.cpp:1670`).
///
/// Below the fee-per-byte heights only the flat minimum is checked; a fixed fee
/// must match exactly; a per-byte fee must land in
/// `[rate * size, 2 * rate * size]` — note the C++ multiplies the rate by the
/// **whole size**, not by the started chunks, which is a different number from
/// the one `getTransactionFee` produced.
pub fn verify_transaction_fee(expected: FeeType, actual_fee: u64, height: u64, size: usize) -> bool {
    if height <= MINIMUM_FEE_V1_HEIGHT + 1 {
        return actual_fee >= constants::MINIMUM_FEE;
    }
    if height < constants::MINIMUM_FEE_PER_BYTE_V1_HEIGHT {
        return actual_fee >= MINIMUM_FEE_V1;
    }
    if let FeeType::FixedFee(f) = expected {
        return f == actual_fee;
    }
    let calculated = (expected.rate(height) * size as f64) as u64;
    actual_fee >= calculated && actual_fee <= calculated.saturating_mul(2)
}

/// `SendTransaction::isTransactionPayloadTooBig` (`Transfer.cpp:673`):
/// `size <= Utilities::getMaxTxSize(height)`.
pub fn is_transaction_payload_too_big(size: usize, height: u64) -> Result<()> {
    let max = fees::wallet_max_tx_size(height);
    if size as u64 > max {
        return Err(WalletError::TooManyInputsToFitInBlock { size: size as u64, max });
    }
    Ok(())
}

/// The default `unlockTime` of a send (`Transfer.cpp:196`):
/// `networkHeight + (height > 1,500,000 ? 20 : 40) + 15`.
pub fn default_unlock_time(network_height: u64) -> u64 {
    let window = if network_height > UNLOCK_TIME_HEIGHT_V2 {
        UNLOCK_TIME_TRANSACTION_POOL_WINDOW_V2
    } else {
        UNLOCK_TIME_TRANSACTION_POOL_WINDOW
    };
    network_height + window + MINIMUM_UNLOCK_TIME_BLOCKS
}

////////////////////////
/* DENOMINATIONS      */
////////////////////////

/// `SendTransaction::splitAmountIntoDenominations(amount, preventTooLargeOutputs)`
/// (`Transfer.cpp:1310`): each non-zero decimal digit times its power of ten,
/// least significant first.
///
/// With `prevent_too_large` a denomination above `MAX_OUTPUT_SIZE_CLIENT`
/// (500,000,000,000) becomes ten or more equal pieces: the digit's value is
/// divided by ten repeatedly until each piece is small enough, and that many
/// copies are emitted. Note the C++ loop divides `denomination / 10` once and
/// then keeps dividing *the piece*, so a digit of 9 at 10^12 yields 10 pieces
/// of 9 · 10^11 — each still above the cap? No: 9 · 10^11 > 5 · 10^11, so it
/// divides again, to 100 pieces of 9 · 10^10.
///
/// With `prevent_too_large` off this is exactly `decompose_amount(amount, 0)`.
pub fn split_amount_into_denominations(amount: u64, prevent_too_large: bool) -> Vec<u64> {
    let mut out = Vec::new();
    let mut multiplier: u64 = 1;
    let mut amount = amount;

    while amount > 0 {
        let denomination = multiplier.wrapping_mul(amount % 10);

        if denomination > MAX_OUTPUT_SIZE_CLIENT && prevent_too_large {
            let mut num_split_amounts: u64 = 10;
            let mut split_amount = denomination / 10;

            while split_amount > MAX_OUTPUT_SIZE_CLIENT {
                split_amount /= 10;
                num_split_amounts *= 10;
            }

            out.extend(std::iter::repeat_n(split_amount, num_split_amounts as usize));
        } else if denomination != 0 {
            out.push(denomination);
        }

        amount /= 10;
        multiplier = multiplier.wrapping_mul(10);
    }

    out
}

/// `SendTransaction::setupDestinations` (`Transfer.cpp:786`): append the change
/// destination, then split every amount into denominations.
pub fn setup_destinations(
    addresses_and_amounts: &[(String, u64)],
    change_required: u64,
    change_address: &str,
) -> Result<Vec<TransactionDestination>> {
    let mut all: Vec<(&str, u64)> = addresses_and_amounts.iter().map(|(a, n)| (a.as_str(), *n)).collect();
    if change_required != 0 {
        all.push((change_address, change_required));
    }

    let mut destinations = Vec::new();
    for (address, amount) in all {
        let keys = base58::parse_address(address).map_err(WalletError::InvalidAddress)?;
        for denomination in split_amount_into_denominations(amount, true) {
            destinations.push(TransactionDestination {
                amount: denomination,
                receiver_public_spend_key: Hex32(keys.spend_public_key),
                receiver_public_view_key: Hex32(keys.view_public_key),
            });
        }
    }
    Ok(destinations)
}

////////////////////////
/* EXTRA              */
////////////////////////

/// The extra a wallet writes, in the C++ order (`Transfer.cpp:1487-1510`,
/// `TransactionPoW.cpp:96`):
///
/// ```text
/// 01 ‖ R
/// 02 ‖ varint(len) ‖ [ 00 ‖ 32-byte id | 03 ‖ 8-byte encrypted id ] [ 7f ‖ varint(len) ‖ data ]
/// 04 ‖ 8-byte proof-of-work nonce
/// ```
///
/// The nonce length is a **varint** here, where
/// [`wrkz_primitives::tx::build_extra`] writes a single byte: the C++ wallet
/// uses `Tools::uintToVarintVector`, and only the two agree below 128 bytes.
/// (The consensus parser reads a single byte, so a nonce of 128 bytes or more
/// is not something either side can parse back — it is what the C++ writes all
/// the same.)
pub fn build_wallet_extra(tx_public_key: &PublicKey, nonce: &[u8], pow_nonce: Option<&[u8; 8]>) -> Vec<u8> {
    let mut extra = Vec::with_capacity(33 + nonce.len() + 11);
    extra.push(TX_EXTRA_TAG_PUBKEY);
    extra.extend_from_slice(tx_public_key.as_bytes());

    if !nonce.is_empty() {
        extra.push(TX_EXTRA_NONCE);
        varint::write(&mut extra, nonce.len() as u64);
        extra.extend_from_slice(nonce);
    }

    if let Some(p) = pow_nonce {
        extra.push(TX_EXTRA_TRANSACTION_POW_NONCE);
        extra.extend_from_slice(p);
    }

    extra
}

/// The `extraNonce` payload: the payment id sub-field, then the arbitrary data
/// sub-field (`Transfer.cpp:1420-1480`). Same bytes as
/// [`wrkz_primitives::tx::build_nonce`].
fn build_extra_nonce(payment_id: Option<&PaymentId>, extra_data: &[u8]) -> Vec<u8> {
    tx::build_nonce(payment_id, if extra_data.is_empty() { None } else { Some(extra_data) })
}

////////////////////////////
/* TRANSACTION PROOF OF WORK */
////////////////////////////

/// `CryptoNote::generateTransactionPoWHeight` (`TransactionPoW.cpp:85`), minus
/// the optional remote solver, which [`TransferDaemon::remote_pow`] supplies.
///
/// `prefix` must already end in the eight nonce bytes (the `0x04` tag and eight
/// placeholder bytes appended to `extra`); the search rewrites exactly those
/// trailing bytes and hashes the whole prefix with `cn_upx`, which is byte for
/// byte what re-serializing the transaction with that nonce would produce.
///
/// Returns the winning nonce and how many hashes it took. `threads` splits the
/// nonce space into residue classes, exactly as the C++ does; with `threads`
/// of 1 the search is deterministic given `start`.
pub fn transaction_pow_search(
    prefix: &[u8],
    difficulty: u64,
    start: u64,
    threads: usize,
) -> Option<([u8; TX_POW_NONCE_SIZE], u64)> {
    assert!(prefix.len() >= TX_POW_NONCE_SIZE, "the prefix must end in the nonce field");
    let threads = threads.max(1);

    if threads == 1 {
        return pow_worker(prefix, difficulty, start, 1, &std::sync::atomic::AtomicBool::new(false));
    }

    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let stop = &stop;
                scope.spawn(move || pow_worker(prefix, difficulty, start.wrapping_add(i as u64), threads as u64, stop))
            })
            .collect();

        let mut winner = None;
        let mut hashes = 0u64;
        for h in handles {
            if let Some((nonce, n)) = h.join().expect("proof-of-work thread") {
                hashes += n;
                winner.get_or_insert(nonce);
            } else {
                hashes += 0;
            }
        }
        winner.map(|n| (n, hashes))
    })
}

/// Whether `nonce`, written over the trailing nonce bytes of `prefix`, meets
/// `difficulty`: `nonceSatisfies` (`TransactionPoW.cpp:115`), the one hash
/// that decides whether a proof-of-work server's answer is trusted.
pub fn nonce_satisfies(prefix: &[u8], nonce: &[u8; TX_POW_NONCE_SIZE], difficulty: u64) -> bool {
    let Some(at) = prefix.len().checked_sub(TX_POW_NONCE_SIZE) else { return false };
    let mut prefix = prefix.to_vec();
    prefix[at..].copy_from_slice(nonce);
    wrkz_pow::check_hash(&wrkz_pow::cn_upx(&prefix), difficulty)
}

fn pow_worker(
    base_prefix: &[u8],
    difficulty: u64,
    start: u64,
    step: u64,
    stop: &std::sync::atomic::AtomicBool,
) -> Option<([u8; TX_POW_NONCE_SIZE], u64)> {
    use std::sync::atomic::Ordering;

    let mut prefix = base_prefix.to_vec();
    let at = prefix.len() - TX_POW_NONCE_SIZE;
    let mut nonce = start;
    let mut hashes = 0u64;

    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }

        prefix[at..].copy_from_slice(&nonce.to_le_bytes());
        hashes += 1;

        if wrkz_pow::check_hash(&wrkz_pow::cn_upx(&prefix), difficulty) {
            let mut out = [0u8; TX_POW_NONCE_SIZE];
            out.copy_from_slice(&prefix[at..]);
            stop.store(true, Ordering::Relaxed);
            return Some((out, hashes));
        }

        nonce = nonce.wrapping_add(step);

        // A full sweep of the nonce space with no solution. Unreachable at any
        // real difficulty; the C++ loops forever here.
        if nonce == start {
            return None;
        }
    }
}

////////////////////////
/* INPUT SELECTION    */
////////////////////////

/// How many decimal digits `amount` has, the C++ `floor(log10(amount)) + 1`
/// computed in integers so the bucket of a power of ten cannot depend on the
/// rounding of `log10` (`SubWallets.cpp:578`).
fn digit_count(amount: u64) -> u32 {
    let mut digits = 1;
    let mut a = amount / 10;
    while a > 0 {
        digits += 1;
        a /= 10;
    }
    digits
}

impl Wallet {
    /// `SubWallets::getSpendableTransactionInputs` (`SubWallets.cpp:530`) with
    /// the clock supplied.
    ///
    /// Every unlocked unspent input of the chosen subwallets, sorted largest
    /// first, bucketed by decimal digit count, then taken round-robin one per
    /// bucket starting from the smallest bucket, smallest amount within each
    /// bucket first. Spending in that order keeps small denominations moving
    /// and leaves the wallet with fewer, larger inputs.
    pub fn spendable_transaction_inputs_at(
        &self,
        take_from_all: bool,
        take_from: &[PublicKey],
        height: u64,
        now: u64,
    ) -> Result<Vec<OwnedSpendableInput>> {
        if self.sub_wallets.is_view_wallet {
            return Err(WalletError::IllegalViewWalletOperation);
        }

        let keys: Vec<PublicKey> =
            if take_from_all { self.sub_wallets.public_spend_keys.clone() } else { take_from.to_vec() };

        let mut available = Vec::new();
        for key in &keys {
            let Some(sub) = self.sub_wallet(key) else {
                return Err(WalletError::AddressNotInWallet(key.to_hex()));
            };
            for input in &sub.unspent_inputs {
                if is_input_unlocked_at(input.unlock_time, height, now) {
                    available.push(OwnedSpendableInput {
                        public_spend_key: sub.public_spend_key,
                        private_spend_key: sub.private_spend_key.clone(),
                        input: input.clone(),
                    });
                }
            }
        }

        // Largest first, so that `pop()` from a bucket hands back its smallest.
        available.sort_by_key(|a| std::cmp::Reverse(a.input.amount));

        let mut buckets: BTreeMap<u32, Vec<OwnedSpendableInput>> = BTreeMap::new();
        for input in available {
            buckets.entry(digit_count(input.input.amount)).or_default().push(input);
        }

        let mut ordered = Vec::new();
        while !buckets.is_empty() {
            for bucket in buckets.values_mut() {
                if let Some(input) = bucket.pop() {
                    ordered.push(input);
                }
            }
            buckets.retain(|_, b| !b.is_empty());
        }

        Ok(ordered)
    }

    /// [`Wallet::spendable_transaction_inputs_at`] against the system clock.
    pub fn spendable_transaction_inputs(
        &self,
        take_from_all: bool,
        take_from: &[PublicKey],
        height: u64,
    ) -> Result<Vec<OwnedSpendableInput>> {
        self.spendable_transaction_inputs_at(take_from_all, take_from, height, now_seconds())
    }

    /// `SubWallets::getFusionTransactionInputs` (`SubWallets.cpp:615`).
    ///
    /// Shuffle every unlocked input, drop anything at or above the optimize
    /// target, bucket by digit count, and take from the first bucket that holds
    /// at least `FUSION_TX_MIN_INPUT_COUNT` inputs (the buckets themselves
    /// shuffled first) — or from every bucket when none is that full. Stops at
    /// as many inputs as fit in `FUSION_TX_MAX_SIZE` at this ring size.
    ///
    /// Returns `(inputs, max_inputs_to_take, found_money)`.
    pub fn fusion_transaction_inputs<R: TransferRandom>(
        &self,
        take_from_all: bool,
        take_from: &[PublicKey],
        mixin: u64,
        height: u64,
        now: u64,
        optimize_target: Option<u64>,
        random: &mut R,
    ) -> Result<(Vec<OwnedSpendableInput>, usize, u64)> {
        if self.sub_wallets.is_view_wallet {
            return Err(WalletError::IllegalViewWalletOperation);
        }

        let keys: Vec<PublicKey> =
            if take_from_all { self.sub_wallets.public_spend_keys.clone() } else { take_from.to_vec() };

        let mut available = Vec::new();
        for key in &keys {
            let Some(sub) = self.sub_wallet(key) else {
                return Err(WalletError::AddressNotInWallet(key.to_hex()));
            };
            for input in &sub.unspent_inputs {
                if is_input_unlocked_at(input.unlock_time, height, now) {
                    available.push(OwnedSpendableInput {
                        public_spend_key: sub.public_spend_key,
                        private_spend_key: sub.private_spend_key.clone(),
                        input: input.clone(),
                    });
                }
            }
        }

        let max_inputs_to_take =
            approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, mixin);

        shuffle(&mut available, random);

        // `std::unordered_map`, so the C++ bucket order is unspecified; the
        // buckets are shuffled below anyway, and a `BTreeMap` makes the seeded
        // path reproducible.
        let mut buckets: BTreeMap<u32, Vec<OwnedSpendableInput>> = BTreeMap::new();
        for input in available {
            if let Some(target) = optimize_target {
                if input.input.amount >= target {
                    continue;
                }
            }
            buckets.entry(digit_count(input.input.amount)).or_default().push(input);
        }

        let mut full: Vec<Vec<OwnedSpendableInput>> =
            buckets.values().filter(|b| b.len() >= FUSION_TX_MIN_INPUT_COUNT).cloned().collect();
        shuffle(&mut full, random);

        let take_from_buckets: Vec<Vec<OwnedSpendableInput>> =
            if let Some(first) = full.into_iter().next() { vec![first] } else { buckets.into_values().collect() };

        let mut inputs_to_use = Vec::new();
        let mut found_money = 0u64;

        for bucket in take_from_buckets {
            for input in bucket {
                found_money = found_money.wrapping_add(input.input.amount);
                inputs_to_use.push(input);

                if inputs_to_use.len() >= max_inputs_to_take {
                    return Ok((inputs_to_use, max_inputs_to_take, found_money));
                }
            }
        }

        Ok((inputs_to_use, max_inputs_to_take, found_money))
    }
}

/// Fisher-Yates with the randomness the caller supplies, standing in for
/// `std::shuffle(..., std::random_device{})`.
fn shuffle<T, R: TransferRandom>(items: &mut [T], random: &mut R) {
    if items.len() < 2 {
        return;
    }
    for i in (1..items.len()).rev() {
        let j = random.next_below(i as u64 + 1) as usize;
        items.swap(i, j);
    }
}

fn now_seconds() -> u64 {
    crate::platform::now_seconds()
}

////////////////////////
/* VALIDATION         */
////////////////////////

/// `validateAddresses(addresses, integratedAddressesAllowed)`
/// (`ValidateParameters.cpp:337`), in its order: length, prefix, then the
/// base58 and key checks, so the error code matches the C++ one.
pub fn validate_address(address: &str, integrated_allowed: bool) -> Result<base58::Address> {
    if address.len() != STANDARD_ADDRESS_LENGTH
        && address.len() != INTEGRATED_ADDRESS_LENGTH
        && address.len() != INTEGRATED_ADDRESS_LENGTH_LONG
    {
        return Err(WalletError::InvalidAddress(base58::Base58Error::WrongAddressLength(address.len())));
    }

    if !address.starts_with(constants::ADDRESS_PREFIX) {
        return Err(WalletError::InvalidAddress(base58::Base58Error::WrongPrefix(0)));
    }

    if base58::is_integrated_address(address) {
        if !integrated_allowed {
            return Err(WalletError::AddressIsIntegrated(address.to_string()));
        }
        let parsed = base58::parse_address(address).map_err(|e| match e {
            base58::Base58Error::BadPaymentId => WalletError::IntegratedAddressPaymentIdInvalid,
            other => WalletError::InvalidAddress(other),
        })?;
        // `validateAddresses` re-validates the embedded id through
        // `validatePaymentID`, which only accepts 16 or 64 hex characters.
        if let Some(id) = &parsed.payment_id {
            validate_payment_id(id).map_err(|_| WalletError::IntegratedAddressPaymentIdInvalid)?;
        }
        return Ok(parsed);
    }

    base58::parse_address(address).map_err(WalletError::InvalidAddress)
}

/// `validatePaymentID` (`ValidateParameters.cpp:163`): empty, or 16 or 64 hex
/// characters.
pub fn validate_payment_id(payment_id: &str) -> Result<()> {
    if payment_id.is_empty() {
        return Ok(());
    }
    if payment_id.len() != SHORT_PAYMENT_ID_LENGTH && payment_id.len() != LONG_PAYMENT_ID_LENGTH {
        return Err(WalletError::PaymentIdWrongLength(payment_id.len()));
    }
    if !payment_id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(WalletError::PaymentIdInvalid);
    }
    Ok(())
}

/// `validateMixin(mixin, height)` (`ValidateParameters.cpp:205`).
pub fn validate_mixin(mixin: u64, height: u64) -> Result<()> {
    let range = mixins::mixin_allowable_range(height);
    if mixin < range.min {
        return Err(WalletError::MixinTooSmall { mixin, min: range.min });
    }
    if mixin > range.max {
        return Err(WalletError::MixinTooBig { mixin, max: range.max });
    }
    Ok(())
}

/// `validateUnlockTime(unlockTime, currentHeight)` (`ValidateParameters.cpp:487`).
///
/// Above `CRYPTONOTE_MAX_BLOCK_NUMBER` the value is a unix time and must be at
/// least fifteen block times ahead of the clock; below it, a block index at
/// least fifteen blocks ahead of the network height.
pub fn validate_unlock_time_at(unlock_time: u64, current_height: u64, now: u64) -> Result<()> {
    if unlock_time > constants::CRYPTONOTE_MAX_BLOCK_NUMBER {
        let minimum = now + MINIMUM_UNLOCK_TIME_BLOCKS * constants::DIFFICULTY_TARGET;
        if unlock_time < minimum {
            return Err(WalletError::UnlockTimeTooSmall { unlock_time, minimum });
        }
    } else {
        let minimum = current_height + MINIMUM_UNLOCK_TIME_BLOCKS;
        if unlock_time < minimum {
            return Err(WalletError::UnlockTimeTooSmall { unlock_time, minimum });
        }
    }
    Ok(())
}

/// `validateOurAddresses` (`ValidateParameters.cpp:433`): a valid standard
/// address that this container holds.
fn validate_our_address(wallet: &Wallet, address: &str) -> Result<PublicKey> {
    let parsed = validate_address(address, false)?;
    let spend = Hex32(parsed.spend_public_key);
    if !wallet.is_our_spend_key(&spend) {
        return Err(WalletError::AddressNotInWallet(address.to_string()));
    }
    Ok(spend)
}

/// `validateIntegratedAddresses(destinations, paymentID)`
/// (`ValidateParameters.cpp:120`): every integrated address must agree with the
/// payment id given, and with each other.
///
/// The C++ takes `paymentID` **by value**, so the first integrated address only
/// fills a local copy; a second one is compared against that copy, which is why
/// two integrated addresses with different ids conflict even when no explicit
/// payment id was given.
fn validate_integrated_addresses(destinations: &[(String, u64)], payment_id: &str) -> Result<()> {
    let mut payment_id = payment_id.to_string();
    for (address, _) in destinations {
        if !base58::is_integrated_address(address) {
            continue;
        }
        let parsed = base58::parse_address(address).map_err(WalletError::InvalidAddress)?;
        let extracted = parsed.payment_id.unwrap_or_default();
        if payment_id.is_empty() {
            payment_id = extracted;
        } else if payment_id != extracted {
            return Err(WalletError::ConflictingPaymentIds);
        }
    }
    Ok(())
}

/// `validateTransaction(...)` (`ValidateParameters.cpp:60`), in its exact
/// order: destinations, integrated addresses, subwallets to take from, amount
/// and balance, mixin, payment id, change address, unlock time.
///
/// Stops at the first failure, so a caller that wants to know every problem has
/// to fix them one at a time — the same contract the C++ has.
pub fn validate_transaction_parameters(
    wallet: &Wallet,
    destinations: &[(String, u64)],
    mixin: u64,
    fee: FeeType,
    payment_id: &str,
    addresses_to_take_from: &[String],
    change_address: &str,
    unlock_time: u64,
    current_height: u64,
    now: u64,
) -> Result<()> {
    // validateDestinations (line 307)
    if destinations.is_empty() {
        return Err(WalletError::NoDestinationsGiven);
    }
    // The C++ walks the destinations once for the zero check, collecting the
    // addresses, and validates all of them afterwards, so a zero amount is
    // reported before a bad address even when the bad address comes first.
    for (_, amount) in destinations {
        if *amount == 0 {
            return Err(WalletError::AmountIsZero);
        }
    }
    for (address, _) in destinations {
        validate_address(address, true)?;
    }

    validate_integrated_addresses(destinations, payment_id)?;

    for address in addresses_to_take_from {
        validate_our_address(wallet, address)?;
    }

    validate_amount(wallet, destinations, fee, addresses_to_take_from, current_height, now)?;

    validate_mixin(mixin, current_height)?;
    validate_payment_id(payment_id)?;
    validate_our_address(wallet, change_address)?;
    validate_unlock_time_at(unlock_time, current_height, now)?;

    Ok(())
}

/// `validateAmount` (`ValidateParameters.cpp:236`): the rate floor for a
/// per-byte fee, the overflow check, and the balance check.
fn validate_amount(
    wallet: &Wallet,
    destinations: &[(String, u64)],
    fee: FeeType,
    addresses_to_take_from: &[String],
    current_height: u64,
    now: u64,
) -> Result<()> {
    let min_rate =
        if current_height > MINIMUM_FEE_PER_BYTE_V2_HEIGHT { MINIMUM_FEE_PER_BYTE_V2 } else { MINIMUM_FEE_PER_BYTE_V1 };
    if let FeeType::FeePerByte(rate) = fee {
        if rate < min_rate {
            return Err(WalletError::FeeTooSmall);
        }
    }

    let mut spend_keys = Vec::new();
    for address in addresses_to_take_from {
        spend_keys.push(validate_our_address(wallet, address)?);
    }

    let available = balance_of(wallet, addresses_to_take_from.is_empty(), &spend_keys, current_height, now);

    let mut amounts: Vec<u64> = Vec::with_capacity(destinations.len() + 1);
    if fee.is_fixed() {
        amounts.push(fee.fixed_fee());
    }
    amounts.extend(destinations.iter().map(|(_, a)| *a));

    let mut total: u64 = 0;
    for a in &amounts {
        total = total.checked_add(*a).ok_or(WalletError::WillOverflow)?;
    }

    if total > available {
        return Err(WalletError::NotEnoughBalance { needed: total, available });
    }

    Ok(())
}

/// The unlocked balance of the chosen subwallets (`SubWallets::getBalance`).
fn balance_of(wallet: &Wallet, take_from_all: bool, spend_keys: &[PublicKey], height: u64, now: u64) -> u64 {
    let keys: Vec<PublicKey> =
        if take_from_all { wallet.sub_wallets.public_spend_keys.clone() } else { spend_keys.to_vec() };
    keys.iter().map(|k| wallet.balance_for_spend_key_at(k, height, now).0).fold(0u64, u64::wrapping_add)
}

////////////////////////
/* RING ASSEMBLY      */
////////////////////////

/// `SendTransaction::getRingParticipants` (`Transfer.cpp:823`): ask the daemon
/// for `mixin + 1` outputs per input amount and check the answer.
///
/// Returns the per-amount output sets in request order together with the
/// largest mixin the chain can support for these denominations, which is what
/// the fallback of [`send_transaction_advanced`] retries at.
fn get_ring_participants<D: TransferDaemon>(
    mixin: u64,
    daemon: &D,
    sources: &[OwnedSpendableInput],
) -> Inner<Vec<Vec<(u64, PublicKey)>>> {
    if mixin == 0 {
        return Ok(Vec::new());
    }

    let requested_outs = mixin + 1;
    let amounts: Vec<u64> = sources.iter().map(|s| s.input.amount).collect();

    let response = daemon
        .random_outs(&amounts, requested_outs)
        .map_err(|e| Failure { error: classify_random_outs_error(&e), achievable_mixin: 0 })?;

    // The daemon's entries, decoded once. `out_key` is hex on the wire.
    let mut outs: Vec<(u64, Vec<(u64, PublicKey)>)> = Vec::with_capacity(response.outs.len());
    for entry in &response.outs {
        let mut set = Vec::with_capacity(entry.outs.len());
        for o in &entry.outs {
            let key = Hex32::from_hex(&o.out_key).ok_or_else(|| Failure {
                error: WalletError::DaemonError(format!("/getrandom_outs returned a bad output key {}", o.out_key)),
                achievable_mixin: 0,
            })?;
            set.push((o.global_amount_index, key));
        }
        outs.push((entry.amount, set));
    }

    // `achievableMixin`: the smallest set we were handed, minus the one entry
    // that may turn out to be our own output. Any requested amount missing from
    // the answer forces it to zero.
    let achievable_mixin = {
        let smallest = outs.iter().map(|(_, s)| s.len() as u64).min();
        let all_present = amounts.iter().all(|a| outs.iter().any(|(amount, _)| amount == a));
        match smallest {
            Some(n) if n > 0 && all_present => n - 1,
            _ => 0,
        }
    };

    let fail = |msg: String| Failure { error: WalletError::NotEnoughFakeOutputs(msg), achievable_mixin };

    for amount in &amounts {
        let Some((_, set)) = outs.iter().find(|(a, _)| a == amount) else {
            return Err(fail(format!("failed to get any matching outputs for amount {amount}")));
        };
        if (set.len() as u64) < mixin {
            return Err(fail(format!(
                "failed to get enough matching outputs for amount {amount}. Requested outputs: {requested_outs}, \
                 found outputs: {}",
                set.len()
            )));
        }
    }

    if outs.len() != amounts.len() {
        return Err(fail(format!("the daemon returned {} output sets for {} inputs", outs.len(), amounts.len())));
    }

    // The second pass the C++ makes so that spending one denomination twice is
    // checked against both drawn sets and not only the first.
    for (amount, set) in &outs {
        if (set.len() as u64) < mixin {
            return Err(fail(format!(
                "failed to get enough matching outputs for amount {amount}. Requested outputs: {requested_outs}, \
                 found outputs: {}",
                set.len()
            )));
        }
    }

    // `Transfer.cpp:965` sorts each set here, on a loop *copy*; see the module
    // docs. The daemon's own order is what goes on the wire, and it is
    // ascending.
    Ok(outs.into_iter().map(|(_, s)| s).collect())
}

/// `SendTransaction::prepareRingParticipants` (`Transfer.cpp:973`): pad every
/// input with decoys and record where the real output ended up.
fn prepare_ring_participants<D: TransferDaemon>(
    mut sources: Vec<OwnedSpendableInput>,
    mixin: u64,
    daemon: &D,
) -> Inner<(Vec<ObscuredInput>, Vec<OwnedSpendableInput>, u64)> {
    // Sorted by amount so the inputs line up with the daemon's answer, which
    // comes back in request order. `sort_by_key` is stable, where the C++
    // `std::sort` is not; equal amounts therefore keep their selection order
    // here and may not there, which changes nothing about the transaction
    // beyond which of two identical denominations got which decoy set.
    sources.sort_by_key(|s| s.input.amount);

    let fake_outs = get_ring_participants(mixin, daemon, &sources)?;

    let mut result = Vec::with_capacity(sources.len());

    for (i, source) in sources.iter().enumerate() {
        let Some(global_index) = source.input.global_output_index else {
            return Err(WalletError::DaemonError(
                "Missing global output index for one or more wallet outputs. Let sync continue on a full node and \
                 retry."
                    .to_string(),
            )
            .into());
        };

        let mut ring: Vec<(u64, PublicKey)> = Vec::with_capacity(mixin as usize + 1);

        if mixin != 0 {
            for (index, key) in &fake_outs[i] {
                // This fake output is our output: skip it.
                if *index == global_index {
                    continue;
                }
                ring.push((*index, *key));
                if ring.len() as u64 >= mixin {
                    break;
                }
            }
        }

        if (ring.len() as u64) < mixin {
            // Already stripped of our own output, so what we gathered is
            // directly the ring we could have built.
            return Err(Failure {
                error: WalletError::NotEnoughFakeOutputs(format!(
                    "failed to get enough matching outputs for amount {}. Requested outputs: {mixin}, found outputs: \
                     {}",
                    source.input.amount,
                    ring.len()
                )),
                achievable_mixin: ring.len() as u64,
            });
        }

        // Where the real output belongs among the decoys: before the first one
        // whose global index is at or above ours.
        let position = ring.iter().position(|(index, _)| *index >= global_index).unwrap_or(ring.len());
        ring.insert(position, (global_index, source.input.key));

        result.push(ObscuredInput {
            amount: source.input.amount,
            key_image: source.input.key_image,
            ring,
            real_output: position,
            private_ephemeral: source.input.private_ephemeral.clone().unwrap_or(SecretKey::NULL),
            owner_public_spend_key: source.public_spend_key,
        });
    }

    Ok((result, sources, mixin))
}

/// `SendTransaction::setupInputs` (`Transfer.cpp:1073`): key image, amount and
/// relative offsets per input, plus the one-time secret each ring signature
/// needs.
///
/// The one-time secret is the cached `privateEphemeral` when sync stored one,
/// and is otherwise re-derived from the parent transaction's public key and our
/// private view and spend keys.
fn setup_inputs(
    obscured: &mut [ObscuredInput],
    sources: &[OwnedSpendableInput],
    private_view_key: &SecretKey,
) -> Result<(Vec<Input>, Vec<SecretKey>)> {
    let mut inputs = Vec::with_capacity(obscured.len());
    let mut secrets = Vec::with_capacity(obscured.len());

    for (input, source) in obscured.iter_mut().zip(sources) {
        if input.private_ephemeral.is_null() {
            let derivation = curve::generate_key_derivation(
                source.input.transaction_public_key.as_bytes(),
                private_view_key.as_bytes(),
            )
            .ok_or(WalletError::InvalidPublicKey)?;

            input.private_ephemeral = SecretKey::from_bytes(curve::derive_secret_key(
                &derivation,
                source.input.transaction_index,
                source.private_spend_key.as_bytes(),
            ));
        }

        secrets.push(input.private_ephemeral.clone());

        // Relative offsets, `copy[i] = absolute[i] - absolute[i - 1]` on
        // `uint32_t` — wrapping, exactly as `Transfer.cpp:1141` does, so that a
        // ring the daemon handed back out of order produces the same bytes here
        // as there.
        let absolute: Vec<u32> = input.ring.iter().map(|(i, _)| *i as u32).collect();
        let mut offsets: Vec<u64> = Vec::with_capacity(absolute.len());
        for (i, value) in absolute.iter().enumerate() {
            if i == 0 {
                offsets.push(u64::from(*value));
            } else {
                offsets.push(u64::from(value.wrapping_sub(absolute[i - 1])));
            }
        }

        inputs.push(Input::Key { amount: input.amount, key_offsets: offsets, key_image: *input.key_image.as_bytes() });
    }

    Ok((inputs, secrets))
}

/// `SendTransaction::setupOutputs` (`Transfer.cpp:1167`): sort the destinations
/// by amount, draw the transaction key pair, and derive one one-time key per
/// destination.
///
/// `P_i = H_s(8 · r · A_i ‖ i) · G + B_i`. Sorting hides which output is the
/// change; the C++ `std::sort` is unstable, so equal amounts to different
/// addresses may come out in a different order there — the stable sort here
/// keeps the destination order, which is the change-last order of
/// [`setup_destinations`].
fn setup_outputs<R: TransferRandom>(
    mut destinations: Vec<TransactionDestination>,
    random: &mut R,
) -> Result<(Vec<KeyOutput>, SecretKey, PublicKey)> {
    destinations.sort_by_key(|d| d.amount);

    let (secret, public) = random.key_pair();

    let mut outputs = Vec::with_capacity(destinations.len());
    for (index, destination) in destinations.iter().enumerate() {
        let derivation = curve::generate_key_derivation(destination.receiver_public_view_key.as_bytes(), &secret)
            .ok_or(WalletError::InvalidPublicKey)?;
        let key = curve::derive_public_key(&derivation, index as u64, destination.receiver_public_spend_key.as_bytes())
            .ok_or(WalletError::InvalidPublicKey)?;
        outputs.push(KeyOutput { amount: destination.amount, key: Hex32(key) });
    }

    Ok((outputs, SecretKey::from_bytes(secret), Hex32(public)))
}

/// `SendTransaction::generateRingSignatures` (`Transfer.cpp:1216`): one ring
/// signature per input over the final prefix hash, each verified immediately.
fn generate_ring_signatures<R: TransferRandom>(
    tx: &mut RawTransaction,
    rings: &[ObscuredInput],
    secrets: &[SecretKey],
    random: &mut R,
) -> Result<()> {
    let prefix_hash = tx.prefix.hash();

    for (i, ring) in rings.iter().enumerate() {
        let keys: Vec<[u8; 32]> = ring.ring.iter().map(|(_, k)| *k.as_bytes()).collect();
        let (k, decoys) = random.ring_randomness(keys.len());

        let signatures = curve::generate_ring_signature_with_randomness(
            &prefix_hash,
            ring.key_image.as_bytes(),
            &keys,
            secrets[i].as_bytes(),
            ring.real_output,
            &k,
            &decoys,
        )
        .ok_or(WalletError::FailedToCreateRingSignature)?;

        tx.signatures.push(signatures);
    }

    // The C++ verifies every signature in a second pass before returning.
    for (i, ring) in rings.iter().enumerate() {
        let keys: Vec<[u8; 32]> = ring.ring.iter().map(|(_, k)| *k.as_bytes()).collect();
        if !curve::check_ring_signature(&prefix_hash, ring.key_image.as_bytes(), &keys, &tx.signatures[i]) {
            return Err(WalletError::FailedToCreateRingSignature);
        }
    }

    Ok(())
}

////////////////////////
/* BUILD              */
////////////////////////

/// What `makeTransaction` needs beyond the inputs and destinations.
struct BuildContext<'a> {
    mixin: u64,
    payment_id: &'a str,
    recipient_view_key: Option<PublicKey>,
    private_view_key: &'a SecretKey,
    unlock_time: u64,
    extra_data: &'a [u8],
    network_height: u64,
    pow_threads: usize,
    /// Fusion transactions must satisfy the daemon's fusion proof-of-work
    /// difficulty and skip the fee escape; see the module docs.
    is_fusion: bool,
}

/// `SendTransaction::makeTransaction` (`Transfer.cpp:1386`): rings, inputs,
/// outputs, extra, proof of work, signatures.
fn make_transaction<D: TransferDaemon, R: TransferRandom>(
    ctx: &BuildContext<'_>,
    daemon: &D,
    our_inputs: &[OwnedSpendableInput],
    destinations: Vec<TransactionDestination>,
    random: &mut R,
) -> Inner<Built> {
    let (mut rings, sorted_inputs, _) = prepare_ring_participants(our_inputs.to_vec(), ctx.mixin, daemon)?;

    let (inputs, secrets) = setup_inputs(&mut rings, &sorted_inputs, ctx.private_view_key)?;

    let (outputs, tx_private_key, tx_public_key) = setup_outputs(destinations, random)?;

    // ---- extra -----------------------------------------------------------
    let payment_id_field = if ctx.payment_id.is_empty() {
        None
    } else if ctx.payment_id.len() == SHORT_PAYMENT_ID_LENGTH {
        // Encrypted against the shared secret between us and the receiver, so
        // only they can read it (`Transfer.cpp:1425`).
        let view_key = ctx.recipient_view_key.ok_or(WalletError::PaymentIdInvalid)?;
        let encrypted = encrypt_payment_id_hex(ctx.payment_id, view_key.as_bytes(), tx_private_key.as_bytes());
        if encrypted.is_empty() {
            return Err(WalletError::PaymentIdInvalid.into());
        }
        let mut bytes = [0u8; 8];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&encrypted[i * 2..i * 2 + 2], 16).map_err(|_| WalletError::PaymentIdInvalid)?;
        }
        Some(PaymentId::EncryptedShort(bytes))
    } else {
        let id = Hex32::from_hex(ctx.payment_id).ok_or(WalletError::PaymentIdInvalid)?;
        Some(PaymentId::Long(*id.as_bytes()))
    };

    let nonce = build_extra_nonce(payment_id_field.as_ref(), ctx.extra_data);
    let extra = build_wallet_extra(&tx_public_key, &nonce, None);

    // ---- the prefix ------------------------------------------------------
    let mut tx = RawTransaction {
        prefix: TransactionPrefix {
            version: tx::CURRENT_TRANSACTION_VERSION,
            unlock_time: ctx.unlock_time,
            inputs,
            outputs: outputs.iter().map(|o| Output { amount: o.amount, key: *o.key.as_bytes() }).collect(),
            extra,
        },
        signatures: Vec::new(),
    };

    if tx.prefix.outputs.len() > NORMAL_TX_MAX_OUTPUT_COUNT_V1 {
        return Err(WalletError::OutputDecomposition {
            outputs: tx.prefix.outputs.len(),
            max: NORMAL_TX_MAX_OUTPUT_COUNT_V1,
        }
        .into());
    }

    // ---- transaction proof of work ---------------------------------------
    // Comes before signing: the signatures are over the prefix hash, and the
    // nonce lives in `extra`, which is part of the prefix.
    let fee = sum_transaction_fee(&tx).unwrap_or(0);
    let needs_pow = ctx.is_fusion
        || ctx.network_height < TRANSACTION_POW_PASS_WITH_FEE_HEIGHT
        || fee < TRANSACTION_POW_PASS_WITH_FEE;

    let (pow_nonce, pow_difficulty, pow_hashes) = if needs_pow {
        let difficulty = constants::transaction_pow_difficulty(
            ctx.network_height,
            ctx.is_fusion,
            tx.prefix.inputs.len() as u64,
            tx.prefix.outputs.len() as u64,
        )
        .unwrap_or(0);

        if difficulty == 0 {
            (None, 0, 0)
        } else {
            let mut extra = std::mem::take(&mut tx.prefix.extra);
            extra.push(TX_EXTRA_TRANSACTION_POW_NONCE);
            extra.resize(extra.len() + TX_POW_NONCE_SIZE, 0);
            tx.prefix.extra = extra;

            let prefix = tx.prefix.to_bytes();
            // Drawn before any server is asked, so a seeded build consumes the
            // same randomness whichever way the nonce is found.
            let start = random.pow_nonce_start();
            let remote = daemon
                .remote_pow(&prefix, difficulty, ctx.network_height)
                .filter(|nonce| nonce_satisfies(&prefix, nonce, difficulty));
            let (nonce, hashes) = match remote {
                // Found elsewhere: no hashes were computed here.
                Some(nonce) => (nonce, 0),
                None => transaction_pow_search(&prefix, difficulty, start, ctx.pow_threads)
                    .ok_or(WalletError::TransactionPoWFailed)?,
            };

            let at = tx.prefix.extra.len() - TX_POW_NONCE_SIZE;
            tx.prefix.extra[at..].copy_from_slice(&nonce);

            (Some(nonce), difficulty, hashes)
        }
    } else {
        (None, 0, 0)
    };

    // ---- signatures ------------------------------------------------------
    generate_ring_signatures(&mut tx, &rings, &secrets, random)?;

    // Serialize, read back, and re-verify every signature: what the daemon
    // will do (`Transfer.cpp:1595`, which only logs a warning; a failure here
    // means the transaction would be rejected, so it is an error).
    let blob = tx.to_bytes().map_err(|e| WalletError::Json(e.to_string()))?;
    let round_tripped = RawTransaction::from_bytes(&blob).map_err(|e| WalletError::Json(e.to_string()))?;
    if round_tripped.prefix.hash() != tx.prefix.hash() {
        return Err(WalletError::FailedToCreateRingSignature.into());
    }

    Ok(Built {
        transaction: tx,
        size: blob.len(),
        rings,
        outputs,
        tx_private_key,
        tx_public_key,
        pow_nonce,
        pow_difficulty,
        pow_hashes,
        inputs: sorted_inputs,
    })
}

/// `WalletTypes::TransactionResult` (`WalletTypes.h:540`).
struct Built {
    transaction: RawTransaction,
    size: usize,
    rings: Vec<ObscuredInput>,
    outputs: Vec<KeyOutput>,
    tx_private_key: SecretKey,
    tx_public_key: PublicKey,
    pow_nonce: Option<[u8; 8]>,
    pow_difficulty: u64,
    pow_hashes: u64,
    /// The inputs in the order the transaction spends them (amount ascending),
    /// which is the order the signatures are in.
    inputs: Vec<OwnedSpendableInput>,
}

/// `SendTransaction::tryMakeFeePerByteTransaction` (`Transfer.cpp:566`).
///
/// Build, measure, recompute the fee from the real size; if the built fee
/// covers it we are done, otherwise raise the fee and build again — or, when
/// the inputs no longer cover it, report how much is needed so the caller can
/// add another input.
fn try_make_fee_per_byte_transaction<D: TransferDaemon, R: TransferRandom>(
    ctx: &BuildContext<'_>,
    daemon: &D,
    sum_of_inputs: u64,
    mut amount_pre_fee: u64,
    mut amount_including_fee: u64,
    fee_per_byte: f64,
    addresses_and_amounts: &mut [(String, u64)],
    change_address: &str,
    our_inputs: &[OwnedSpendableInput],
    send_all: bool,
    random: &mut R,
) -> Inner<FeeLoopOutcome> {
    loop {
        let change_required = sum_of_inputs - amount_including_fee;
        let destinations = setup_destinations(addresses_and_amounts, change_required, change_address)?;

        let built = make_transaction(ctx, daemon, our_inputs, destinations, random)?;

        let actual_fee = fees::transaction_fee(built.size, ctx.network_height, fee_per_byte);

        if amount_including_fee - amount_pre_fee >= actual_fee {
            return Ok(FeeLoopOutcome::Built { built: Box::new(built), change_required });
        }

        if send_all {
            amount_pre_fee = amount_including_fee - actual_fee;
            addresses_and_amounts[0].1 = amount_pre_fee;
        }

        if amount_pre_fee + actual_fee > sum_of_inputs {
            return Ok(FeeLoopOutcome::NeedMoreInputs { needed: amount_pre_fee + actual_fee });
        }

        amount_including_fee = amount_pre_fee + actual_fee;
    }
}

enum FeeLoopOutcome {
    Built { built: Box<Built>, change_required: u64 },
    NeedMoreInputs { needed: u64 },
}

////////////////////////
/* PUBLIC ENTRY       */
////////////////////////

/// `SendTransaction::sendTransactionBasic` (`Transfer.cpp:39`): one
/// destination, the tier default mixin, the minimum fee, change to the primary
/// address.
///
/// Note the C++ passes `unlockTime = 0`, which
/// `sendTransactionAdvancedWithMixin` then replaces with the derived value.
pub fn send_transaction_basic<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    destination: &str,
    amount: u64,
    payment_id: &str,
    network_height: u64,
    send_all: bool,
    random: &mut R,
) -> Result<PreparedTransaction> {
    let params = SendParams { send_all, ..SendParams::basic(destination, amount, payment_id, network_height) };
    send_transaction_advanced(wallet, daemon, &params, random)
}

/// `SendTransaction::sendTransactionAdvanced` (`Transfer.cpp:92`): build, relay
/// and record, retrying at a smaller ring when the chain cannot supply decoys.
///
/// On `NOT_ENOUGH_FAKE_OUTPUTS` the send is retried once at the largest ring
/// the chain was measured to support and, if that fails too, once at the
/// network minimum — three round trips in the worst case, and only on a path
/// that would otherwise be a hard failure.
pub fn send_transaction_advanced<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    params: &SendParams,
    random: &mut R,
) -> Result<PreparedTransaction> {
    let prepared = build_with_mixin_fallback(wallet, daemon, params, random)?;
    relay_and_store(wallet, daemon, prepared)
}

/// The build half of [`send_transaction_advanced`]: everything except the relay
/// and the wallet updates (`sendTransaction = false`).
///
/// The result can be handed to [`send_prepared_transaction`] later, which
/// re-checks that the inputs are still spendable first.
pub fn prepare_transaction<D: TransferDaemon, R: TransferRandom>(
    wallet: &Wallet,
    daemon: &D,
    params: &SendParams,
    random: &mut R,
) -> Result<PreparedTransaction> {
    build_with_mixin_fallback(wallet, daemon, params, random)
}

fn build_with_mixin_fallback<D: TransferDaemon, R: TransferRandom>(
    wallet: &Wallet,
    daemon: &D,
    params: &SendParams,
    random: &mut R,
) -> Result<PreparedTransaction> {
    let result = send_transaction_advanced_with_mixin(wallet, daemon, params, params.mixin, random);

    let Err(failure) = result else {
        return result.map_err(|f| f.error);
    };

    if !matches!(failure.error, WalletError::NotEnoughFakeOutputs(_)) {
        return Err(failure.error);
    }

    let min = mixins::mixin_allowable_range(params.network_height).min;
    let Some(retry) = next_fallback_mixin(params.mixin, failure.achievable_mixin, min) else {
        return Err(failure.error);
    };

    let second = send_transaction_advanced_with_mixin(wallet, daemon, params, retry, random);
    match second {
        Ok(p) => Ok(p),
        Err(f) if matches!(f.error, WalletError::NotEnoughFakeOutputs(_)) && retry > min => {
            send_transaction_advanced_with_mixin(wallet, daemon, params, min, random).map_err(|f| f.error)
        }
        Err(f) => Err(f.error),
    }
}

/// `sendTransactionAdvancedWithMixin` (`Transfer.cpp:182`): the pipeline, in
/// its order.
fn send_transaction_advanced_with_mixin<D: TransferDaemon, R: TransferRandom>(
    wallet: &Wallet,
    daemon: &D,
    params: &SendParams,
    mixin: u64,
    random: &mut R,
) -> Inner<PreparedTransaction> {
    let now = now_seconds();
    let network_height = params.network_height;

    // 1. defaults
    let change_address = if params.change_address.is_empty() {
        wallet.primary_address().ok_or(WalletError::IllegalViewWalletOperation)?.to_string()
    } else {
        params.change_address.clone()
    };

    let unlock_time = if params.unlock_time == 0 { default_unlock_time(network_height) } else { params.unlock_time };

    // 2. validation
    validate_transaction_parameters(
        wallet,
        &params.destinations,
        mixin,
        params.fee,
        &params.payment_id,
        &params.addresses_to_take_from,
        &change_address,
        unlock_time,
        network_height,
        now,
    )?;

    // 3. integrated addresses become address + payment id
    let mut addresses_and_amounts = params.destinations.clone();
    let mut payment_id = params.payment_id.clone();
    for (address, _) in addresses_and_amounts.iter_mut() {
        if !base58::is_integrated_address(address) {
            continue;
        }
        let parsed = base58::parse_address(address).map_err(WalletError::InvalidAddress)?;
        payment_id = parsed.payment_id.clone().unwrap_or_default();
        *address = base58::standard_address(&parsed.spend_public_key, &parsed.view_public_key);
    }

    // 4. a short payment id needs exactly one receiver to encrypt to
    let mut recipient_view_key = None;
    if payment_id.len() == SHORT_PAYMENT_ID_LENGTH {
        if addresses_and_amounts.len() != 1 {
            return Err(WalletError::ShortPaymentIdNeedsSingleDestination(addresses_and_amounts.len()).into());
        }
        let parsed = base58::parse_address(&addresses_and_amounts[0].0).map_err(WalletError::InvalidAddress)?;
        recipient_view_key = Some(Hex32(parsed.view_public_key));
    }

    // 5. inputs
    let take_from_all = params.addresses_to_take_from.is_empty();
    let mut spend_keys = Vec::new();
    for address in &params.addresses_to_take_from {
        spend_keys.push(validate_our_address(wallet, address)?);
    }

    let available_inputs = wallet.spendable_transaction_inputs_at(take_from_all, &spend_keys, network_height, now)?;

    let mut total_amount: u64 = addresses_and_amounts.iter().map(|(_, a)| *a).fold(0, u64::wrapping_add);
    if params.fee.is_fixed() {
        total_amount += params.fee.fixed_fee();
    }

    let ctx = BuildContext {
        mixin,
        payment_id: &payment_id,
        recipient_view_key,
        private_view_key: wallet.private_view_key(),
        unlock_time,
        extra_data: &params.extra_data,
        network_height,
        pow_threads: params.pow_threads,
        is_fusion: false,
    };

    // 6. add inputs one at a time until they cover the amount (and the fee)
    let mut our_inputs: Vec<OwnedSpendableInput> = Vec::new();
    let mut sum_of_inputs: u64 = 0;
    let mut change_required: u64 = 0;
    let mut required_amount = total_amount;
    let mut built: Option<Built> = None;

    for input in available_inputs {
        sum_of_inputs = sum_of_inputs.wrapping_add(input.input.amount);
        our_inputs.push(input);

        if sum_of_inputs < total_amount {
            continue;
        }

        change_required = sum_of_inputs - total_amount;
        let destinations = setup_destinations(&addresses_and_amounts, change_required, &change_address)?;

        if params.fee.is_fixed() {
            // The fixed-fee path builds once and then checks the fee against
            // the built size (`Transfer.cpp:419`).
            let candidate = make_transaction(&ctx, daemon, &our_inputs, destinations, random)?;
            let min_fee = fees::minimum_transaction_fee(candidate.size, network_height);
            if params.fee.fixed_fee() >= min_fee {
                built = Some(candidate);
                break;
            }
            return Err(WalletError::FeeTooSmall.into());
        }

        // The fee-per-byte path: guess the size, see whether the inputs cover
        // the fee that implies, then measure.
        let mixins_per_input = vec![mixin; our_inputs.len()];
        let transaction_size = estimate_transaction_size(
            &mixins_per_input,
            destinations.len(),
            !payment_id.is_empty(),
            params.extra_data.len(),
        );

        let fee_per_byte = params.fee.rate(network_height);
        let estimated_fee = fees::transaction_fee(transaction_size, network_height, fee_per_byte);

        if params.send_all {
            let (address, amount) = addresses_and_amounts[0].clone();
            if estimated_fee > amount {
                return Err(WalletError::NotEnoughBalance { needed: estimated_fee, available: amount }.into());
            }
            total_amount -= estimated_fee;
            addresses_and_amounts[0] = (address, amount - estimated_fee);
            change_required = sum_of_inputs - total_amount;
            // `Transfer.cpp:377` rebuilds `destinations` here; nothing reads it
            // again, because the fee loop below recomputes it every round.
        }

        let estimated_amount = total_amount + estimated_fee;

        if sum_of_inputs >= estimated_amount {
            match try_make_fee_per_byte_transaction(
                &ctx,
                daemon,
                sum_of_inputs,
                total_amount,
                estimated_amount,
                fee_per_byte,
                &mut addresses_and_amounts,
                &change_address,
                &our_inputs,
                params.send_all,
                random,
            )? {
                FeeLoopOutcome::Built { built: b, change_required: change } => {
                    built = Some(*b);
                    change_required = change;
                    break;
                }
                FeeLoopOutcome::NeedMoreInputs { needed } => {
                    required_amount = needed;
                    continue;
                }
            }
        } else {
            required_amount = estimated_amount;
        }
    }

    if sum_of_inputs < required_amount {
        return Err(WalletError::NotEnoughBalance { needed: required_amount, available: sum_of_inputs }.into());
    }

    // The loop ended without building anything even though the inputs cover the
    // requirement. The only way in is a zero total with no available inputs,
    // which `validateDestinations` already rules out; the C++ falls through here
    // with a default-constructed `TransactionResult` and its own comment about
    // reading uninitialised stack (`Transfer.cpp:281`). Reporting the balance is
    // the honest version of that.
    let built = built.ok_or(WalletError::NotEnoughBalance { needed: required_amount, available: sum_of_inputs })?;

    // 7. final self-checks, in the C++ order
    is_transaction_payload_too_big(built.size, network_height)?;

    if !verify_amounts(&built.transaction) {
        return Err(WalletError::AmountsNotPretty.into());
    }

    let actual_fee = sum_transaction_fee(&built.transaction)
        .ok_or(WalletError::UnexpectedFee { expected: params.fee.fixed_fee(), actual: 0 })?;

    if !verify_transaction_fee(params.fee, actual_fee, network_height, built.size) {
        return Err(WalletError::UnexpectedFee {
            expected: (params.fee.rate(network_height) * built.size as f64) as u64,
            actual: actual_fee,
        }
        .into());
    }

    Ok(finish(built, actual_fee, mixin, payment_id, change_address, change_required, network_height))
}

/// Wrap a built transaction as a [`PreparedTransaction`], filling in the hash
/// and the fusion classification.
fn finish(
    built: Built,
    fee: u64,
    mixin: u64,
    payment_id: String,
    change_address: String,
    change_required: u64,
    network_height: u64,
) -> PreparedTransaction {
    let hash = built.transaction.hash().unwrap_or([0u8; 32]);
    let is_fusion = is_fusion_transaction(&built.transaction, built.size, network_height);
    PreparedTransaction {
        transaction_hash: Hex32(hash),
        size: built.size,
        fee,
        mixin,
        payment_id,
        change_address,
        change_required,
        inputs: built.inputs,
        outputs: built.outputs,
        tx_private_key: built.tx_private_key,
        tx_public_key: built.tx_public_key,
        rings: built.rings,
        pow_nonce: built.pow_nonce,
        pow_difficulty: built.pow_difficulty,
        pow_hashes: built.pow_hashes,
        is_fusion,
        transaction: built.transaction,
    }
}

/// `Currency::isFusionTransaction(tx, size, height)` (`Currency.cpp:353`;
/// spec/06 "Fusion transactions"), so the wallet can tell whether what it built
/// will be judged as a fusion transaction.
pub fn is_fusion_transaction(tx: &RawTransaction, size: usize, height: u64) -> bool {
    if size > FUSION_TX_MAX_SIZE {
        return false;
    }
    if tx.prefix.inputs.len() < FUSION_TX_MIN_INPUT_COUNT {
        return false;
    }
    if tx.prefix.inputs.len() < FUSION_TX_MIN_IN_OUT_COUNT_RATIO * tx.prefix.outputs.len() {
        return false;
    }

    let threshold = constants::default_fusion_dust_threshold(height);

    let mut total: u64 = 0;
    for input in &tx.prefix.inputs {
        let Input::Key { amount, .. } = input else {
            return false;
        };
        if *amount < threshold {
            return false;
        }
        total = match total.checked_add(*amount) {
            Some(t) => t,
            None => return false,
        };
    }

    if (constants::FUSION_FEE_V1_HEIGHT..constants::FUSION_ZERO_FEE_V2_HEIGHT).contains(&height) {
        total = match total.checked_sub(constants::FUSION_FEE_V1) {
            Some(t) => t,
            None => return false,
        };
    }

    let mut expected = tx::decompose_amount(total, threshold);
    expected.sort_unstable();

    let actual: Vec<u64> = tx.prefix.outputs.iter().map(|o| o.amount).collect();
    expected == actual
}

////////////////////////
/* FUSION             */
////////////////////////

/// `validateFusionTransaction` (`ValidateParameters.cpp:27`): mixin, the
/// subwallets to take from, the destination, and the optimize target.
fn validate_fusion_parameters(wallet: &Wallet, params: &FusionParams, destination: &str) -> Result<()> {
    validate_mixin(params.mixin, params.network_height)?;
    for address in &params.addresses_to_take_from {
        validate_our_address(wallet, address)?;
    }
    validate_our_address(wallet, destination)?;
    if let Some(target) = params.optimize_target {
        // `validateOptimizeTarget`: a single significant digit.
        if !constants::is_pretty_amount(target) {
            return Err(WalletError::AmountUgly);
        }
    }
    Ok(())
}

/// A fusion send with every default: the tier mixin, all subwallets, the
/// primary address, no optimize target.
///
/// **This build of the C++ wallet has no fusion send.** `Transfer.cpp` lost it,
/// and only `SubWallets::getFusionTransactionInputs` (`SubWallets.cpp:615`)
/// survives, which is the selector this reproduces. The transaction built here
/// is therefore defined by what the *daemon* accepts as a fusion transaction
/// (`Currency::isFusionTransaction`, spec/06) rather than by C++ wallet code to
/// diff against: zero fee, at least twelve inputs, at least four inputs per
/// output, outputs that are exactly the ascending decomposition of the input
/// sum, and the fusion proof-of-work difficulty.
pub fn send_fusion_transaction_basic<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    network_height: u64,
    random: &mut R,
) -> Result<PreparedTransaction> {
    send_fusion_transaction_advanced(wallet, daemon, &FusionParams::basic(network_height), random)
}

/// A fusion send with the selection knobs exposed. See
/// [`send_fusion_transaction_basic`] for what defines the shape.
pub fn send_fusion_transaction_advanced<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    params: &FusionParams,
    random: &mut R,
) -> Result<PreparedTransaction> {
    let prepared = prepare_fusion_transaction(wallet, daemon, params, random)?;
    relay_and_store(wallet, daemon, prepared)
}

/// Build a fusion transaction without relaying it.
pub fn prepare_fusion_transaction<D: TransferDaemon, R: TransferRandom>(
    wallet: &Wallet,
    daemon: &D,
    params: &FusionParams,
    random: &mut R,
) -> Result<PreparedTransaction> {
    let now = now_seconds();
    let network_height = params.network_height;

    let destination = if params.destination_address.is_empty() {
        wallet.primary_address().ok_or(WalletError::IllegalViewWalletOperation)?.to_string()
    } else {
        params.destination_address.clone()
    };

    validate_fusion_parameters(wallet, params, &destination)?;

    // At this ring size, do twelve inputs even fit in the fusion size budget?
    if approximate_maximum_input_count(FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, params.mixin)
        < FUSION_TX_MIN_INPUT_COUNT
    {
        return Err(WalletError::FusionMixinTooLarge);
    }

    let take_from_all = params.addresses_to_take_from.is_empty();
    let mut spend_keys = Vec::new();
    for address in &params.addresses_to_take_from {
        spend_keys.push(validate_our_address(wallet, address)?);
    }

    let (mut inputs, _, _) = wallet.fusion_transaction_inputs(
        take_from_all,
        &spend_keys,
        params.mixin,
        network_height,
        now,
        params.optimize_target,
        random,
    )?;

    if inputs.len() < FUSION_TX_MIN_INPUT_COUNT {
        return Err(WalletError::FullyOptimized);
    }

    let keys = base58::parse_address(&destination).map_err(WalletError::InvalidAddress)?;
    let threshold = constants::default_fusion_dust_threshold(network_height);

    let ctx = BuildContext {
        mixin: params.mixin,
        payment_id: "",
        recipient_view_key: None,
        private_view_key: wallet.private_view_key(),
        unlock_time: default_unlock_time(network_height),
        extra_data: &[],
        network_height,
        pow_threads: params.pow_threads,
        is_fusion: true,
    };

    // Drop inputs until the decomposition of their sum satisfies the four
    // inputs per output rule; twelve inputs is the floor below which no fusion
    // transaction exists.
    loop {
        if inputs.len() < FUSION_TX_MIN_INPUT_COUNT {
            return Err(WalletError::FullyOptimized);
        }

        let total: u64 = inputs.iter().map(|i| i.input.amount).fold(0, u64::wrapping_add);
        // `decompose_amount` with the fusion dust threshold is what the daemon
        // compares the outputs against; at every height from 400,000 the
        // threshold is zero and this is `splitAmountIntoDenominations(total,
        // false)`.
        let mut amounts = tx::decompose_amount(total, threshold);
        amounts.sort_unstable();

        if inputs.len() < FUSION_TX_MIN_IN_OUT_COUNT_RATIO * amounts.len() {
            inputs.pop();
            continue;
        }

        let destinations: Vec<TransactionDestination> = amounts
            .iter()
            .map(|amount| TransactionDestination {
                amount: *amount,
                receiver_public_spend_key: Hex32(keys.spend_public_key),
                receiver_public_view_key: Hex32(keys.view_public_key),
            })
            .collect();

        let built = make_transaction(&ctx, daemon, &inputs, destinations, random).map_err(|f| f.error)?;

        is_transaction_payload_too_big(built.size, network_height)?;

        if built.size > FUSION_TX_MAX_SIZE {
            inputs.pop();
            continue;
        }

        if !is_fusion_transaction(&built.transaction, built.size, network_height) {
            inputs.pop();
            continue;
        }

        let fee = sum_transaction_fee(&built.transaction).unwrap_or(0);
        if !fees::is_valid_fusion_fee(fee, network_height) {
            return Err(WalletError::FeeTooSmall);
        }

        return Ok(finish(built, fee, params.mixin, String::new(), destination, 0, network_height));
    }
}

////////////////////////
/* RELAY AND RECORD   */
////////////////////////

/// `SendTransaction::relayTransaction` (`Transfer.cpp:764`): the daemon answers
/// HTTP 200 either way, with `status` `OK` or an `error` string.
fn relay_transaction<D: TransferDaemon>(daemon: &D, tx_hex: &str) -> Result<()> {
    let result = daemon.send_raw_transaction(tx_hex).map_err(|e| WalletError::DaemonOffline(e.to_string()))?;
    if result.status != "OK" {
        return Err(WalletError::DaemonError(result.error.unwrap_or(result.status)));
    }
    Ok(())
}

/// The relay half of a send, on its own: hand a transaction built by
/// [`prepare_transaction`] to the daemon and change nothing in any wallet.
///
/// For a caller that builds from one copy of a wallet and records the send
/// into another once the relay has succeeded ([`apply_sent_transaction`]):
/// `wrkz-wallet-api` builds from the copy its read-only routes answer from, so
/// that neither the proof of work nor the daemon holds the wallet it serves.
pub fn relay_prepared_transaction<D: TransferDaemon>(daemon: &D, prepared: &PreparedTransaction) -> Result<()> {
    relay_transaction(daemon, &prepared.to_hex())
}

/// `SendTransaction::sendPreparedTransaction` (`Transfer.cpp:519`): re-check
/// that every input is still spendable, then relay and record.
pub fn send_prepared_transaction<D: TransferDaemon>(
    wallet: &mut Wallet,
    daemon: &D,
    prepared: PreparedTransaction,
    network_height: u64,
) -> Result<PreparedTransaction> {
    let now = now_seconds();
    for input in &prepared.inputs {
        if !wallet.have_spendable_input_at(&input.input, network_height, now) {
            return Err(WalletError::PreparedTransactionExpired);
        }
    }
    relay_and_store(wallet, daemon, prepared)
}

/// Relay, then apply every change a send makes to the wallet
/// (`Transfer.cpp:487-513`), in that order:
///
/// 1. record the unconfirmed outgoing transaction (negative transfers per
///    spending key, positive change);
/// 2. store our own outputs as unconfirmed incoming amounts;
/// 3. keep the transaction private key;
/// 4. move every spent input into `lockedInputs`.
fn relay_and_store<D: TransferDaemon>(
    wallet: &mut Wallet,
    daemon: &D,
    prepared: PreparedTransaction,
) -> Result<PreparedTransaction> {
    relay_transaction(daemon, &prepared.to_hex())?;
    apply_sent_transaction(wallet, &prepared);
    Ok(prepared)
}

/// The wallet-side half of a successful send, split out so a test can drive it
/// without a daemon and so `sendPreparedTransaction` can reuse it.
pub fn apply_sent_transaction(wallet: &mut Wallet, prepared: &PreparedTransaction) {
    wallet.store_sent_transaction(
        prepared.transaction_hash,
        prepared.fee,
        &prepared.payment_id,
        &prepared.inputs,
        &prepared.change_address,
        prepared.change_required,
    );

    wallet.store_unconfirmed_incoming_inputs(&prepared.outputs, &prepared.tx_public_key, prepared.transaction_hash);

    wallet.store_tx_private_key(prepared.transaction_hash, prepared.tx_private_key.clone());

    for input in &prepared.inputs {
        wallet.mark_input_as_locked(&input.input.key_image, &input.public_spend_key);
    }
}

impl Wallet {
    /// `SubWallets::haveSpendableInput` (`SubWallets.cpp:505`): the input is
    /// still in `unspentInputs` and still unlocked.
    pub fn have_spendable_input_at(&self, input: &TransactionInput, height: u64, now: u64) -> bool {
        self.sub_wallets.sub_wallet.iter().any(|sub| {
            sub.unspent_inputs.iter().any(|i| i.key == input.key && is_input_unlocked_at(i.unlock_time, height, now))
        })
    }

    /// `SendTransaction::storeSentTransaction` (`Transfer.cpp:723`) plus
    /// `SubWallets::addUnconfirmedTransaction` (`SubWallets.cpp:337`).
    ///
    /// One negative transfer per spending key and, when there is change, a
    /// positive one for the change address's key. Height, timestamp and unlock
    /// time are zero until the transaction is seen in a block. A hash already
    /// in `lockedTransactions` is ignored.
    pub fn store_sent_transaction(
        &mut self,
        hash: Hash,
        fee: u64,
        payment_id: &str,
        inputs: &[OwnedSpendableInput],
        change_address: &str,
        change_required: u64,
    ) {
        let mut transfers: Vec<Transfer> = Vec::new();
        let mut add = |key: PublicKey, delta: i64| {
            if let Some(t) = transfers.iter_mut().find(|t| t.public_key == key) {
                t.amount = t.amount.saturating_add(delta);
            } else {
                transfers.push(Transfer { amount: delta, public_key: key });
            }
        };

        for input in inputs {
            add(input.public_spend_key, -(input.input.amount as i64));
        }

        if change_required != 0 {
            if let Ok(parsed) = base58::parse_address(change_address) {
                add(Hex32(parsed.spend_public_key), change_required as i64);
            }
        }

        if self.sub_wallets.locked_transactions.iter().any(|t| t.hash == hash) {
            return;
        }

        self.sub_wallets.locked_transactions.push(Transaction {
            block_height: 0,
            fee,
            hash,
            is_coinbase_transaction: false,
            payment_id: payment_id.to_string(),
            timestamp: 0,
            transfers,
            unlock_time: 0,
        });
    }

    /// `SendTransaction::storeUnconfirmedIncomingInputs` (`Transfer.cpp:690`):
    /// underive every output key and record the ones that come back to us, so
    /// change shows as locked balance until it confirms.
    pub fn store_unconfirmed_incoming_inputs(
        &mut self,
        outputs: &[KeyOutput],
        tx_public_key: &PublicKey,
        tx_hash: Hash,
    ) {
        let view_key = self.private_view_key().clone();
        let Some(derivation) = curve::generate_key_derivation(tx_public_key.as_bytes(), view_key.as_bytes()) else {
            return;
        };

        for (index, output) in outputs.iter().enumerate() {
            let Some(spend_key) = curve::underive_public_key(&derivation, index as u64, output.key.as_bytes()) else {
                continue;
            };
            let spend_key = Hex32(spend_key);
            if !self.is_our_spend_key(&spend_key) {
                continue;
            }
            if let Some(sub) = self.sub_wallets.sub_wallet.iter_mut().find(|s| s.public_spend_key == spend_key) {
                sub.unconfirmed_incoming_amounts.push(UnconfirmedInput {
                    amount: output.amount,
                    key: output.key,
                    parent_transaction_hash: tx_hash,
                });
            }
        }
    }

    /// `SubWallets::storeTxPrivateKey` (`SubWallets.cpp:1055`).
    pub fn store_tx_private_key(&mut self, transaction_hash: Hash, tx_private_key: SecretKey) {
        if let Some(existing) =
            self.sub_wallets.tx_private_keys.iter_mut().find(|k| k.transaction_hash == transaction_hash)
        {
            existing.tx_private_key = tx_private_key;
            return;
        }
        self.sub_wallets.tx_private_keys.push(crate::file::TxPrivateKey { transaction_hash, tx_private_key });
    }

    /// `SubWallets::getTxPrivateKey` (`SubWallets.cpp:1060`).
    pub fn tx_private_key(&self, transaction_hash: &Hash) -> Option<&SecretKey> {
        self.sub_wallets
            .tx_private_keys
            .iter()
            .find(|k| k.transaction_hash == *transaction_hash)
            .map(|k| &k.tx_private_key)
    }
}

////////////////////////
/* SWEEP              */
////////////////////////

/// One batch of a sweep: the hash it relayed, or why it failed.
///
/// `WalletBackend::sweepToAddress` returns `std::vector<std::tuple<Error, Hash>>`
/// (`WalletBackend.cpp:1040`) and the API prints one object per entry, so a
/// per-batch error is part of a **200** response rather than a failure of the
/// whole call.
pub type SweepResult = Result<Hash>;

/// The C++ leaves room for two outputs — one destination, one change — when it
/// works out how many inputs fit (`WalletBackend.cpp:1106`).
const SWEEP_OUTPUT_ALLOWANCE: usize = 2;

/// The nine bytes `makeTransaction` appends for the proof-of-work nonce, which
/// the size estimate has to pay for (`WalletBackend.cpp:1163`).
const SWEEP_POW_NONCE_OVERHEAD: usize = 9;

/// How many transactions a sweep would take and what they would cost in fees
/// (`WalletBackend::estimateSweep`, `WalletBackend.cpp:1395`).
///
/// `(0, 0)` when nothing can be swept: no spendable inputs, or not even one
/// input fits in a transaction at this ring size.
pub fn estimate_sweep(wallet: &Wallet, payment_id: &str, amount_to_sweep: u64, network_height: u64) -> (usize, u64) {
    let mixin = mixins::mixin_allowable_range(network_height).default;
    let Some(batches) = sweep_batches(wallet, payment_id, amount_to_sweep, network_height, mixin) else {
        return (0, 0);
    };

    let mut tx_count = 0usize;
    let mut total_fee = 0u64;

    for batch in &batches {
        let batch_sum: u64 = batch.iter().map(|i| i.input.amount).sum();
        let fee = sweep_batch_fee_estimate(batch.len(), batch_sum, payment_id, amount_to_sweep, network_height, mixin);
        if fee >= batch_sum {
            continue;
        }
        tx_count += 1;
        total_fee += fee;
    }

    (tx_count, total_fee)
}

/// `WalletBackend::sweepToAddress` (`WalletBackend.cpp:1040`): send an amount —
/// or the whole unlocked balance, when `amount_to_sweep` is zero — to one
/// address across as many transactions as it takes, with no fusion first.
///
/// Every batch that relays is recorded in `wallet` before the next is built, so
/// a partial sweep leaves a consistent wallet. The returned vector has one
/// entry per batch, in order; a rejected destination or a view wallet is a
/// single `Err` entry, which is what the C++ returns.
pub fn sweep_to_address<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    destination: &str,
    payment_id: &str,
    amount_to_sweep: u64,
    network_height: u64,
    random: &mut R,
) -> Vec<SweepResult> {
    sweep_to_address_reporting(
        wallet,
        daemon,
        destination,
        payment_id,
        amount_to_sweep,
        network_height,
        random,
        &mut |_| {},
    )
}

/// [`sweep_to_address`], handing each batch to `on_sent` the moment it has
/// been relayed and recorded in `wallet`, before the next one is built.
///
/// A caller sweeping a copy of the wallet records each batch into the wallet
/// it serves from there, so a sweep of many batches shows each spend as it
/// happens rather than all of them at the end.
pub fn sweep_to_address_reporting<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    destination: &str,
    payment_id: &str,
    amount_to_sweep: u64,
    network_height: u64,
    random: &mut R,
    on_sent: &mut dyn FnMut(&PreparedTransaction),
) -> Vec<SweepResult> {
    if wallet.is_view_wallet() {
        return vec![Err(WalletError::IllegalViewWalletOperation)];
    }

    // `validateAddresses({destination}, true)` — integrated allowed.
    let parsed = match base58::parse_address(destination) {
        Ok(p) => p,
        Err(e) => return vec![Err(WalletError::InvalidAddress(e))],
    };

    let mut resolved_destination = destination.to_string();
    let mut resolved_payment_id = payment_id.to_string();
    if let Some(embedded) = parsed.payment_id.clone() {
        resolved_destination = base58::standard_address(&parsed.spend_public_key, &parsed.view_public_key);
        if resolved_payment_id.is_empty() {
            resolved_payment_id = embedded;
        }
    }

    if let Err(e) = validate_payment_id(&resolved_payment_id) {
        return vec![Err(e)];
    }

    // A short payment id is encrypted to the one receiver every batch pays.
    let recipient_view_key =
        if resolved_payment_id.len() == SHORT_PAYMENT_ID_LENGTH { Some(Hex32(parsed.view_public_key)) } else { None };

    let range = mixins::mixin_allowable_range(network_height);
    let mixin = range.default;
    let unlock_time = default_unlock_time(network_height);
    let Some(change_address) = wallet.primary_address().map(str::to_string) else {
        return vec![Err(WalletError::IllegalViewWalletOperation)];
    };

    let max_size = fees::wallet_max_tx_size(network_height);
    if approximate_maximum_input_count(max_size as usize, SWEEP_OUTPUT_ALLOWANCE, mixin) == 0 {
        return vec![Err(WalletError::TooManyInputsToFitInBlock { size: 0, max: max_size })];
    }

    let Some(batches) = sweep_batches(wallet, &resolved_payment_id, amount_to_sweep, network_height, mixin) else {
        return vec![Err(WalletError::NotEnoughBalance { needed: amount_to_sweep, available: 0 })];
    };

    let mut results = Vec::with_capacity(batches.len());

    for batch in batches {
        let batch_sum: u64 = batch.iter().map(|i| i.input.amount).sum();
        let mut batch_fee = sweep_batch_fee_estimate(
            batch.len(),
            batch_sum,
            &resolved_payment_id,
            amount_to_sweep,
            network_height,
            mixin,
        );

        if batch_fee >= batch_sum {
            // The fee would eat the whole batch; the C++ skips it with an error
            // entry rather than abandoning the sweep.
            results.push(Err(WalletError::NotEnoughBalance { needed: batch_fee, available: batch_sum }));
            continue;
        }

        let params = SweepBatchParams {
            destination: &resolved_destination,
            payment_id: &resolved_payment_id,
            recipient_view_key,
            change_address: &change_address,
            amount_to_sweep,
            unlock_time,
            network_height,
            mixin,
            min_mixin: range.min,
        };

        results.push(sweep_one_batch(wallet, daemon, &batch, batch_sum, &mut batch_fee, params, random, on_sent));
    }

    results
}

struct SweepBatchParams<'a> {
    destination: &'a str,
    payment_id: &'a str,
    recipient_view_key: Option<PublicKey>,
    change_address: &'a str,
    amount_to_sweep: u64,
    unlock_time: u64,
    network_height: u64,
    mixin: u64,
    min_mixin: u64,
}

/// Split the spendable inputs into the batches a sweep would send, or `None`
/// when there are none. Shared by [`sweep_to_address`] and [`estimate_sweep`]
/// so the estimate cannot drift from what the sweep does.
fn sweep_batches(
    wallet: &Wallet,
    payment_id: &str,
    amount_to_sweep: u64,
    network_height: u64,
    mixin: u64,
) -> Option<Vec<Vec<OwnedSpendableInput>>> {
    let max_inputs_per_tx = approximate_maximum_input_count(
        fees::wallet_max_tx_size(network_height) as usize,
        SWEEP_OUTPUT_ALLOWANCE,
        mixin,
    );
    if max_inputs_per_tx == 0 {
        return None;
    }

    let mut all = wallet.spendable_transaction_inputs(true, &[], network_height).ok()?;
    if all.is_empty() {
        return None;
    }

    if amount_to_sweep > 0 {
        // Take only as many inputs as cover the amount plus a worst-case fee
        // for each batch they would need (`WalletBackend.cpp:1114`).
        let worst_case_size = estimate_transaction_size(&vec![mixin; max_inputs_per_tx], 1, !payment_id.is_empty(), 0);
        let fee_per_batch = fees::minimum_transaction_fee(worst_case_size, network_height);

        let mut sum: u64 = 0;
        let mut needed = Vec::new();
        for input in all {
            sum = sum.saturating_add(input.input.amount);
            needed.push(input);
            let batch_count = needed.len().div_ceil(max_inputs_per_tx) as u64;
            if sum >= amount_to_sweep.saturating_add(batch_count.saturating_mul(fee_per_batch)) {
                break;
            }
        }
        all = needed;
    }

    Some(all.chunks(max_inputs_per_tx).map(<[OwnedSpendableInput]>::to_vec).collect())
}

/// The three-round output-count/fee fixed point of `WalletBackend.cpp:1160`.
fn sweep_batch_fee_estimate(
    batch_len: usize,
    batch_sum: u64,
    payment_id: &str,
    amount_to_sweep: u64,
    network_height: u64,
    mixin: u64,
) -> u64 {
    let mut num_outputs = 1usize;
    let mut estimated_fee = 0u64;

    for _ in 0..3 {
        let size = estimate_transaction_size(&vec![mixin; batch_len], num_outputs, !payment_id.is_empty(), 0)
            + SWEEP_POW_NONCE_OVERHEAD;
        estimated_fee = fees::minimum_transaction_fee(size, network_height);

        if estimated_fee >= batch_sum {
            break;
        }

        let available = batch_sum - estimated_fee;
        let new_num_outputs = if amount_to_sweep > 0 {
            let dest = amount_to_sweep.min(available);
            let change = available.saturating_sub(amount_to_sweep);
            split_amount_into_denominations(dest, true).len()
                + if change > 0 { split_amount_into_denominations(change, true).len() } else { 0 }
        } else {
            split_amount_into_denominations(available, true).len()
        };

        if new_num_outputs == num_outputs {
            break;
        }
        num_outputs = new_num_outputs;
    }

    estimated_fee
}

/// Build, re-weigh the fee against the built size, relay and record one batch
/// (`WalletBackend.cpp:1231`).
fn sweep_one_batch<D: TransferDaemon, R: TransferRandom>(
    wallet: &mut Wallet,
    daemon: &D,
    batch: &[OwnedSpendableInput],
    batch_sum: u64,
    batch_fee: &mut u64,
    params: SweepBatchParams<'_>,
    random: &mut R,
    on_sent: &mut dyn FnMut(&PreparedTransaction),
) -> Result<Hash> {
    let mut batch_mixin = params.mixin;
    let mut change_required = 0u64;
    let mut built: Option<Built> = None;

    for _ in 0..3 {
        if *batch_fee >= batch_sum {
            return Err(WalletError::NotEnoughBalance { needed: *batch_fee, available: batch_sum });
        }

        let available = batch_sum - *batch_fee;
        let destination_amount =
            if params.amount_to_sweep > 0 { params.amount_to_sweep.min(available) } else { available };
        change_required = if params.amount_to_sweep > 0 && available > params.amount_to_sweep {
            available - params.amount_to_sweep
        } else {
            0
        };

        let destinations = setup_destinations(
            &[(params.destination.to_string(), destination_amount)],
            change_required,
            params.change_address,
        )?;

        let mut ctx = BuildContext {
            mixin: batch_mixin,
            payment_id: params.payment_id,
            recipient_view_key: params.recipient_view_key,
            private_view_key: wallet.private_view_key(),
            unlock_time: params.unlock_time,
            extra_data: &[],
            network_height: params.network_height,
            pow_threads: default_pow_threads(),
            is_fusion: false,
        };

        let mut result = make_transaction(&ctx, daemon, batch, destinations.clone(), random);

        // The same ring fallback `sendTransactionAdvanced` applies: sweep
        // deliberately gathers thin denominations, so one of them running out
        // of decoys must not fail the whole batch (`WalletBackend.cpp:1265`).
        for attempt in 0..2 {
            let Err(failure) = &result else { break };
            if !matches!(failure.error, WalletError::NotEnoughFakeOutputs(_)) {
                break;
            }
            let achievable = if attempt == 0 { failure.achievable_mixin } else { params.min_mixin };
            let Some(retry) = next_fallback_mixin(batch_mixin, achievable, params.min_mixin) else {
                break;
            };
            batch_mixin = retry;
            ctx.mixin = retry;
            result = make_transaction(&ctx, daemon, batch, destinations.clone(), random);
        }

        let candidate = result.map_err(|f| f.error)?;
        let required_fee = fees::minimum_transaction_fee(candidate.size, params.network_height);

        if *batch_fee >= required_fee {
            built = Some(candidate);
            break;
        }

        // The estimate came out under what the finished transaction costs; pay
        // the real price and build again (`WalletBackend.cpp:1320`).
        *batch_fee = required_fee;
    }

    let built = built.ok_or(WalletError::NotEnoughBalance { needed: *batch_fee, available: batch_sum })?;

    is_transaction_payload_too_big(built.size, params.network_height)?;

    if !verify_amounts(&built.transaction) {
        return Err(WalletError::AmountsNotPretty);
    }

    let actual_fee = sum_transaction_fee(&built.transaction)
        .ok_or(WalletError::UnexpectedFee { expected: *batch_fee, actual: 0 })?;

    let prepared = finish(
        built,
        actual_fee,
        batch_mixin,
        params.payment_id.to_string(),
        params.change_address.to_string(),
        change_required,
        params.network_height,
    );

    relay_transaction(daemon, &prepared.to_hex())?;
    apply_sent_transaction(wallet, &prepared);
    on_sent(&prepared);

    Ok(prepared.transaction_hash)
}

/// Every core, which is what an interactive send passes.
fn default_pow_threads() -> usize {
    crate::platform::available_threads()
}

#[cfg(test)]
mod tests;
