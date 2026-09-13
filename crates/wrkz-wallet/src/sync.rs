// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Wallet synchronization: download blocks, scan them for our outputs, apply
//! them to the [`Wallet`] (spec/10-wallet.md "Sync algorithm", "Balances and
//! spendability"; spec/09-rpc-and-wallet-sync.md).
//!
//! Mirrors, function by function:
//!
//! | C++ | here |
//! | --- | --- |
//! | `BlockDownloader::getBlockCheckpoints` (`BlockDownloader.cpp:326`) | [`Synchronizer::block_checkpoints`] |
//! | `BlockDownloader::downloadBlocks` (line 373) | `Synchronizer::download_step` |
//! | `BlockDownloader::downloadBlocksInParallel` (line 664) | `Synchronizer::download_height_windows` |
//! | `Nigel::getWalletSyncData` (`Nigel.cpp:481`) | `Synchronizer::request_sync_data` |
//! | `Nigel::decreaseRequestedBlockCount` (line 433) | [`Synchronizer::decrease_requested_block_count`] |
//! | `Nigel::resetRequestedBlockCount` (line 453) | [`Synchronizer::reset_requested_block_count`] |
//! | `WalletSynchronizer::processTransactionOutputs` (line 645) | `Synchronizer::process_transaction_outputs` |
//! | `WalletSynchronizer::getGlobalIndexes` (line 705) | `Synchronizer::global_indexes` |
//! | `WalletSynchronizer::completeBlockProcessing` (line 368) | `Synchronizer::complete_block_processing` |
//! | `WalletSynchronizer::processBlockTransactions` (line 457) | `Synchronizer::process_block_transactions` |
//! | `WalletSynchronizer::decryptPaymentID` (line 530) | `Synchronizer::decrypt_payment_id` |
//! | `WalletSynchronizer::checkLockedTransactions` (line 721) | [`Synchronizer::check_locked_transactions`] |
//! | `SubWallet::getTxInputKeyImage` (`SubWallet.cpp:61`) | [`tx_input_key_image`] |
//! | `SubWallets::storeTransactionInput` (`SubWallets.cpp:450`) | [`Wallet::store_transaction_input`] |
//! | `SubWallets::markInputAsSpent` (line 808) | [`Wallet::mark_input_as_spent`] |
//! | `SubWallets::removeForkedTransactions` (line 836) | [`Wallet::remove_forked_transactions`] |
//! | `SubWallets::addTransaction` (line 364) | [`Wallet::add_transaction`] |
//! | `SubWallets::getBalance` (line 778), `SubWallet::getBalance` (line 135) | [`Wallet::balance`] |
//! | `Utilities::isInputUnlocked` (`Utilities.cpp:45`) | [`is_input_unlocked`] |
//! | `WalletBackend::getSyncStatus` (line 1673) | [`Synchronizer::sync_status`] |
//!
//! # Driving it
//!
//! The C++ runs a download thread, `m_threadCount` scanning threads and a main
//! loop that applies the scanned blocks in arrival order
//! (`WalletSynchronizer::mainLoop`). This port has the shape of
//! `WalletSynchronizer::syncStep`, which the WASM build drives and which the C
//! API exposes as `wallet_sync_step`: download one batch into the store, then
//! scan and apply the store. The scan of a chunk runs on
//! [`SyncConfig::scan_threads`] threads, as the C++'s does; applying stays on
//! the caller's thread, strictly in block order. The wire traffic and the
//! resulting wallet are the same at every thread count.
//!
//! [`Synchronizer::sync_step`] never sleeps. It reports the backoff the C++
//! would have slept for (20 s after a `429`, 5 s otherwise) so a caller can
//! wait, and so a test can assert the policy without waiting.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use wrkz_pow::curve;
use wrkz_primitives::constants::{
    BLOCKS_SYNCHRONIZING_DEFAULT_COUNT, CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS,
    CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS, CRYPTONOTE_MAX_ALT_BLOCK_DEPTH, CRYPTONOTE_MAX_BLOCK_NUMBER,
    GLOBAL_INDEXES_OBSCURITY, LAST_KNOWN_BLOCK_HASHES_SIZE, MAX_BLOCKS_PER_SYNC_REQUEST, PRUNE_SPENT_INPUTS_INTERVAL,
    SYNC_REQUEST_CONCURRENCY,
};
use wrkz_primitives::tx::ENCRYPTED_PAYMENT_ID_TAIL;

#[cfg(feature = "native")]
use crate::daemon::Daemon;
use crate::daemon::{
    self, DaemonError, GlobalIndexes, Info, SyncBlock, SyncRequest, SyncTransaction, TransactionsStatus, WalletSyncData,
};
use crate::file::{Hash, Hex32, KeyImage, PublicKey, SecretKey, Transaction, TransactionInput, Transfer, Wallet};

////////////////
/* CONSTANTS  */
////////////////

/// Re-exported from [`wrkz_primitives::constants`], where every network and
/// wallet parameter lives. They were defined here while 2.3 was in flight;
/// keeping the names in scope means callers of either path still compile.
pub use wrkz_primitives::constants::{
    BLOCKS_SYNCHRONIZING_SKIP_EMPTY_MAX_SCAN, BLOCKS_SYNCHRONIZING_SKIP_EMPTY_SCAN_MULTIPLIER, BLOCK_PROCESSING_CHUNK,
    GLOBAL_INDEX_MAX_RETRIES, UNEXPLAINED_SYNC_START_LIMIT,
};

/// What `BlockDownloader::downloader` sleeps after a rate limited request
/// (`BlockDownloader.cpp:232`); spec/09's "wallets treat 429 as back off 20 s".
pub const RATE_LIMITED_BACKOFF: Duration = Duration::from_secs(20);

/// What it sleeps after any other unproductive round (`BlockDownloader.cpp:233`).
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(5);

/// What `blockProcessingThread` sleeps between global index retries
/// (`WalletSynchronizer.cpp:309`).
pub const GLOBAL_INDEX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// `WalletConfig::shortPaymentIDLength`, re-exported from
/// [`wrkz_primitives::constants`].
pub use wrkz_primitives::constants::SHORT_PAYMENT_ID_LENGTH;

/// An output the wallet owns, as scanning produces it: the spend key it was
/// sent to, and the input record. The C++ passes these around as
/// `std::tuple<Crypto::PublicKey, WalletTypes::TransactionInput>`.
pub type OwnedInput = (PublicKey, TransactionInput);

/// A key image of ours seen spent, with the spend key that owns it
/// (`BlockScanTmpInfo::keyImagesToMarkSpent`).
pub type SpentKeyImage = (PublicKey, KeyImage);

////////////////////
/* DAEMON TRAIT   */
////////////////////

/// The daemon calls sync makes. A trait so tests can feed canned responses;
/// [`Daemon`] implements it with the real HTTP client.
///
/// The C++ equivalent is `Nigel`, minus what sync does not use. An
/// implementation must **not** retry or back off: the batch sizing, the `400`
/// halving and the `429` wait live in [`Synchronizer`], the same split the C++
/// makes between `Nigel` and `BlockDownloader`.
pub trait SyncDaemon {
    /// `POST /getwalletsyncdata`.
    fn wallet_sync_data(&self, req: &SyncRequest) -> daemon::Result<WalletSyncData>;

    /// `POST /get_global_indexes_for_range`, `[start, end)`.
    fn global_indexes_for_range(&self, start: u64, end: u64) -> daemon::Result<GlobalIndexes>;

    /// `POST /get_transactions_status`.
    fn transactions_status(&self, hashes: &[String]) -> daemon::Result<TransactionsStatus>;

    /// `GET /info`.
    fn info(&self) -> daemon::Result<Info>;
}

#[cfg(feature = "native")]
impl SyncDaemon for Daemon {
    fn wallet_sync_data(&self, req: &SyncRequest) -> daemon::Result<WalletSyncData> {
        Daemon::wallet_sync_data(self, req)
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> daemon::Result<GlobalIndexes> {
        Daemon::global_indexes_for_range(self, start, end)
    }

    fn transactions_status(&self, hashes: &[String]) -> daemon::Result<TransactionsStatus> {
        Daemon::transactions_status(self, hashes)
    }

    fn info(&self) -> daemon::Result<Info> {
        Daemon::info(self)
    }
}

////////////////////
/* HELPERS        */
////////////////////

/// `Utilities::getLowerBound` (`Utilities.cpp:32`).
pub fn lower_bound(val: u64, nearest_multiple: u64) -> u64 {
    val - (val % nearest_multiple)
}

/// `Utilities::getUpperBound` (`Utilities.cpp:40`).
pub fn upper_bound(val: u64, nearest_multiple: u64) -> u64 {
    lower_bound(val, nearest_multiple) + nearest_multiple
}

/// `Utilities::isInputUnlocked` (`Utilities.cpp:45`) with the clock supplied.
///
/// - `unlock_time == 0` — unlocked; true for nearly every non-coinbase output;
/// - `unlock_time >= CRYPTONOTE_MAX_BLOCK_NUMBER` (500,000,000) — a unix
///   timestamp: unlocked once `now + 60 >= unlock_time`;
/// - otherwise a block height: unlocked once `height + 1 >= unlock_time`.
///
/// A coinbase carries `blockHeight + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` (40)
/// in this field, put there by the daemon's `constructMinerTx`, so the mined
/// money window needs no separate rule here: it arrives as an unlock height
/// like any other.
pub fn is_input_unlocked_at(unlock_time: u64, current_height: u64, now: u64) -> bool {
    if unlock_time == 0 {
        return true;
    }

    if unlock_time >= CRYPTONOTE_MAX_BLOCK_NUMBER {
        return now.saturating_add(CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS) >= unlock_time;
    }

    current_height.saturating_add(CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS) >= unlock_time
}

/// [`is_input_unlocked_at`] against the system clock, as the C++ `std::time(nullptr)`.
pub fn is_input_unlocked(unlock_time: u64, current_height: u64) -> bool {
    is_input_unlocked_at(unlock_time, current_height, now_seconds())
}

fn now_seconds() -> u64 {
    crate::platform::now_seconds()
}

/// `Utilities::encryptPaymentId` (`PaymentIdEncryption.cpp:16`), spec/03
/// "Encrypted short payment ids": XOR the 8 bytes with the first 8 bytes of
/// `keccak(D ‖ 0x8d)`. Encryption and decryption are the same operation.
pub fn encrypt_payment_id(payment_id: &mut [u8; 8], derivation: &[u8; 32]) {
    let mut buffer = [0u8; 33];
    buffer[..32].copy_from_slice(derivation);
    buffer[32] = ENCRYPTED_PAYMENT_ID_TAIL;

    let keystream = wrkz_pow::cn_fast_hash(&buffer);

    for (i, b) in payment_id.iter_mut().enumerate() {
        *b ^= keystream[i];
    }
}

/// `Utilities::encryptPaymentIdHex` (`PaymentIdEncryption.cpp:56`): the hex
/// form, which reports an empty string for anything it cannot process — a
/// payment id that is not 16 hex characters, or a transaction public key that
/// is not a curve point.
pub fn encrypt_payment_id_hex(payment_id_hex: &str, public_key: &[u8; 32], secret_key: &[u8; 32]) -> String {
    let bytes = payment_id_hex.as_bytes();

    if bytes.len() != SHORT_PAYMENT_ID_LENGTH {
        return String::new();
    }

    let mut id = [0u8; 8];
    for (i, b) in id.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16);
        let lo = (bytes[i * 2 + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => *b = (h * 16 + l) as u8,
            _ => return String::new(),
        }
    }

