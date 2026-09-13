// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Wallet file acceptance tests (spec/10 "Wallet file", spec/12 stage 2 step 2).
//!
//! The three `.wallet` fixtures were written by the C++ `wrkz-wallet-api`
//! (`build/src/Release/wrkz-wallet-api.exe`, WRKZCoin v0.4.8.280) from the
//! published spec/05 seed; the `.json` beside each is that same wallet as
//! `POST /export/json` dumped it, which is `WalletBackend::toJSON()` verbatim.
//! See `tests/fixtures/README.md`.

use std::path::PathBuf;
use std::time::Instant;

use wrkz_wallet::crypto::SALT_SIZE;
use wrkz_wallet::file::{
    decode_wallet_file, encode_wallet_file_with_salt, Hex32, SecretKey, SubWallet, Transaction, TransactionInput,
    Transfer, TxPrivateKey, UnconfirmedInput, Wallet, WalletError,
};

const PASSWORD: &str = "password";

const SPEC_MNEMONIC: &str = "eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell different \
                             dating intended richly slower inundate ridges slug inundate ridges slug were rotate \
                             rudely viewpoint";
const SPEC_SPEND_SECRET: &str = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
const SPEC_SPEND_PUBLIC: &str = "857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3";
const SPEC_VIEW_SECRET: &str = "779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700";
const SPEC_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

/// `spec/vectors/primitives.txt`, "deterministic subwallets".
const SUBWALLET_VECTORS: [(u64, &str, &str); 3] = [
    (
        1,
        "2c7d88e6b43bb83f7215ecc744e73589d8d1a841e7ab8f26672c5490c1aa2b0a",
        "2c1c4f98aed340fd311ab7d1fe51a1c2e879fde0eb74695e3d10b33d62cc5086",
    ),
    (
        2,
        "b5f66b6627238ac68d776a1319f785243feb071a3a11cf611c6bc69e0b40a20e",
        "560f3e3b47ffd155f6a42b9764464b751f63f2269dc22e775dc67c543097c456",
    ),
    (
        5,
        "a4ddafa869de6372de50b571f2d8aa6f99de80d2da2ef7bf0b84fcc878026c08",
        "4d4dffbb254aba5cbeed60ac4bab40ae295cc806517f5047ed0c8bb6110879bd",
    ),
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// The exported JSON, without the trailing `std::endl` (which `std::ofstream`
/// in text mode turns into CRLF on Windows).
fn fixture_json(name: &str) -> Vec<u8> {
    let mut bytes = fixture_bytes(name);
    while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        bytes.pop();
    }
    bytes
}

fn temp_path(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("wrkz-wallet-test-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

//////////////////////////////////////////////////
// Reading what the C++ wrote
//////////////////////////////////////////////////

#[test]
fn opens_a_wallet_file_written_by_the_cpp_wallet() {
    let wallet = Wallet::open(fixture("spec05-seed.wallet"), PASSWORD).unwrap();

    assert_eq!(wallet.wallet_file_format_version, 0);
    assert!(!wallet.is_view_wallet());
    assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));
    assert_eq!(&*wallet.private_view_key().to_hex(), SPEC_VIEW_SECRET);
    assert_eq!(wallet.sub_wallets.public_spend_keys.len(), 1);
    assert_eq!(wallet.sub_wallets.public_spend_keys[0].to_hex(), SPEC_SPEND_PUBLIC);
    assert_eq!(wallet.sub_wallets.sub_wallet_index_counter, 0);
    assert!(wallet.sub_wallets.transactions.is_empty());
    assert!(wallet.sub_wallets.locked_transactions.is_empty());
    assert!(wallet.sub_wallets.tx_private_keys.is_empty());

    let primary = wallet.primary_sub_wallet().unwrap();
    assert!(primary.is_primary_address);
    assert_eq!(primary.wallet_index, 0);
    assert_eq!(&*primary.private_spend_key.to_hex(), SPEC_SPEND_SECRET);
    assert_eq!(primary.sync_start_height, 4_213_000);
    assert_eq!(primary.sync_start_timestamp, 0);
    assert!(primary.all_inputs().next().is_none());

    let sync = &wallet.wallet_synchronizer;
    assert_eq!(sync.start_height, 4_213_000);
    assert_eq!(sync.start_timestamp, 0);
    assert_eq!(&*sync.private_view_key.to_hex(), SPEC_VIEW_SECRET);
    assert_eq!(sync.transaction_synchronizer_status.last_known_block_height, 0);
    assert!(sync.transaction_synchronizer_status.block_hash_checkpoints.is_empty());
    assert!(sync.transaction_synchronizer_status.last_known_block_hashes.is_empty());

    // the seed the C++ would print for this wallet
    assert_eq!(&**wallet.mnemonic_seed().unwrap(), SPEC_MNEMONIC);
}

