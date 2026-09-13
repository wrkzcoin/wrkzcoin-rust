// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The wallet file: header, cipher, JSON schema, open and save.
//!
//! Sources: `src/walletbackend/WalletBackend.cpp` (`openWallet` line 496,
//! `saveWalletJSONToDisk` line 630, `unsafeToJSON`/`fromJSON` line 1935),
//! `src/walletbackend/Constants.h`, `src/subwallets/SubWallets.cpp`
//! (`toJSON` line 1183, `fromJSON` line 1118), `src/subwallets/SubWallet.cpp`,
//! `include/WalletTypes.h` (`TransactionInput`, `Transaction`,
//! `UnconfirmedInput`), `src/walletbackend/SynchronizationStatus.cpp`,
//! `src/walletbackend/WalletSynchronizer.cpp` (line 974), `include/JsonHelper.h`.
//! Spec: `spec/10-wallet.md` "Wallet file", `spec/03-crypto-primitives.md`
//! "Wallet file encryption", `spec/05-addresses-keys-mnemonics.md`.
//!
//! # File layout
//!
//! ```text
//! bytes 0..64    IS_A_WALLET_IDENTIFIER      (plaintext)
//! bytes 64..80   salt                        (plaintext, 16 random bytes)
//! bytes 80..     AES-128-CBC(PKCS#7) of  IS_CORRECT_PASSWORD_IDENTIFIER ‖ JSON
//!                key = PBKDF2-HMAC-SHA256(password, salt, 500000, 16), iv = salt
//! ```
//!
//! A wrong password is *only* ever reported as [`WalletError::WrongPassword`]:
//! the C++ code deliberately does not distinguish a padding failure from a
//! wrong identifier, because the distinction is a padding oracle
//! (`WalletBackend.cpp:565`).
//!
//! # Field order is part of the format
//!
//! The C++ side serializes with `nlohmann::json`, whose default object type is
//! `std::map<std::string, ...>`, so `dump()` emits **keys in lexicographic
//! order**, not insertion order, with no whitespace. Every struct below
//! declares its fields in exactly that order and `serde_json::to_vec` reproduces
//! the bytes. Do not reorder a field without reordering the C++ key it stands
//! for — the order, not just the names, is what makes a file we write open in
//! the C++ `wrkz-wallet` CLI byte for byte.
//!
//! Two containers the C++ holds in `std::unordered_map` have no defined order
//! on its side: `subWallets.subWallet`, `subWallets.txPrivateKeys` and a
//! transaction's `transfers`. This module preserves whatever order it read (and
//! appends in insertion order), which round-trips a C++ file unchanged but
//! cannot be predicted from the wallet's contents alone.
//!
//! # Where the C++ reader is lenient
//!
//! Only in three places, all mirrored here with `#[serde(default)]`:
//!
//! - `subWallets.subWalletIndexCounter` (`SubWallets.cpp:1130`, `contains`),
//! - `subWallet[].walletIndex` (`SubWallet.cpp`, `contains`),
//! - `transactionInput.privateEphemeral` (`WalletTypes.h:229`, `contains`).
//!
//! Everything else goes through `getUint64FromJSON` / `j.at()`, which throw on a
//! missing key; the throw is caught in `openWallet` and reported as
//! [`WalletError::WalletFileCorrupted`]. Unknown keys are ignored everywhere,
//! by both readers.
//!
//! The file does **not** store the daemon host, port or SSL flag: they are
//! arguments to `WalletBackend::openWallet` and are re-supplied on every open
//! (`WalletBackend.cpp:496`). Nor does it store the API password.

use crate::crypto::{decrypt_wallet_file, encrypt_wallet_file, random_salt, SALT_SIZE};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::Path;
use wrkz_pow::curve;
use wrkz_primitives::base58::{self, Base58Error};
use wrkz_primitives::constants::{
    BLOCK_HASH_CHECKPOINTS_INTERVAL, CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT, CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3,
    CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4, DIFFICULTY_TARGET, GENESIS_BLOCK_TIMESTAMP, INTEGRATED_ADDRESS_LENGTH,
    INTEGRATED_ADDRESS_LENGTH_LONG, IS_A_WALLET_IDENTIFIER, IS_CORRECT_PASSWORD_IDENTIFIER,
    LAST_KNOWN_BLOCK_HASHES_SIZE, STANDARD_ADDRESS_LENGTH, WALLET_FILE_FORMAT_VERSION,
};
use wrkz_primitives::mnemonic::{self, MnemonicError};
use zeroize::{Zeroize, Zeroizing};

/// Length of the plaintext wallet marker at the front of the file.
pub const IS_A_WALLET_IDENTIFIER_SIZE: usize = IS_A_WALLET_IDENTIFIER.len();

/// Length of the encrypted "the password was right" marker.
pub const IS_CORRECT_PASSWORD_IDENTIFIER_SIZE: usize = IS_CORRECT_PASSWORD_IDENTIFIER.len();

/// Offset of the ciphertext: the 64-byte marker plus the 16-byte salt.
pub const CIPHERTEXT_OFFSET: usize = IS_A_WALLET_IDENTIFIER_SIZE + SALT_SIZE;

//////////////
/* ERRORS   */
//////////////

/// Everything `open`, `save` and the constructors can fail with.
///
/// [`WalletError::code`] gives the numeric code from the C++ `src/errors/Errors.h`,
/// which the C API of stage 2.5 has to keep because the front ends switch on it.
#[derive(Debug)]
pub enum WalletError {
    /// `FILENAME_NON_EXISTENT` (1).
    FilenameNonExistent,
    /// `INVALID_WALLET_FILENAME` (2): could not write where we were told to.
    InvalidWalletFilename(std::io::Error),
    /// `NOT_A_WALLET_FILE` (3): the 64-byte marker is missing or wrong. The C++
    /// code then tries the legacy `WalletGreen` format; this port does not.
    NotAWalletFile,
    /// `WALLET_FILE_CORRUPTED` (4): too short for the salt or the password
    /// marker, or the JSON did not parse / was missing a required field.
    WalletFileCorrupted(String),
    /// `WRONG_PASSWORD` (5): decryption failed, or the decrypted bytes did not
    /// start with the password marker. Never distinguished further.
    WrongPassword,
    /// `UNSUPPORTED_WALLET_FILE_FORMAT_VERSION` (6).
    UnsupportedWalletFileFormatVersion(u64),
    /// `INVALID_MNEMONIC` (7).
    InvalidMnemonic(MnemonicError),
    /// `WALLET_FILE_ALREADY_EXISTS` (8).
    WalletFileAlreadyExists,
    /// `ADDRESS_*` (12-15), depending on the reason.
    InvalidAddress(Base58Error),
    /// `SUBWALLET_ALREADY_EXISTS` (38).
    SubWalletAlreadyExists,
    /// `ILLEGAL_VIEW_WALLET_OPERATION` (39).
    IllegalViewWalletOperation,
    /// `INVALID_PRIVATE_KEY` (52): `sc_check` rejected the scalar.
    InvalidPrivateKey,
    /// `INVALID_PUBLIC_KEY` (51): a key that should be a curve point is not, or
    /// a key derivation could not be computed from it.
    InvalidPublicKey,

    /// `ILLEGAL_NON_VIEW_WALLET_OPERATION` (40): a view-only operation asked
    /// of a wallet that holds spend keys.
    IllegalNonViewWalletOperation,
    /// `KEYS_NOT_DETERMINISTIC` (41): the view key of this address is not
    /// derived from its spend key, so it has no mnemonic seed.
    KeysNotDeterministic,
    /// `CANNOT_DELETE_PRIMARY_ADDRESS` (42).
    CannotDeletePrimaryAddress,
    /// `TX_PRIVATE_KEY_NOT_FOUND` (43).
    TxPrivateKeyNotFound,
    /// `HASH_WRONG_LENGTH` (48): a transaction hash that is not 64 characters.
    HashWrongLength,
    /// `HASH_INVALID` (49): a transaction hash that is not hex.
    HashInvalid,
    /// `INVALID_EXTRA_DATA` (53): the `extra` field given was not hex.
    InvalidExtraData,
    /// `PREPARED_TRANSACTION_NOT_FOUND` (58).
    PreparedTransactionNotFound,
    /// `LITE_NODE_CANNOT_RESCAN_THAT_LOW` (62): the daemon holds no block data
    /// below its lite height, so a rescan from lower would silently drop
    /// transactions this wallet already holds.
    LiteNodeCannotRescanThatLow(String),

