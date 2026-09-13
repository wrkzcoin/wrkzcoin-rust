// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Wallet synchronization tests (spec/12 stage 2 step 3, spec/10 "Sync algorithm",
//! spec/09 "Wallet sync endpoints").
//!
//! Everything here is offline and deterministic. The chain is synthetic but the
//! cryptography is not: outputs are derived with the real curve functions from
//! `wrkz-pow`, and where `spec/03-crypto-primitives.md` publishes a vector for
//! a value (the transaction key, the derivation, the one-time keys, the key
//! images, the encrypted payment id) the test asserts against the published
//! vector rather than against what this code computes.
//!
//! The one `#[ignore]` test at the bottom syncs against the live seed node.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;

use wrkz_pow::curve;
use wrkz_wallet::daemon::{
    DaemonError, GlobalIndexEntry, GlobalIndexes, Info, SyncBlock, SyncInput, SyncOutput, SyncRequest, SyncTransaction,
    TopBlock, TransactionsStatus, WalletSyncData,
};
use wrkz_wallet::file::{Hex32, SecretKey, Transaction, Wallet};
use wrkz_wallet::sync::{encrypt_payment_id_hex, is_input_unlocked_at, SyncConfig, SyncDaemon, SyncStep, Synchronizer};

////////////////////
/* SPEC VECTORS   */
////////////////////

const SPEC_MNEMONIC: &str = "eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell different \
                             dating intended richly slower inundate ridges slug inundate ridges slug were rotate \
                             rudely viewpoint";
const SPEC_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
const SPEC_VIEW_SECRET: &str = "779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700";
const SPEC_SPEND_PUBLIC: &str = "857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3";

/// spec/03 "Key derivation and one-time keys", vector with tx key seed 32 × 0x22.
const TX_SECRET: &str = "487a3668ed5bfd7175e832dc642e64f821222222222222222222222222222202";
const TX_PUBLIC: &str = "512e1a2060d978a11a9ce65bbb6b98dcf3300b762c520b13fe2e658b583593bf";
const DERIVATION: &str = "5d450058695ab466965468876364cb788bc4656123bc40479246d23d24b2c9dd";
/// The one-time output key for index 0 of that transaction, sent to the spec
/// wallet, and its one-time secret and key image.
const OUT0_KEY: &str = "22c90af32cfded17237a447122de92e736b59313f635bf43f69bacfad3756723";
const OUT0_EPHEMERAL: &str = "f0b2033e6f607de659fa6545ca0498de1f3f55021caa129d803f4d787a84c50d";
const OUT0_KEY_IMAGE: &str = "4b8483b5c810b23cb58ec80547f3fa43fd587e5c5f53f29e61ee5e949df7e7f4";
/// spec/03 "Encrypted short payment ids": the same derivation encrypts
/// `0102030405060708` to this.
const SHORT_PAYMENT_ID_PLAIN: &str = "0102030405060708";
const SHORT_PAYMENT_ID_CIPHER: &str = "5309f07bf6999800";

/// The block of `spec/vectors/mainnet_getwalletsyncdata_4213000.json`, whose
/// coinbase hash is the key of `mainnet_get_global_indexes_for_range.json`.
const VECTOR_HEIGHT: u64 = 4_213_000;
const VECTOR_COINBASE_HASH: &str = "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f";
const VECTOR_GLOBAL_INDEX: u64 = 3_808_773;

////////////////////
/* MOCK DAEMON    */
////////////////////

/// A [`SyncDaemon`] that answers from a script, and records what it was asked.
#[derive(Default)]
struct MockDaemon {
    sync_data: RefCell<VecDeque<Result<WalletSyncData, DaemonError>>>,
    sync_requests: RefCell<Vec<SyncRequest>>,
    global_indexes: RefCell<VecDeque<Result<GlobalIndexes, DaemonError>>>,
    global_index_requests: RefCell<Vec<(u64, u64)>>,
    transactions_status: RefCell<Option<TransactionsStatus>>,
    info: RefCell<Option<Info>>,
}

impl MockDaemon {
    fn new() -> MockDaemon {
        MockDaemon::default()
    }

    /// Queue one `/getwalletsyncdata` answer.
    fn push(&self, data: WalletSyncData) -> &Self {
        self.sync_data.borrow_mut().push_back(Ok(data));
        self
    }

    /// Queue one `/getwalletsyncdata` failure.
    fn push_error(&self, error: DaemonError) -> &Self {
        self.sync_data.borrow_mut().push_back(Err(error));
        self
    }

    /// Queue one `/get_global_indexes_for_range` answer.
    fn push_indexes(&self, indexes: GlobalIndexes) -> &Self {
        self.global_indexes.borrow_mut().push_back(Ok(indexes));
        self
    }

    fn requests(&self) -> Vec<SyncRequest> {
        self.sync_requests.borrow().clone()
    }

    fn block_counts(&self) -> Vec<u64> {
        self.sync_requests.borrow().iter().map(|r| r.block_count).collect()
    }

    fn index_requests(&self) -> Vec<(u64, u64)> {
        self.global_index_requests.borrow().clone()
    }
}

impl SyncDaemon for MockDaemon {
    fn wallet_sync_data(&self, req: &SyncRequest) -> Result<WalletSyncData, DaemonError> {
        self.sync_requests.borrow_mut().push(req.clone());

        match self.sync_data.borrow_mut().pop_front() {
            Some(response) => response,
            // Out of script: the daemon has nothing more for us, which is what
            // a synced daemon answers.
            None => Ok(empty_response()),
        }
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> Result<GlobalIndexes, DaemonError> {
        self.global_index_requests.borrow_mut().push((start, end));

        match self.global_indexes.borrow_mut().pop_front() {
            Some(response) => response,
            None => Ok(GlobalIndexes { indexes: Vec::new(), status: "OK".into() }),
        }
    }

    fn transactions_status(&self, _hashes: &[String]) -> Result<TransactionsStatus, DaemonError> {
        match self.transactions_status.borrow().clone() {
            Some(status) => Ok(status),
            None => Ok(TransactionsStatus {
                transactions_in_pool: Vec::new(),
                transactions_in_block: Vec::new(),
                transactions_unknown: Vec::new(),
                status: "OK".into(),
            }),
        }
    }

    fn info(&self) -> Result<Info, DaemonError> {
        match self.info.borrow().clone() {
            Some(info) => Ok(info),
            // A full node far above anything these tests reach.
            None => Ok(info(10_000_000, &[], 0)),
        }
    }
}

/// An `/info` body with the fields sync reads.
fn info(height: u64, features: &[&str], lite_start_height: u64) -> Info {
    let features: Vec<String> = features.iter().map(|f| format!("\"{f}\"")).collect();

    serde_json::from_str(&format!(
        r#"{{"height":{height},"network_height":{height},"difficulty":1,"incoming_connections_count":1,
            "outgoing_connections_count":1,"lite_start_height":{lite_start_height},
            "sync_features":[{}],"synced":true,"status":"OK"}}"#,
        features.join(",")
    ))
    .unwrap()
}