    let Some(derivation) = curve::generate_key_derivation(public_key, secret_key) else {
        return String::new();
    };

    encrypt_payment_id(&mut id, &derivation);

    let mut out = String::with_capacity(SHORT_PAYMENT_ID_LENGTH);
    for b in id {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap());
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap());
    }
    out
}

/// `SubWallet::getTxInputKeyImage` (`SubWallet.cpp:61`): the one-time key pair
/// of an output, and its key image.
///
/// `x = Hs(D‖i) + b`, `P = Hs(D‖i)·G + B`, `I = x·Hp(P)`. A view wallet has no
/// `b`, so it stores a zero key image and a zero ephemeral — the C++
/// default-constructed `KeyImage()` and `SecretKey()` — and therefore cannot
/// see its own outputs being spent.
pub fn tx_input_key_image(
    public_spend_key: &PublicKey,
    private_spend_key: &SecretKey,
    derivation: &[u8; 32],
    output_index: u64,
    is_view_wallet: bool,
) -> (KeyImage, SecretKey) {
    if is_view_wallet {
        return (Hex32([0u8; 32]), SecretKey::NULL);
    }

    let Some(tmp_public) = curve::derive_public_key(derivation, output_index, public_spend_key.as_bytes()) else {
        return (Hex32([0u8; 32]), SecretKey::NULL);
    };

    let tmp_secret = curve::derive_secret_key(derivation, output_index, private_spend_key.as_bytes());

    (Hex32(curve::generate_key_image(&tmp_public, &tmp_secret)), SecretKey::from_bytes(tmp_secret))
}

//////////////////////////
/* WALLET: QUERIES      */
//////////////////////////

/// The three heights the front ends show (`WalletBackend::getSyncStatus`,
/// line 1673). `wallet_block_count` is the last block this wallet processed;
/// the daemon counts are what `/info` last reported, which are counts, not
/// indexes (spec/09: the wallet subtracts one where it wants an index).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncStatus {
    /// `SynchronizationStatus::getHeight`: the last block height applied.
    pub wallet_block_count: u64,
    /// `/info` `height`.
    pub local_daemon_block_count: u64,
    /// `/info` `network_height`.
    pub network_block_count: u64,
}

impl SyncStatus {
    /// Whether the wallet has caught up with the network, the condition
    /// `WalletSynchronizer::mainLoop` uses to fire `onSynced`.
    pub fn is_synced(&self) -> bool {
        self.wallet_block_count >= self.network_block_count
    }
}

/// Unlocked and locked balance of one address (`SubWallets::getBalances`,
/// `SubWallets.cpp:1096`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressBalance {
    pub address: String,
    pub unlocked: u64,
    pub locked: u64,
}

impl Wallet {
    /// `SynchronizationStatus::getHeight`: the height of the last block applied.
    pub fn wallet_height(&self) -> u64 {
        self.wallet_synchronizer.transaction_synchronizer_status.last_known_block_height
    }

    /// `SubWallets::isOurSpendKey` (`SubWallets.cpp:481`).
    pub fn is_our_spend_key(&self, spend_key: &PublicKey) -> bool {
        self.sub_wallets.public_spend_keys.iter().any(|k| k == spend_key)
    }

    /// `SubWallets::getKeyImageOwner` (`SubWallets.cpp:497`) as a whole map,
    /// rebuilt from the stored inputs.
    ///
    /// The C++ keeps `m_keyImageOwners` alive beside the inputs and fills it in
    /// `storeTransactionInput`; this rebuilds the same thing from a loaded
    /// wallet file, which holds the inputs but not the map. Every input of
    /// every list contributes, exactly as the C++ map does.
    pub fn key_image_owners(&self) -> HashMap<[u8; 32], PublicKey> {
        let mut map = HashMap::new();
        if self.sub_wallets.is_view_wallet {
            return map;
        }
        for sub in &self.sub_wallets.sub_wallet {
            for input in sub.all_inputs() {
                map.insert(*input.key_image.as_bytes(), sub.public_spend_key);
            }
        }
        map
    }

    /// `SubWallet::getBalance` (`SubWallet.cpp:135`) for one subwallet:
    /// `(unlocked, locked)`.
    ///
    /// Unlocked is every unspent input whose unlock time has passed at
    /// `current_height`; locked is the rest, plus the unconfirmed incoming
    /// amounts (change on the way back from a send we made). Inputs held by an
    /// in-flight send live in `lockedInputs` and count towards neither.
    pub fn balance_for_spend_key_at(&self, spend_key: &PublicKey, current_height: u64, now: u64) -> (u64, u64) {
        let Some(sub) = self.sub_wallet(spend_key) else {
            return (0, 0);
        };

        let mut unlocked = 0u64;
        let mut locked = 0u64;

        for input in &sub.unspent_inputs {
            if is_input_unlocked_at(input.unlock_time, current_height, now) {
                unlocked = unlocked.wrapping_add(input.amount);
            } else {
                locked = locked.wrapping_add(input.amount);
            }
        }

        for unconfirmed in &sub.unconfirmed_incoming_amounts {
            locked = locked.wrapping_add(unconfirmed.amount);
        }

        (unlocked, locked)
    }

    /// [`Wallet::balance_for_spend_key_at`] against the system clock.
    pub fn balance_for_spend_key(&self, spend_key: &PublicKey, current_height: u64) -> (u64, u64) {
        self.balance_for_spend_key_at(spend_key, current_height, now_seconds())
    }

    /// `SubWallets::getBalance(_, takeFromAll = true, currentHeight)`
    /// (`SubWallets.cpp:778`): the container total as `(unlocked, locked)`.
    ///
    /// `current_height` is the network height in the C++ (`WalletBackend::getTotalBalance`),
    /// not the wallet's own — a locked input becomes spendable when the chain
    /// reaches its unlock height, whether or not this wallet has scanned that
    /// far.
    pub fn balance(&self, current_height: u64) -> (u64, u64) {
        let now = now_seconds();
        self.sub_wallets
            .public_spend_keys
            .iter()
            .map(|k| self.balance_for_spend_key_at(k, current_height, now))
            .fold((0, 0), |(u, l), (a, b)| (u.wrapping_add(a), l.wrapping_add(b)))
    }

    /// `WalletBackend::getBalance(address)` (line 876). `None` when the address
    /// is not one of ours, which the C++ reports as `ADDRESS_NOT_IN_WALLET`.
    pub fn balance_for_address(&self, address: &str, current_height: u64) -> Option<(u64, u64)> {
        let sub = self.sub_wallets.sub_wallet.iter().find(|s| s.address == address)?;
        Some(self.balance_for_spend_key(&sub.public_spend_key, current_height))
    }

    /// `SubWallets::getBalances` (`SubWallets.cpp:1096`): one row per address,
    /// in container order.
    pub fn address_balances(&self, current_height: u64) -> Vec<AddressBalance> {
        let now = now_seconds();
        self.sub_wallets
            .sub_wallet
            .iter()
            .map(|s| {
                let (unlocked, locked) = self.balance_for_spend_key_at(&s.public_spend_key, current_height, now);
                AddressBalance { address: s.address.clone(), unlocked, locked }
            })
            .collect()
    }

    /// `SubWallets::getTransactions` (`SubWallets.cpp:1026`): the confirmed
    /// transactions, in the order they were applied.
    pub fn transactions(&self) -> &[Transaction] {
        &self.sub_wallets.transactions
    }

    /// `SubWallets::getUnconfirmedTransactions` (`SubWallets.cpp:1036`): sent
    /// transactions not yet seen in a block.
    pub fn unconfirmed_transactions(&self) -> &[Transaction] {
        &self.sub_wallets.locked_transactions
    }

    /// The transaction with this hash, confirmed or not.
    pub fn transaction(&self, hash: &Hash) -> Option<&Transaction> {
        self.sub_wallets.transactions.iter().chain(&self.sub_wallets.locked_transactions).find(|t| t.hash == *hash)
    }