    // ---- transaction construction (`transfer`), `src/errors/Errors.h` ------
    /// `WILL_OVERFLOW` (9): the destination amounts (plus a fixed fee) sum past
    /// `u64::MAX` (`validateAmount`).
    WillOverflow,
    /// `ADDRESS_NOT_IN_WALLET` (10): a subwallet-to-take-from or the change
    /// address is not one of ours (`validateOurAddresses`).
    AddressNotInWallet(String),
    /// `NOT_ENOUGH_BALANCE` (11): the unlocked balance of the chosen subwallets
    /// does not cover the amount plus the fee. `needed` is what the last
    /// estimate asked for, `available` what was available to cover it.
    NotEnoughBalance { needed: u64, available: u64 },
    /// `INTEGRATED_ADDRESS_PAYMENT_ID_INVALID` (16).
    IntegratedAddressPaymentIdInvalid,
    /// `FEE_TOO_SMALL` (17): the fee per byte is below the network minimum, or
    /// a fixed fee is below `getMinimumTransactionFee` for the built size.
    FeeTooSmall,
    /// `NO_DESTINATIONS_GIVEN` (18).
    NoDestinationsGiven,
    /// `AMOUNT_IS_ZERO` (19).
    AmountIsZero,
    /// `FAILED_TO_CREATE_RING_SIGNATURE` (20).
    FailedToCreateRingSignature,
    /// `MIXIN_TOO_SMALL` (21).
    MixinTooSmall { mixin: u64, min: u64 },
    /// `MIXIN_TOO_BIG` (22).
    MixinTooBig { mixin: u64, max: u64 },
    /// `PAYMENT_ID_WRONG_LENGTH` (23): neither 16 nor 64 characters.
    PaymentIdWrongLength(usize),
    /// `PAYMENT_ID_INVALID` (24): not hex, or the short id could not be
    /// encrypted to the receiver.
    PaymentIdInvalid,
    /// `ADDRESS_IS_INTEGRATED` (25): an integrated address where only a
    /// standard one is allowed (the change address, a subwallet to take from).
    AddressIsIntegrated(String),
    /// `CONFLICTING_PAYMENT_IDS` (26): two integrated addresses with different
    /// payment ids, or an integrated address plus a different explicit one.
    ConflictingPaymentIds,
    /// `CANT_GET_FAKE_OUTPUTS` (27).
    CantGetFakeOutputs(String),
    /// `NOT_ENOUGH_FAKE_OUTPUTS` (28): the chain does not hold enough outputs
    /// of some denomination to build a ring of the requested size.
    NotEnoughFakeOutputs(String),
    /// `DAEMON_OFFLINE` (30).
    DaemonOffline(String),
    /// `DAEMON_ERROR` (31): the daemon answered, and refused.
    DaemonError(String),
    /// `TOO_MANY_INPUTS_TO_FIT_IN_BLOCK` (32): the serialized transaction is
    /// above `Utilities::getMaxTxSize(height)`.
    TooManyInputsToFitInBlock { size: u64, max: u64 },
    /// `FULLY_OPTIMIZED` (36): no fusion transaction can be built.
    FullyOptimized,
    /// `FUSION_MIXIN_TOO_LARGE` (37): at that ring size, fewer than
    /// `FUSION_TX_MIN_INPUT_COUNT` inputs fit in `FUSION_TX_MAX_SIZE`.
    FusionMixinTooLarge,
    /// `AMOUNTS_NOT_PRETTY` (44): an output amount is not in `PRETTY_AMOUNTS`.
    AmountsNotPretty,
    /// `UNEXPECTED_FEE` (45): the built fee is outside what was asked for.
    UnexpectedFee { expected: u64, actual: u64 },
    /// `OUTPUT_DECOMPOSITION` (56): more than
    /// `NORMAL_TX_MAX_OUTPUT_COUNT_V1` (90) outputs.
    OutputDecomposition { outputs: usize, max: usize },
    /// `PREPARED_TRANSACTION_EXPIRED` (57): an input of a prepared transaction
    /// is no longer spendable.
    PreparedTransactionExpired,
    /// `AMOUNT_UGLY` (59): a fusion optimize target with more than one
    /// significant digit.
    AmountUgly,
    /// `UNLOCK_TIME_TOO_SMALL` (60).
    UnlockTimeTooSmall { unlock_time: u64, minimum: u64 },
    /// `SHORT_PAYMENT_ID_NEEDS_SINGLE_DESTINATION` (61).
    ShortPaymentIdNeedsSingleDestination(usize),
    /// Not a C++ code: the transaction proof-of-work search exhausted the whole
    /// nonce space without a hash meeting the difficulty. Unreachable in
    /// practice; reported as `UNKNOWN_ERROR` (54).
    TransactionPoWFailed,
    /// Not a C++ code: reading or writing the file itself failed.
    Io(std::io::Error),
    /// Not a C++ code: serializing the wallet to JSON failed.
    Json(String),
}

impl WalletError {
    /// The numeric code of `src/errors/Errors.h`. Zero is `SUCCESS`, which no
    /// error maps to; the two variants with no C++ counterpart report the code
    /// the C++ would have used for the same situation.
    pub fn code(&self) -> u16 {
        match self {
            WalletError::FilenameNonExistent => 1,
            WalletError::InvalidWalletFilename(_) => 2,
            WalletError::NotAWalletFile => 3,
            WalletError::WalletFileCorrupted(_) | WalletError::Json(_) => 4,
            WalletError::WrongPassword => 5,
            WalletError::UnsupportedWalletFileFormatVersion(_) => 6,
            WalletError::InvalidMnemonic(_) => 7,
            WalletError::WalletFileAlreadyExists => 8,
            WalletError::InvalidAddress(e) => match e {
                Base58Error::WrongAddressLength(_) => 12,
                Base58Error::WrongPrefix(_) => 13,
                Base58Error::InvalidCharacter { .. } | Base58Error::InvalidLength(_) => 14,
                _ => 15,
            },
            WalletError::SubWalletAlreadyExists => 38,
            WalletError::IllegalViewWalletOperation => 39,
            WalletError::InvalidPrivateKey => 52,
            WalletError::InvalidPublicKey => 51,
            WalletError::Io(_) => 2,
            WalletError::IllegalNonViewWalletOperation => 40,
            WalletError::KeysNotDeterministic => 41,
            WalletError::CannotDeletePrimaryAddress => 42,
            WalletError::TxPrivateKeyNotFound => 43,
            WalletError::HashWrongLength => 48,
            WalletError::HashInvalid => 49,
            WalletError::InvalidExtraData => 53,
            WalletError::PreparedTransactionNotFound => 58,
            WalletError::LiteNodeCannotRescanThatLow(_) => 62,

            WalletError::WillOverflow => 9,
            WalletError::AddressNotInWallet(_) => 10,
            WalletError::NotEnoughBalance { .. } => 11,
            WalletError::IntegratedAddressPaymentIdInvalid => 16,
            WalletError::FeeTooSmall => 17,
            WalletError::NoDestinationsGiven => 18,
            WalletError::AmountIsZero => 19,
            WalletError::FailedToCreateRingSignature => 20,
            WalletError::MixinTooSmall { .. } => 21,
            WalletError::MixinTooBig { .. } => 22,
            WalletError::PaymentIdWrongLength(_) => 23,
            WalletError::PaymentIdInvalid => 24,
            WalletError::AddressIsIntegrated(_) => 25,
            WalletError::ConflictingPaymentIds => 26,
            WalletError::CantGetFakeOutputs(_) => 27,
            WalletError::NotEnoughFakeOutputs(_) => 28,
            WalletError::DaemonOffline(_) => 30,
            WalletError::DaemonError(_) => 31,
            WalletError::TooManyInputsToFitInBlock { .. } => 32,
            WalletError::FullyOptimized => 36,
            WalletError::FusionMixinTooLarge => 37,
            WalletError::AmountsNotPretty => 44,
            WalletError::UnexpectedFee { .. } => 45,
            WalletError::OutputDecomposition { .. } => 56,
            WalletError::PreparedTransactionExpired => 57,
            WalletError::AmountUgly => 59,
            WalletError::UnlockTimeTooSmall { .. } => 60,
            WalletError::ShortPaymentIdNeedsSingleDestination(_) => 61,
            WalletError::TransactionPoWFailed => 54,
        }
    }
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalletError::FilenameNonExistent => f.write_str(
                "The filename you are attempting to open does not exist, or the wallet does not have permission to \
                 open it.",
            ),
            WalletError::InvalidWalletFilename(e) => {
                write!(f, "We could not open/save to the filename given: {e}")
 }
 WalletError::NotAWalletFile => f.write_str(
 "This file is not a wallet file, or is not a wallet file type supported by this wallet version.",
 ),
 WalletError::WalletFileCorrupted(why) => {
 write!(f, "This wallet file appears to have gotten corrupted: {why}")
            }
            WalletError::WrongPassword => f.write_str("The password given for this wallet is incorrect."),
            WalletError::UnsupportedWalletFileFormatVersion(v) => write!(
                f,
                "This wallet file appears to be from a newer or older version of the software, that we do not \
                 support (version {v})."
            ),
            WalletError::InvalidMnemonic(e) => write!(f, "The mnemonic seed given is invalid: {e:?}"),
 WalletError::WalletFileAlreadyExists => {
 f.write_str("The wallet file you are attempting to create already exists. Please delete it first.")
            }
            // `validateAddresses` (`ValidateParameters.cpp:338`) builds the
            // length message itself and returns the plain `Errors.cpp` text for
            // the rest.
            WalletError::InvalidAddress(Base58Error::WrongAddressLength(n)) => write!(
                f,
                "The address given is the wrong length. It should be {STANDARD_ADDRESS_LENGTH} chars, {INTEGRATED_ADDRESS_LENGTH} chars, or {INTEGRATED_ADDRESS_LENGTH_LONG} chars, but it is {n} chars."
            ),
            WalletError::InvalidAddress(Base58Error::WrongPrefix(_)) => f.write_str(
                "The address does not have the correct prefix corresponding to this coin - it appears to be an address for another cryptocurrency.",
            ),
            WalletError::InvalidAddress(Base58Error::InvalidCharacter { .. })
            | WalletError::InvalidAddress(Base58Error::InvalidLength(_)) => {
                f.write_str("The address contains invalid characters, that are not in the base58 set.")
            }
            WalletError::InvalidAddress(_) => {
                f.write_str("The address given is not valid. Possibly invalid checksum. Most likely a typo.")
 }
 WalletError::SubWalletAlreadyExists => f.write_str("A subwallet with the given key already exists."),
            WalletError::IllegalViewWalletOperation => {
                f.write_str("This function cannot be called when using a view wallet.")
            }
            WalletError::InvalidPrivateKey => f.write_str("The private key given is not a valid ed25519 private key."),
            WalletError::InvalidPublicKey => {
                f.write_str("A public key given is not a valid ed25519 point, or no derivation could be made from it.")
 }
 WalletError::Io(e) => write!(f, "Wallet file I/O failed: {e}"),
            WalletError::IllegalNonViewWalletOperation => {
                f.write_str("This function can only be used when using a view wallet.")
            }
            WalletError::KeysNotDeterministic => f.write_str(
                "You cannot get a mnemonic seed for this address, as the view key is derived in terms of the spend key.",
            ),
            WalletError::CannotDeletePrimaryAddress => f.write_str(
                "Each wallet has a primary address when created, this address cannot be removed.",
            ),
            WalletError::TxPrivateKeyNotFound => f.write_str(
                "Couldn't find the private key for this transaction. The transaction must exist, and have been sent by this program. Transaction private keys cannot be found upon rescanning/reimporting.",
            ),
            WalletError::HashWrongLength => f.write_str("The hash given is not 64 characters long."),
            WalletError::HashInvalid => f.write_str("The hash given is not a hex string (A-Za-z0-9)"),
            WalletError::InvalidExtraData => {
                f.write_str("The extra data given for the transaction could not be decoded.")
            }
            WalletError::PreparedTransactionNotFound => f.write_str(
                "The prepared transaction hash given does not exist, either because it never existed or because the wallet process was restarted and the previously prepared transactions were lost. Please re-prepare and re-send the transaction, ensuring you specify the correct transaction hash.",
            ),
            WalletError::LiteNodeCannotRescanThatLow(why) => write!(f, "{why}"),
            WalletError::Json(e) => write!(f, "Could not serialize the wallet to JSON: {e}"),