fn empty_response() -> WalletSyncData {
    WalletSyncData { items: Vec::new(), scanned_to_height: None, synced: false, top_block: None, status: "OK".into() }
}

fn response(items: Vec<SyncBlock>) -> WalletSyncData {
    let scanned_to_height = items.last().map(|b| b.block_height);
    WalletSyncData { items, scanned_to_height, synced: false, top_block: None, status: "OK".into() }
}

fn synced_response(top: TopBlock) -> WalletSyncData {
    WalletSyncData {
        items: Vec::new(),
        scanned_to_height: Some(top.height),
        synced: true,
        top_block: Some(top),
        status: "OK".into(),
    }
}

////////////////////
/* CHAIN BUILDER  */
////////////////////

fn hex32(s: &str) -> [u8; 32] {
    Hex32::from_hex(s).expect("32 byte hex").0
}

/// A deterministic hash to name a block or a transaction with.
fn label_hash(label: &str) -> String {
    Hex32(wrkz_pow::cn_fast_hash(label.as_bytes())).to_hex()
}

/// Someone who is not us: a deterministic key pair to send decoy outputs to.
struct Stranger {
    spend_public: [u8; 32],
    view_public: [u8; 32],
}

fn stranger(label: &str) -> Stranger {
    let (spend_secret, spend_public) = curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(label.as_bytes()));
    let (_, view_public) = curve::generate_view_from_spend(&spend_secret);
    Stranger { spend_public, view_public }
}

/// A transaction under construction, built the way a sender builds one: one
/// transaction key `(r, R)`, and `P_i = Hs(8·r·A_i ‖ i)·G + B_i` per output.
struct TxBuilder {
    hash: String,
    tx_secret: [u8; 32],
    tx_public: [u8; 32],
    outputs: Vec<SyncOutput>,
    inputs: Vec<SyncInput>,
    unlock_time: u64,
    payment_id: String,
}

impl TxBuilder {
    /// A transaction with a transaction key derived from `label`.
    fn new(label: &str) -> TxBuilder {
        let (tx_secret, tx_public) = curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(label.as_bytes()));
        TxBuilder {
            hash: label_hash(label),
            tx_secret,
            tx_public,
            outputs: Vec::new(),
            inputs: Vec::new(),
            unlock_time: 0,
            payment_id: String::new(),
        }
    }

    /// A transaction whose key is the spec/03 vector key, so that the outputs
    /// it sends to the spec wallet are the published ones.
    fn spec_vector(label: &str) -> TxBuilder {
        TxBuilder {
            hash: label_hash(label),
            tx_secret: hex32(TX_SECRET),
            tx_public: hex32(TX_PUBLIC),
            outputs: Vec::new(),
            inputs: Vec::new(),
            unlock_time: 0,
            payment_id: String::new(),
        }
    }

    fn hash(mut self, hash: &str) -> Self {
        self.hash = hash.to_string();
        self
    }

    fn unlock_time(mut self, unlock_time: u64) -> Self {
        self.unlock_time = unlock_time;
        self
    }

    fn payment_id(mut self, payment_id: &str) -> Self {
        self.payment_id = payment_id.to_string();
        self
    }

    /// An output to a spend/view key pair, at the next output index.
    fn output_to(mut self, spend_public: &[u8; 32], view_public: &[u8; 32], amount: u64) -> Self {
        let index = self.outputs.len() as u64;
        let derivation = curve::generate_key_derivation(view_public, &self.tx_secret).expect("view key is a point");
        let key = curve::derive_public_key(&derivation, index, spend_public).expect("spend key is a point");

        self.outputs.push(SyncOutput { amount, key: Hex32(key).to_hex(), global_index: None });
        self
    }

    /// An output to the wallet under test.
    fn output_to_wallet(self, wallet: &Wallet, amount: u64) -> Self {
        let (spend_public, view_public) = wallet_keys(wallet);
        self.output_to(&spend_public, &view_public, amount)
    }

    /// An output to someone else.
    fn output_to_stranger(self, who: &str, amount: u64) -> Self {
        let s = stranger(who);
        self.output_to(&s.spend_public, &s.view_public, amount)
    }

    /// A key input, named by the key image it spends.
    fn input(mut self, key_image: &str, amount: u64) -> Self {
        self.inputs.push(SyncInput { amount, k_image: key_image.to_string(), key_offsets: Vec::new() });
        self
    }

    /// An input spending some output that is not ours.
    fn stranger_input(self, label: &str, amount: u64) -> Self {
        let image = label_hash(label);
        self.input(&image, amount)
    }

    fn build(&self) -> SyncTransaction {
        SyncTransaction {
            hash: self.hash.clone(),
            outputs: self.outputs.clone(),
            tx_public_key: Hex32(self.tx_public).to_hex(),
            unlock_time: self.unlock_time,
            payment_id: self.payment_id.clone(),
            inputs: self.inputs.clone(),
        }
    }
}

/// The wallet's public spend key and public view key.
fn wallet_keys(wallet: &Wallet) -> ([u8; 32], [u8; 32]) {
    let spend_public = wallet.primary_sub_wallet().unwrap().public_spend_key.0;
    let view_public = curve::secret_key_to_public_key(wallet.private_view_key().as_bytes()).unwrap();
    (spend_public, view_public)
}

fn block(label: &str, height: u64, timestamp: u64) -> SyncBlock {
    SyncBlock {
        block_hash: label_hash(label),
        block_height: height,
        block_timestamp: timestamp,
        coinbase_tx: None,
        transactions: Vec::new(),
    }
}

fn with_coinbase(mut b: SyncBlock, tx: SyncTransaction) -> SyncBlock {
    b.coinbase_tx = Some(tx);
    b
}

fn with_tx(mut b: SyncBlock, tx: SyncTransaction) -> SyncBlock {
    b.transactions.push(tx);
    b
}

/// The spec/05 wallet, restored from its seed at `scan_height`.
fn spec_wallet(scan_height: u64) -> Wallet {
    let wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, scan_height).unwrap();
    assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));
    assert_eq!(wallet.private_view_key().to_hex().as_str(), SPEC_VIEW_SECRET);
    assert_eq!(wallet.primary_sub_wallet().unwrap().public_spend_key.to_hex(), SPEC_SPEND_PUBLIC);
    wallet
}

/// A view-only wallet for the same address.
fn spec_view_wallet(scan_height: u64) -> Wallet {
    Wallet::import_view_only(&SecretKey::from_hex(SPEC_VIEW_SECRET).unwrap(), SPEC_ADDRESS, scan_height).unwrap()
}

fn synchronizer(daemon: MockDaemon, wallet: Wallet) -> Synchronizer<MockDaemon> {
    let config = SyncConfig {
        // Never sleep in a test.
        global_index_retry_delay: Duration::ZERO,
        ..SyncConfig::default()
    };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);
    // `Nigel::init` reads /info before sync starts, and the download path
    // refuses to run while the daemon looks shorter than the wallet.
    sync.refresh_info().unwrap();
    sync
}