#[test]
fn a_view_only_wallet_has_the_null_spend_key() {
    let wallet = Wallet::open(fixture("spec05-view.wallet"), PASSWORD).unwrap();

    assert!(wallet.is_view_wallet());
    assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));
    assert_eq!(&*wallet.private_view_key().to_hex(), SPEC_VIEW_SECRET);

    let primary = wallet.primary_sub_wallet().unwrap();
    assert!(primary.private_spend_key.is_null());
    assert!(!primary.has_spend_key());
    assert_eq!(&*primary.private_spend_key.to_hex(), &"0".repeat(64));
    // a view wallet has no seed, and cannot grow deterministic subwallets
    assert!(wallet.mnemonic_seed().is_none());

    let mut wallet = wallet;
    assert!(matches!(wallet.add_sub_wallet(), Err(WalletError::IllegalViewWalletOperation)));
    assert!(matches!(wallet.import_sub_wallet_at_index(1, 0), Err(WalletError::IllegalViewWalletOperation)));
}

#[test]
fn opens_a_multi_subwallet_file_and_keeps_the_indexes() {
    let wallet = Wallet::open(fixture("spec05-subwallets.wallet"), PASSWORD).unwrap();

    assert_eq!(wallet.sub_wallets.sub_wallet.len(), 3);
    assert_eq!(wallet.sub_wallets.sub_wallet_index_counter, 2);
    assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));

    for (index, secret, public) in SUBWALLET_VECTORS.iter().take(2) {
        let sub = wallet.sub_wallets.sub_wallet.iter().find(|s| s.wallet_index == *index).unwrap();
        assert_eq!(&*sub.private_spend_key.to_hex(), *secret);
        assert_eq!(sub.public_spend_key.to_hex(), *public);
        assert!(!sub.is_primary_address);
        // added subwallets scan from height 0 and record a creation timestamp
        assert_eq!(sub.sync_start_height, 0);
        assert!(sub.sync_start_timestamp > 1_529_831_318);
    }

    // publicSpendKeys is the flat insertion-ordered list of the same keys
    assert_eq!(wallet.sub_wallets.public_spend_keys.len(), 3);
    for sub in &wallet.sub_wallets.sub_wallet {
        assert!(wallet.sub_wallets.public_spend_keys.contains(&sub.public_spend_key));
    }
}

#[test]
fn the_json_we_write_is_byte_identical_to_the_cpp_export() {
    for (wallet_file, json_file) in [
        ("spec05-seed.wallet", "spec05-seed.json"),
        ("spec05-view.wallet", "spec05-view.json"),
        ("spec05-subwallets.wallet", "spec05-subwallets.json"),
    ] {
        let wallet = Wallet::open(fixture(wallet_file), PASSWORD).unwrap();
        let ours = wallet.to_json_bytes().unwrap();
        let theirs = fixture_json(json_file);
        assert_eq!(
            String::from_utf8_lossy(&ours),
            String::from_utf8_lossy(&theirs),
            "{wallet_file}: our JSON is not what WalletBackend::toJSON() produced"
        );
    }
}

#[test]
fn the_decrypted_fixture_is_the_exported_json() {
    // Straight from the file bytes, without going through the model at all:
    // proves the header layout and the cipher, not just our serializer.
    let json = decode_wallet_file(&fixture_bytes("spec05-seed.wallet"), PASSWORD).unwrap();
    assert_eq!(String::from_utf8_lossy(&json), String::from_utf8_lossy(&fixture_json("spec05-seed.json")));
}

//////////////////////////////////////////////////
// Round trips
//////////////////////////////////////////////////