 WalletError::WillOverflow => f.write_str("This operation will cause an integer overflow."),
            WalletError::AddressNotInWallet(a) => write!(
                f,
                "The address given ({a}) does not exist in the wallet container, but it is required to exist for \
                 this operation."
            ),
            // `Errors.cpp:68`. The numbers are carried in the variant for the
            // front ends to print, but the message itself is the C++'s, because
            // a client may be matching on it.
            WalletError::NotEnoughBalance { .. } => f.write_str(
                "Not enough unlocked funds were found to cover this transaction in the subwallets specified (or all wallets, if not specified). (Sum of amounts + fee)",
            ),
            WalletError::IntegratedAddressPaymentIdInvalid => {
                f.write_str("The payment ID encoded in the integrated address is not valid.")
 }
 WalletError::FeeTooSmall => f.write_str("The fee given is lower than the minimum the network will accept."),
            WalletError::NoDestinationsGiven => f.write_str("The destinations array is empty."),
            WalletError::AmountIsZero => f.write_str("One of the destination parameters has an amount given of zero."),
            WalletError::FailedToCreateRingSignature => {
                f.write_str("Something went wrong creating the ring signatures.")
            }
            WalletError::MixinTooSmall { mixin, min } => {
                write!(f, "The mixin value given ({mixin}) is lower than the minimum mixin allowed ({min})")
            }
            WalletError::MixinTooBig { mixin, max } => {
                write!(f, "The mixin value given ({mixin}) is greater than the maximum mixin allowed ({max})")
            }
            WalletError::PaymentIdWrongLength(n) => {
                write!(f, "The payment ID given is {n} characters; it must be 16 or 64.")
            }
            WalletError::PaymentIdInvalid => f.write_str("The payment ID given is not a valid hex string."),
            WalletError::AddressIsIntegrated(a) => write!(
                f,
                "The address given ({a}) is an integrated address, but integrated addresses aren't valid for this \
                 parameter."
            ),
            WalletError::ConflictingPaymentIds => f.write_str(
                "Conflicting payment IDs were found: an integrated address carries a different payment ID to the one \
                 given.",
            ),
            WalletError::CantGetFakeOutputs(why) => write!(f, "Failed to get mixin outputs from the daemon: {why}"),
            WalletError::NotEnoughFakeOutputs(why) => {
                write!(f, "Failed to get enough matching outputs to build the ring: {why}")
            }
            WalletError::DaemonOffline(why) => {
                write!(f, "Could not contact the daemon to complete the request: {why}")
            }
            WalletError::DaemonError(why) => write!(f, "The daemon refused the request: {why}"),
            WalletError::TooManyInputsToFitInBlock { size, max } => write!(
                f,
                "Transaction is too large: ({size} bytes). Max allowed size is {max} bytes. Decrease the amount you \
                 are sending, or perform some fusion transactions."
 ),
 WalletError::FullyOptimized => {
 f.write_str("Don't have enough inputs to make a fusion transaction, the wallet is fully optimized.")
            }
            WalletError::FusionMixinTooLarge => f.write_str(
                "The mixin given for this fusion transaction is too large to be able to hit the minimum input \
                 requirement.",
            ),
            WalletError::AmountsNotPretty => {
                f.write_str("The transaction was created with an output amount that is not a valid denomination.")
            }
            WalletError::UnexpectedFee { expected, actual } => {
                write!(f, "The transaction fee is not the fee specified: expected {expected}, got {actual}.")
            }
            WalletError::OutputDecomposition { outputs, max } => write!(
                f,
                "The transaction has more outputs ({outputs}) than are permitted ({max}) for the inputs provided."
            ),
            WalletError::PreparedTransactionExpired => f.write_str(
                "The inputs included in the prepared transaction have since been spent or are for some other reason \
                 no longer available.",
            ),
            WalletError::AmountUgly => f.write_str("The amount given does not have only a single significant digit."),
            WalletError::UnlockTimeTooSmall { unlock_time, minimum } => {
                write!(f, "The unlock time given ({unlock_time}) does not meet the required minimum ({minimum}).")
            }
            WalletError::ShortPaymentIdNeedsSingleDestination(n) => {
                write!(f, "A short payment ID is encrypted to a single receiver, but {n} destinations were given.")
            }
            WalletError::TransactionPoWFailed => {
                f.write_str("The transaction proof of work search did not find a nonce.")
            }
        }
    }
}

impl std::error::Error for WalletError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WalletError::Io(e) | WalletError::InvalidWalletFilename(e) => Some(e),
            WalletError::InvalidAddress(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WalletError {
    fn from(e: std::io::Error) -> Self {
        WalletError::Io(e)
    }
}

/// `Result` with this module's error.
pub type Result<T> = std::result::Result<T, WalletError>;

//////////////////
/* HEX AND KEYS */
//////////////////

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = (b[i * 2] as char).to_digit(16)?;
        let lo = (b[i * 2 + 1] as char).to_digit(16)?;
        *byte = (hi * 16 + lo) as u8;
    }
    Some(out)
}

fn write_hex32(bytes: &[u8; 32], buf: &mut [u8; 64]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (i, b) in bytes.iter().enumerate() {
        buf[i * 2] = DIGITS[(b >> 4) as usize];
        buf[i * 2 + 1] = DIGITS[(b & 0x0f) as usize];
    }
}

fn hex32_string(bytes: &[u8; 32]) -> String {
    let mut buf = [0u8; 64];
    write_hex32(bytes, &mut buf);
    String::from_utf8(buf.to_vec()).expect("hex digits are ascii")
}

fn serialize_hex32<S: Serializer>(bytes: &[u8; 32], s: S) -> std::result::Result<S::Ok, S::Error> {
    let mut buf = [0u8; 64];
    write_hex32(bytes, &mut buf);
    s.serialize_str(std::str::from_utf8(&buf).expect("hex digits are ascii"))
}

struct Hex32Visitor;