fn assert_processed(step: SyncStep, blocks: usize) -> u64 {
    match step {
        SyncStep::Processed { blocks: got, height, .. } => {
            assert_eq!(got, blocks, "blocks applied");
            height
        }
        other => panic!("expected {blocks} blocks processed, got {other:?}"),
    }
}

fn transfers_of(tx: &Transaction) -> Vec<(String, i64)> {
    tx.transfers.iter().map(|t| (t.public_key.to_hex(), t.amount)).collect()
}

////////////////////
/* TESTS          */
////////////////////

/// Scanning finds the output the spec/03 vector sends to the spec wallet, with
/// the published one-time key, ephemeral and key image, and fills the global
/// index from the 10-block window around the block
/// (`WalletSynchronizer::processTransactionOutputs`, `getGlobalIndexes`).
#[test]
fn finds_our_outputs_with_the_spec_vector_key_images() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    // Two outputs: index 0 to us (the vector output), index 1 to a stranger.
    let tx = TxBuilder::spec_vector("tx-a")
        .hash(VECTOR_COINBASE_HASH)
        .output_to_wallet(&wallet, 5000)
        .output_to_stranger("bob", 4000)
        .stranger_input("someone-elses-input", 10_000)
        .build();

    let coinbase =
        TxBuilder::new("cb-a").output_to_stranger("miner", 1_000_000).unlock_time(VECTOR_HEIGHT + 40).build();

    let b = with_tx(with_coinbase(block("a", VECTOR_HEIGHT, 1_788_894_799), coinbase), tx);

    daemon.push(response(vec![b]));
    // The published /get_global_indexes_for_range sample, whose key is the hash
    // we gave the transaction above.
    daemon.push_indexes(
        serde_json::from_str(
            &std::fs::read_to_string(vectors().join("mainnet_get_global_indexes_for_range.json")).unwrap(),
        )
        .unwrap(),
    );

    let mut sync = synchronizer(daemon, wallet);

    assert_eq!(assert_processed(sync.sync_step(), 1), VECTOR_HEIGHT);

    // The output key is the vector's index 0 key, and so are the ephemeral and
    // the key image.
    let sub = sync.wallet().primary_sub_wallet().unwrap();
    assert_eq!(sub.unspent_inputs.len(), 1);

    let input = &sub.unspent_inputs[0];
    assert_eq!(input.key.to_hex(), OUT0_KEY);
    assert_eq!(input.key_image.to_hex(), OUT0_KEY_IMAGE);
    assert_eq!(input.private_ephemeral.as_ref().unwrap().to_hex().as_str(), OUT0_EPHEMERAL);
    assert_eq!(input.amount, 5000);
    assert_eq!(input.block_height, VECTOR_HEIGHT);
    assert_eq!(input.transaction_index, 0);
    assert_eq!(input.spend_height, 0);
    assert_eq!(input.transaction_public_key.to_hex(), TX_PUBLIC);
    assert_eq!(input.parent_transaction_hash.to_hex(), VECTOR_COINBASE_HASH);

    // Filled from the sample, asked for over the obscurity window.
    assert_eq!(input.global_output_index, Some(VECTOR_GLOBAL_INDEX));
    assert_eq!(sync.daemon().index_requests(), vec![(VECTOR_HEIGHT, VECTOR_HEIGHT + 10)]);

    // One transaction, ours, +5000, fee = 10000 - 9000.
    let txs = sync.wallet().transactions();
    assert_eq!(txs.len(), 1);
    assert_eq!(txs[0].hash.to_hex(), VECTOR_COINBASE_HASH);
    assert_eq!(txs[0].fee, 1000);
    assert!(!txs[0].is_coinbase_transaction);
    assert_eq!(txs[0].timestamp, 1_788_894_799);
    assert_eq!(txs[0].block_height, VECTOR_HEIGHT);
    assert_eq!(transfers_of(&txs[0]), vec![(SPEC_SPEND_PUBLIC.to_string(), 5000)]);
    assert_eq!(txs[0].total_amount(), 5000);

    // The miner's coinbase was not ours, so it is not recorded at all.
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT), (5000, 0));
    assert_eq!(sync.wallet().wallet_height(), VECTOR_HEIGHT);
}

/// A later block spending that output marks it spent and records the outgoing
/// transaction with its fee (`processTransaction`, `SubWallet::markInputAsSpent`).
#[test]
fn a_spend_of_our_key_image_is_detected() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    let receive = TxBuilder::spec_vector("receive").output_to_wallet(&wallet, 5000).build();

    let spend = TxBuilder::new("spend")
        .input(OUT0_KEY_IMAGE, 5000)
        .output_to_stranger("charlie", 3000)
        .output_to_stranger("charlie", 1000)
        .build();

    daemon.push(response(vec![with_tx(block("r", VECTOR_HEIGHT, 100), receive)]));
    daemon.push(response(vec![with_tx(block("s", VECTOR_HEIGHT + 1, 160), spend)]));

    let mut sync = synchronizer(daemon, wallet);

    assert_processed(sync.sync_step(), 1);
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT), (5000, 0));

    assert_processed(sync.sync_step(), 1);

    let sub = sync.wallet().primary_sub_wallet().unwrap();
    assert!(sub.unspent_inputs.is_empty());
    assert_eq!(sub.spent_inputs.len(), 1);
    assert_eq!(sub.spent_inputs[0].key_image.to_hex(), OUT0_KEY_IMAGE);
    assert_eq!(sub.spent_inputs[0].spend_height, VECTOR_HEIGHT + 1);

    let txs = sync.wallet().transactions();
    assert_eq!(txs.len(), 2);
    assert_eq!(transfers_of(&txs[1]), vec![(SPEC_SPEND_PUBLIC.to_string(), -5000)]);
    assert_eq!(txs[1].total_amount(), -5000);
    // 5000 in, 4000 out.
    assert_eq!(txs[1].fee, 1000);
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT + 1), (0, 0));
}