    /// `SubWallet::getSpendableInputs` (`SubWallet.cpp:458`) across the
    /// container: every unlocked unspent input with its owning spend key.
    pub fn spendable_inputs(&self, current_height: u64) -> Vec<OwnedInput> {
        let now = now_seconds();
        let mut out = Vec::new();
        for sub in &self.sub_wallets.sub_wallet {
            for input in &sub.unspent_inputs {
                if is_input_unlocked_at(input.unlock_time, current_height, now) {
                    out.push((sub.public_spend_key, input.clone()));
                }
            }
        }
        out
    }
}

//////////////////////////
/* WALLET: MUTATORS     */
//////////////////////////

impl Wallet {
    fn sub_wallet_mut(&mut self, spend_key: &PublicKey) -> Option<&mut crate::file::SubWallet> {
        self.sub_wallets.sub_wallet.iter_mut().find(|s| s.public_spend_key == *spend_key)
    }

    /// `SubWallets::storeTransactionInput` (`SubWallets.cpp:450`) and
    /// `SubWallet::storeTransactionInput` (`SubWallet.cpp:92`).
    ///
    /// A non-view wallet first drops any unconfirmed incoming amount with the
    /// same output key — this input *is* that change, now confirmed. The input
    /// is then appended to `unspentInputs` unless one with the same output key
    /// is already there, which the C++ logs and ignores.
    ///
    /// Returns whether the input was stored.
    pub fn store_transaction_input(&mut self, spend_key: &PublicKey, input: TransactionInput) -> bool {
        let is_view_wallet = self.sub_wallets.is_view_wallet;

        let Some(sub) = self.sub_wallet_mut(spend_key) else {
            return false;
        };

        if !is_view_wallet {
            sub.unconfirmed_incoming_amounts.retain(|stored| stored.key != input.key);
        }

        if sub.unspent_inputs.iter().any(|x| x.key == input.key) {
            return false;
        }

        sub.unspent_inputs.push(input);
        true
    }

    /// `SubWallets::markInputAsSpent` (`SubWallets.cpp:808`) and
    /// `SubWallet::markInputAsSpent` (`SubWallet.cpp:195`): move the input from
    /// `unspentInputs` (or `lockedInputs`) into `spentInputs` and record the
    /// height it was spent at. A key image already in `spentInputs` is not
    /// added twice.
    pub fn mark_input_as_spent(&mut self, key_image: &KeyImage, spend_key: &PublicKey, spend_height: u64) -> bool {
        let Some(sub) = self.sub_wallet_mut(spend_key) else {
            return false;
        };

        let in_spent = sub.spent_inputs.iter().any(|x| x.key_image == *key_image);

        for list in [0usize, 1] {
            let vec = if list == 0 { &mut sub.unspent_inputs } else { &mut sub.locked_inputs };
            if let Some(pos) = vec.iter().position(|x| x.key_image == *key_image) {
                let mut input = vec.remove(pos);
                input.spend_height = spend_height;
                if !in_spent {
                    sub.spent_inputs.push(input);
                }
                return true;
            }
        }

        false
    }

    /// `SubWallets::markInputAsLocked` (`SubWallets.cpp:825`): an input an
    /// in-flight send is using. Not part of sync; 2.4 calls it.
    pub fn mark_input_as_locked(&mut self, key_image: &KeyImage, spend_key: &PublicKey) -> bool {
        let Some(sub) = self.sub_wallet_mut(spend_key) else {
            return false;
        };

        let Some(pos) = sub.unspent_inputs.iter().position(|x| x.key_image == *key_image) else {
            return false;
        };

        let input = sub.unspent_inputs.remove(pos);

        if !sub.locked_inputs.iter().any(|x| x.key_image == *key_image) {
            sub.locked_inputs.push(input);
        }

        true
    }

    /// `SubWallets::addTransaction` (`SubWallets.cpp:364`).
    ///
    /// A payment id recorded when we sent this transaction wins over the one
    /// scanning produced: a short id is encrypted to the receiver, so the copy
    /// coming back from the chain is ciphertext we cannot read. The locked copy
    /// is then dropped, and a transaction whose hash is already recorded is
    /// ignored.
    pub fn add_transaction(&mut self, mut tx: Transaction) -> bool {
        if let Some(locked) = self.sub_wallets.locked_transactions.iter().find(|t| t.hash == tx.hash) {
            if !locked.payment_id.is_empty() {
                tx.payment_id = locked.payment_id.clone();
            }
        }

        self.sub_wallets.locked_transactions.retain(|t| t.hash != tx.hash);

        if self.sub_wallets.transactions.iter().any(|t| t.hash == tx.hash) {
            return false;
        }

        self.sub_wallets.transactions.push(tx);
        true
    }

    /// `SubWallets::removeForkedTransactions` (`SubWallets.cpp:836`) and
    /// `SubWallet::removeForkedInputs` (`SubWallet.cpp:319`). Returns the key
    /// images that left the container, which the caller drops from its owner
    /// map — empty for a view wallet, as the C++ returns.
    ///
    /// 1. a transaction we *sent* at or above the fork height, with a payment
    ///    id, goes back into `lockedTransactions` with height and timestamp
    ///    cleared, so its plaintext payment id survives the rescan;
    /// 2. every transaction at or above the fork height is dropped;
    /// 3. unconfirmed incoming amounts are cleared, and every input *received*
    ///    at or above the fork height leaves all three lists;
    /// 4. an input received below the fork height but *spent* at or above it
    ///    has its spend height cleared and returns to `unspentInputs`.
    pub fn remove_forked_transactions(&mut self, fork_height: u64) -> Vec<KeyImage> {
        if !self.sub_wallets.is_view_wallet {
            let readd: Vec<Transaction> = self
                .sub_wallets
                .transactions
                .iter()
                .filter(|t| t.block_height >= fork_height && !t.payment_id.is_empty() && t.total_amount() < 0)
                .map(|t| Transaction { block_height: 0, timestamp: 0, ..t.clone() })
                .collect();

            self.sub_wallets.locked_transactions.extend(readd);
        }

        self.sub_wallets.transactions.retain(|t| t.block_height < fork_height);

        let mut key_images_to_remove = Vec::new();

        for sub in &mut self.sub_wallets.sub_wallet {
            sub.unconfirmed_incoming_amounts.clear();

            for list in [&mut sub.locked_inputs, &mut sub.unspent_inputs, &mut sub.spent_inputs] {
                for input in list.iter() {
                    if input.block_height >= fork_height {
                        key_images_to_remove.push(input.key_image);
                    }
                }
                list.retain(|input| input.block_height < fork_height);
            }

            let mut still_spent = Vec::with_capacity(sub.spent_inputs.len());
            for mut input in std::mem::take(&mut sub.spent_inputs) {
                if input.spend_height >= fork_height {
                    input.spend_height = 0;
                    if !sub.unspent_inputs.iter().any(|x| x.key == input.key) {
                        sub.unspent_inputs.push(input);
                    }
                } else {
                    still_spent.push(input);
                }
            }
            sub.spent_inputs = still_spent;
        }

        if self.sub_wallets.is_view_wallet {
            return Vec::new();
        }

        key_images_to_remove
    }

    /// `SubWallets::pruneSpentInputs` (`SubWallets.cpp:1110`) and
    /// `SubWallet::pruneSpentInputs` (`SubWallet.cpp:495`): drop spent inputs
    /// spent at or below `prune_height`, once they are too old to be undone by
    /// any fork we would follow.
    pub fn prune_spent_inputs(&mut self, prune_height: u64) {
        for sub in &mut self.sub_wallets.sub_wallet {
            sub.spent_inputs.retain(|input| input.spend_height > prune_height);
        }
    }

    /// `SubWallets::removeCancelledTransactions` (`SubWallets.cpp:906`) and
    /// `SubWallet::removeCancelledTransactions` (`SubWallet.cpp:404`): a send
    /// the daemon has never heard of is dropped and its inputs return to
    /// `unspentInputs`.
    pub fn remove_cancelled_transactions(&mut self, cancelled: &[Hash]) {
        let is_cancelled = |h: &Hash| cancelled.contains(h);

        self.sub_wallets.locked_transactions.retain(|t| !is_cancelled(&t.hash));

        for sub in &mut self.sub_wallets.sub_wallet {
            let mut still_locked = Vec::with_capacity(sub.locked_inputs.len());
            for mut input in std::mem::take(&mut sub.locked_inputs) {
                if is_cancelled(&input.parent_transaction_hash) {
                    input.spend_height = 0;
                    sub.unspent_inputs.push(input);
                } else {
                    still_locked.push(input);
                }
            }
            sub.locked_inputs = still_locked;

            sub.unconfirmed_incoming_amounts.retain(|i| !is_cancelled(&i.parent_transaction_hash));
        }
    }

    /// `SubWallets::convertSyncTimestampToHeight` (`SubWallets.cpp:1086`) and
    /// `SubWallet::convertSyncTimestampToHeight` (`SubWallet.cpp:486`).
    ///
    /// Note the C++ writes the timestamp back rather than clearing it, so a
    /// subwallet ends up with both a start height and a start timestamp set.
    /// Reproduced as-is: the wallet file records it that way.
    pub fn convert_sync_timestamp_to_height(&mut self, timestamp: u64, height: u64) {
        for sub in &mut self.sub_wallets.sub_wallet {
            if sub.sync_start_timestamp != 0 {
                sub.sync_start_timestamp = timestamp;
                sub.sync_start_height = height;
            }
        }
    }