impl serde::de::Visitor<'_> for Hex32Visitor {
    type Value = [u8; 32];

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("64 hexadecimal characters")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<[u8; 32], E> {
        // `Common::podFromHex` accepts either case; we always write lowercase.
        parse_hex32(v).ok_or_else(|| E::custom("wrong length or not hex"))
    }
}

/// 32 public bytes carried as 64 lowercase hex characters: a hash, a public
/// key, a key image or a key derivation.
///
/// The C++ types are `Crypto::Hash`, `Crypto::PublicKey` and
/// `Crypto::KeyImage`, all of which serialize through `Common::podToHex` and
/// parse through `fromString`, which throws `std::invalid_argument` on anything
/// that is not exactly 64 hex characters (`include/CryptoTypes.h:48`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hex32(pub [u8; 32]);

/// A block or transaction hash.
pub type Hash = Hex32;
/// An ed25519 public key.
pub type PublicKey = Hex32;
/// A key image.
pub type KeyImage = Hex32;

impl Hex32 {
    /// The 32 raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 64 lowercase hex characters.
    pub fn to_hex(self) -> String {
        hex32_string(&self.0)
    }

    /// Parse 64 hex characters of either case.
    pub fn from_hex(s: &str) -> Option<Hex32> {
        parse_hex32(s).map(Hex32)
    }
}

impl From<[u8; 32]> for Hex32 {
    fn from(b: [u8; 32]) -> Self {
        Hex32(b)
    }
}

impl fmt::Debug for Hex32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Display for Hex32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Hex32 {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        serialize_hex32(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for Hex32 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Hex32, D::Error> {
        d.deserialize_str(Hex32Visitor).map(Hex32)
    }
}

/// A private key: the spend key, the view key, a transaction key or a cached
/// one-time secret.
///
/// Wiped on drop, and its [`Debug`] never shows the bytes — a wallet struct is
/// the sort of thing that ends up in a log line or a panic message. The hex is
/// only produced by [`SecretKey::to_hex`] and by serialization, which the file
/// format requires.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    /// `Constants::NULL_SECRET_KEY`: what a view wallet stores for the private
    /// spend key of every subwallet.
    pub const NULL: SecretKey = SecretKey([0u8; 32]);

    /// Wrap 32 raw bytes. No validation; see [`validate_private_key`].
    pub fn from_bytes(b: [u8; 32]) -> SecretKey {
        SecretKey(b)
    }

    /// Parse 64 hex characters of either case.
    pub fn from_hex(s: &str) -> Option<SecretKey> {
        parse_hex32(s).map(SecretKey)
    }

    /// The 32 raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 64 lowercase hex characters, in a wiped-on-drop buffer.
    pub fn to_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex32_string(&self.0))
    }

    /// All zeros — the view wallet marker, not a usable key.
    pub fn is_null(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_null() { "SecretKey(null)" } else { "SecretKey(<redacted>)" })
    }
}

impl Serialize for SecretKey {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        serialize_hex32(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for SecretKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<SecretKey, D::Error> {
        d.deserialize_str(Hex32Visitor).map(SecretKey)
    }
}

/// `validatePrivateKey` (`src/errors/ValidateParameters.cpp`): `sc_check`.
pub fn validate_private_key(key: &SecretKey) -> Result<()> {
    if curve::sc_check(key.as_bytes()) {
        Ok(())
    } else {
        Err(WalletError::InvalidPrivateKey)
    }
}

/// `Utilities::privateKeysToAddress`: both public keys, then the 98-character
/// standard address.
pub fn private_keys_to_address(private_spend_key: &SecretKey, private_view_key: &SecretKey) -> Result<String> {
    let spend_public =
        curve::secret_key_to_public_key(private_spend_key.as_bytes()).ok_or(WalletError::InvalidPrivateKey)?;
    let view_public =
        curve::secret_key_to_public_key(private_view_key.as_bytes()).ok_or(WalletError::InvalidPrivateKey)?;
    Ok(base58::standard_address(&spend_public, &view_public))
}

//////////////////
/* JSON SCHEMA  */
//////////////////

/// One entry of a transaction's `transfers` array: how much of the transaction
/// belonged to (positive) or was spent by (negative) one owned spend key.
///
/// The C++ holds these in an `unordered_map<PublicKey, int64_t>` and converts
/// to an array on the way out precisely so other languages can read it
/// (`JsonSerialization.cpp:85`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    /// Signed: negative for the amount this key spent.
    #[serde(rename = "amount")]
    pub amount: i64,
    /// The owned public spend key this amount belongs to.
    #[serde(rename = "publicKey")]
    pub public_key: PublicKey,
}

/// A transaction as the wallet records it (`WalletTypes.h:360`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    /// Height of the block holding it; `0` while it is only in the pool.
    #[serde(rename = "blockHeight")]
    pub block_height: u64,
    /// Always positive; `0` for a coinbase and for a fusion transaction.
    #[serde(rename = "fee")]
    pub fee: u64,
    #[serde(rename = "hash")]
    pub hash: Hash,
    #[serde(rename = "isCoinbaseTransaction")]
    pub is_coinbase_transaction: bool,
    /// 16 or 64 hex characters, or empty. Note the C++ key is `paymentID`.
    #[serde(rename = "paymentID")]
    pub payment_id: String,
    /// The block timestamp.
    #[serde(rename = "timestamp")]
    pub timestamp: u64,
    #[serde(rename = "transfers")]
    pub transfers: Vec<Transfer>,
    #[serde(rename = "unlockTime")]
    pub unlock_time: u64,
}

impl Transaction {
    /// `WalletTypes::Transaction::totalAmount`: the transfers summed.
    pub fn total_amount(&self) -> i64 {
        self.transfers.iter().map(|t| t.amount).sum()
    }

    /// `WalletTypes::Transaction::isFusionTransaction`: zero fee and not a
    /// coinbase. Not conclusive on its own — the daemon enforces the rest.
    pub fn is_fusion_transaction(&self) -> bool {
        self.fee == 0 && !self.is_coinbase_transaction
    }
}

/// An output the wallet owns (`WalletTypes.h:161`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionInput {
    #[serde(rename = "amount")]
    pub amount: u64,
    /// The height of the block the parent transaction was in.
    #[serde(rename = "blockHeight")]
    pub block_height: u64,
    /// Filled from `/get_global_indexes_for_range` after the block is scanned.
    ///
    /// The C++ holds `std::optional<uint64_t>` but writes `value_or(0)`
    /// unconditionally and reads it back as present, so an index that sync
    /// never resolved is indistinguishable from index 0 once saved. This field
    /// reproduces that exactly: `None` writes `0`, and reading always yields
    /// `Some`.
    #[serde(
        rename = "globalOutputIndex",
        serialize_with = "serialize_global_output_index",
        deserialize_with = "deserialize_global_output_index"
    )]
    pub global_output_index: Option<u64>,
    /// The one-time output key `P` taken from the transaction.
    #[serde(rename = "key")]
    pub key: PublicKey,
    /// All zeros in a view wallet, which cannot compute it.
    #[serde(rename = "keyImage")]
    pub key_image: KeyImage,
    #[serde(rename = "parentTransactionHash")]
    pub parent_transaction_hash: Hash,
    /// The cached one-time secret `x`, so spending does not re-derive it.
    /// Absent from the JSON when unset — one of the three optional keys.
    #[serde(rename = "privateEphemeral", default, skip_serializing_if = "Option::is_none")]
    pub private_ephemeral: Option<SecretKey>,
    /// The height this input was spent at, or `0`.
    #[serde(rename = "spendHeight")]
    pub spend_height: u64,
    /// The index of this output within its transaction.
    #[serde(rename = "transactionIndex")]
    pub transaction_index: u64,
    /// The `01` tag of the parent transaction's extra.
    #[serde(rename = "transactionPublicKey")]
    pub transaction_public_key: PublicKey,
    #[serde(rename = "unlockTime")]
    pub unlock_time: u64,
}

fn serialize_global_output_index<S: Serializer>(v: &Option<u64>, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_u64(v.unwrap_or(0))
}

fn deserialize_global_output_index<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<u64>, D::Error> {
    u64::deserialize(d).map(Some)
}

/// Money on the way in from a transaction we sent (change, or a send to
/// ourselves) that is not in a block yet (`WalletTypes.h:452`). Only enough to
/// show it as locked balance; it cannot be spent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnconfirmedInput {
    #[serde(rename = "amount")]
    pub amount: u64,
    #[serde(rename = "key")]
    pub key: PublicKey,
    #[serde(rename = "parentTransactionHash")]
    pub parent_transaction_hash: Hash,
}