/// `SubWallet::getBalance` with `Utilities::isInputUnlocked`: a coinbase is
/// locked until its unlock height, and `unlockTime` above
/// `CRYPTONOTE_MAX_BLOCK_NUMBER` is a timestamp.
#[test]
fn balances_follow_the_unlock_rules() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    // A coinbase to us, unlocking 40 blocks later, as constructMinerTx sets it.
    let coinbase =
        TxBuilder::spec_vector("cb").output_to_wallet(&wallet, 1_000_000).unlock_time(VECTOR_HEIGHT + 40).build();

    daemon.push(response(vec![with_coinbase(block("c", VECTOR_HEIGHT, 100), coinbase)]));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);

    // Recorded as a coinbase, fee zero, no payment id.
    let txs = sync.wallet().transactions();
    assert_eq!(txs.len(), 1);
    assert!(txs[0].is_coinbase_transaction);
    assert_eq!(txs[0].fee, 0);
    assert_eq!(txs[0].payment_id, "");
    assert_eq!(txs[0].unlock_time, VECTOR_HEIGHT + 40);

    // height + 1 >= unlockTime is the rule, so one block early it is locked.
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT + 38), (0, 1_000_000));
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT + 39), (1_000_000, 0));
    assert_eq!(sync.wallet().balance_for_address(SPEC_ADDRESS, VECTOR_HEIGHT + 39), Some((1_000_000, 0)));
    assert_eq!(sync.wallet().balance_for_address("Wrkznope", VECTOR_HEIGHT), None);

    let balances = sync.wallet().address_balances(VECTOR_HEIGHT + 39);
    assert_eq!(balances.len(), 1);
    assert_eq!(balances[0].address, SPEC_ADDRESS);
    assert_eq!(balances[0].unlocked, 1_000_000);

    // The timestamp branch: >= 500,000,000 is a unix time, unlocked once
    // now + 60 reaches it.
    assert!(is_input_unlocked_at(0, 0, 0));
    assert!(!is_input_unlocked_at(500_000_000, 4_000_000, 499_999_939));
    assert!(is_input_unlocked_at(500_000_000, 4_000_000, 499_999_940));
    // Below the threshold it is a height, whatever the clock says.
    assert!(!is_input_unlocked_at(499_999_999, 499_999_997, u64::MAX / 2));
    assert!(is_input_unlocked_at(499_999_999, 499_999_998, 0));
}

/// A short payment id decrypts for an incoming transaction (spec/03's published
/// ciphertext) and is reported empty on one we sent, where our view key is not
/// the one it was encrypted to (`WalletSynchronizer::decryptPaymentID`).
#[test]
fn payment_ids_follow_the_send_rule() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    // The vector: this ciphertext under the vector derivation is 0102030405060708.
    assert_eq!(
        encrypt_payment_id_hex(SHORT_PAYMENT_ID_PLAIN, &hex32(TX_PUBLIC), &hex32(SPEC_VIEW_SECRET)),
        SHORT_PAYMENT_ID_CIPHER
    );
    assert_eq!(curve::generate_key_derivation(&hex32(TX_PUBLIC), &hex32(SPEC_VIEW_SECRET)), Some(hex32(DERIVATION)));

    let incoming =
        TxBuilder::spec_vector("in").payment_id(SHORT_PAYMENT_ID_CIPHER).output_to_wallet(&wallet, 5000).build();

    // A long id is plaintext and passes straight through.
    let long_id = "ab".repeat(32);
    let long = TxBuilder::new("long").payment_id(&long_id).output_to_wallet(&wallet, 700).build();

    // A transaction we sent: it spends our key image, so the id must not be
    // decrypted with our own view key.
    let outgoing = TxBuilder::new("out")
        .payment_id(SHORT_PAYMENT_ID_CIPHER)
        .input(OUT0_KEY_IMAGE, 5000)
        .output_to_stranger("dave", 4000)
        .build();

    daemon.push(response(vec![with_tx(with_tx(block("p", VECTOR_HEIGHT, 100), incoming), long)]));
    daemon.push(response(vec![with_tx(block("q", VECTOR_HEIGHT + 1, 160), outgoing)]));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);
    assert_processed(sync.sync_step(), 1);

    let txs = sync.wallet().transactions();
    assert_eq!(txs.len(), 3);
    assert_eq!(txs[0].payment_id, SHORT_PAYMENT_ID_PLAIN);
    assert_eq!(txs[1].payment_id, long_id);
    assert_eq!(txs[2].payment_id, "");
}

/// A daemon answering from below our height is a fork: everything at or above
/// it is unwound and the new chain applied (`SubWallets::removeForkedTransactions`).
#[test]
fn a_fork_unwinds_and_reapplies() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    // Chain A: 4213000 pays us 5000; 4213001 spends it and pays us 900.
    let receive = TxBuilder::spec_vector("receive").output_to_wallet(&wallet, 5000).build();

    let spend_and_change = TxBuilder::new("spend-a")
        .input(OUT0_KEY_IMAGE, 5000)
        .output_to_wallet(&wallet, 900)
        .output_to_stranger("eve", 3000)
        .build();

    // Chain B: a different 4213001 that does not spend anything, then 4213002.
    let unrelated = TxBuilder::new("unrelated-b").output_to_stranger("frank", 10).build();
    let later = TxBuilder::new("later-b").output_to_wallet(&wallet, 77).build();

    daemon.push(response(vec![
        with_tx(block("a0", VECTOR_HEIGHT, 100), receive),
        with_tx(block("a1", VECTOR_HEIGHT + 1, 160), spend_and_change),
    ]));
    daemon.push(response(vec![
        with_tx(block("b1", VECTOR_HEIGHT + 1, 161), unrelated),
        with_tx(block("b2", VECTOR_HEIGHT + 2, 221), later),
    ]));

    let mut sync = synchronizer(daemon, wallet);

    assert_processed(sync.sync_step(), 2);
    assert_eq!(sync.wallet().transactions().len(), 2);
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT + 1), (900, 0));

    let hashes_before: Vec<String> = sync.wallet().transactions().iter().map(|t| t.hash.to_hex()).collect();

    // The daemon now hands us a different 4213001. Its parent is not our last
    // known block, and its height is one we have already recorded.
    assert_processed(sync.sync_step(), 2);
    assert_eq!(sync.forks_resolved(), 1);

    // The spend is gone, the 900 change it created is gone, and the 5000 it
    // spent is unspent again.
    let txs = sync.wallet().transactions();
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[0].hash.to_hex(), hashes_before[0]);
    assert_eq!(txs[1].hash.to_hex(), label_hash("later-b"));

    let sub = sync.wallet().primary_sub_wallet().unwrap();
    assert!(sub.spent_inputs.is_empty());
    assert_eq!(sub.unspent_inputs.len(), 2, "the 5000 is back, plus the 77 from the new chain");
    assert_eq!(sub.unspent_inputs[0].key_image.to_hex(), OUT0_KEY_IMAGE);
    assert_eq!(sub.unspent_inputs[0].spend_height, 0);
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT + 2), (5077, 0));
    assert_eq!(sync.wallet().wallet_height(), VECTOR_HEIGHT + 2);

    // And the block hash list now names the new chain, newest first.
    let status = &sync.wallet().wallet_synchronizer.transaction_synchronizer_status;
    assert_eq!(status.last_known_block_hashes[0].to_hex(), label_hash("b2"));
    assert_eq!(status.last_known_block_hashes[1].to_hex(), label_hash("b1"));
}