    /// `SubWallets::reset` (`SubWallets.cpp:980`) plus
    /// `WalletSynchronizer::reset` (line 838): forget every input, transaction
    /// and block hash, and scan again from `scan_height`.
    pub fn reset(&mut self, scan_height: u64) {
        for sub in &mut self.sub_wallets.sub_wallet {
            sub.sync_start_timestamp = 0;
            sub.sync_start_height = scan_height;
            sub.locked_inputs.clear();
            sub.unconfirmed_incoming_amounts.clear();
            sub.unspent_inputs.clear();
            sub.spent_inputs.clear();
        }

        self.sub_wallets.transactions.clear();
        self.sub_wallets.locked_transactions.clear();

        self.wallet_synchronizer.start_height = scan_height;
        self.wallet_synchronizer.start_timestamp = 0;
        self.wallet_synchronizer.transaction_synchronizer_status = Default::default();
    }
}

//////////////////////////
/* SYNCHRONIZER         */
//////////////////////////

/// What sync is allowed to ask for, and how it reacts when it cannot get it.
///
/// The defaults are the C++ ones: `Config::config.wallet.skipCoinbaseTransactions`
/// off (which `wrkz-wallet` and `wrkz-wallet-api` now keep unless told to
/// skip, where the C++ front ends turned it on unless told to scan), `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT` (100) to start,
/// `WalletConfig::maxBlocksPerSyncRequest` (1000) as the ceiling,
/// `Constants::BLOCK_PROCESSING_CHUNK` (500) blocks applied per step.
#[derive(Clone, Debug)]
pub struct SyncConfig {
    /// `skipCoinbaseTransactions`: do not ask for, and do not scan, coinbases.
    /// Turning this on also makes non-contiguous heights expected, because a
    /// daemon drops blocks that then hold nothing (`BlockDownloader.cpp:400`).
    pub skip_coinbase_transactions: bool,
    /// Where the batch size starts (`BLOCKS_SYNCHRONIZING_DEFAULT_COUNT`), and
    /// the floor the `400` halving stops at.
    pub start_block_count: u64,
    /// The ceiling the batch grows to (`WalletConfig::maxBlocksPerSyncRequest`),
    /// lowered whenever a daemon answers `400`.
    pub max_block_count: u64,
    /// Blocks applied per [`Synchronizer::sync_step`] (`BLOCK_PROCESSING_CHUNK`).
    pub block_processing_chunk: usize,
    /// Reported after a `429` (`RATE_LIMITED_BACKOFF`).
    pub rate_limited_backoff: Duration,
    /// Reported after any other unproductive round (`FAILURE_BACKOFF`).
    pub failure_backoff: Duration,
    /// `GLOBAL_INDEX_MAX_RETRIES`: daemon calls per block whose global indexes
    /// will not resolve, after which the index is left unset.
    pub global_index_retries: usize,
    /// Slept between those retries. Zero in tests.
    pub global_index_retry_delay: Duration,
    /// Whether to use the height-window path far below the tip
    /// (`downloadBlocksInParallel`). Off by default; see
    /// `Synchronizer::download_height_windows`.
    pub height_windows: bool,
    /// `WalletConfig::syncRequestConcurrency` (4): windows per round.
    pub sync_request_concurrency: u64,
    /// `m_threadCount`: threads scanning a chunk of stored blocks for our
    /// outputs — the key derivation per transaction and the underive per
    /// output, which is where a rescan spends its time. `1` is the sequential
    /// path, and the outcome is identical at every value
    /// (`Synchronizer::scan_blocks`).
    pub scan_threads: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            skip_coinbase_transactions: false,
            start_block_count: BLOCKS_SYNCHRONIZING_DEFAULT_COUNT as u64,
            max_block_count: MAX_BLOCKS_PER_SYNC_REQUEST as u64,
            block_processing_chunk: BLOCK_PROCESSING_CHUNK,
            rate_limited_backoff: RATE_LIMITED_BACKOFF,
            failure_backoff: FAILURE_BACKOFF,
            global_index_retries: GLOBAL_INDEX_MAX_RETRIES,
            global_index_retry_delay: GLOBAL_INDEX_RETRY_DELAY,
            height_windows: false,
            sync_request_concurrency: SYNC_REQUEST_CONCURRENCY as u64,
            scan_threads: crate::platform::available_threads().clamp(1, 16),
        }
    }
}

/// What the last `/info` said, as `Nigel` caches it.
///
/// The two block counts are **top indexes**: `/info` reports counts and
/// `Nigel::getDaemonInfo` (line 867) subtracts one from each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DaemonState {
    /// `Nigel::localDaemonBlockCount`.
    pub local_block_count: u64,
    /// `Nigel::networkBlockCount`.
    pub network_block_count: u64,
    /// `Nigel::liteStartHeight`: this daemon holds nothing below it.
    pub lite_start_height: u64,
    /// `sync_features` contains `skipEmptyBlocks`.
    pub skips_empty_blocks: bool,
    /// `sync_features` contains `heightRange`.
    pub supports_height_range: bool,
    /// `/info` `top_block_hash`.
    pub top_block_hash: Option<String>,
}

/// What one [`Synchronizer::sync_step`] did, and what the C++ would sleep next.
#[derive(Debug)]
pub enum SyncStep {
    /// Blocks were downloaded and applied.
    Processed {
        /// Blocks applied this step.
        blocks: usize,
        /// New transactions recorded.
        transactions: usize,
        /// The height of the last block applied.
        height: u64,
    },
    /// The daemon had no blocks for us and named its top block, which is now
    /// our height: `blocks.empty() && topBlock && store.empty()`
    /// (`BlockDownloader.cpp:501`). When the top block is one we already
    /// recorded, `storeBlockHash` keeps the checkpoint lists unchanged and only
    /// the height moves — the "top hash is ours already" no-op.
    Synced {
        /// The daemon's top block height.
        height: u64,
    },
    /// Nothing to do (no blocks, or an answer that could not be trusted and
    /// will be retried). Wait `backoff`.
    Idle {
        /// What `BlockDownloader::downloader` would sleep.
        backoff: Duration,
    },
    /// The daemon call failed. Wait `backoff` — 20 s after a `429`.
    Failed {
        /// The failure as the client classified it.
        error: DaemonError,
        /// What `BlockDownloader::downloader` would sleep.
        backoff: Duration,
    },
    /// Sync has stopped: this daemon holds nothing below
    /// `daemon_serves_from`, and we have only covered up to `covered_to`.
    /// Carrying on would record unscanned blocks as scanned
    /// (`BlockDownloader::recordSyncGap`).
    Gap {
        /// The highest block this wallet has actually scanned.
        covered_to: u64,
        /// The lowest block the daemon can serve.
        daemon_serves_from: u64,
    },
}

/// The outcome of the download half of a step (`BlockDownloader::downloadBlocks`
/// returns a bare bool; this says why).
#[derive(Debug)]
enum Download {
    Stored,
    Synced(u64),
    Idle,
    Failed(DaemonError),
    Gap,
}

/// Downloads blocks, scans them against the wallet's keys, and applies them.
///
/// Owns the [`Wallet`] while syncing; [`Synchronizer::into_wallet`] takes it
/// back to save. The daemon is any [`SyncDaemon`], so tests drive canned
/// responses through the same code the HTTP client runs.
pub struct Synchronizer<D: SyncDaemon> {
    daemon: D,
    wallet: Wallet,
    config: SyncConfig,
    state: DaemonState,

    /// `Nigel::m_blockCount`.
    block_count: u64,
    /// `Nigel::m_maxBlockCount`, lowered by a `400`.
    max_block_count: u64,
    /// `Nigel::m_lastRequestRateLimited`.
    last_request_rate_limited: bool,

    /// `BlockDownloader::m_startHeight` / `m_startTimestamp`. The downloader
    /// keeps its own copies and resolves the timestamp into a height on the
    /// first answer; `WalletSynchronizer`'s copies, which is what the wallet
    /// file holds, are left alone (`BlockDownloader::fromJSON`).
    start_height: u64,
    start_timestamp: u64,

    /// `BlockDownloader::m_storedBlocks`: downloaded, not yet applied.
    stored_blocks: VecDeque<SyncBlock>,
    /// `BlockDownloader::m_nextDownloadHeight`, the height-window cursor.
    next_download_height: u64,
    /// `BlockDownloader::m_unexplainedStartCount`.
    unexplained_start_count: u64,
    /// `BlockDownloader::m_syncGapCoveredTo` / `m_syncGapDaemonServesFrom`.
    sync_gap: Option<(u64, u64)>,

    /// `SubWallets::m_keyImageOwners`.
    key_image_owners: HashMap<[u8; 32], PublicKey>,

    /// Counted for tests and diagnostics; the C++ only logs it.
    forks_resolved: u64,

    /// The hashes the last [`Synchronizer::sync_step`] added to the wallet, in
    /// the order it added them: what `m_eventHandler->onTransaction` fires for
    /// (`WalletSynchronizer.cpp:407`). Cleared at the start of every step, so
    /// it never holds more than one step's worth.
    step_added: Vec<Hash>,
}

impl<D: SyncDaemon> Synchronizer<D> {
    /// A synchronizer for `wallet`, with the C++ defaults.
    pub fn new(daemon: D, wallet: Wallet) -> Self {
        Self::with_config(daemon, wallet, SyncConfig::default())
    }

    /// [`Synchronizer::new`] with the knobs set.
    pub fn with_config(daemon: D, wallet: Wallet, config: SyncConfig) -> Self {
        let key_image_owners = wallet.key_image_owners();
        let start_height = wallet.wallet_synchronizer.start_height;
        let start_timestamp = wallet.wallet_synchronizer.start_timestamp;
        let block_count = config.start_block_count;
        let max_block_count = config.max_block_count;

        Synchronizer {
            daemon,
            wallet,
            config,
            state: DaemonState::default(),
            block_count,
            max_block_count,
            last_request_rate_limited: false,
            start_height,
            start_timestamp,
            stored_blocks: VecDeque::new(),
            next_download_height: 0,
            unexplained_start_count: 0,
            sync_gap: None,
            key_image_owners,
            forks_resolved: 0,
            step_added: Vec::new(),
        }
    }