#[test]
fn round_trip_open_save_open_is_byte_identical() {
    for name in ["spec05-seed.wallet", "spec05-view.wallet", "spec05-subwallets.wallet"] {
        let original = Wallet::open(fixture(name), PASSWORD).unwrap();
        let json = original.to_json_bytes().unwrap();

        let path = temp_path(name);
        original.save(&path, "a different password").unwrap();

        let reopened = Wallet::open(&path, "a different password").unwrap();
        assert_eq!(&*reopened.to_json_bytes().unwrap(), &*json, "{name} did not survive a save/open");

        // the password really did change with the save
        assert!(matches!(Wallet::open(&path, PASSWORD), Err(WalletError::WrongPassword)));

        // and no temporary file was left behind
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists());

        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn saving_twice_differs_only_in_the_salt() {
    let wallet = Wallet::open(fixture("spec05-seed.wallet"), PASSWORD).unwrap();
    let a = wallet.to_file_bytes(PASSWORD).unwrap();
    let b = wallet.to_file_bytes(PASSWORD).unwrap();

    // `saveWalletJSONToDisk` generates a fresh salt on every save, so two saves
    // of the same wallet share no ciphertext.
    assert_eq!(a[..64], b[..64]);
    assert_ne!(a[64..80], b[64..80], "the salt was reused");
    assert_ne!(a[80..], b[80..]);
    assert_eq!(a.len(), b.len());

    // and both decrypt to the same document
    assert_eq!(&*decode_wallet_file(&a, PASSWORD).unwrap(), &*decode_wallet_file(&b, PASSWORD).unwrap());
}

#[test]
fn a_fixed_salt_gives_reproducible_file_bytes() {
    let wallet = Wallet::open(fixture("spec05-seed.wallet"), PASSWORD).unwrap();
    let salt = [0x42u8; SALT_SIZE];
    let a = wallet.to_file_bytes_with_salt(PASSWORD, &salt).unwrap();
    let b = encode_wallet_file_with_salt(&wallet.to_json_bytes().unwrap(), PASSWORD, &salt);
    assert_eq!(a, b);
    assert_eq!(&a[64..80], &salt[..]);
}

#[test]
fn every_field_of_the_schema_survives_a_round_trip() {
    let mut wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();

    let input = TransactionInput {
        amount: 1_234_567,
        block_height: 4_213_001,
        global_output_index: Some(3_808_773),
        key: Hex32::from_hex("22c90af32cfded17237a447122de92e736b59313f635bf43f69bacfad3756723").unwrap(),
        key_image: Hex32::from_hex("17578ae6fcb167f76bcdbeece316b383e13a164e2daca2ed2397c2aebd22b007").unwrap(),
        parent_transaction_hash: Hex32::from_hex(&"ab".repeat(32)).unwrap(),
        private_ephemeral: Some(
            SecretKey::from_hex("f0b2033e6f607de659fa6545ca0498de1f3f55021caa129d803f4d787a84c50d").unwrap(),
        ),
        spend_height: 0,
        transaction_index: 2,
        transaction_public_key: Hex32::from_hex(&"cd".repeat(32)).unwrap(),
        unlock_time: 4_213_021,
    };

    let mut spent = input.clone();
    spent.spend_height = 4_213_100;
    spent.private_ephemeral = None;

    let sub = &mut wallet.sub_wallets.sub_wallet[0];
    sub.unspent_inputs.push(input.clone());
    sub.locked_inputs.push(input.clone());
    sub.spent_inputs.push(spent);
    sub.unconfirmed_incoming_amounts.push(UnconfirmedInput {
        amount: 500,
        key: Hex32::from_hex(&"11".repeat(32)).unwrap(),
        parent_transaction_hash: Hex32::from_hex(&"22".repeat(32)).unwrap(),
    });

    wallet.sub_wallets.transactions.push(Transaction {
        block_height: 4_213_001,
        fee: 8_000,
        hash: Hex32::from_hex(&"33".repeat(32)).unwrap(),
        is_coinbase_transaction: false,
        payment_id: "0102030405060708".to_string(),
        timestamp: 1_788_000_000,
        transfers: vec![
            Transfer { amount: -10_000, public_key: Hex32::from_hex(SPEC_SPEND_PUBLIC).unwrap() },
            Transfer { amount: 2_000, public_key: Hex32::from_hex(&"44".repeat(32)).unwrap() },
        ],
        unlock_time: 0,
    });
    wallet.sub_wallets.transactions.push(Transaction {
        block_height: 4_213_002,
        fee: 0,
        hash: Hex32::from_hex(&"55".repeat(32)).unwrap(),
        is_coinbase_transaction: true,
        payment_id: String::new(),
        timestamp: 1_788_000_060,
        transfers: vec![Transfer { amount: 900_000, public_key: Hex32::from_hex(SPEC_SPEND_PUBLIC).unwrap() }],
        unlock_time: 4_213_022,
    });
    wallet.sub_wallets.locked_transactions.push(wallet.sub_wallets.transactions[0].clone());
    wallet.sub_wallets.tx_private_keys.push(TxPrivateKey {
        transaction_hash: Hex32::from_hex(&"33".repeat(32)).unwrap(),
        tx_private_key: SecretKey::from_hex("487a3668ed5bfd7175e832dc642e64f821222222222222222222222222222202")
            .unwrap(),
    });

    let status = &mut wallet.wallet_synchronizer.transaction_synchronizer_status;
    for height in [4_213_001u64, 4_213_002] {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&height.to_le_bytes());
        status.store_block_hash(Hex32(h), height);
    }

    let json = wallet.to_json_bytes().unwrap();
    let reopened = Wallet::from_json_bytes(&json).unwrap();
    assert_eq!(&*reopened.to_json_bytes().unwrap(), &*json);

    let sub = &reopened.sub_wallets.sub_wallet[0];
    assert_eq!(sub.unspent_inputs[0], input);
    assert_eq!(
        sub.locked_inputs[0].private_ephemeral.as_ref().map(|k| k.to_hex().to_string()),
        input.private_ephemeral.as_ref().map(|k| k.to_hex().to_string())
    );
    assert_eq!(sub.spent_inputs[0].spend_height, 4_213_100);
    assert!(sub.spent_inputs[0].private_ephemeral.is_none());
    assert_eq!(sub.unconfirmed_incoming_amounts[0].amount, 500);

    let tx = &reopened.sub_wallets.transactions[0];
    assert_eq!(tx.total_amount(), -8_000);
    assert_eq!(tx.payment_id, "0102030405060708");
    assert!(!tx.is_fusion_transaction());
    assert!(reopened.sub_wallets.transactions[1].is_coinbase_transaction);
    assert!(!reopened.sub_wallets.transactions[1].is_fusion_transaction(), "a coinbase is never a fusion");
    assert_eq!(reopened.sub_wallets.locked_transactions.len(), 1);
    assert_eq!(reopened.sub_wallets.tx_private_keys.len(), 1);

    let status = &reopened.wallet_synchronizer.transaction_synchronizer_status;
    assert_eq!(status.last_known_block_height, 4_213_002);
    assert_eq!(status.last_known_block_hashes.len(), 2);
    assert_eq!(status.last_known_block_hashes[0].0[..8], 4_213_002u64.to_le_bytes());

    // the JSON really does carry every documented key, in order
    let text = String::from_utf8(json.to_vec()).unwrap();
    for key in [
        "\"subWallets\"",
        "\"walletFileFormatVersion\"",
        "\"walletSynchronizer\"",
        "\"isViewWallet\"",
        "\"lockedTransactions\"",
        "\"privateViewKey\"",
        "\"publicSpendKeys\"",
        "\"subWallet\"",
        "\"subWalletIndexCounter\"",
        "\"transactions\"",
        "\"txPrivateKeys\"",
        "\"isPrimaryAddress\"",
        "\"lockedInputs\"",
        "\"privateSpendKey\"",
        "\"publicSpendKey\"",
        "\"spentInputs\"",
        "\"syncStartHeight\"",
        "\"syncStartTimestamp\"",
        "\"unconfirmedIncomingAmounts\"",
        "\"unspentInputs\"",
        "\"walletIndex\"",
        "\"globalOutputIndex\"",
        "\"keyImage\"",
        "\"parentTransactionHash\"",
        "\"privateEphemeral\"",
        "\"spendHeight\"",
        "\"transactionIndex\"",
        "\"transactionPublicKey\"",
        "\"unlockTime\"",
        "\"isCoinbaseTransaction\"",
        "\"paymentID\"",
        "\"transfers\"",
        "\"transactionHash\"",
        "\"txPrivateKey\"",
        "\"blockHashCheckpoints\"",
        "\"lastKnownBlockHashes\"",
        "\"lastKnownBlockHeight\"",
        "\"startHeight\"",
        "\"startTimestamp\"",
        "\"transactionSynchronizerStatus\"",
    ] {
        assert!(text.contains(key), "{key} missing from the document");
    }
    // nlohmann dumps with no whitespace at all
    assert!(!text.contains(": "));
    assert!(!text.contains(", "));
    // and `m_lastSavedCheckpointAt` is not part of the format
    assert!(!text.contains("lastSavedCheckpointAt"));
}