/// Three answers that do not hold our transaction leave the index unset, and
/// cost exactly three calls (`GLOBAL_INDEX_MAX_RETRIES`).
#[test]
fn global_index_gives_up_after_three_tries() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    let tx = TxBuilder::spec_vector("gi").output_to_wallet(&wallet, 5000).build();
    daemon.push(response(vec![with_tx(block("gi", VECTOR_HEIGHT, 100), tx)]));

    // Every answer is for other transactions only.
    for _ in 0..3 {
        daemon.push_indexes(GlobalIndexes {
            indexes: vec![GlobalIndexEntry { key: label_hash("someone else"), value: vec![1, 2, 3] }],
            status: "OK".into(),
        });
    }

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);

    assert_eq!(sync.wallet().primary_sub_wallet().unwrap().unspent_inputs[0].global_output_index, None);
    // Rounded down and up to the obscurity window, three times.
    assert_eq!(sync.daemon().index_requests(), vec![(VECTOR_HEIGHT, VECTOR_HEIGHT + 10); 3]);
}

/// A view wallet stores the input for its balance but has no key image, so it
/// cannot see the output being spent, and it never asks for global indexes.
#[test]
fn a_view_wallet_sees_incoming_only() {
    let wallet = spec_view_wallet(VECTOR_HEIGHT);
    assert!(wallet.is_view_wallet());

    let daemon = MockDaemon::new();

    let receive = TxBuilder::spec_vector("receive").output_to_wallet(&wallet, 5000).build();
    let spend = TxBuilder::new("spend").input(OUT0_KEY_IMAGE, 5000).output_to_stranger("gina", 4000).build();

    daemon.push(response(vec![with_tx(block("v0", VECTOR_HEIGHT, 100), receive)]));
    daemon.push(response(vec![with_tx(block("v1", VECTOR_HEIGHT + 1, 160), spend)]));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);
    assert_processed(sync.sync_step(), 1);

    let sub = sync.wallet().primary_sub_wallet().unwrap();
    assert_eq!(sub.unspent_inputs.len(), 1);
    assert_eq!(sub.unspent_inputs[0].key_image, Hex32([0u8; 32]));
    assert!(sub.spent_inputs.is_empty(), "a view wallet cannot detect its own spends");
    assert_eq!(sync.wallet().transactions().len(), 1);
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT + 1), (5000, 0));
    assert!(sync.daemon().index_requests().is_empty());
}

/// `Nigel::getWalletSyncData`'s `400` path: remember the daemon's ceiling,
/// halve, retry at once; and `resetRequestedBlockCount` climbing back to it.
#[test]
fn a_400_halves_the_batch_and_retries() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    // Three good answers to grow the batch 100 -> 200 -> 400 -> 800...
    for i in 0..3u64 {
        daemon.push(response(vec![block(&format!("g{i}"), VECTOR_HEIGHT + i, 100 + i)]));
    }
    // ...then the daemon refuses the fourth as too large, and accepts the retry.
    daemon.push_error(DaemonError::BadRequest("blockCount too large".into()));
    daemon.push(response(vec![block("g3", VECTOR_HEIGHT + 3, 200)]));

    let mut sync = synchronizer(daemon, wallet);

    for _ in 0..4 {
        assert_processed(sync.sync_step(), 1);
    }

    // 100, 200, 400 accepted; 800 refused; retried at 400 and accepted.
    assert_eq!(sync.daemon().block_counts(), vec![100, 200, 400, 800, 400]);
    // The ceiling is now what the daemon accepted, and the batch climbs no higher.
    assert_eq!(sync.max_block_count(), 400);
    assert_eq!(sync.requested_block_count(), 400);
}

/// A `429` is a wait, not a smaller batch: shrinking would only spend more rate
/// limit slots per block (`Nigel::decreaseRequestedBlockCount`).
#[test]
fn a_429_backs_off_without_shrinking_the_batch() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    daemon.push(response(vec![block("rl", VECTOR_HEIGHT, 100)]));
    daemon.push_error(DaemonError::RateLimited);
    daemon.push_error(DaemonError::Transport("connection reset".into()));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);
    assert_eq!(sync.requested_block_count(), 200);

    match sync.sync_step() {
        SyncStep::Failed { error: DaemonError::RateLimited, backoff } => {
            assert_eq!(backoff, Duration::from_secs(20));
        }
        other => panic!("expected a rate limited step, got {other:?}"),
    }
    assert!(sync.last_request_rate_limited());
    assert_eq!(sync.requested_block_count(), 200, "the batch survives a 429");

    // An ordinary failure does halve it, and waits five seconds.
    match sync.sync_step() {
        SyncStep::Failed { error: DaemonError::Transport(_), backoff } => {
            assert_eq!(backoff, Duration::from_secs(5));
        }
        other => panic!("expected a transport failure, got {other:?}"),
    }
    assert_eq!(sync.requested_block_count(), 100);
}

/// The daemon says we are at its top: the height moves, and when the hash is
/// the one we already hold, nothing else does (`storeBlockHash`'s early return).
#[test]
fn the_top_block_hash_path_is_a_no_op_when_it_is_ours() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    daemon.push(response(vec![block("top", VECTOR_HEIGHT, 100)]));
    daemon.push(synced_response(TopBlock { hash: label_hash("top"), height: VECTOR_HEIGHT }));
    daemon.push(synced_response(TopBlock { hash: label_hash("newer"), height: VECTOR_HEIGHT + 1 }));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);

    let before = sync.wallet().wallet_synchronizer.transaction_synchronizer_status.clone();
    assert_eq!(before.last_known_block_hashes.len(), 1);

    match sync.sync_step() {
        SyncStep::Synced { height } => assert_eq!(height, VECTOR_HEIGHT),
        other => panic!("expected synced, got {other:?}"),
    }

    let after = &sync.wallet().wallet_synchronizer.transaction_synchronizer_status;
    assert_eq!(after.last_known_block_hashes.len(), 1, "the same hash is not stored twice");
    assert_eq!(after.block_hash_checkpoints.len(), before.block_hash_checkpoints.len());
    assert_eq!(after.last_known_block_height, VECTOR_HEIGHT);

    // A different top block does move us on, with no block ever downloaded.
    match sync.sync_step() {
        SyncStep::Synced { height } => assert_eq!(height, VECTOR_HEIGHT + 1),
        other => panic!("expected synced, got {other:?}"),
    }
    let after = &sync.wallet().wallet_synchronizer.transaction_synchronizer_status;
    assert_eq!(after.last_known_block_height, VECTOR_HEIGHT + 1);
    assert_eq!(after.last_known_block_hashes.len(), 2);
    assert_eq!(after.last_known_block_hashes[0].to_hex(), label_hash("newer"));
}