    /// The wallet being synced.
    pub fn wallet(&self) -> &Wallet {
        &self.wallet
    }

    /// The wallet, mutably. Changing its inputs behind the synchronizer's back
    /// invalidates the key image owner map; call [`Synchronizer::refresh_key_image_owners`].
    pub fn wallet_mut(&mut self) -> &mut Wallet {
        &mut self.wallet
    }

    /// The container and the daemon at once, which a send needs: `&mut Wallet`
    /// to record what it spent, and `&D` to fetch decoys and relay. Two
    /// separate accessors cannot give both, because one borrows `self` mutably.
    pub fn split_for_transfer(&mut self) -> (&mut Wallet, &D) {
        (&mut self.wallet, &self.daemon)
    }

    /// Take the wallet back, to save it.
    pub fn into_wallet(self) -> Wallet {
        self.wallet
    }

    /// The daemon this synchronizer talks to.
    pub fn daemon(&self) -> &D {
        &self.daemon
    }

    /// What the last `/info` said.
    pub fn daemon_state(&self) -> &DaemonState {
        &self.state
    }

    /// The configuration.
    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    /// `Nigel::requestedBlockCount`: the current batch size.
    pub fn requested_block_count(&self) -> u64 {
        self.block_count
    }

    /// The ceiling the batch may grow back to, lowered by a `400`.
    pub fn max_block_count(&self) -> u64 {
        self.max_block_count
    }

    /// Whether the last request was answered with `429`.
    pub fn last_request_rate_limited(&self) -> bool {
        self.last_request_rate_limited
    }

    /// Blocks downloaded but not yet applied.
    pub fn stored_block_count(&self) -> usize {
        self.stored_blocks.len()
    }

    /// How many times a fork has been unwound.
    pub fn forks_resolved(&self) -> u64 {
        self.forks_resolved
    }

    /// The transactions the last [`Synchronizer::sync_step`] recorded, by hash,
    /// in order — incoming and outgoing alike, including one of ours that has
    /// just confirmed. A front end reads it to log them or to fire
    /// `--tx-notify`, the way the C++ subscribes to `onTransaction`; a later
    /// block in the same step may have forked one away again, which the C++
    /// event does not account for either.
    pub fn last_step_added(&self) -> &[Hash] {
        &self.step_added
    }

    /// `BlockDownloader::getSyncGap`: `(coveredTo, daemonServesFrom)` when sync
    /// has stopped because the daemon cannot serve the range we still need.
    pub fn sync_gap(&self) -> Option<(u64, u64)> {
        self.sync_gap
    }

    /// Rebuild `SubWallets::m_keyImageOwners` from the wallet's inputs.
    pub fn refresh_key_image_owners(&mut self) {
        self.key_image_owners = self.wallet.key_image_owners();
    }

    /// `WalletBackend::getSyncStatus` (line 1673).
    pub fn sync_status(&self) -> SyncStatus {
        SyncStatus {
            wallet_block_count: self.wallet.wallet_height(),
            local_daemon_block_count: self.state.local_block_count,
            network_block_count: self.state.network_block_count,
        }
    }

    /// `WalletBackend::getTotalBalance` (line 893): the container balance at
    /// the network height.
    pub fn total_balance(&self) -> (u64, u64) {
        self.wallet.balance(self.state.network_block_count)
    }

    /// `Nigel::getDaemonInfo` (line 847): refresh the cached daemon state.
    /// The C++ background thread does this every ten seconds, and `syncStep`
    /// keeps that cadence (`WalletSynchronizer.cpp:886`).
    pub fn refresh_info(&mut self) -> daemon::Result<Info> {
        let info = self.daemon.info()?;

        self.state.local_block_count = info.height.saturating_sub(1);
        self.state.network_block_count = info.network_height.saturating_sub(1);
        self.state.lite_start_height = info.lite_start_height;
        self.state.skips_empty_blocks = info.supports("skipEmptyBlocks");
        self.state.supports_height_range = info.supports("heightRange");
        self.state.top_block_hash = info.top_block_hash.clone();

        Ok(info)
    }

    ////////////////////////
    /* BATCH SIZE         */
    ////////////////////////

    /// `Nigel::decreaseRequestedBlockCount` (line 433): halve after an empty or
    /// failed answer, never below one, and never after a `429` — a rate limited
    /// request never reached the daemon's block assembly, and a smaller batch
    /// would only spend more rate limit slots per block.
    pub fn decrease_requested_block_count(&mut self) {
        if self.last_request_rate_limited {
            return;
        }

        if self.block_count > 1 {
            self.block_count /= 2;
        }
    }

    /// `Nigel::resetRequestedBlockCount` (line 453): after a successful fetch,
    /// climb back — to the default if below it, else double, capped by the
    /// ceiling a `400` may have lowered.
    pub fn reset_requested_block_count(&mut self) {
        let ceiling = self.max_block_count;
        let mut current = self.block_count;

        if current < self.config.start_block_count {
            current = self.config.start_block_count;
        } else if current < ceiling {
            current = (current * 2).min(ceiling);
        }

        self.block_count = current.min(ceiling);
    }

    ////////////////////////
    /* REQUESTS           */
    ////////////////////////

    /// `BlockDownloader::highestKnownHeight` (line 134): the highest block we
    /// hold, processed or merely downloaded.
    pub fn highest_known_height(&self) -> u64 {
        let processed = self.wallet.wallet_height();
        match self.stored_blocks.back() {
            Some(block) => processed.max(block.block_height),
            None => processed,
        }
    }

    /// `BlockDownloader::getBlockCheckpoints` (line 326), the shape spec/10
    /// pins: the hashes of the up-to-50 most recently *downloaded but
    /// unprocessed* blocks, newest first; padded to 50 with the most recently
    /// *processed* hashes; then the sparse 5000-block checkpoints. The daemon
    /// resumes after the first hash it knows, so a reorganisation shallower
    /// than the recent list is recovered without the wallet doing anything.
    pub fn block_checkpoints(&self) -> Vec<String> {
        let mut result: Vec<String> = self
            .stored_blocks
            .iter()
            .rev()
            .take(LAST_KNOWN_BLOCK_HASHES_SIZE)
            .map(|b| b.block_hash.to_lowercase())
            .collect();

        let status = &self.wallet.wallet_synchronizer.transaction_synchronizer_status;

        if result.len() < LAST_KNOWN_BLOCK_HASHES_SIZE {
            let take = (LAST_KNOWN_BLOCK_HASHES_SIZE - result.len()).min(status.last_known_block_hashes.len());
            result.extend(status.last_known_block_hashes[..take].iter().map(|h| h.to_hex()));
        }

        result.extend(status.block_hash_checkpoints.iter().map(|h| h.to_hex()));

        result
    }

    /// `Nigel::getWalletSyncData` (`Nigel.cpp:481`): the request body, plus the
    /// `400` and `429` policy.
    ///
    /// A `400` with a batch above the default means the daemon's
    /// `--rpc-max-block-count` is lower than what we asked: remember the
    /// ceiling, halve, and retry at once. A `429` is reported to the caller,
    /// which waits 20 s and keeps the batch size.
    ///
    /// `skipInputKeyOffsets` is always sent — ring offsets are only needed to
    /// build a spend, and that comes from `/getrandom_outs`. `skipEmptyBlocks`
    /// is sent only when coinbases are being skipped *and* the daemon
    /// advertised the feature. `encoding: base64` is never sent: this port does
    /// not decode it.
    fn request_sync_data(
        &mut self,
        checkpoints: &[String],
        start_height: u64,
        start_timestamp: u64,
        end_height: Option<u64>,
    ) -> daemon::Result<WalletSyncData> {
        loop {
            let request = SyncRequest {
                block_hash_checkpoints: checkpoints.to_vec(),
                start_height,
                start_timestamp,
                block_count: self.block_count,
                skip_coinbase_transactions: self.config.skip_coinbase_transactions,
                skip_input_key_offsets: Some(true),
                skip_empty_blocks: (self.config.skip_coinbase_transactions && self.state.skips_empty_blocks)
                    .then_some(true),
                encoding: None,
                end_height,
            };

            match self.daemon.wallet_sync_data(&request) {
                Ok(data) => {
                    self.last_request_rate_limited = false;
                    return Ok(data);
                }
                Err(DaemonError::RateLimited) => {
                    self.last_request_rate_limited = true;
                    return Err(DaemonError::RateLimited);
                }
                Err(DaemonError::BadRequest(body)) => {
                    self.last_request_rate_limited = false;

                    if self.block_count > self.config.start_block_count {
                        let reduced = (self.block_count / 2).max(self.config.start_block_count);
                        self.max_block_count = reduced;
                        self.block_count = reduced;
                        continue;
                    }

                    return Err(DaemonError::BadRequest(body));
                }
                Err(e) => {
                    self.last_request_rate_limited = false;
                    return Err(e);
                }
            }
        }
    }

    fn record_sync_gap(&mut self, covered_to: u64, daemon_serves_from: u64) {
        self.sync_gap = Some((covered_to, daemon_serves_from));
        self.next_download_height = 0;
    }

    fn clear_sync_gap(&mut self) {
        self.unexplained_start_count = 0;
        self.sync_gap = None;
    }

    /// `BlockDownloader::storeDownloadedBlocks` (line 634), with the block
    /// hashes validated here rather than at parse time: a hash that is not 64
    /// hex characters would otherwise only fail once it reached the wallet
    /// file.
    fn store_downloaded_blocks(&mut self, blocks: Vec<SyncBlock>) -> daemon::Result<usize> {
        for block in &blocks {
            if Hex32::from_hex(&block.block_hash).is_none() {
                return Err(DaemonError::Json(format!(
                    "block {} has a malformed hash {:?}",
                    block.block_height, block.block_hash
                )));
            }
        }

        let count = blocks.len();
        self.stored_blocks.extend(blocks);
        Ok(count)
    }