/// One address of the container (`SubWallet.cpp`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubWallet {
    /// The 98-character address, stored rather than recomputed.
    #[serde(rename = "address")]
    pub address: String,
    /// True for exactly one subwallet, the one the container was created with.
    #[serde(rename = "isPrimaryAddress")]
    pub is_primary_address: bool,
    /// Inputs held by an in-flight send until it confirms or is cancelled.
    #[serde(rename = "lockedInputs")]
    pub locked_inputs: Vec<TransactionInput>,
    /// `Constants::NULL_SECRET_KEY` (all zeros) in a view wallet.
    #[serde(rename = "privateSpendKey")]
    pub private_spend_key: SecretKey,
    #[serde(rename = "publicSpendKey")]
    pub public_spend_key: PublicKey,
    #[serde(rename = "spentInputs")]
    pub spent_inputs: Vec<TransactionInput>,
    /// One of `syncStartHeight` and `syncStartTimestamp` is zero.
    #[serde(rename = "syncStartHeight")]
    pub sync_start_height: u64,
    #[serde(rename = "syncStartTimestamp")]
    pub sync_start_timestamp: u64,
    #[serde(rename = "unconfirmedIncomingAmounts")]
    pub unconfirmed_incoming_amounts: Vec<UnconfirmedInput>,
    #[serde(rename = "unspentInputs")]
    pub unspent_inputs: Vec<TransactionInput>,
    /// The deterministic subwallet index. `0` for the primary address and for
    /// any key imported directly. Optional on read (`SubWallet::fromJSON`).
    #[serde(rename = "walletIndex", default)]
    pub wallet_index: u64,
}

impl SubWallet {
    /// The primary record of a fresh container: no inputs, no transactions, and
    /// `isPrimaryAddress` set. A view wallet passes [`SecretKey::NULL`].
    ///
    /// One of `scan_height` and `scan_timestamp` is always zero: a wallet is
    /// created from a timestamp and restored from a height, never both
    /// (`SubWallets::SubWallets`, `SubWallets.cpp:27`).
    pub fn new_primary(
        public_spend_key: PublicKey,
        private_spend_key: SecretKey,
        address: String,
        scan_height: u64,
        scan_timestamp: u64,
    ) -> SubWallet {
        SubWallet {
            address,
            is_primary_address: true,
            locked_inputs: Vec::new(),
            private_spend_key,
            public_spend_key,
            spent_inputs: Vec::new(),
            sync_start_height: scan_height,
            sync_start_timestamp: scan_timestamp,
            unconfirmed_incoming_amounts: Vec::new(),
            unspent_inputs: Vec::new(),
            wallet_index: 0,
        }
    }

    /// Whether this subwallet can sign: a view wallet stores all zeros.
    pub fn has_spend_key(&self) -> bool {
        !self.private_spend_key.is_null()
    }

    /// Every input of the three lists, in the order the C++ writes them.
    pub fn all_inputs(&self) -> impl Iterator<Item = &TransactionInput> {
        self.unspent_inputs.iter().chain(&self.locked_inputs).chain(&self.spent_inputs)
    }
}

/// `transactionHash` → `txPrivateKey`: the `r` of a transaction we sent, kept
/// so a proof of payment can be produced later (`SubWallets.cpp:1220`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TxPrivateKey {
    #[serde(rename = "transactionHash")]
    pub transaction_hash: Hash,
    #[serde(rename = "txPrivateKey")]
    pub tx_private_key: SecretKey,
}

/// The container: one view key and one or more addresses (`SubWallets.cpp`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubWallets {
    /// A view wallet has no spend keys and can neither see its own spends nor
    /// send.
    #[serde(rename = "isViewWallet")]
    pub is_view_wallet: bool,
    /// Sent transactions that are not in a block yet.
    #[serde(rename = "lockedTransactions")]
    pub locked_transactions: Vec<Transaction>,
    /// Shared by every subwallet; also duplicated in `walletSynchronizer`.
    #[serde(rename = "privateViewKey")]
    pub private_view_key: SecretKey,
    /// Insertion order; the same keys as `subWallet`, as a flat list.
    #[serde(rename = "publicSpendKeys")]
    pub public_spend_keys: Vec<PublicKey>,
    /// Singular, because the C++ key is singular.
    #[serde(rename = "subWallet")]
    pub sub_wallet: Vec<SubWallet>,
    /// The highest deterministic index handed out. Optional on read.
    #[serde(rename = "subWalletIndexCounter", default)]
    pub sub_wallet_index_counter: u64,
    #[serde(rename = "transactions")]
    pub transactions: Vec<Transaction>,
    #[serde(rename = "txPrivateKeys")]
    pub tx_private_keys: Vec<TxPrivateKey>,
}

/// Where sync got to (`SynchronizationStatus.cpp`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SynchronizationStatus {
    /// Newest first, one every [`BLOCK_HASH_CHECKPOINTS_INTERVAL`] blocks.
    #[serde(rename = "blockHashCheckpoints")]
    pub block_hash_checkpoints: Vec<Hash>,
    /// Newest first, at most [`LAST_KNOWN_BLOCK_HASHES_SIZE`].
    #[serde(rename = "lastKnownBlockHashes")]
    pub last_known_block_hashes: Vec<Hash>,
    #[serde(rename = "lastKnownBlockHeight")]
    pub last_known_block_height: u64,

    /// `m_lastSavedCheckpointAt`, which the C++ does **not** serialize: after a
    /// load it is zero again, so the next block processed always lands a sparse
    /// checkpoint whatever the height. Kept out of the JSON here for the same
    /// reason — writing it would change the bytes.
    #[serde(skip)]
    pub last_saved_checkpoint_at: u64,
}

impl SynchronizationStatus {
    /// `SynchronizationStatus::storeBlockHash`.
    pub fn store_block_hash(&mut self, hash: Hash, height: u64) {
        self.last_known_block_height = height;

        // The C++ compares against the *back* of the deque, which is the oldest
        // entry, not the newest. Reproduced as-is: it only ever fires when the
        // recent list holds a single hash.
        if self.last_known_block_hashes.last() == Some(&hash) {
            return;
        }

        if self.last_saved_checkpoint_at + BLOCK_HASH_CHECKPOINTS_INTERVAL < height {
            self.last_saved_checkpoint_at = height;
            self.block_hash_checkpoints.insert(0, hash);
        }

        self.last_known_block_hashes.insert(0, hash);

        if self.last_known_block_hashes.len() > LAST_KNOWN_BLOCK_HASHES_SIZE {
            self.last_known_block_hashes.pop();
        }
    }
}

/// The synchronizer's own state (`WalletSynchronizer.cpp:974`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WalletSynchronizerState {
    /// The same key as `subWallets.privateViewKey`.
    #[serde(rename = "privateViewKey")]
    pub private_view_key: SecretKey,
    /// Where sync starts. One of this and `startTimestamp` is zero.
    #[serde(rename = "startHeight")]
    pub start_height: u64,
    #[serde(rename = "startTimestamp")]
    pub start_timestamp: u64,
    #[serde(rename = "transactionSynchronizerStatus")]
    pub transaction_synchronizer_status: SynchronizationStatus,
}

/// A whole wallet: the document the file holds, and the model the rest of the
/// library works on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wallet {
    #[serde(rename = "subWallets")]
    pub sub_wallets: SubWallets,
    /// [`WALLET_FILE_FORMAT_VERSION`]; anything else is refused on open.
    #[serde(rename = "walletFileFormatVersion")]
    pub wallet_file_format_version: u64,
    #[serde(rename = "walletSynchronizer")]
    pub wallet_synchronizer: WalletSynchronizerState,
}

impl Default for Wallet {
    fn default() -> Self {
        Wallet {
            sub_wallets: SubWallets::default(),
            wallet_file_format_version: WALLET_FILE_FORMAT_VERSION as u64,
            wallet_synchronizer: WalletSynchronizerState::default(),
        }
    }
}

/// Only used to read the version before the rest of the document, so that a
/// file from another format version is reported as such and not as corrupt —
/// the order `WalletBackend::fromJSON` checks in.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(rename = "walletFileFormatVersion")]
    wallet_file_format_version: u64,
}

////////////////////
/* TIME HELPERS   */
////////////////////

/// `Utilities::getCurrentTimestampAdjusted`: now, minus the largest amount a
/// block timestamp may run ahead of the clock, so a wallet created now cannot
/// miss a block that is already in flight.
pub fn current_timestamp_adjusted() -> u64 {
    let adjust = CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT
        .max(CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3)
        .max(CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4);
    crate::platform::now_seconds().saturating_sub(adjust)
}

/// `Utilities::timestampToScanHeight`: seconds since the genesis timestamp over
/// the block target, less 10,000 blocks of slack.
///
/// The C++ writes `std::max<uint64_t>(0, (delta / 60) - 10000)`, which for a
/// timestamp inside the chain's first week underflows and yields a height near
/// `u64::MAX` instead of 0 — `max` against an unsigned zero can never clamp
/// anything. This floors at 0 as spec/10 describes; the divergence is
/// unreachable for any real wallet, whose creation timestamp is always recent.
pub fn timestamp_to_scan_height(timestamp: u64) -> u64 {
    if timestamp == 0 || timestamp <= GENESIS_BLOCK_TIMESTAMP {
        return 0;
    }
    ((timestamp - GENESIS_BLOCK_TIMESTAMP) / DIFFICULTY_TARGET).saturating_sub(10_000)
}