/// The checkpoint list of `BlockDownloader::getBlockCheckpoints`: unprocessed
/// hashes newest first, padded to 50 with processed ones, then the sparse
/// 5000-block checkpoints.
#[test]
fn the_checkpoint_list_has_the_cpp_shape() {
    let wallet = spec_wallet(5001);
    let daemon = MockDaemon::new();

    // 60 blocks in one answer, so that the recent list overflows its 50 entries,
    // crossing two 5000-block checkpoint boundaries.
    let blocks: Vec<SyncBlock> = (0..60u64).map(|i| block(&format!("h{i}"), 5001 + i * 200, 100 + i)).collect();
    daemon.push(response(blocks));
    daemon.push(empty_response());

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 60);

    let status = &sync.wallet().wallet_synchronizer.transaction_synchronizer_status;
    assert_eq!(status.last_known_block_hashes.len(), 50);
    assert_eq!(status.last_known_block_hashes[0].to_hex(), label_hash("h59"));
    assert_eq!(status.last_known_block_height, 5001 + 59 * 200);

    // A sparse checkpoint every time 5000 blocks have passed since the last one:
    // heights 5001, 10201, 15401 ...
    assert_eq!(status.block_hash_checkpoints.len(), 3, "one every 5000 blocks: 5001, 10201, 15401");
    assert_eq!(status.block_hash_checkpoints.last().unwrap().to_hex(), label_hash("h0"));

    let sparse_count = status.block_hash_checkpoints.len();

    // The next request carries them: 50 recent hashes newest first, then the
    // sparse ones.
    let _ = sync.sync_step();
    let request = sync.daemon().requests().last().unwrap().clone();
    let expected_recent: Vec<String> = (0..50).map(|i| label_hash(&format!("h{}", 59 - i))).collect();
    assert_eq!(request.block_hash_checkpoints[..50].to_vec(), expected_recent);
    assert_eq!(request.block_hash_checkpoints.len(), 50 + sparse_count);
    assert_eq!(request.start_height, 5001);
    assert_eq!(request.start_timestamp, 0);
    assert!(!request.skip_coinbase_transactions);
    assert_eq!(request.skip_input_key_offsets, Some(true));
    assert_eq!(request.skip_empty_blocks, None);
    assert_eq!(request.encoding, None);
    assert_eq!(request.end_height, None);
}

/// Blocks downloaded but not yet processed lead the checkpoint list, so a
/// daemon resumes from what we hold rather than from what we have applied.
#[test]
fn unprocessed_blocks_lead_the_checkpoint_list() {
    let wallet = spec_wallet(10);
    let daemon = MockDaemon::new();

    daemon.push(response(vec![block("s0", 10, 1), block("s1", 11, 2), block("s2", 12, 3)]));

    // One block applied per step, so two stay in the store.
    let config = SyncConfig { block_processing_chunk: 1, ..SyncConfig::default() };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);

    assert_processed(sync.sync_step(), 1);
    assert_eq!(sync.stored_block_count(), 2);

    let checkpoints = sync.block_checkpoints();
    assert_eq!(checkpoints[0], label_hash("s2"), "newest downloaded first");
    assert_eq!(checkpoints[1], label_hash("s1"));
    assert_eq!(checkpoints[2], label_hash("s0"), "then the processed one");
}

/// A daemon that cannot serve the range we still need stops sync rather than
/// letting us record blocks nobody scanned (`BlockDownloader::recordSyncGap`).
#[test]
fn a_lite_daemon_below_our_height_stops_sync() {
    let wallet = spec_wallet(1000);
    let daemon = MockDaemon::new();

    daemon.push(response(vec![block("l0", 1000, 1)]));
    daemon.info.replace(Some(info(4_000_000, &["heightRange"], 2_000_000)));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);

    sync.refresh_info().unwrap();

    match sync.sync_step() {
        SyncStep::Gap { covered_to, daemon_serves_from } => {
            assert_eq!(covered_to, 1000);
            assert_eq!(daemon_serves_from, 2_000_000);
        }
        other => panic!("expected a gap, got {other:?}"),
    }
    assert_eq!(sync.sync_gap(), Some((1000, 2_000_000)));
    // It did not even ask.
    assert_eq!(sync.daemon().requests().len(), 1);
}

/// A daemon answering from higher up with nothing to explain it is retried
/// twice — a reorg at the tip looks exactly like this — and only then reported
/// (`Constants::UNEXPLAINED_SYNC_START_LIMIT`).
#[test]
fn an_unexplained_start_height_is_retried_then_reported() {
    let wallet = spec_wallet(1000);
    let daemon = MockDaemon::new();

    for i in 0..3 {
        daemon.push(response(vec![block(&format!("far{i}"), 9999, 1)]));
    }

    let mut sync = synchronizer(daemon, wallet);

    for attempt in 0..2 {
        match sync.sync_step() {
            SyncStep::Idle { backoff } => assert_eq!(backoff, Duration::from_secs(5), "attempt {attempt}"),
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    match sync.sync_step() {
        SyncStep::Gap { covered_to, daemon_serves_from } => {
            assert_eq!(covered_to, 999);
            assert_eq!(daemon_serves_from, 9999);
        }
        other => panic!("expected a gap, got {other:?}"),
    }
}

/// `checkLockedTransactions`: a send the daemon has never heard of is cancelled
/// and its inputs return to `unspentInputs`.
#[test]
fn cancelled_transactions_return_their_inputs() {
    let mut wallet = spec_wallet(VECTOR_HEIGHT);

    // A confirmed input, then locked by a send that never made it.
    let daemon = MockDaemon::new();
    let receive = TxBuilder::spec_vector("receive").output_to_wallet(&wallet, 5000).build();
    daemon.push(response(vec![with_tx(block("c0", VECTOR_HEIGHT, 100), receive)]));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);

    wallet = sync.into_wallet();

    let sent_hash = Hex32::from_hex(&label_hash("a send")).unwrap();
    let spend_key = wallet.primary_sub_wallet().unwrap().public_spend_key;
    let key_image = Hex32::from_hex(OUT0_KEY_IMAGE).unwrap();

    wallet.sub_wallets.locked_transactions.push(Transaction {
        block_height: 0,
        fee: 1000,
        hash: sent_hash,
        is_coinbase_transaction: false,
        payment_id: SHORT_PAYMENT_ID_PLAIN.into(),
        timestamp: 0,
        transfers: Vec::new(),
        unlock_time: 0,
    });
    assert!(wallet.mark_input_as_locked(&key_image, &spend_key));
    wallet.sub_wallets.sub_wallet[0].locked_inputs[0].parent_transaction_hash = sent_hash;

    assert_eq!(wallet.balance(VECTOR_HEIGHT), (0, 0), "an input in flight counts towards neither");

    let daemon = MockDaemon::new();
    daemon.transactions_status.replace(Some(TransactionsStatus {
        transactions_in_pool: Vec::new(),
        transactions_in_block: Vec::new(),
        transactions_unknown: vec![sent_hash.to_hex()],
        status: "OK".into(),
    }));

    let mut sync = synchronizer(daemon, wallet);
    assert_eq!(sync.check_locked_transactions().unwrap(), 1);

    assert!(sync.wallet().unconfirmed_transactions().is_empty());
    assert_eq!(sync.wallet().primary_sub_wallet().unwrap().unspent_inputs.len(), 1);
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT), (5000, 0));
}