//////////////////////////////////////////////////
// Refusals
//////////////////////////////////////////////////

#[test]
fn a_wrong_password_is_reported_as_a_wrong_password() {
    let bytes = fixture_bytes("spec05-seed.wallet");
    assert!(matches!(Wallet::from_file_bytes(&bytes, "not the password"), Err(WalletError::WrongPassword)));
    assert!(matches!(Wallet::from_file_bytes(&bytes, ""), Err(WalletError::WrongPassword)));
    assert!(matches!(Wallet::from_file_bytes(&bytes, "Password"), Err(WalletError::WrongPassword)));
}

/// Hand-built files: the identifier and the salt are right and the ciphertext
/// decrypts cleanly, so only the inner marker decides.
fn file_around(plaintext: &[u8], password: &str) -> Vec<u8> {
    let salt = [9u8; SALT_SIZE];
    let mut file = Vec::new();
    file.extend_from_slice(&wrkz_primitives::constants::IS_A_WALLET_IDENTIFIER[..]);
    file.extend_from_slice(&salt);
    file.extend_from_slice(&wrkz_wallet::crypto::encrypt_wallet_file(plaintext, password.as_bytes(), &salt));
    file
}

#[test]
fn a_readable_file_with_the_wrong_inner_marker_is_still_a_wrong_password() {
    // One byte of `IS_CORRECT_PASSWORD_IDENTIFIER` changed. The decryption
    // succeeded, so this can only mean the password was wrong; the C++ refuses
    // to report anything more specific, because "bad padding" versus "bad
    // marker" is a padding oracle (`WalletBackend.cpp:565`).
    let mut plaintext = b"You're a big guy.
For you."
        .to_vec();
    assert_eq!(plaintext.len(), 26);
    plaintext[25] = b'!';
    plaintext.extend_from_slice(br#"{"walletFileFormatVersion":0}"#);
    let file = file_around(&plaintext, PASSWORD);
    assert!(matches!(Wallet::from_file_bytes(&file, PASSWORD), Err(WalletError::WrongPassword)));

    // The same file with the right marker opens as far as the JSON.
    let mut plaintext = b"You're a big guy.
For you."
        .to_vec();
    plaintext.extend_from_slice(br#"{"walletFileFormatVersion":0}"#);
    let file = file_around(&plaintext, PASSWORD);
    assert!(matches!(Wallet::from_file_bytes(&file, PASSWORD), Err(WalletError::WalletFileCorrupted(_))));

    // Plaintext too short to hold the marker at all is corruption, not a wrong
    // password: `hasMagicIdentifier` is given WALLET_FILE_CORRUPTED for that.
    let file = file_around(b"tiny", PASSWORD);
    match Wallet::from_file_bytes(&file, PASSWORD) {
        Err(WalletError::WalletFileCorrupted(_)) => {}
        other => panic!("expected WalletFileCorrupted, got {other:?}"),
    }
}

#[test]
fn a_file_that_is_not_a_wallet_is_reported_as_such() {
    assert!(matches!(Wallet::from_file_bytes(b"", PASSWORD), Err(WalletError::NotAWalletFile)));
    assert!(matches!(Wallet::from_file_bytes(b"hello", PASSWORD), Err(WalletError::NotAWalletFile)));
    assert!(matches!(Wallet::from_file_bytes(&[0u8; 4096], PASSWORD), Err(WalletError::NotAWalletFile)));

    // one byte of the 64-byte marker changed is enough
    let mut bytes = fixture_bytes("spec05-seed.wallet");
    bytes[63] ^= 1;
    assert!(matches!(Wallet::from_file_bytes(&bytes, PASSWORD), Err(WalletError::NotAWalletFile)));

    // a JSON file, or a legacy WalletGreen file, looks like this
    assert!(matches!(
        Wallet::from_file_bytes(&fixture_bytes("spec05-seed.json"), PASSWORD),
        Err(WalletError::NotAWalletFile)
    ));
}

#[test]
fn truncated_files_are_refused_at_the_right_stage() {
    let bytes = fixture_bytes("spec05-seed.wallet");

    // shorter than the wallet marker: NOT_A_WALLET_FILE, the same error the C++
    // gives for a wrong marker (`hasMagicIdentifier` is passed it twice)
    for cut in [0usize, 1, 63] {
        assert!(
            matches!(Wallet::from_file_bytes(&bytes[..cut], PASSWORD), Err(WalletError::NotAWalletFile)),
            "{cut} bytes"
        );
    }

    // marker present, salt incomplete: WALLET_FILE_CORRUPTED
    for cut in [64usize, 65, 79] {
        match Wallet::from_file_bytes(&bytes[..cut], PASSWORD) {
            Err(WalletError::WalletFileCorrupted(_)) => {}
            other => panic!("{cut} bytes: expected WalletFileCorrupted, got {other:?}"),
        }
    }

    // salt present but the ciphertext gone or ragged: decryption fails, and
    // every decryption failure is a wrong password
    for cut in [80usize, 81, 95, 100, bytes.len() - 1] {
        assert!(
            matches!(Wallet::from_file_bytes(&bytes[..cut], PASSWORD), Err(WalletError::WrongPassword)),
            "{cut} bytes"
        );
    }

    // A truncation on a block boundary still decrypts each block, so it fails
    // later: either the padding of the new last block is not valid (a wrong
    // password) or it is and the JSON stops mid-document (corruption).
    for cut in [96usize, bytes.len() - 16, bytes.len() - 160] {
        match Wallet::from_file_bytes(&bytes[..cut], PASSWORD) {
            Err(WalletError::WalletFileCorrupted(_)) | Err(WalletError::WrongPassword) => {}
            other => panic!("{cut} bytes: expected a refusal, got {other:?}"),
        }
    }
}

#[test]
fn a_flipped_ciphertext_byte_is_refused() {
    let mut bytes = fixture_bytes("spec05-seed.wallet");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    assert!(Wallet::from_file_bytes(&bytes, PASSWORD).is_err());

    let mut bytes = fixture_bytes("spec05-seed.wallet");
    bytes[64] ^= 0x01; // the salt, which is also the IV and the KDF input
    assert!(matches!(Wallet::from_file_bytes(&bytes, PASSWORD), Err(WalletError::WrongPassword)));
}

#[test]
fn another_format_version_is_reported_as_unsupported() {
    let json = br#"{"subWallets":{},"walletFileFormatVersion":1,"walletSynchronizer":{}}"#;
    match Wallet::from_json_bytes(json) {
        Err(WalletError::UnsupportedWalletFileFormatVersion(1)) => {}
        other => panic!("expected an unsupported version, got {other:?}"),
    }

    // and the version is read before anything else, exactly as
    // `WalletBackend::fromJSON` does, so a document we could not read either
    // way still reports the version
    let json = br#"{"walletFileFormatVersion":9000}"#;
    assert!(matches!(Wallet::from_json_bytes(json), Err(WalletError::UnsupportedWalletFileFormatVersion(9000))));

    // a missing version is corruption, not version 0
    assert!(matches!(Wallet::from_json_bytes(b"{}"), Err(WalletError::WalletFileCorrupted(_))));
    assert!(matches!(Wallet::from_json_bytes(b"not json"), Err(WalletError::WalletFileCorrupted(_))));
}

#[test]
fn a_missing_file_is_reported_as_missing() {
    let path = temp_path("does-not-exist.wallet");
    let _ = std::fs::remove_file(&path);
    assert!(matches!(Wallet::open(&path, PASSWORD), Err(WalletError::FilenameNonExistent)));
}

#[test]
fn required_fields_are_required_and_optional_ones_default() {
    let full = String::from_utf8(fixture_json("spec05-seed.json")).unwrap();

    // the three keys the C++ reader guards with `contains`
    let without_counter = full.replace(r#""subWalletIndexCounter":0,"#, "");
    assert!(!without_counter.contains("subWalletIndexCounter"));
    let wallet = Wallet::from_json_bytes(without_counter.as_bytes()).unwrap();
    assert_eq!(wallet.sub_wallets.sub_wallet_index_counter, 0);

    let without_index = full.replace(r#","walletIndex":0"#, "");
    let wallet = Wallet::from_json_bytes(without_index.as_bytes()).unwrap();
    assert_eq!(wallet.sub_wallets.sub_wallet[0].wallet_index, 0);

    // everything else throws `std::invalid_argument` in the C++ and is caught
    // as WALLET_FILE_CORRUPTED
    for key in [
        r#""isViewWallet":false,"#,
        r#""lockedTransactions":[],"#,
        r#""transactions":[],"#,
        r#""txPrivateKeys":[]"#,
        r#""syncStartHeight":4213000,"#,
        r#""isPrimaryAddress":true,"#,
        r#""unspentInputs":[],"#,
        r#""startHeight":4213000,"#,
        r#""lastKnownBlockHeight":0"#,
    ] {
        let broken = full.replace(key, "");
        assert!(broken.len() < full.len(), "{key} was not in the fixture");
        match Wallet::from_json_bytes(broken.as_bytes()) {
            Err(WalletError::WalletFileCorrupted(_)) => {}
            other => panic!("removing {key} should have been corruption, got {other:?}"),
        }
    }
}

#[test]
fn unknown_keys_are_ignored() {
    // Both readers use `at()` / lookups by name and never object to extra keys;
    // a file from a newer minor build must still open.
    let full = String::from_utf8(fixture_json("spec05-seed.json")).unwrap();
    let extended = full.replacen(r#"{"subWallets""#, r#"{"somethingNew":[1,2,3],"subWallets""#, 1);
    let wallet = Wallet::from_json_bytes(extended.as_bytes()).unwrap();
    assert_eq!(wallet.primary_address(), Some(SPEC_ADDRESS));

    // we drop the unknown key on the next save, which is what the C++ does too
    assert_eq!(String::from_utf8(wallet.to_json_bytes().unwrap().to_vec()).unwrap(), full);
}

#[test]
fn hex_fields_are_checked() {
    let full = String::from_utf8(fixture_json("spec05-seed.json")).unwrap();
    // `Crypto::Hash::fromString` throws on the wrong length or a non-hex digit
    for bad in [
        r#""privateViewKey":"779e""#,
        r#""privateViewKey":"zz9e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700""#,
    ] {
        let broken = full.replacen(
            r#""privateViewKey":"779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700""#,
            bad,
            1,
        );
        match Wallet::from_json_bytes(broken.as_bytes()) {
            Err(WalletError::WalletFileCorrupted(_)) => {}
            other => panic!("{bad} should have been refused, got {other:?}"),
        }
    }
}

//////////////////////////////////////////////////
// Creating wallets
//////////////////////////////////////////////////

#[test]
fn a_wallet_built_from_the_spec_seed_equals_the_cpp_file() {
    // The fixture was made by `POST /wallet/import/seed` with this seed and
    // scanHeight; building it here must land on the same document.
    let ours = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();
    let theirs = fixture_json("spec05-seed.json");
    assert_eq!(String::from_utf8(ours.to_json_bytes().unwrap().to_vec()).unwrap(), String::from_utf8(theirs).unwrap());
}

#[test]
fn a_view_wallet_built_from_the_view_key_equals_the_cpp_file() {
    let view = SecretKey::from_hex(SPEC_VIEW_SECRET).unwrap();
    let ours = Wallet::import_view_only(&view, SPEC_ADDRESS, 4_213_000).unwrap();
    assert_eq!(
        String::from_utf8(ours.to_json_bytes().unwrap().to_vec()).unwrap(),
        String::from_utf8(fixture_json("spec05-view.json")).unwrap()
    );
}

#[test]
fn import_from_keys_matches_import_from_the_seed() {
    let spend = SecretKey::from_hex(SPEC_SPEND_SECRET).unwrap();
    let view = SecretKey::from_hex(SPEC_VIEW_SECRET).unwrap();
    let from_keys = Wallet::import_from_keys(&spend, &view, 4_213_000).unwrap();
    let from_seed = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();
    assert_eq!(&*from_keys.to_json_bytes().unwrap(), &*from_seed.to_json_bytes().unwrap());
}

#[test]
fn subwallet_derivation_indexes_match_the_vectors() {
    let mut wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();

    // addSubWallet hands out 1, then 2, and bumps the counter each time
    for (index, secret, public) in SUBWALLET_VECTORS.iter().take(2) {
        let (address, handed_out) = wallet.add_sub_wallet().unwrap();
        assert_eq!(handed_out, *index);
        assert_eq!(wallet.sub_wallets.sub_wallet_index_counter, *index);
        let sub = wallet.sub_wallets.sub_wallet.last().unwrap();
        assert_eq!(sub.address, address);
        assert_eq!(&*sub.private_spend_key.to_hex(), *secret);
        assert_eq!(sub.public_spend_key.to_hex(), *public);
        assert_eq!(sub.wallet_index, *index);
    }

    // the addresses are the ones the C++ wallet produced for this seed
    let from_cpp = Wallet::open(fixture("spec05-subwallets.wallet"), PASSWORD).unwrap();
    for (index, ..) in SUBWALLET_VECTORS.iter().take(2) {
        let theirs = from_cpp.sub_wallets.sub_wallet.iter().find(|s| s.wallet_index == *index).unwrap();
        let ours = wallet.sub_wallets.sub_wallet.iter().find(|s| s.wallet_index == *index).unwrap();
        assert_eq!(ours.address, theirs.address);
        assert_eq!(ours.public_spend_key, theirs.public_spend_key);
    }

    // importSubWallet(index) jumps the counter to the index it was given
    let (_, secret5, public5) = SUBWALLET_VECTORS[2];
    wallet.import_sub_wallet_at_index(5, 4_213_000).unwrap();
    assert_eq!(wallet.sub_wallets.sub_wallet_index_counter, 5);
    let sub = wallet.sub_wallets.sub_wallet.last().unwrap();
    assert_eq!(&*sub.private_spend_key.to_hex(), secret5);
    assert_eq!(sub.public_spend_key.to_hex(), public5);
    // an index import stores walletIndex 0 and a scan height, like the C++
    assert_eq!(sub.wallet_index, 0);
    assert_eq!(sub.sync_start_height, 4_213_000);
    assert_eq!(sub.sync_start_timestamp, 0);

    // the same key twice is refused
    assert!(matches!(wallet.import_sub_wallet_at_index(5, 0), Err(WalletError::SubWalletAlreadyExists)));

    // an arbitrary key is index 0 and does not move the counter
    let extra = SecretKey::from_hex("2c7d88e6b43bb83f7215ecc744e73589d8d1a841e7ab8f26672c5490c1aa2b0a").unwrap();
    assert!(matches!(wallet.import_sub_wallet_from_key(&extra, 0), Err(WalletError::SubWalletAlreadyExists)));
    assert_eq!(wallet.sub_wallets.sub_wallet.len(), 4);
    assert_eq!(wallet.sub_wallets.public_spend_keys.len(), 4);

    // and the whole thing round trips
    let json = wallet.to_json_bytes().unwrap();
    assert_eq!(&*Wallet::from_json_bytes(&json).unwrap().to_json_bytes().unwrap(), &*json);
}

#[test]
fn a_new_wallet_saves_and_reopens() {
    let wallet = Wallet::create_new(4_213_000).unwrap();
    let path = temp_path("created.wallet");
    let _ = std::fs::remove_file(&path);

    wrkz_wallet::file::check_new_wallet_filename(&path).unwrap();
    wallet.save(&path, "hunter2").unwrap();
    assert!(matches!(wrkz_wallet::file::check_new_wallet_filename(&path), Err(WalletError::WalletFileAlreadyExists)));

    let reopened = Wallet::open(&path, "hunter2").unwrap();
    assert_eq!(reopened.primary_address(), wallet.primary_address());
    assert_eq!(&*reopened.to_json_bytes().unwrap(), &*wallet.to_json_bytes().unwrap());
    // a fresh wallet is deterministic: the view key comes from the spend key
    assert!(reopened.mnemonic_seed().is_some());
    // 4,213,000 is below the height today's timestamp implies, so it wins
    assert_eq!(reopened.wallet_synchronizer.start_height, 4_213_000);
    assert_eq!(reopened.min_initial_sync_start().0, 0);

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn saving_over_an_existing_wallet_replaces_it_atomically() {
    let path = temp_path("replaced.wallet");
    let first = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 1).unwrap();
    first.save(&path, PASSWORD).unwrap();

    let second = Wallet::create_new(0).unwrap();
    second.save(&path, PASSWORD).unwrap();

    let reopened = Wallet::open(&path, PASSWORD).unwrap();
    assert_eq!(reopened.primary_address(), second.primary_address());
    std::fs::remove_file(&path).unwrap();
}

//////////////////////////////////////////////////
// Size
//////////////////////////////////////////////////

#[test]
fn a_wallet_with_ten_thousand_inputs_opens_quickly() {
    let mut wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();

    let mut sub: SubWallet = wallet.sub_wallets.sub_wallet[0].clone();
    sub.unspent_inputs = (0..10_000u64)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&i.to_le_bytes());
            TransactionInput {
                amount: 1_000 + i,
                block_height: 4_000_000 + i,
                global_output_index: Some(i * 7),
                key: Hex32(bytes),
                key_image: Hex32(bytes),
                parent_transaction_hash: Hex32(bytes),
                private_ephemeral: Some(SecretKey::from_bytes(bytes)),
                spend_height: 0,
                transaction_index: i % 90,
                transaction_public_key: Hex32(bytes),
                unlock_time: 0,
            }
        })
        .collect();
    wallet.sub_wallets.sub_wallet[0] = sub;

    wallet.sub_wallets.transactions = (0..1_000u64)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&i.to_le_bytes());
            Transaction {
                block_height: 4_000_000 + i,
                fee: 8_000,
                hash: Hex32(bytes),
                is_coinbase_transaction: false,
                payment_id: String::new(),
                timestamp: 1_700_000_000 + i,
                transfers: vec![Transfer { amount: 1_000, public_key: Hex32::from_hex(SPEC_SPEND_PUBLIC).unwrap() }],
                unlock_time: 0,
            }
        })
        .collect();

    let path = temp_path("large.wallet");
    let started = Instant::now();
    wallet.save(&path, PASSWORD).unwrap();
    let saved = started.elapsed();

    let started = Instant::now();
    let reopened = Wallet::open(&path, PASSWORD).unwrap();
    let opened = started.elapsed();

    assert_eq!(reopened.sub_wallets.sub_wallet[0].unspent_inputs.len(), 10_000);
    assert_eq!(reopened.sub_wallets.transactions.len(), 1_000);
    assert_eq!(&*reopened.to_json_bytes().unwrap(), &*wallet.to_json_bytes().unwrap());

    let size = std::fs::metadata(&path).unwrap().len();
    println!("10,000 inputs: {size} bytes, saved in {saved:?}, opened in {opened:?}");
    // Generous: this is a debug build and both halves are dominated by the
    // 500,000 PBKDF2 rounds, which are the same for a one-input wallet.
    assert!(saved.as_secs() < 30, "saving took {saved:?}");
    assert!(opened.as_secs() < 30, "opening took {opened:?}");

    std::fs::remove_file(&path).unwrap();
}