//////////////////
/* FILE CODEC   */
//////////////////

/// Split a wallet file into its plaintext JSON.
///
/// The failure order is `WalletBackend::openWallet`'s, and it is load bearing:
/// too short for the outer marker is `NOT_A_WALLET_FILE` (not "corrupted"),
/// while too short for the salt or for the inner marker is
/// `WALLET_FILE_CORRUPTED`, and everything that could reveal anything about the
/// key is [`WalletError::WrongPassword`].
pub fn decode_wallet_file(bytes: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>> {
    if bytes.len() < IS_A_WALLET_IDENTIFIER_SIZE || bytes[..IS_A_WALLET_IDENTIFIER_SIZE] != IS_A_WALLET_IDENTIFIER[..] {
        // `hasMagicIdentifier(buffer, IS_A_WALLET_IDENTIFIER, NOT_A_WALLET_FILE,
        // NOT_A_WALLET_FILE)` — the same error for both reasons.
        return Err(WalletError::NotAWalletFile);
    }

    let rest = &bytes[IS_A_WALLET_IDENTIFIER_SIZE..];

    if rest.len() < SALT_SIZE {
        return Err(WalletError::WalletFileCorrupted("shorter than the salt".into()));
    }

    let mut salt = [0u8; SALT_SIZE];
    salt.copy_from_slice(&rest[..SALT_SIZE]);

    let decrypted =
        decrypt_wallet_file(&rest[SALT_SIZE..], password.as_bytes(), &salt).ok_or(WalletError::WrongPassword)?;

    if decrypted.len() < IS_CORRECT_PASSWORD_IDENTIFIER_SIZE {
        return Err(WalletError::WalletFileCorrupted("shorter than the password identifier".into()));
    }

    if decrypted[..IS_CORRECT_PASSWORD_IDENTIFIER_SIZE] != IS_CORRECT_PASSWORD_IDENTIFIER[..] {
        return Err(WalletError::WrongPassword);
    }

    Ok(Zeroizing::new(decrypted[IS_CORRECT_PASSWORD_IDENTIFIER_SIZE..].to_vec()))
}

/// Build the file bytes around a JSON document with the given salt.
///
/// `saveWalletJSONToDisk` always uses a fresh random salt; this is the seam the
/// round-trip tests use to compare bytes, and the only reason it is public.
pub fn encode_wallet_file_with_salt(json: &[u8], password: &str, salt: &[u8; SALT_SIZE]) -> Vec<u8> {
    let mut plaintext = Zeroizing::new(Vec::with_capacity(IS_CORRECT_PASSWORD_IDENTIFIER_SIZE + json.len()));
    plaintext.extend_from_slice(&IS_CORRECT_PASSWORD_IDENTIFIER[..]);
    plaintext.extend_from_slice(json);

    let ciphertext = encrypt_wallet_file(&plaintext, password.as_bytes(), salt);

    let mut out = Vec::with_capacity(CIPHERTEXT_OFFSET + ciphertext.len());
    out.extend_from_slice(&IS_A_WALLET_IDENTIFIER[..]);
    out.extend_from_slice(salt);
    out.extend_from_slice(&ciphertext);
    out
}

/// Build the file bytes around a JSON document, with a fresh random salt —
/// `saveWalletJSONToDisk` generates one on every save.
pub fn encode_wallet_file(json: &[u8], password: &str) -> Vec<u8> {
    encode_wallet_file_with_salt(json, password, &random_salt())
}

/// `checkNewWalletFilename`: refuse to create over an existing file.
pub fn check_new_wallet_filename(path: impl AsRef<Path>) -> Result<()> {
    if path.as_ref().exists() {
        return Err(WalletError::WalletFileAlreadyExists);
    }
    Ok(())
}

//////////////////////
/* OPEN AND SAVE    */
//////////////////////

impl Wallet {
    /// Open a wallet file.
    pub fn open(path: impl AsRef<Path>, password: &str) -> Result<Wallet> {
        let bytes = std::fs::read(path.as_ref()).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => WalletError::FilenameNonExistent,
            _ => WalletError::Io(e),
        })?;
        Wallet::from_file_bytes(&bytes, password)
    }

    /// Open a wallet file already in memory (the WASM build's path, and every
    /// test's).
    pub fn from_file_bytes(bytes: &[u8], password: &str) -> Result<Wallet> {
        let json = decode_wallet_file(bytes, password)?;
        Wallet::from_json_bytes(&json)
    }

    /// Parse the plaintext JSON document.
    pub fn from_json_bytes(json: &[u8]) -> Result<Wallet> {
        // `WalletBackend::fromJSON` reads and checks the version before it
        // touches `subWallets`, so a future format version is reported as such
        // even when the rest of the document means nothing to us.
        let probe: VersionProbe =
            serde_json::from_slice(json).map_err(|e| WalletError::WalletFileCorrupted(e.to_string()))?;

        if probe.wallet_file_format_version != WALLET_FILE_FORMAT_VERSION as u64 {
            return Err(WalletError::UnsupportedWalletFileFormatVersion(probe.wallet_file_format_version));
        }

        serde_json::from_slice(json).map_err(|e| WalletError::WalletFileCorrupted(e.to_string()))
    }

    /// The exact bytes `WalletBackend::unsafeToJSON` would produce: no
    /// whitespace, keys in lexicographic order.
    pub fn to_json_bytes(&self) -> Result<Zeroizing<Vec<u8>>> {
        serde_json::to_vec(self).map(Zeroizing::new).map_err(|e| WalletError::Json(e.to_string()))
    }

    /// The whole file, ready to write, with a fresh random salt.
    pub fn to_file_bytes(&self, password: &str) -> Result<Vec<u8>> {
        Ok(encode_wallet_file(&self.to_json_bytes()?, password))
    }

    /// The whole file with a chosen salt. See [`encode_wallet_file_with_salt`].
    pub fn to_file_bytes_with_salt(&self, password: &str, salt: &[u8; SALT_SIZE]) -> Result<Vec<u8>> {
        Ok(encode_wallet_file_with_salt(&self.to_json_bytes()?, password, salt))
    }

    /// Write the wallet to disk.
    ///
    /// Through `<path>.tmp` and a rename, as `saveWalletJSONToDisk` does: an
    /// interrupted in-place write leaves a truncated file, and a truncated
    /// wallet file is unopenable. The rename is atomic on NTFS and on POSIX.
    pub fn save(&self, path: impl AsRef<Path>, password: &str) -> Result<()> {
        let path = path.as_ref();
        let bytes = self.to_file_bytes(password)?;

        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);

        std::fs::write(&tmp, &bytes).map_err(WalletError::InvalidWalletFilename)?;

        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(WalletError::InvalidWalletFilename(e));
        }

        Ok(())
    }
}

//////////////////////
/* CONSTRUCTION     */
//////////////////////

impl Wallet {
    fn with_primary(
        private_view_key: SecretKey,
        primary: SubWallet,
        is_view_wallet: bool,
        sync_start_height: u64,
        sync_start_timestamp: u64,
    ) -> Wallet {
        Wallet {
            sub_wallets: SubWallets {
                is_view_wallet,
                locked_transactions: Vec::new(),
                private_view_key: private_view_key.clone(),
                public_spend_keys: vec![primary.public_spend_key],
                sub_wallet: vec![primary],
                sub_wallet_index_counter: 0,
                transactions: Vec::new(),
                tx_private_keys: Vec::new(),
            },
            wallet_file_format_version: WALLET_FILE_FORMAT_VERSION as u64,
            wallet_synchronizer: WalletSynchronizerState {
                private_view_key,
                start_height: sync_start_height,
                start_timestamp: sync_start_timestamp,
                transaction_synchronizer_status: SynchronizationStatus::default(),
            },
        }
    }

    /// `WalletBackend::createWallet`: a random spend key, the view key derived
    /// from it, and the creation timestamp as the scan start.
    ///
    /// `network_height` is what `/info` last reported, which
    /// `WalletBackend::init` (line 760) folds into the synchronizer's start
    /// height as `min(network height, timestampToScanHeight(created))`. Pass
    /// `0` when no daemon has answered yet — the same value `networkBlockCount()`
    /// returns then, and the same branch the C++ takes.
    pub fn create_new(network_height: u64) -> Result<Wallet> {
        let (spend_secret, spend_public) = curve::generate_keys();
        Wallet::create_from_spend_key(SecretKey::from_bytes(spend_secret), Hex32(spend_public), network_height)
    }