/// The published `/getwalletsyncdata` sample, processed by a wallet that owns
/// nothing: no transactions, no inputs, and the checkpoints advance.
#[test]
fn the_mainnet_sample_finds_nothing_and_advances() {
    let data: WalletSyncData = serde_json::from_str(
        &std::fs::read_to_string(vectors().join("mainnet_getwalletsyncdata_4213000.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(data.items.len(), 2);
    assert_eq!(data.scanned_to_height, Some(4_213_001));

    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();
    daemon.push(data);

    let mut sync = synchronizer(daemon, wallet);
    assert_eq!(assert_processed(sync.sync_step(), 2), 4_213_001);

    assert!(sync.wallet().transactions().is_empty());
    assert!(sync.wallet().primary_sub_wallet().unwrap().unspent_inputs.is_empty());
    assert_eq!(sync.wallet().balance(4_213_001), (0, 0));
    // No owned outputs, so no global index request was needed.
    assert!(sync.daemon().index_requests().is_empty());

    let status = &sync.wallet().wallet_synchronizer.transaction_synchronizer_status;
    assert_eq!(status.last_known_block_height, 4_213_001);
    assert_eq!(
        status.last_known_block_hashes.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        vec![
            "f89ae4b7d0b132e14f2b5cb03646b1a3f1a28485d0c2153c1c17b20a021b033a".to_string(),
            "c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604".to_string(),
        ]
    );
    // The first block lands a sparse checkpoint (nothing saved yet); the second
    // is not 5000 blocks later, so it does not.
    assert_eq!(status.block_hash_checkpoints.len(), 1);
    assert_eq!(
        status.block_hash_checkpoints[0].to_hex(),
        "c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604"
    );

    // And the sync status reads as the C++ reports it.
    let status = sync.sync_status();
    assert_eq!(status.wallet_block_count, 4_213_001);
    assert!(!status.is_synced());
}

/// Far below the tip the C++ asks for four height windows at once. This port
/// asks for the same four, in the same order, one at a time.
#[test]
fn height_windows_are_requested_below_the_tip() {
    let wallet = spec_wallet(1000);
    let daemon = MockDaemon::new();

    daemon.info.replace(Some(info(4_000_000, &["heightRange", "skipEmptyBlocks"], 0)));

    // The sequential answer that establishes where we are.
    daemon.push(response(vec![block("w0", 1000, 1)]));
    // Then one answer per window, each covering it completely.
    for i in 0..4u64 {
        let start = 1001 + i * 4000;
        daemon.push(WalletSyncData {
            items: vec![block(&format!("win{i}"), start, 10 + i)],
            scanned_to_height: Some(start + 4000 - 1),
            synced: false,
            top_block: None,
            status: "OK".into(),
        });
    }

    let config = SyncConfig { height_windows: true, global_index_retry_delay: Duration::ZERO, ..SyncConfig::default() };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);
    sync.refresh_info().unwrap();

    assert_processed(sync.sync_step(), 1);
    // The first success grew the batch to 200, so each window is 200 * 20.
    assert_processed(sync.sync_step(), 4);

    let requests = sync.daemon().requests();
    assert_eq!(requests.len(), 5);

    let window = 200 * 20;
    for (i, request) in requests[1..].iter().enumerate() {
        let start = 1001 + (i as u64) * window;
        assert!(request.block_hash_checkpoints.is_empty(), "windows carry no checkpoints");
        assert_eq!(request.start_height, start);
        assert_eq!(request.end_height, Some(start + window));
    }
}

/// `skipEmptyBlocks` is only asked for when coinbases are being skipped and the
/// daemon has advertised the feature (`Nigel::getWalletSyncData`).
#[test]
fn skip_empty_blocks_is_only_asked_for_when_earned() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();
    daemon.info.replace(Some(info(4_213_010, &["skipEmptyBlocks"], 0)));

    let config = SyncConfig { skip_coinbase_transactions: true, ..SyncConfig::default() };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);

    // Before /info is read the feature is unknown, so it is not asked for.
    let _ = sync.sync_step();
    assert_eq!(sync.daemon().requests()[0].skip_empty_blocks, None);
    assert!(sync.daemon().requests()[0].skip_coinbase_transactions);

    sync.refresh_info().unwrap();
    let _ = sync.sync_step();
    assert_eq!(sync.daemon().requests()[1].skip_empty_blocks, Some(true));

    // A coinbase is neither requested nor scanned when it is skipped.
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();
    let coinbase = TxBuilder::spec_vector("cb").output_to_wallet(&wallet, 1_000_000).build();
    daemon.push(response(vec![with_coinbase(block("skip", VECTOR_HEIGHT, 1), coinbase)]));

    let config = SyncConfig { skip_coinbase_transactions: true, ..SyncConfig::default() };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);
    assert_processed(sync.sync_step(), 1);
    assert!(sync.wallet().transactions().is_empty());
    assert_eq!(sync.wallet().balance(VECTOR_HEIGHT), (0, 0));
}

/// Spent inputs older than the confirmation window are pruned
/// (`Constants::PRUNE_SPENT_INPUTS_INTERVAL`, 2880 blocks).
#[test]
fn spent_inputs_are_pruned_after_the_confirmation_window() {
    let wallet = spec_wallet(100);
    let daemon = MockDaemon::new();

    let receive = TxBuilder::spec_vector("receive").output_to_wallet(&wallet, 5000).build();
    let spend = TxBuilder::new("spend").input(OUT0_KEY_IMAGE, 5000).output_to_stranger("hank", 4000).build();

    daemon.push(response(vec![
        with_tx(block("p0", 100, 1), receive),
        with_tx(block("p1", 101, 2), spend),
        // A block on the prune boundary, more than 2880 blocks later.
        block("p2", 2880 * 2, 3),
    ]));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 3);

    let sub = sync.wallet().primary_sub_wallet().unwrap();
    assert!(sub.spent_inputs.is_empty(), "spent at 101, pruned at 5760 - 2880");
    assert_eq!(sync.wallet().transactions().len(), 2, "the transactions themselves are kept");
}

/// `SubWallets::addTransaction`: the payment id recorded when we sent a
/// transaction survives the scan of the block it lands in, where the short id
/// is ciphertext we cannot read, and the locked copy is dropped.
#[test]
fn a_send_keeps_the_payment_id_it_was_sent_with() {
    let mut wallet = spec_wallet(VECTOR_HEIGHT);

    // The block that confirms a send of ours: it spends our key image, so
    // decryptPaymentID reports nothing for it.
    let daemon = MockDaemon::new();
    let receive = TxBuilder::spec_vector("receive").output_to_wallet(&wallet, 5000).build();
    daemon.push(response(vec![with_tx(block("s0", VECTOR_HEIGHT, 100), receive)]));

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);
    wallet = sync.into_wallet();

    let sent = TxBuilder::new("our send")
        .payment_id(SHORT_PAYMENT_ID_CIPHER)
        .input(OUT0_KEY_IMAGE, 5000)
        .output_to_stranger("ivy", 4000)
        .build();

    wallet.sub_wallets.locked_transactions.push(Transaction {
        block_height: 0,
        fee: 1000,
        hash: Hex32::from_hex(&sent.hash).unwrap(),
        is_coinbase_transaction: false,
        payment_id: SHORT_PAYMENT_ID_PLAIN.into(),
        timestamp: 0,
        transfers: Vec::new(),
        unlock_time: 0,
    });

    let daemon = MockDaemon::new();
    daemon.push(response(vec![with_tx(block("s1", VECTOR_HEIGHT + 1, 160), sent)]));
    // The same block again, which must not record the transaction twice.
    daemon.push(empty_response());

    let mut sync = synchronizer(daemon, wallet);
    assert_processed(sync.sync_step(), 1);

    let txs = sync.wallet().transactions();
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[1].payment_id, SHORT_PAYMENT_ID_PLAIN, "the plaintext we recorded at send time");
    assert!(sync.wallet().unconfirmed_transactions().is_empty(), "the locked copy is dropped");
}