    /// `BlockDownloader::downloadBlocks` (line 373): one `/getwalletsyncdata`
    /// round.
    fn download_step(&mut self) -> Download {
        if self.state.local_block_count < self.wallet.wallet_height() {
            return Download::Idle;
        }

        let covered_to = self.highest_known_height();
        let lite_start_height = self.state.lite_start_height;

        if lite_start_height != 0 && covered_to == 0 && self.start_height < lite_start_height {
            // Nothing scanned yet, so meet the lite daemon where it starts. A
            // wallet created from a timestamp has to resolve it here, since we
            // are about to clear it.
            if self.start_timestamp != 0 {
                self.wallet.convert_sync_timestamp_to_height(self.start_timestamp, lite_start_height);
            }
            self.start_height = lite_start_height;
            self.start_timestamp = 0;
        } else if lite_start_height != 0 && covered_to != 0 && covered_to + 1 < lite_start_height {
            // We have scanned part of the chain and this daemon cannot serve
            // the rest of it. Storing what it would answer moves our position
            // over blocks nobody looked at: a silently wrong balance.
            self.record_sync_gap(covered_to, lite_start_height);
            return Download::Gap;
        }

        let checkpoints = self.block_checkpoints();
        let (start_height, start_timestamp) = (self.start_height, self.start_timestamp);

        let data = match self.request_sync_data(&checkpoints, start_height, start_timestamp, None) {
            Ok(data) => data,
            Err(e) => {
                // Anything but a clean answer leaves us unsure where we are, so
                // the height-window path must stand down.
                self.next_download_height = 0;
                self.decrease_requested_block_count();
                return Download::Failed(e);
            }
        };

        if data.items.is_empty() {
            // A daemon with no blocks for us is not failing to serve anything,
            // so a recorded gap has outlived its cause.
            self.clear_sync_gap();

            if data.synced {
                if let Some(top) = &data.top_block {
                    if self.stored_blocks.is_empty() {
                        let Some(hash) = Hex32::from_hex(&top.hash) else {
                            return Download::Failed(DaemonError::Json(format!(
                                "top block hash {:?} is malformed",
                                top.hash
                            )));
                        };

                        // storeBlockHash returns early when this is the hash it
                        // already holds, so a repeated "you are at the top"
                        // answer changes nothing but the height.
                        self.wallet
                            .wallet_synchronizer
                            .transaction_synchronizer_status
                            .store_block_hash(hash, top.height);

                        return Download::Synced(top.height);
                    }
                }
            }

            self.decrease_requested_block_count();
            return Download::Idle;
        }

        self.reset_requested_block_count();

        // What comes back has to carry on from where we are. The one hole we
        // can expect is one we asked for, by telling the daemon to leave
        // coinbase-only blocks out. A timestamp start is exempt: the daemon
        // decides what height it resolves to.
        let holes_expected = self.config.skip_coinbase_transactions;
        let first_height = data.items[0].block_height;

        if !holes_expected && self.start_timestamp == 0 {
            let expected_start =
                if covered_to == 0 { self.start_height } else { (covered_to + 1).max(self.start_height) };

            if first_height > expected_start {
                let covered_to_report = expected_start.saturating_sub(1);

                if lite_start_height != 0 && expected_start < lite_start_height {
                    self.record_sync_gap(covered_to_report, lite_start_height);
                    return Download::Gap;
                }

                self.next_download_height = 0;
                self.unexplained_start_count += 1;

                if self.unexplained_start_count < UNEXPLAINED_SYNC_START_LIMIT {
                    // A reorg at the tip looks exactly like this and is over by
                    // the next request. Retry rather than stopping a wallet
                    // dead over one block.
                    return Download::Idle;
                }

                self.record_sync_gap(covered_to_report, first_height);
                return Download::Gap;
            }
        }

        // A timestamp is transient; a block height is not.
        if self.start_timestamp != 0 {
            let previous = self.start_timestamp;
            self.start_timestamp = 0;
            self.start_height = first_height;
            self.wallet.convert_sync_timestamp_to_height(previous, first_height);
        }

        let scanned_to_height = data.scanned_to_height.unwrap_or(0);
        let last_height = data.items[data.items.len() - 1].block_height;

        if let Err(e) = self.store_downloaded_blocks(data.items) {
            return Download::Failed(e);
        }

        // The daemon told us how far it looked, which is not the same as the
        // highest block it sent: the heights in between held nothing for us.
        self.next_download_height = if scanned_to_height != 0 { scanned_to_height + 1 } else { last_height + 1 };

        self.clear_sync_gap();

        Download::Stored
    }

    /// `BlockDownloader::downloadBlocksInParallel` (line 664): once the
    /// sequential path has established where we are, and while that is far
    /// enough below the tip that no reorganisation can reach
    /// (`4 · window + 4 · CRYPTONOTE_MAX_ALT_BLOCK_DEPTH`), ask for four
    /// consecutive height windows carrying no checkpoints.
    ///
    /// The C++ issues the four requests concurrently and stores the answers in
    /// order. This port issues the same four requests, with the same bodies and
    /// in the same order, one after another — same wire traffic, same stored
    /// blocks, no threads. Off by default ([`SyncConfig::height_windows`]).
    ///
    /// `None` means the path does not apply and the sequential one should run.
    fn download_height_windows(&mut self) -> Option<Download> {
        let first_height = self.next_download_height;

        if !self.config.height_windows
            || self.config.sync_request_concurrency < 2
            || first_height == 0
            || self.sync_gap.is_some()
            || !self.state.supports_height_range
        {
            return None;
        }

        let window = (self.block_count * BLOCKS_SYNCHRONIZING_SKIP_EMPTY_SCAN_MULTIPLIER)
            .min(BLOCKS_SYNCHRONIZING_SKIP_EMPTY_MAX_SCAN);

        let reorg_safety_margin = 4 * CRYPTONOTE_MAX_ALT_BLOCK_DEPTH;
        let span = window * self.config.sync_request_concurrency + reorg_safety_margin;
        let daemon_height = self.state.local_block_count;

        if window == 0 || daemon_height < first_height || daemon_height - first_height < span {
            return None;
        }

        let mut completed_windows = 0;

        for i in 0..self.config.sync_request_concurrency {
            let window_start = first_height + window * i;
            let window_end = window_start + window;

            let Ok(data) = self.request_sync_data(&[], window_start, 0, Some(window_end)) else {
                break;
            };

            let scanned_to_height = data.scanned_to_height.unwrap_or(0);

            // A window we cannot vouch for ends the run: taking the ones behind
            // it would leave a hole, and a hole is a transaction never seen.
            if scanned_to_height == 0 {
                break;
            }

            if let Err(e) = self.store_downloaded_blocks(data.items) {
                return Some(Download::Failed(e));
            }

            self.next_download_height = scanned_to_height + 1;
            completed_windows += 1;

            // The daemon stopped short of the window, almost certainly on its
            // response size budget. Resume from where it reached.
            if scanned_to_height + 1 < window_end {
                break;
            }
        }

        if completed_windows == 0 {
            self.next_download_height = 0;
            return None;
        }

        self.reset_requested_block_count();

        Some(Download::Stored)
    }

    ////////////////////////
    /* SCANNING           */
    ////////////////////////

    /// `WalletSynchronizer::processTransactionOutputs` (line 645): derive once
    /// from the transaction public key and our private view key, then underive
    /// every output and keep the ones whose base spend key is one of ours.
    ///
    /// A transaction public key that is not a curve point makes
    /// `generate_key_derivation` fail. The C++ ignores that failure and
    /// underives against an uninitialised derivation; this skips the
    /// transaction, which finds the same nothing without reading uninitialised
    /// memory.
    fn process_transaction_outputs(
        wallet: &Wallet,
        tx: &SyncTransaction,
        block_height: u64,
    ) -> daemon::Result<Vec<OwnedInput>> {
        let mut inputs = Vec::new();

        let Some(tx_public_key) = Hex32::from_hex(&tx.tx_public_key) else {
            return Err(DaemonError::Json(format!("transaction {} has a malformed public key", tx.hash)));
        };

        let Some(tx_hash) = Hex32::from_hex(&tx.hash) else {
            return Err(DaemonError::Json(format!("transaction hash {:?} is malformed", tx.hash)));
        };

        let view_key = *wallet.private_view_key().as_bytes();

        let Some(derivation) = curve::generate_key_derivation(tx_public_key.as_bytes(), &view_key) else {
            return Ok(inputs);
        };

        let is_view_wallet = wallet.is_view_wallet();

        for (output_index, output) in tx.outputs.iter().enumerate() {
            let output_index = output_index as u64;

            let Some(key) = Hex32::from_hex(&output.key) else {
                return Err(DaemonError::Json(format!("transaction {} has a malformed output key", tx.hash)));
            };

            let Some(derived_spend_key) = curve::underive_public_key(&derivation, output_index, key.as_bytes()) else {
                continue;
            };

            let derived_spend_key = Hex32(derived_spend_key);

            if !wallet.is_our_spend_key(&derived_spend_key) {
                continue;
            }

            let private_spend_key =
                wallet.sub_wallet(&derived_spend_key).map(|s| s.private_spend_key.clone()).unwrap_or(SecretKey::NULL);

            let (key_image, private_ephemeral) =
                tx_input_key_image(&derived_spend_key, &private_spend_key, &derivation, output_index, is_view_wallet);

            inputs.push((
                derived_spend_key,
                TransactionInput {
                    amount: output.amount,
                    block_height,
                    // The daemon's /getwalletsyncdata never supplies this; a
                    // blockchain cache API does (`WalletTypes.h:679`).
                    global_output_index: output.global_index,
                    key,
                    key_image,
                    parent_transaction_hash: tx_hash,
                    private_ephemeral: Some(private_ephemeral),
                    spend_height: 0,
                    transaction_index: output_index,
                    transaction_public_key: tx_public_key,
                    unlock_time: tx.unlock_time,
                },
            ));
        }

        Ok(inputs)
    }