    /// [`Wallet::create_new`] from a spend key that is already chosen — the
    /// deterministic half of it, so a test can pin the keys and still take the
    /// "new wallet" path (timestamp start rather than height start).
    pub fn create_from_spend_key(
        private_spend_key: SecretKey,
        public_spend_key: PublicKey,
        network_height: u64,
    ) -> Result<Wallet> {
        validate_private_key(&private_spend_key)?;

        let (view_secret, view_public) = curve::generate_view_from_spend(private_spend_key.as_bytes());
        let private_view_key = SecretKey::from_bytes(view_secret);
        let address = base58::standard_address(public_spend_key.as_bytes(), &view_public);

        let created = current_timestamp_adjusted();

        // `WalletBackend::init` line 760: prefer the lower of the network
        // height and the height the timestamp implies, ignoring whichever is
        // zero; only when both are zero does the timestamp survive into the
        // synchronizer for it to resolve later.
        let timestamp_height = timestamp_to_scan_height(created);
        let candidate = match (network_height, timestamp_height) {
            (0, 0) => 0,
            (0, t) => t,
            (n, 0) => n,
            (n, t) => n.min(t),
        };
        let (start_height, start_timestamp) = if candidate != 0 { (candidate, 0) } else { (0, created) };

        Ok(Wallet::with_primary(
            private_view_key,
            SubWallet::new_primary(public_spend_key, private_spend_key, address, 0, created),
            false,
            start_height,
            start_timestamp,
        ))
    }

    /// `WalletBackend::importWalletFromSeed`: the 25-word seed is the spend
    /// key, the view key is derived from it.
    pub fn import_from_mnemonic(seed: &str, scan_height: u64) -> Result<Wallet> {
        let spend = mnemonic::mnemonic_to_private_key(seed).map_err(WalletError::InvalidMnemonic)?;
        let spend = SecretKey::from_bytes(spend);
        let (view, _) = curve::generate_view_from_spend(spend.as_bytes());
        let view = SecretKey::from_bytes(view);
        validate_private_key(&view)?;
        Wallet::import_from_keys(&spend, &view, scan_height)
    }

    /// `WalletBackend::importWalletFromKeys`: both keys must pass `sc_check`.
    ///
    /// An import is not a new wallet, so the subwallet records a scan *height*
    /// and a zero timestamp, whatever the height is.
    pub fn import_from_keys(
        private_spend_key: &SecretKey,
        private_view_key: &SecretKey,
        scan_height: u64,
    ) -> Result<Wallet> {
        validate_private_key(private_view_key)?;
        validate_private_key(private_spend_key)?;

        let spend_public =
            curve::secret_key_to_public_key(private_spend_key.as_bytes()).ok_or(WalletError::InvalidPrivateKey)?;
        let view_public =
            curve::secret_key_to_public_key(private_view_key.as_bytes()).ok_or(WalletError::InvalidPrivateKey)?;
        let address = base58::standard_address(&spend_public, &view_public);

        Ok(Wallet::with_primary(
            private_view_key.clone(),
            SubWallet::new_primary(Hex32(spend_public), private_spend_key.clone(), address, scan_height, 0),
            false,
            scan_height,
            0,
        ))
    }

    /// `WalletBackend::importViewWallet`: the private spend key is
    /// `NULL_SECRET_KEY` and the container is flagged `isViewWallet`.
    ///
    /// Like the C++, this checks the view key and the address but not that they
    /// belong together — the address supplies both public keys, and a mismatched
    /// view key simply finds nothing.
    pub fn import_view_only(private_view_key: &SecretKey, address: &str, scan_height: u64) -> Result<Wallet> {
        validate_private_key(private_view_key)?;

        let parsed = base58::parse_address(address).map_err(WalletError::InvalidAddress)?;
        if parsed.payment_id.is_some() {
            // `validateAddresses({address}, allowIntegratedAddresses = false)`.
            return Err(WalletError::InvalidAddress(Base58Error::WrongAddressLength(address.len())));
        }

        Ok(Wallet::with_primary(
            private_view_key.clone(),
            SubWallet::new_primary(
                Hex32(parsed.spend_public_key),
                SecretKey::NULL,
                address.to_string(),
                scan_height,
                0,
            ),
            true,
            scan_height,
            0,
        ))
    }
}

//////////////////////
/* SUBWALLETS       */
//////////////////////

impl Wallet {
    /// The primary subwallet, the one the container was created with.
    pub fn primary_sub_wallet(&self) -> Option<&SubWallet> {
        self.sub_wallets.sub_wallet.iter().find(|s| s.is_primary_address)
    }

    /// The primary address.
    pub fn primary_address(&self) -> Option<&str> {
        self.primary_sub_wallet().map(|s| s.address.as_str())
    }

    /// Every address in the container, in insertion order.
    pub fn addresses(&self) -> impl Iterator<Item = &str> {
        self.sub_wallets.sub_wallet.iter().map(|s| s.address.as_str())
    }

    /// The shared private view key.
    pub fn private_view_key(&self) -> &SecretKey {
        &self.sub_wallets.private_view_key
    }

    /// Whether this container can sign.
    pub fn is_view_wallet(&self) -> bool {
        self.sub_wallets.is_view_wallet
    }

    /// The subwallet owning a public spend key.
    pub fn sub_wallet(&self, public_spend_key: &PublicKey) -> Option<&SubWallet> {
        self.sub_wallets.sub_wallet.iter().find(|s| s.public_spend_key == *public_spend_key)
    }

    /// The 25-word seed, when the container is deterministic: a primary spend
    /// key whose derived view key is the container's
    /// (`WalletBackend::getMnemonicSeed`).
    pub fn mnemonic_seed(&self) -> Option<Zeroizing<String>> {
        let primary = self.primary_sub_wallet()?;
        if !primary.has_spend_key() {
            return None;
        }
        let (derived, _) = curve::generate_view_from_spend(primary.private_spend_key.as_bytes());
        if derived != *self.sub_wallets.private_view_key.as_bytes() {
            return None;
        }
        Some(Zeroizing::new(mnemonic::private_key_to_mnemonic(primary.private_spend_key.as_bytes())))
    }

    fn primary_private_spend_key(&self) -> Result<&SecretKey> {
        let primary = self.primary_sub_wallet().ok_or(WalletError::IllegalViewWalletOperation)?;
        if !primary.has_spend_key() {
            return Err(WalletError::IllegalViewWalletOperation);
        }
        Ok(&primary.private_spend_key)
    }

    /// `SubWallets::addSubWallet`: bump the counter and derive the next
    /// deterministic spend key from the primary one.
    ///
    /// The new subwallet scans from height 0 and records the current timestamp,
    /// exactly as the C++ does. Returns its address and index.
    pub fn add_sub_wallet(&mut self) -> Result<(String, u64)> {
        if self.sub_wallets.is_view_wallet {
            return Err(WalletError::IllegalViewWalletOperation);
        }
        let index = self.sub_wallets.sub_wallet_index_counter + 1;
        let base = self.primary_private_spend_key()?.clone();
        let key = SecretKey::from_bytes(curve::generate_deterministic_subwallet_key(base.as_bytes(), index));
        let address = self.push_sub_wallet(&key, 0, current_timestamp_adjusted(), index)?;
        self.sub_wallets.sub_wallet_index_counter = index;
        Ok((address, index))
    }

    /// `SubWallets::importSubWallet(walletIndex, scanHeight)`: re-derive a given
    /// deterministic index, and raise the counter if it is above it.
    pub fn import_sub_wallet_at_index(&mut self, index: u64, scan_height: u64) -> Result<String> {
        if self.sub_wallets.is_view_wallet {
            return Err(WalletError::IllegalViewWalletOperation);
        }
        let base = self.primary_private_spend_key()?.clone();
        let key = SecretKey::from_bytes(curve::generate_deterministic_subwallet_key(base.as_bytes(), index));
        // The C++ routes through importSubWallet(privateSpendKey), which stores
        // walletIndex 0 and a zero timestamp, then bumps the counter.
        let address = self.push_sub_wallet(&key, scan_height, 0, 0)?;
        if index > self.sub_wallets.sub_wallet_index_counter {
            self.sub_wallets.sub_wallet_index_counter = index;
        }
        Ok(address)
    }

    /// `SubWallets::importSubWallet(privateSpendKey, scanHeight)`: an arbitrary
    /// key, stored with index 0 because it is not on the deterministic chain.
    pub fn import_sub_wallet_from_key(&mut self, private_spend_key: &SecretKey, scan_height: u64) -> Result<String> {
        if self.sub_wallets.is_view_wallet {
            return Err(WalletError::IllegalViewWalletOperation);
        }
        validate_private_key(private_spend_key)?;
        self.push_sub_wallet(private_spend_key, scan_height, 0, 0)
    }