/// `sync_until_synced` drives rounds until the daemon says we are at its top.
#[test]
fn sync_until_synced_stops_at_the_top() {
    let wallet = spec_wallet(VECTOR_HEIGHT);
    let daemon = MockDaemon::new();

    daemon.push(response(vec![block("u0", VECTOR_HEIGHT, 1), block("u1", VECTOR_HEIGHT + 1, 2)]));
    daemon.push(response(vec![block("u2", VECTOR_HEIGHT + 2, 3)]));
    daemon.push(synced_response(TopBlock { hash: label_hash("u2"), height: VECTOR_HEIGHT + 2 }));

    let config = SyncConfig {
        failure_backoff: Duration::ZERO,
        rate_limited_backoff: Duration::ZERO,
        global_index_retry_delay: Duration::ZERO,
        ..SyncConfig::default()
    };
    let mut sync = Synchronizer::with_config(daemon, wallet, config);

    match sync.sync_until_synced(10) {
        SyncStep::Synced { height } => assert_eq!(height, VECTOR_HEIGHT + 2),
        other => panic!("expected synced, got {other:?}"),
    }

    assert_eq!(sync.wallet().wallet_height(), VECTOR_HEIGHT + 2);
    let status = sync.sync_status();
    assert_eq!(status.wallet_block_count, VECTOR_HEIGHT + 2);
    assert_eq!(status.local_daemon_block_count, 9_999_999);
}

/// Scanning a chunk on several threads finds exactly what one thread finds, in
/// the same order (`SyncConfig::scan_threads`): the same inputs, the same
/// transactions, the same balance and the same height.
#[test]
fn scanning_on_several_threads_matches_one_thread() {
    let run = |threads: usize| {
        let wallet = spec_view_wallet(VECTOR_HEIGHT);
        let mut blocks = Vec::new();
        for i in 0..24u64 {
            let mut b = block(&format!("par-{i}"), VECTOR_HEIGHT + i, 1_788_894_799 + i * 60);
            // Owned outputs spread unevenly: some blocks pay us twice, some
            // once, some not at all, and every block carries a stranger's.
            if i % 3 != 2 {
                let tx = TxBuilder::new(&format!("ours-{i}"))
                    .output_to_wallet(&wallet, 1000 + i)
                    .output_to_stranger("bob", 7)
                    .build();
                b = with_tx(b, tx);
            }
            if i % 3 == 0 {
                let tx = TxBuilder::new(&format!("ours-again-{i}"))
                    .output_to_stranger("carol", 9)
                    .output_to_wallet(&wallet, 50 + i)
                    .build();
                b = with_tx(b, tx);
            }
            b = with_tx(b, TxBuilder::new(&format!("theirs-{i}")).output_to_stranger("dave", 11).build());
            blocks.push(b);
        }
        let daemon = MockDaemon::new();
        daemon.push(response(blocks));
        let config = SyncConfig {
            global_index_retry_delay: Duration::ZERO,
            block_processing_chunk: 100,
            scan_threads: threads,
            ..SyncConfig::default()
        };
        let mut sync = Synchronizer::with_config(daemon, wallet, config);
        sync.refresh_info().unwrap();
        assert_processed(sync.sync_step(), 24);
        let sub = sync.wallet().primary_sub_wallet().unwrap();
        (
            format!("{:?}", sub.unspent_inputs),
            format!("{:?}", sync.wallet().transactions()),
            sync.wallet().balance(VECTOR_HEIGHT + 100),
            sync.wallet().wallet_height(),
        )
    };
    let one = run(1);
    assert_eq!(one.3, VECTOR_HEIGHT + 23);
    assert!(one.0.contains("amount: 1000"), "the scan found our outputs at all: {}", one.0);
    for threads in [2, 4, 7] {
        assert_eq!(run(threads), one, "{threads} scanning threads");
    }
}

fn vectors() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

////////////////////
/* LIVE           */
////////////////////

/// A fresh wallet synced against the seed node for a handful of batches
/// (`cargo test -p wrkz-wallet -- --ignored`).
///
/// The wallet is created at the current network height minus a few thousand
/// blocks, so the sync is short and finds nothing: no address with history is
/// published, and none is needed for this. The acceptance diff against
/// the C++ `wrkz-wallet` CLI on a real address is the operator's, through the
/// `wrkz-wallet-sync` binary.
#[test]
#[ignore]
fn live_sync_against_the_seed_node() {
    use wrkz_wallet::daemon::Daemon;

    let daemon = Daemon::new("http://node-fin.wrkz.work:17856").unwrap();
    let info = daemon.info().unwrap();
    let top = info.height - 1;
    let start = top - 3000;

    let wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, start).unwrap();

    let mut sync = Synchronizer::new(daemon, wallet);
    sync.refresh_info().unwrap();
    assert!(sync.daemon_state().network_block_count >= top - 1);

    for _ in 0..5 {
        match sync.sync_step() {
            SyncStep::Processed { .. } | SyncStep::Idle { .. } | SyncStep::Synced { .. } => {}
            other => panic!("live sync failed: {other:?}"),
        }
    }

    let height = sync.wallet().wallet_height();
    assert!(height > start, "the wallet advanced from {start} to {height}");

    let status = &sync.wallet().wallet_synchronizer.transaction_synchronizer_status;
    assert!(!status.last_known_block_hashes.is_empty());
    assert!(status.last_known_block_hashes.len() <= 50);
    assert_eq!(status.last_known_block_height, height);

    // The C++ shape: up to 50 hashes of blocks held but not yet applied, newest
    // first, padded with the ones already applied, then the sparse checkpoints.
    let checkpoints = sync.block_checkpoints();
    assert!(checkpoints.len() >= 50 || checkpoints.len() >= status.last_known_block_hashes.len());
    assert!(
        checkpoints.contains(&status.last_known_block_hashes[0].to_hex()) || sync.stored_block_count() >= 50,
        "the newest applied hash is in the list unless 50 unapplied ones fill it"
    );
    assert!(checkpoints.iter().all(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())));

    // Nothing was found, so nothing is owed.
    assert_eq!(sync.total_balance(), (0, 0));
}