    /// `WalletSynchronizer::processBlockOutputs` (line 347).
    ///
    /// A function of the wallet's keys and the block alone — not a method —
    /// so it can run on scanning threads: nothing it reads changes during a
    /// sync (the spend keys and the view key are fixed; what applying a block
    /// changes is the transactions, the inputs and the key-image owners, none
    /// of which it looks at).
    fn process_block_outputs(
        wallet: &Wallet,
        skip_coinbase_transactions: bool,
        block: &SyncBlock,
    ) -> daemon::Result<Vec<OwnedInput>> {
        let mut inputs = Vec::new();

        if !skip_coinbase_transactions {
            if let Some(coinbase) = &block.coinbase_tx {
                inputs.extend(Self::process_transaction_outputs(wallet, coinbase, block.block_height)?);
            }
        }

        for tx in &block.transactions {
            inputs.extend(Self::process_transaction_outputs(wallet, tx, block.block_height)?);
        }

        Ok(inputs)
    }

    /// [`Synchronizer::process_block_outputs`] for every block, on up to
    /// `threads` threads — the C++'s `m_threadCount` scanning threads
    /// (`WalletSynchronizer::blockProcessingThread`).
    ///
    /// The result is in block order and is exactly what one thread produces:
    /// each block's scan is a pure function of the wallet's keys and the block,
    /// and a thread takes the next unclaimed block from a shared counter, so a
    /// run of busy blocks spreads across the threads instead of landing on one.
    fn scan_blocks(
        wallet: &Wallet,
        skip_coinbase_transactions: bool,
        blocks: &[SyncBlock],
        threads: usize,
    ) -> Vec<daemon::Result<Vec<OwnedInput>>> {
        let threads = threads.clamp(1, blocks.len().max(1));
        if threads == 1 {
            return blocks.iter().map(|b| Self::process_block_outputs(wallet, skip_coinbase_transactions, b)).collect();
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut results: Vec<Option<daemon::Result<Vec<OwnedInput>>>> = (0..blocks.len()).map(|_| None).collect();
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut scanned = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(block) = blocks.get(i) else { break };
                            scanned.push((i, Self::process_block_outputs(wallet, skip_coinbase_transactions, block)));
                        }
                        scanned
                    })
                })
                .collect();
            for worker in workers {
                // A panic on a scanning thread is a bug here, not in the data;
                // carry it to the caller exactly as the sequential path would.
                for (i, result) in worker.join().unwrap_or_else(|p| std::panic::resume_unwind(p)) {
                    results[i] = Some(result);
                }
            }
        });
        results.into_iter().map(|r| r.expect("every block is claimed by exactly one thread")).collect()
    }

    /// `WalletSynchronizer::getGlobalIndexes` (line 705): the indexes for the
    /// 10-block window containing this height, so the daemon cannot tell which
    /// transaction is ours (`GLOBAL_INDEXES_OBSCURITY`). A failure is an empty
    /// map, as the C++ returns.
    fn global_indexes(&self, block_height: u64) -> HashMap<Hash, Vec<u64>> {
        let start = lower_bound(block_height, GLOBAL_INDEXES_OBSCURITY);
        let end = upper_bound(block_height, GLOBAL_INDEXES_OBSCURITY);

        match self.daemon.global_indexes_for_range(start, end) {
            Ok(indexes) => indexes
                .indexes
                .into_iter()
                .filter_map(|entry| Hex32::from_hex(&entry.key).map(|h| (h, entry.value)))
                .collect(),
            Err(_) => HashMap::new(),
        }
    }

    /// `WalletSynchronizer::blockProcessingThread` (line 202), the global index
    /// half: for a block with owned outputs, ask once for the window, and retry
    /// up to [`GLOBAL_INDEX_MAX_RETRIES`] times when the answer does not hold
    /// our transaction — a fork, or a faulty daemon. After that the index is
    /// left unset with a warning, and spending that input needs a rescan
    /// against a full node.
    fn fill_global_indexes(&mut self, block_height: u64, inputs: &mut [OwnedInput]) {
        if self.wallet.is_view_wallet() {
            return;
        }

        let mut indexes: Option<HashMap<Hash, Vec<u64>>> = None;

        for (_, input) in inputs.iter_mut() {
            if input.global_output_index.is_some() {
                continue;
            }

            if indexes.is_none() {
                indexes = Some(self.global_indexes(block_height));
            }

            let mut attempts = 1;

            loop {
                let found = indexes
                    .as_ref()
                    .and_then(|m| m.get(&input.parent_transaction_hash))
                    .filter(|v| v.len() > input.transaction_index as usize)
                    .map(|v| v[input.transaction_index as usize]);

                if let Some(index) = found {
                    input.global_output_index = Some(index);
                    break;
                }

                if attempts >= self.config.global_index_retries {
                    break;
                }

                attempts += 1;

                crate::platform::sleep(self.config.global_index_retry_delay);

                indexes = Some(self.global_indexes(block_height));
            }
        }
    }

    ////////////////////////
    /* APPLYING           */
    ////////////////////////

    /// `WalletSynchronizer::decryptPaymentID` (line 530): a 64-character id is
    /// plaintext and passes through; a 16-character one is the ciphertext of an
    /// encrypted short id, decrypted with the transaction public key and our
    /// view key — but only if we did **not** spend any input here. If we did,
    /// we were the sender, the id was encrypted to the receiver, and decrypting
    /// with our own key would hand back eight bytes of plausible noise. Report
    /// nothing instead; `addTransaction` puts back the plaintext we recorded at
    /// send time.
    fn decrypt_payment_id(&self, tx: &SyncTransaction, we_spent_inputs: bool) -> String {
        if tx.payment_id.len() != SHORT_PAYMENT_ID_LENGTH {
            return tx.payment_id.clone();
        }

        if we_spent_inputs {
            return String::new();
        }

        let Some(tx_public_key) = Hex32::from_hex(&tx.tx_public_key) else {
            return String::new();
        };

        encrypt_payment_id_hex(&tx.payment_id, tx_public_key.as_bytes(), self.wallet.private_view_key().as_bytes())
    }

    /// `WalletSynchronizer::processCoinbaseTransaction` (line 491): outputs
    /// only, fee zero, no payment id, `isCoinbaseTransaction` set.
    fn process_coinbase_transaction(
        &self,
        block: &SyncBlock,
        tx: &SyncTransaction,
        inputs: &[OwnedInput],
    ) -> daemon::Result<Option<Transaction>> {
        let Some(hash) = Hex32::from_hex(&tx.hash) else {
            return Err(DaemonError::Json(format!("transaction hash {:?} is malformed", tx.hash)));
        };

        let mut transfers: Vec<Transfer> = Vec::new();

        for (spend_key, input) in inputs.iter().filter(|(_, i)| i.parent_transaction_hash == hash) {
            add_transfer(&mut transfers, *spend_key, input.amount as i64);
        }

        if transfers.is_empty() {
            return Ok(None);
        }

        Ok(Some(Transaction {
            block_height: block.block_height,
            fee: 0,
            hash,
            is_coinbase_transaction: true,
            payment_id: String::new(),
            timestamp: block.block_timestamp,
            transfers,
            unlock_time: tx.unlock_time,
        }))
    }

    /// `WalletSynchronizer::processTransaction` (line 582): outputs we own as
    /// positive transfers, inputs whose key image we own as negative ones, and
    /// `fee = inputs − outputs` over the whole transaction, not just our part.
    ///
    /// A transaction is recorded whenever any spend key was touched, including
    /// when the transfers cancel to zero — a send of exactly the change back to
    /// ourselves still leaves an entry in the C++ `unordered_map`, and the
    /// emptiness check is on the map, not on the sum.
    fn process_transaction(
        &self,
        block: &SyncBlock,
        tx: &SyncTransaction,
        inputs: &[OwnedInput],
    ) -> daemon::Result<(Option<Transaction>, Vec<SpentKeyImage>)> {
        let Some(hash) = Hex32::from_hex(&tx.hash) else {
            return Err(DaemonError::Json(format!("transaction hash {:?} is malformed", tx.hash)));
        };

        let mut transfers: Vec<Transfer> = Vec::new();

        for (spend_key, input) in inputs.iter().filter(|(_, i)| i.parent_transaction_hash == hash) {
            add_transfer(&mut transfers, *spend_key, input.amount as i64);
        }

        let mut spent_key_images = Vec::new();

        for input in &tx.inputs {
            let Some(key_image) = Hex32::from_hex(&input.k_image) else {
                return Err(DaemonError::Json(format!("transaction {} has a malformed key image", tx.hash)));
            };

            if let Some(owner) = self.key_image_owners.get(key_image.as_bytes()) {
                add_transfer(&mut transfers, *owner, -(input.amount as i64));
                spent_key_images.push((*owner, key_image));
            }
        }

        if transfers.is_empty() {
            return Ok((None, Vec::new()));
        }

        let mut fee: u64 = 0;
        for input in &tx.inputs {
            fee = fee.wrapping_add(input.amount);
        }
        for output in &tx.outputs {
            fee = fee.wrapping_sub(output.amount);
        }

        let payment_id = self.decrypt_payment_id(tx, !spent_key_images.is_empty());

        Ok((
            Some(Transaction {
                block_height: block.block_height,
                fee,
                hash,
                is_coinbase_transaction: false,
                payment_id,
                timestamp: block.block_timestamp,
                transfers,
                unlock_time: tx.unlock_time,
            }),
            spent_key_images,
        ))
    }

    /// `WalletSynchronizer::processBlockTransactions` (line 457).
    fn process_block_transactions(
        &self,
        block: &SyncBlock,
        inputs: &[OwnedInput],
    ) -> daemon::Result<(Vec<Transaction>, Vec<SpentKeyImage>)> {
        let mut transactions = Vec::new();
        let mut key_images_to_mark_spent = Vec::new();

        if !self.config.skip_coinbase_transactions {
            if let Some(coinbase) = &block.coinbase_tx {
                if let Some(tx) = self.process_coinbase_transaction(block, coinbase, inputs)? {
                    transactions.push(tx);
                }
            }
        }

        for raw in &block.transactions {
            let (tx, spent) = self.process_transaction(block, raw, inputs)?;

            if let Some(tx) = tx {
                transactions.push(tx);
                key_images_to_mark_spent.extend(spent);
            }
        }

        Ok((transactions, key_images_to_mark_spent))
    }

    /// `WalletSynchronizer::completeBlockProcessing` (line 368): fork unwind,
    /// spent-input pruning, the transactions and inputs of the block, and
    /// finally the block hash into the synchronization status.
    ///
    /// The order matters and is the C++ order. In particular the inputs of this
    /// block are stored *after* its transactions are scanned, so an output
    /// received and spent inside the same block is not seen as spent there —
    /// the key image is not in the owner map yet.
    fn complete_block_processing(&mut self, block: &SyncBlock, our_inputs: &[OwnedInput]) -> daemon::Result<usize> {
        let Some(block_hash) = Hex32::from_hex(&block.block_hash) else {
            return Err(DaemonError::Json(format!("block hash {:?} is malformed", block.block_hash)));
        };

        let wallet_height = self.wallet.wallet_height();

        // The chain forked: this block is at or below a height we have already
        // recorded, which can only happen because the daemon resumed from a
        // checkpoint of ours that is no longer on its main chain.
        if wallet_height >= block.block_height && block.block_height != 0 {
            for key_image in self.wallet.remove_forked_transactions(block.block_height) {
                self.key_image_owners.remove(key_image.as_bytes());
            }
            self.forks_resolved += 1;
        }

        // Prune spent inputs that are out of the confirmation window.
        if block.block_height.is_multiple_of(PRUNE_SPENT_INPUTS_INTERVAL)
            && block.block_height > PRUNE_SPENT_INPUTS_INTERVAL
        {
            self.wallet.prune_spent_inputs(block.block_height - PRUNE_SPENT_INPUTS_INTERVAL);
        }

        let (transactions, key_images_to_mark_spent) = self.process_block_transactions(block, our_inputs)?;

        let mut added = 0;

        for tx in transactions {
            let hash = tx.hash;
            if self.wallet.add_transaction(tx) {
                added += 1;
                self.step_added.push(hash);
            }
        }

        let is_view_wallet = self.wallet.is_view_wallet();

        for (spend_key, input) in our_inputs {
            if !is_view_wallet && self.wallet.sub_wallet(spend_key).is_some() {
                self.key_image_owners.insert(*input.key_image.as_bytes(), *spend_key);
            }
            self.wallet.store_transaction_input(spend_key, input.clone());
        }

        for (spend_key, key_image) in key_images_to_mark_spent {
            self.wallet.mark_input_as_spent(&key_image, &spend_key, block.block_height);
        }

        // Last, once the transactions are fully processed: a save between the
        // two would lose the block.
        self.wallet
            .wallet_synchronizer
            .transaction_synchronizer_status
            .store_block_hash(block_hash, block.block_height);

        Ok(added)
    }

    /// Apply one downloaded block whose outputs [`Synchronizer::scan_blocks`]
    /// has already scanned: global indexes, then the block itself
    /// (`WalletSynchronizer::syncStep`'s body, line 878).
    fn apply_scanned_block(&mut self, block: &SyncBlock, mut our_inputs: Vec<OwnedInput>) -> daemon::Result<usize> {
        if !our_inputs.is_empty() {
            self.fill_global_indexes(block.block_height, &mut our_inputs);
        }

        self.complete_block_processing(block, &our_inputs)
    }

    /// Apply up to [`SyncConfig::block_processing_chunk`] stored blocks.
    /// Returns `(blocks, transactions added)`.
    fn process_stored_blocks(&mut self) -> daemon::Result<(usize, usize)> {
        let take = self.config.block_processing_chunk.min(self.stored_blocks.len());

        if take == 0 {
            return Ok((0, 0));
        }

        let blocks: Vec<SyncBlock> = self.stored_blocks.drain(..take).collect();

        // The expensive half — reading nothing a sync changes — runs for the
        // whole chunk across threads first. The other half applies the blocks
        // strictly in order, as before: the fork unwind, the key-image owners
        // and the spent marks all depend on the block before. An error stops
        // the chunk at the same block it did when both halves ran per block.
        let scanned =
            Self::scan_blocks(&self.wallet, self.config.skip_coinbase_transactions, &blocks, self.config.scan_threads);

        let mut transactions = 0;

        for (block, our_inputs) in blocks.iter().zip(scanned) {
            transactions += self.apply_scanned_block(block, our_inputs?)?;
        }

        Ok((blocks.len(), transactions))
    }

    /// One round: download a batch, then scan and apply what is stored.
    ///
    /// Never sleeps. [`SyncStep::Idle`] and [`SyncStep::Failed`] carry the
    /// backoff `BlockDownloader::downloader` would have slept for.
    pub fn sync_step(&mut self) -> SyncStep {
        self.step_added.clear();

        let download = match self.download_height_windows() {
            Some(outcome) => outcome,
            None => self.download_step(),
        };

        let processed = match self.process_stored_blocks() {
            Ok(processed) => processed,
            Err(e) => return SyncStep::Failed { error: e, backoff: self.config.failure_backoff },
        };

        if processed.0 > 0 {
            return SyncStep::Processed {
                blocks: processed.0,
                transactions: processed.1,
                height: self.wallet.wallet_height(),
            };
        }

        match download {
            Download::Stored => SyncStep::Idle { backoff: Duration::ZERO },
            Download::Synced(height) => SyncStep::Synced { height },
            Download::Idle => SyncStep::Idle { backoff: self.config.failure_backoff },
            Download::Failed(error) => {
                let backoff = if self.last_request_rate_limited {
                    self.config.rate_limited_backoff
                } else {
                    self.config.failure_backoff
                };
                SyncStep::Failed { error, backoff }
            }
            Download::Gap => {
                let (covered_to, daemon_serves_from) = self.sync_gap.unwrap_or((0, 0));
                SyncStep::Gap { covered_to, daemon_serves_from }
            }
        }
    }

    /// Drive [`Synchronizer::sync_step`] until the wallet is synced, a gap
    /// stops it, or `max_steps` rounds have run, sleeping whatever backoff a
    /// step reports. `/info` is refreshed every ten seconds, the cadence
    /// `Nigel`'s background thread keeps (`WalletSynchronizer.cpp:886`).
    ///
    /// Returns the last step taken. Not in a browser, which has no
    /// `Instant` and must not block: its host calls `sync_step` from a timer.
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    pub fn sync_until_synced(&mut self, max_steps: usize) -> SyncStep {
        let mut last = SyncStep::Idle { backoff: Duration::ZERO };
        let mut last_info = std::time::Instant::now() - Duration::from_secs(60);

        for _ in 0..max_steps {
            if last_info.elapsed() >= Duration::from_secs(10) {
                let _ = self.refresh_info();
                last_info = std::time::Instant::now();
            }

            last = self.sync_step();

            match &last {
                SyncStep::Gap { .. } => return last,
                SyncStep::Synced { .. } => return last,
                SyncStep::Idle { backoff } | SyncStep::Failed { backoff, .. } => {
                    if !backoff.is_zero() {
                        std::thread::sleep(*backoff);
                    }
                }
                SyncStep::Processed { .. } => {}
            }
        }

        last
    }

    /// `WalletSynchronizer::checkLockedTransactions` (line 721): ask the daemon
    /// about the sends we are still waiting on, and cancel the ones it has
    /// never heard of — the transaction is dropped and its inputs return to
    /// `unspentInputs`. A view wallet has no locked transactions and skips it.
    ///
    /// Returns how many transactions were cancelled.
    pub fn check_locked_transactions(&mut self) -> daemon::Result<usize> {
        if self.wallet.is_view_wallet() {
            return Ok(0);
        }

        let hashes: Vec<String> = self.wallet.sub_wallets.locked_transactions.iter().map(|t| t.hash.to_hex()).collect();

        if hashes.is_empty() {
            return Ok(0);
        }

        let status = self.daemon.transactions_status(&hashes)?;

        let cancelled: Vec<Hash> = status.transactions_unknown.iter().filter_map(|h| Hex32::from_hex(h)).collect();

        if cancelled.is_empty() {
            return Ok(0);
        }

        self.wallet.remove_cancelled_transactions(&cancelled);

        Ok(cancelled.len())
    }
}

/// `transfers[publicSpendKey] += amount` over the C++ `unordered_map`, kept as
/// a vector in first-touch order so the JSON is deterministic. An entry that
/// nets to zero stays, exactly as a map entry would.
fn add_transfer(transfers: &mut Vec<Transfer>, public_key: PublicKey, amount: i64) {
    match transfers.iter_mut().find(|t| t.public_key == public_key) {
        Some(existing) => existing.amount = existing.amount.wrapping_add(amount),
        None => transfers.push(Transfer { amount, public_key }),
    }
}