//////////////////////////////////////////////////
// Interoperability, driven by hand
//////////////////////////////////////////////////

/// Write a wallet exercising the whole schema, for the C++ wallet to open.
///
/// This is the other half of the fixtures: those prove we read what the C++
/// writes, this proves it reads what we write. It needs a C++ build, so it is
/// ignored by default. Run it, then point the C++ `wrkz-wallet-api` at the file
/// and compare its `POST /export/json` with the `.json` written beside it:
///
/// ```text
/// WRKZ_INTEROP_WALLET=/tmp/interop.wallet cargo test -p wrkz-wallet -- --ignored interop
/// wrkz-wallet-api --no-console -r <pw> -p 18856 &
/// curl -H "X-API-KEY: <pw>" -d '{"filename":"/tmp/interop.wallet","password":"password"}' \
///      http://127.0.0.1:18856/wallet/open
/// curl -H "X-API-KEY: <pw>" -d '{"filename":"/tmp/interop.json"}' \
///      http://127.0.0.1:18856/export/json
/// ```
#[test]
#[ignore = "writes a file for a C++ wallet to open; needs a C++ build to finish"]
fn interop_write_a_wallet_for_the_cpp_wallet_to_open() {
    let path = PathBuf::from(
        std::env::var("WRKZ_INTEROP_WALLET")
            .unwrap_or_else(|_| temp_path("interop.wallet").to_string_lossy().into_owned()),
    );

    let mut wallet = Wallet::import_from_mnemonic(SPEC_MNEMONIC, 4_213_000).unwrap();
    wallet.add_sub_wallet().unwrap();

    let hash = Hex32::from_hex(&"33".repeat(32)).unwrap();
    let input = TransactionInput {
        amount: 1_234_567,
        block_height: 4_213_001,
        global_output_index: Some(3_808_773),
        key: Hex32::from_hex("22c90af32cfded17237a447122de92e736b59313f635bf43f69bacfad3756723").unwrap(),
        key_image: Hex32::from_hex("17578ae6fcb167f76bcdbeece316b383e13a164e2daca2ed2397c2aebd22b007").unwrap(),
        parent_transaction_hash: hash,
        private_ephemeral: Some(
            SecretKey::from_hex("f0b2033e6f607de659fa6545ca0498de1f3f55021caa129d803f4d787a84c50d").unwrap(),
        ),
        spend_height: 0,
        transaction_index: 0,
        transaction_public_key: Hex32::from_hex(&"cd".repeat(32)).unwrap(),
        unlock_time: 0,
    };
    wallet.sub_wallets.sub_wallet[0].unspent_inputs.push(input);
    wallet.sub_wallets.sub_wallet[0].unconfirmed_incoming_amounts.push(UnconfirmedInput {
        amount: 500,
        key: Hex32::from_hex(&"11".repeat(32)).unwrap(),
        parent_transaction_hash: hash,
    });
    wallet.sub_wallets.transactions.push(Transaction {
        block_height: 4_213_001,
        fee: 8_000,
        hash,
        is_coinbase_transaction: false,
        payment_id: "0102030405060708".to_string(),
        timestamp: 1_788_000_000,
        transfers: vec![Transfer { amount: 1_234_567, public_key: Hex32::from_hex(SPEC_SPEND_PUBLIC).unwrap() }],
        unlock_time: 0,
    });
    wallet.sub_wallets.tx_private_keys.push(TxPrivateKey {
        transaction_hash: hash,
        tx_private_key: SecretKey::from_hex("487a3668ed5bfd7175e832dc642e64f821222222222222222222222222222202")
            .unwrap(),
    });
    let status = &mut wallet.wallet_synchronizer.transaction_synchronizer_status;
    status.store_block_hash(Hex32::from_hex(&"aa".repeat(32)).unwrap(), 4_213_001);

    let _ = std::fs::remove_file(&path);
    wallet.save(&path, PASSWORD).unwrap();
    std::fs::write(path.with_extension("ours.json"), &*wallet.to_json_bytes().unwrap()).unwrap();

    // and it still opens here
    let reopened = Wallet::open(&path, PASSWORD).unwrap();
    assert_eq!(&*reopened.to_json_bytes().unwrap(), &*wallet.to_json_bytes().unwrap());
    println!("wrote {} for the C++ wallet to open with password {PASSWORD:?}", path.display());
}