    fn push_sub_wallet(
        &mut self,
        private_spend_key: &SecretKey,
        scan_height: u64,
        scan_timestamp: u64,
        wallet_index: u64,
    ) -> Result<String> {
        let public =
            curve::secret_key_to_public_key(private_spend_key.as_bytes()).ok_or(WalletError::InvalidPrivateKey)?;
        let public = Hex32(public);

        if self.sub_wallets.sub_wallet.iter().any(|s| s.public_spend_key == public) {
            return Err(WalletError::SubWalletAlreadyExists);
        }

        let address = private_keys_to_address(private_spend_key, &self.sub_wallets.private_view_key)?;

        self.sub_wallets.sub_wallet.push(SubWallet {
            address: address.clone(),
            is_primary_address: false,
            locked_inputs: Vec::new(),
            private_spend_key: private_spend_key.clone(),
            public_spend_key: public,
            spent_inputs: Vec::new(),
            sync_start_height: scan_height,
            sync_start_timestamp: scan_timestamp,
            unconfirmed_incoming_amounts: Vec::new(),
            unspent_inputs: Vec::new(),
            wallet_index,
        });
        self.sub_wallets.public_spend_keys.push(public);

        Ok(address)
    }

    /// `SubWallets::getMinInitialSyncStart`: the earliest start any subwallet
    /// asked for, as `(height, timestamp)`.
    pub fn min_initial_sync_start(&self) -> (u64, u64) {
        let height = self.sub_wallets.sub_wallet.iter().map(|s| s.sync_start_height).min().unwrap_or(0);
        let timestamp = self.sub_wallets.sub_wallet.iter().map(|s| s.sync_start_timestamp).min().unwrap_or(0);
        (height, timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 25-word seed of spec/05, whose keys and address are pinned in
    /// `spec/vectors/primitives.txt`.
    pub const SPEC_MNEMONIC: &str = "eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell \
                                     different dating intended richly slower inundate ridges slug inundate ridges \
                                     slug were rotate rudely viewpoint";
    pub const SPEC_SPEND_SECRET: &str = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
    pub const SPEC_VIEW_SECRET: &str = "779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700";
    pub const SPEC_ADDRESS: &str =
        "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

    #[test]
    fn hex32_round_trips_and_rejects_bad_input() {
        let h = Hex32::from_hex(SPEC_VIEW_SECRET).unwrap();
        assert_eq!(h.to_hex(), SPEC_VIEW_SECRET);
        // uppercase is accepted on read, like `Common::podFromHex`
        assert_eq!(Hex32::from_hex(&SPEC_VIEW_SECRET.to_uppercase()), Some(h));
        assert!(Hex32::from_hex("").is_none());
        assert!(Hex32::from_hex(&SPEC_VIEW_SECRET[..62]).is_none());
        assert!(Hex32::from_hex(&format!("zz{}", &SPEC_VIEW_SECRET[2..])).is_none());
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let key = SecretKey::from_hex(SPEC_SPEND_SECRET).unwrap();
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "SecretKey(<redacted>)");
        assert!(!rendered.contains("243d1bb4"));
        assert_eq!(format!("{:?}", SecretKey::NULL), "SecretKey(null)");

        // and not through the wallet either
        let wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 100).unwrap();
        let rendered = format!("{wallet:?}");
        assert!(!rendered.contains("243d1bb4"), "the spend key leaked into Debug");
        assert!(!rendered.contains("779e4dd2"), "the view key leaked into Debug");
    }

    #[test]
    fn timestamp_to_scan_height_matches_the_formula() {
        assert_eq!(timestamp_to_scan_height(0), 0);
        assert_eq!(timestamp_to_scan_height(GENESIS_BLOCK_TIMESTAMP), 0);
        // inside the first 10,000 blocks the C++ underflows; we floor at zero
        assert_eq!(timestamp_to_scan_height(GENESIS_BLOCK_TIMESTAMP + 60), 0);
        assert_eq!(timestamp_to_scan_height(GENESIS_BLOCK_TIMESTAMP + 60 * 20_000), 10_000);
    }

    #[test]
    fn store_block_hash_keeps_fifty_recent_and_a_sparse_checkpoint_list() {
        let mut status = SynchronizationStatus::default();
        for height in 1..=12_000u64 {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&height.to_le_bytes());
            status.store_block_hash(Hex32(h), height);
        }
        assert_eq!(status.last_known_block_height, 12_000);
        assert_eq!(status.last_known_block_hashes.len(), LAST_KNOWN_BLOCK_HASHES_SIZE);
        // newest first
        assert_eq!(status.last_known_block_hashes[0].0[..8], 12_000u64.to_le_bytes());
        // 5001, 10002 -> two checkpoints, newest first
        assert_eq!(status.block_hash_checkpoints.len(), 2);
        assert_eq!(status.block_hash_checkpoints[0].0[..8], 10_002u64.to_le_bytes());
        assert_eq!(status.block_hash_checkpoints[1].0[..8], 5_001u64.to_le_bytes());
    }

    #[test]
    fn the_header_is_the_cpp_layout() {
        let wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();
        let salt = [7u8; SALT_SIZE];
        let bytes = wallet.to_file_bytes_with_salt("password", &salt).unwrap();

        assert_eq!(&bytes[..64], &IS_A_WALLET_IDENTIFIER[..]);
        assert_eq!(&bytes[64..80], &salt[..]);
        assert_eq!(&bytes[..64], b"If I pull that off, will you die?\nIt would be extremely painful.");

        let plaintext = crate::crypto::decrypt_wallet_file(&bytes[80..], b"password", &salt).unwrap();
        assert_eq!(&plaintext[..26], &IS_CORRECT_PASSWORD_IDENTIFIER[..]);
        assert_eq!(&plaintext[..26], b"You're a big guy.\nFor you.");
        assert_eq!(&plaintext[26..], &wallet.to_json_bytes().unwrap()[..]);

        // the ciphertext is a whole number of AES blocks and PKCS#7 always pads
        assert!((bytes.len() - 80).is_multiple_of(16));
        assert!(bytes.len() - 80 > plaintext.len());
    }

    #[test]
    fn mnemonic_import_reproduces_the_spec_vector() {
        let wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();
        let primary = wallet.primary_sub_wallet().unwrap();
        assert_eq!(primary.address, SPEC_ADDRESS);
        assert_eq!(&*primary.private_spend_key.to_hex(), SPEC_SPEND_SECRET);
        assert_eq!(&*wallet.private_view_key().to_hex(), SPEC_VIEW_SECRET);
        assert_eq!(primary.sync_start_height, 4_213_000);
        assert_eq!(primary.sync_start_timestamp, 0);
        assert!(!wallet.is_view_wallet());
        assert_eq!(&**wallet.mnemonic_seed().unwrap(), SPEC_MNEMONIC);
    }

    #[test]
    fn import_from_keys_and_view_only() {
        let spend = SecretKey::from_hex(SPEC_SPEND_SECRET).unwrap();
        let view = SecretKey::from_hex(SPEC_VIEW_SECRET).unwrap();
        let wallet = Wallet::import_from_keys(&spend, &view, 12).unwrap();
        assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));

        let view_only = Wallet::import_view_only(&view, SPEC_ADDRESS, 12).unwrap();
        assert!(view_only.is_view_wallet());
        assert!(view_only.primary_sub_wallet().unwrap().private_spend_key.is_null());
        assert!(view_only.mnemonic_seed().is_none());

        // an all-ones scalar is above the group order and sc_check rejects it
        let bad = SecretKey::from_bytes([0xff; 32]);
        assert!(matches!(Wallet::import_from_keys(&bad, &view, 0), Err(WalletError::InvalidPrivateKey)));
        assert!(matches!(Wallet::import_view_only(&view, "not an address", 0), Err(WalletError::InvalidAddress(_))));
    }

    #[test]
    fn create_new_uses_a_timestamp_start_and_the_network_height() {
        let wallet = Wallet::create_new(0).unwrap();
        let primary = wallet.primary_sub_wallet().unwrap();
        assert_eq!(primary.sync_start_height, 0);
        assert!(primary.sync_start_timestamp > GENESIS_BLOCK_TIMESTAMP);
        // with no daemon answer the timestamp height is what survives
        assert_eq!(wallet.wallet_synchronizer.start_height, timestamp_to_scan_height(primary.sync_start_timestamp));
        assert_eq!(wallet.wallet_synchronizer.start_timestamp, 0);

        // with one, the lower of the two
        let wallet = Wallet::create_new(1_000).unwrap();
        assert_eq!(wallet.wallet_synchronizer.start_height, 1_000);
    }

    #[test]
    fn errors_carry_the_cpp_codes() {
        assert_eq!(WalletError::FilenameNonExistent.code(), 1);
        assert_eq!(WalletError::NotAWalletFile.code(), 3);
        assert_eq!(WalletError::WalletFileCorrupted(String::new()).code(), 4);
        assert_eq!(WalletError::WrongPassword.code(), 5);
        assert_eq!(WalletError::UnsupportedWalletFileFormatVersion(1).code(), 6);
        assert_eq!(WalletError::WalletFileAlreadyExists.code(), 8);
        assert_eq!(WalletError::SubWalletAlreadyExists.code(), 38);
        assert_eq!(WalletError::IllegalViewWalletOperation.code(), 39);
        assert_eq!(WalletError::InvalidPrivateKey.code(), 52);
        // Display and Error are both live
        let e: Box<dyn std::error::Error> = Box::new(WalletError::WrongPassword);
        assert!(e.to_string().contains("password"));
    }
}
