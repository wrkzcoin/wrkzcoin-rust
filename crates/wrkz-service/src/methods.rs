// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The methods, in the order `PaymentServiceJsonRpcServer`'s constructor
//! registers them (`walletservice/PaymentServiceJsonRpcServer.cpp:32`).
//!
//! | Method | C++ handler | `WalletService.cpp` |
//! | --- | --- | --- |
//! | `save` | `handleSave` | `saveWalletNoThrow` `:975` |
//! | `export` | `handleExport` | `exportWallet` `:1002` |
//! | `reset` | `handleReset` | `resetWallet` `:1036` |
//! | `createAddress` | `handleCreateAddress` | `createAddress` `:1069` / `:1163` / `createTrackingAddress` `:1190` |
//! | `createAddressList` | `handleCreateAddressList` | `createAddressList` `:1104` |
//! | `deleteAddress` | `handleDeleteAddress` | `deleteAddress` `:1222` |
//! | `getSpendKeys` | `handleGetSpendKeys` | `getSpendkeys` `:1243` |
//! | `getBalance` | `handleGetBalance` | `getBalance` `:1266` / `:1288` |
//! | `getBlockHashes` | `handleGetBlockHashes` | `getBlockHashes` `:1308` |
//! | `getTransactionHashes` | `handleGetTransactionHashes` | `:1384` / `:1419` |
//! | `getTransactions` | `handleGetTransactions` | `:1452` / `:1487` |
//! | `getUnconfirmedTransactionHashes` | `handleGetUnconfirmed…` | `:1943` |
//! | `getTransaction` | `handleGetTransaction` | `getTransaction` `:1520` |
//! | `sendTransaction` | `handleSendTransaction` | `sendTransaction` `:1601` |
//! | `createDelayedTransaction` | `handleCreateDelayed…` | `:1738` |
//! | `getDelayedTransactionHashes` | `handleGetDelayed…` | `:1848` |
//! | `deleteDelayedTransaction` | `handleDeleteDelayed…` | `:1877` |
//! | `sendDelayedTransaction` | `handleSendDelayed…` | `:1911` |
//! | `getViewKey` | `handleGetViewKey` | `getViewKey` `:1329` |
//! | `getMnemonicSeed` | `handleGetMnemonicSeed` | `getMnemonicSeed` `:1346` |
//! | `getStatus` | `handleGetStatus` | `getStatus` `:1985` |
//! | `getAddresses` | `handleGetAddresses` | `getAddresses` `:1553` |
//! | `createIntegratedAddress` | `handleCreateIntegrated…` | `:2018` |
//! | `getFeeInfo`, `getNodeFeeInfo` | `handleNodeFeeInfo` | `getFeeInfo` `:2056` |
//!
//! A missing required parameter throws `RequestSerializationError` in the C++
//! serializer before the handler runs, which becomes the generic "Request
//! error" ([`AppError::request`]). The rules for which parameters are required,
//! and the two "these two are mutually exclusive" checks, are in
//! `PaymentServiceJsonRpcMessages.cpp` and are reproduced per method below.

use wrkz_rpc::json::{Json, Obj};
use wrkz_wallet::file::{SecretKey, Transaction, WalletError};
use wrkz_wallet::transfer::{self, FeeType, SendParams};

use crate::errors::{AppError, Result, Service, Wallet as WErr};
use crate::{ServiceState, MAX_BLOCK_COUNT};

/// One method.
pub type Handler = fn(&ServiceState, &Json) -> Result<Json>;

/// The table, in the C++ registration order.
pub const METHODS: &[(&str, Handler)] = &[
    ("save", save),
    ("export", export),
    ("reset", reset),
    ("createAddress", create_address),
    ("createAddressList", create_address_list),
    ("deleteAddress", delete_address),
    ("getSpendKeys", get_spend_keys),
    ("getBalance", get_balance),
    ("getBlockHashes", get_block_hashes),
    ("getTransactionHashes", get_transaction_hashes),
    ("getTransactions", get_transactions),
    ("getUnconfirmedTransactionHashes", get_unconfirmed_transaction_hashes),
    ("getTransaction", get_transaction),
    ("sendTransaction", send_transaction),
    ("createDelayedTransaction", create_delayed_transaction),
    ("getDelayedTransactionHashes", get_delayed_transaction_hashes),
    ("deleteDelayedTransaction", delete_delayed_transaction),
    ("sendDelayedTransaction", send_delayed_transaction),
    ("getViewKey", get_view_key),
    ("getMnemonicSeed", get_mnemonic_seed),
    ("getStatus", get_status),
    ("getAddresses", get_addresses),
    ("createIntegratedAddress", create_integrated_address),
    ("getFeeInfo", node_fee_info),
    ("getNodeFeeInfo", node_fee_info),
];

/// The handler for `method`, or `None` for "Method not found".
pub fn lookup(method: &str) -> Option<Handler> {
    METHODS.iter().find(|(name, _)| *name == method).map(|(_, h)| *h)
}

/// An empty result object, which is what a `Response` with no members
/// serializes to.
fn empty() -> Json {
    Json::Object(Vec::new())
}

// ---------------------------------------------------------------------------
// parameters
// ---------------------------------------------------------------------------

/// `serializer(x, "name")` for a string: false when the member is absent, and
/// a *type* mismatch is also just "absent", because the C++'s
/// `JsonInputValueSerializer` returns false rather than throwing.
fn opt_str(p: &Json, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn opt_u64(p: &Json, key: &str) -> Option<u64> {
    p.get(key).and_then(|v| v.as_u64())
}

fn opt_bool(p: &Json, key: &str) -> Option<bool> {
    p.get(key).and_then(|v| v.as_bool())
}

fn opt_f64(p: &Json, key: &str) -> Option<f64> {
    match p.get(key) {
        Some(Json::F64(f)) => Some(*f),
        Some(Json::U64(u)) => Some(*u as f64),
        Some(Json::I64(i)) => Some(*i as f64),
        _ => None,
    }
}

/// A member the serializer throws `RequestSerializationError` without.
fn need_str(p: &Json, key: &str) -> Result<String> {
    opt_str(p, key).ok_or_else(AppError::request)
}

fn need_u64(p: &Json, key: &str) -> Result<u64> {
    opt_u64(p, key).ok_or_else(AppError::request)
}

/// An array of strings; absent is an empty list, a non-array is "absent".
fn str_array(p: &Json, key: &str) -> Vec<String> {
    p.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

/// 64 hex characters into 32 bytes, or `WRONG_HASH_FORMAT` — `parseHash`
/// (`WalletService.cpp:322`).
fn parse_hash(text: &str) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    if text.len() != 64 || hex::decode_to_slice(text, &mut out).is_err() {
        return Err(AppError::service(Service::WrongHashFormat));
    }
    Ok(out)
}

/// 64 hex characters into a secret key, or `WRONG_KEY_FORMAT` — every
/// `podFromHex` on a key in `WalletService.cpp`.
fn parse_secret_key(text: &str) -> Result<SecretKey> {
    SecretKey::from_hex(text).ok_or_else(|| AppError::service(Service::WrongKeyFormat))
}

/// `checkPaymentId` (`WalletService.cpp:50`): 16 or 64 hex characters, either
/// case. `allow_short` is false where only a long payment id will do.
fn valid_payment_id(text: &str, allow_short: bool) -> bool {
    (text.len() == 64 || (allow_short && text.len() == 16)) && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// `validatePaymentId` (`WalletService.cpp:311`).
fn need_valid_payment_id(text: &str) -> Result<()> {
    if valid_payment_id(text, true) {
        Ok(())
    } else {
        Err(AppError::service(Service::WrongPaymentIdFormat))
    }
}

/// `validateAddresses` (`WalletService.cpp:432`): every one a standard address
/// of this network, or `BAD_ADDRESS`.
fn need_valid_addresses(addresses: &[String]) -> Result<()> {
    for address in addresses {
        transfer::validate_address(address, false).map_err(|_| AppError::wallet(WErr::BadAddress))?;
    }
    Ok(())
}

/// Map a wallet-core failure onto the `application_code` the C++ service would
/// have raised for the same condition.
///
/// The C++ raises `CryptoNote::error::WalletErrorCodes` from inside
/// `WalletGreen`, so the mapping is by *condition*, not by call site. Anything
/// with no counterpart there is `INTERNAL_WALLET_ERROR`, which is what
/// `WalletService` itself falls back to when it catches a plain
/// `std::exception`.
pub fn map_wallet_error(e: &WalletError) -> AppError {
    let code = match e {
        WalletError::WrongPassword => WErr::WrongPassword,
        WalletError::InvalidAddress(_) | WalletError::AddressIsIntegrated(_) => WErr::BadAddress,
        WalletError::AddressNotInWallet(_) => WErr::BadAddress,
        WalletError::SubWalletAlreadyExists => WErr::AddressAlreadyExists,
        WalletError::IllegalViewWalletOperation | WalletError::IllegalNonViewWalletOperation => WErr::TrackingMode,
        WalletError::InvalidPrivateKey | WalletError::InvalidPublicKey => WErr::KeyGenerationError,
        WalletError::KeysNotDeterministic => WErr::WrongParameters,
        WalletError::CannotDeletePrimaryAddress => WErr::WrongParameters,
        WalletError::NotEnoughBalance { .. } => WErr::WrongAmount,
        WalletError::WillOverflow => WErr::SumOverflow,
        WalletError::FeeTooSmall | WalletError::UnexpectedFee { .. } => WErr::FeeTooSmall,
        WalletError::NoDestinationsGiven => WErr::ZeroDestination,
        WalletError::AmountIsZero => WErr::WrongAmount,
        WalletError::MixinTooSmall { .. } => WErr::MixinBelowThreshold,
        WalletError::MixinTooBig { .. } | WalletError::FusionMixinTooLarge => WErr::MixinAboveThreshold,
        WalletError::PaymentIdWrongLength(_)
        | WalletError::PaymentIdInvalid
        | WalletError::IntegratedAddressPaymentIdInvalid
        | WalletError::ShortPaymentIdNeedsSingleDestination(_) => WErr::BadPaymentId,
        WalletError::ConflictingPaymentIds => WErr::ConflictingPaymentIds,
        WalletError::InvalidExtraData => WErr::BadTransactionExtra,
        WalletError::TooManyInputsToFitInBlock { .. } => WErr::TransactionSizeTooBig,
        WalletError::OutputDecomposition { .. } => WErr::ExcessiveOutputs,
        WalletError::TxPrivateKeyNotFound
        | WalletError::PreparedTransactionNotFound
        | WalletError::PreparedTransactionExpired => WErr::ObjectNotFound,
        WalletError::HashWrongLength | WalletError::HashInvalid => WErr::WrongParameters,
        WalletError::UnlockTimeTooSmall { .. } => WErr::WrongParameters,
        WalletError::FilenameNonExistent | WalletError::WalletFileAlreadyExists => WErr::WalletNotFound,
        // Everything the C++ has no code for: a daemon that will not answer, a
        // ring it could not build, a container that will not parse.
        _ => WErr::InternalWalletError,
    };
    // Keep the wallet core's own sentence rather than the C++'s one-liner
    // where it says more: "Bad address" tells an integration less than the
    // address that was bad. The code, which is what an integration branches
    // on, is the C++'s.
    AppError { application_code: code as i64, message: e.to_string() }
}

impl From<WalletError> for AppError {
    fn from(e: WalletError) -> AppError {
        map_wallet_error(&e)
    }
}

// ---------------------------------------------------------------------------
// the container
// ---------------------------------------------------------------------------

/// `handleSave` -> `WalletService::saveWalletNoThrow`.
fn save(state: &ServiceState, _p: &Json) -> Result<Json> {
    state.read().save()?;
    Ok(empty())
}

/// `handleExport` -> `WalletService::exportWallet`.
///
/// `fileName` is resolved against the *container's* directory, as the C++ does
/// (`walletPath.parent_path() / fileName`), so a bare name cannot be steered
/// somewhere else by a caller. What is written is this container's plaintext
/// JSON — the modern format — not a WalletGreen container.
fn export(state: &ServiceState, p: &Json) -> Result<Json> {
    let name = need_str(p, "fileName")?;
    let open = state.read();
    let dir = std::path::Path::new(&open.filename).parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let path = dir.join(name);
    let json = open.wallet().to_json_bytes().map_err(AppError::from)?;
    let mut contents = json.to_vec();
    contents.push(b'\n');
    std::fs::write(&path, &contents).map_err(|_| AppError::wallet(WErr::InternalWalletError))?;
    Ok(empty())
}

/// `handleReset` -> `WalletService::resetWallet`. `scanHeight` is optional and
/// defaults to 0, which rescans the whole chain.
fn reset(state: &ServiceState, p: &Json) -> Result<Json> {
    let scan_height = opt_u64(p, "scanHeight").unwrap_or(0);
    let mut open = state.write();
    // A lite daemon holding nothing below its start height would silently lose
    // transactions this container already has; `wrkz-wallet-api` refuses the
    // same way (`WalletBackend::liteRescanImpact`).
    let (lite_start, lost) = open.lite_rescan_impact(scan_height);
    if lost != 0 {
        return Err(AppError {
            application_code: WErr::WrongParameters as i64,
            message: format!(
                "The daemon this service is connected to holds no block data below height {lite_start}. \
                 Rescanning from {scan_height} would lose {lost} transaction(s) already held."
            ),
        });
    }
    open.wallet_mut().reset(scan_height);
    open.sync.refresh_key_image_owners();
    open.sync_gap = None;
    open.save()?;
    Ok(empty())
}

// ---------------------------------------------------------------------------
// addresses and keys
// ---------------------------------------------------------------------------

/// `handleCreateAddress` -> one of three `WalletService` calls, chosen the way
/// `PaymentServiceJsonRpcServer.cpp:253` chooses:
///
/// - neither key: a fresh deterministic subwallet;
/// - `spendSecretKey`: import that spend key;
/// - `spendPublicKey`: import it as a tracking (view-only) address.
///
/// Both keys together, or `newAddress` together with `scanHeight`, are
/// `RequestSerializationError` (`PaymentServiceJsonRpcMessages.cpp:74`).
fn create_address(state: &ServiceState, p: &Json) -> Result<Json> {
    let secret = opt_str(p, "spendSecretKey");
    let public = opt_str(p, "spendPublicKey");
    let new_address = opt_bool(p, "newAddress");
    let scan_height = opt_u64(p, "scanHeight");
    if secret.is_some() && public.is_some() {
        return Err(AppError::request());
    }
    if new_address.is_some() && scan_height.is_some() {
        return Err(AppError::request());
    }

    let mut open = state.write();
    let height = resolve_scan_height(&open, new_address.unwrap_or(false), scan_height.unwrap_or(0));
    let address = match (secret, public) {
        (None, None) => open.wallet_mut().add_sub_wallet().map(|(a, _)| a)?,
        (Some(secret), _) => {
            let key = parse_secret_key(&secret)?;
            open.wallet_mut().import_sub_wallet_from_key(&key, height)?
        }
        (_, Some(public)) => {
            let mut key = [0u8; 32];
            if public.len() != 64 || hex::decode_to_slice(&public, &mut key).is_err() {
                return Err(AppError::service(Service::WrongKeyFormat));
            }
            wrkz_wallet::api::import_view_sub_wallet(open.wallet_mut(), key.into(), height)?
        }
    };
    let mut o = Obj::new();
    o.set("address", address);
    Ok(o.build())
}

/// `newAddress` means "this key was made now", so scanning starts at the tip;
/// otherwise the caller's `scanHeight` (default 0) applies. `WalletGreen`
/// makes the same choice inside `createAddress`.
fn resolve_scan_height(open: &wrkz_wallet::api::OpenWallet, new_address: bool, scan_height: u64) -> u64 {
    if new_address {
        open.network_height()
    } else {
        scan_height
    }
}

/// `handleCreateAddressList` -> `WalletService::createAddressList`. A repeated
/// key is `DUPLICATE_KEY`, checked before any of them is imported, and the
/// same `newAddress`/`scanHeight` exclusion applies.
fn create_address_list(state: &ServiceState, p: &Json) -> Result<Json> {
    let keys = p.get("spendSecretKeys").and_then(|v| v.as_array()).ok_or_else(AppError::request)?;
    let new_address = opt_bool(p, "newAddress");
    let scan_height = opt_u64(p, "scanHeight");
    if new_address.is_some() && scan_height.is_some() {
        return Err(AppError::request());
    }

    let mut seen = std::collections::HashSet::new();
    let mut parsed = Vec::with_capacity(keys.len());
    for key in keys {
        let text = key.as_str().ok_or_else(AppError::request)?;
        if !seen.insert(text.to_string()) {
            return Err(AppError::service(Service::DuplicateKey));
        }
        parsed.push(parse_secret_key(text)?);
    }

    let mut open = state.write();
    let height = resolve_scan_height(&open, new_address.unwrap_or(false), scan_height.unwrap_or(0));
    let mut addresses = Vec::with_capacity(parsed.len());
    for key in &parsed {
        addresses.push(Json::from(open.wallet_mut().import_sub_wallet_from_key(key, height)?));
    }
    let mut o = Obj::new();
    o.set("addresses", Json::Array(addresses));
    Ok(o.build())
}

/// `handleDeleteAddress` -> `WalletService::deleteAddress`.
fn delete_address(state: &ServiceState, p: &Json) -> Result<Json> {
    let address = need_str(p, "address")?;
    let mut open = state.write();
    wrkz_wallet::api::delete_sub_wallet(open.wallet_mut(), &address)?;
    open.sync.refresh_key_image_owners();
    open.save()?;
    Ok(empty())
}

/// `handleGetSpendKeys` -> `WalletService::getSpendkeys`.
fn get_spend_keys(state: &ServiceState, p: &Json) -> Result<Json> {
    let address = need_str(p, "address")?;
    let open = state.read();
    let sub = open
        .wallet()
        .sub_wallets
        .sub_wallet
        .iter()
        .find(|s| s.address == address)
        .ok_or_else(|| AppError::wallet(WErr::WalletNotFound))?;
    let mut o = Obj::new();
    o.set("spendSecretKey", sub.private_spend_key.to_hex().to_string());
    o.set("spendPublicKey", sub.public_spend_key.to_hex());
    Ok(o.build())
}

/// `handleGetViewKey` -> `WalletService::getViewKey`.
fn get_view_key(state: &ServiceState, _p: &Json) -> Result<Json> {
    let open = state.read();
    let mut o = Obj::new();
    o.set("viewSecretKey", open.wallet().private_view_key().to_hex().to_string());
    Ok(o.build())
}

/// `handleGetMnemonicSeed` -> `WalletService::getMnemonicSeed`.
///
/// Only for an address whose view key is the deterministic one derived from
/// its spend key; anything else is `KEYS_NOT_DETERMINISTIC`, because a seed
/// that does not restore the whole container would be a trap.
fn get_mnemonic_seed(state: &ServiceState, p: &Json) -> Result<Json> {
    let address = need_str(p, "address")?;
    let open = state.read();
    let seed = wrkz_wallet::api::mnemonic_seed_for_address(open.wallet(), &address).map_err(|e| match e {
        // The one condition the service has its own code for.
        WalletError::KeysNotDeterministic => AppError::service(Service::KeysNotDeterministic),
        other => map_wallet_error(&other),
    })?;
    let mut o = Obj::new();
    o.set("mnemonicSeed", seed.to_string());
    Ok(o.build())
}

/// `handleGetAddresses` -> `WalletService::getAddresses`.
fn get_addresses(state: &ServiceState, _p: &Json) -> Result<Json> {
    let open = state.read();
    let list: Vec<Json> = open.wallet().addresses().map(Json::from).collect();
    let mut o = Obj::new();
    o.set("addresses", Json::Array(list));
    Ok(o.build())
}

/// `handleCreateIntegratedAddress` -> `WalletService::createIntegratedAddress`.
/// Both members are required; the address must be standard and the payment id
/// 16 or 64 hex characters.
fn create_integrated_address(_state: &ServiceState, p: &Json) -> Result<Json> {
    let address = need_str(p, "address")?;
    let payment_id = need_str(p, "paymentId")?;
    need_valid_addresses(std::slice::from_ref(&address))?;
    need_valid_payment_id(&payment_id)?;
    let parsed = transfer::validate_address(&address, false).map_err(AppError::from)?;
    let integrated =
        wrkz_primitives::base58::integrated_address(&parsed.spend_public_key, &parsed.view_public_key, &payment_id)
            .map_err(|_| AppError::service(Service::WrongPaymentIdFormat))?;
    let mut o = Obj::new();
    o.set("integratedAddress", integrated);
    Ok(o.build())
}

/// `handleNodeFeeInfo` -> `WalletService::getFeeInfo`, for both `getFeeInfo`
/// and `getNodeFeeInfo`.
///
/// Always `""` and `0`: the fee comes from `Nigel`, which reads a `/fee` route
/// no WrkzCoin daemon serves (`wrkz_rpc`, "Routes the C++ does not have"), so
/// the C++ answers the same.
fn node_fee_info(_state: &ServiceState, _p: &Json) -> Result<Json> {
    let mut o = Obj::new();
    o.set("address", "");
    o.set("amount", 0u64);
    Ok(o.build())
}

// ---------------------------------------------------------------------------
// balances and status
// ---------------------------------------------------------------------------

/// `handleGetBalance` -> `WalletService::getBalance`, per address when
/// `address` is a non-empty string and over the whole container otherwise.
fn get_balance(state: &ServiceState, p: &Json) -> Result<Json> {
    let address = opt_str(p, "address").unwrap_or_default();
    let open = state.read();
    let height = open.network_height();
    let (unlocked, locked) = if address.is_empty() {
        open.wallet().balance(height)
    } else {
        open.wallet().balance_for_address(&address, height).ok_or_else(|| AppError::wallet(WErr::BadAddress))?
    };
    let mut o = Obj::new();
    o.set("availableBalance", unlocked);
    o.set("lockedAmount", locked);
    Ok(o.build())
}

/// `handleGetStatus` -> `WalletService::getStatus`.
///
/// The C++ comment on the fields is worth keeping: `blockCount` is what the
/// *wallet* has synced, `knownBlockCount` the top block the daemon knows of,
/// and `localDaemonBlockCount` what the daemon itself has synced. All three
/// are counts.
fn get_status(state: &ServiceState, _p: &Json) -> Result<Json> {
    let open = state.read();
    let status = open.sync.sync_status();
    let last = open
        .wallet()
        .wallet_synchronizer
        .transaction_synchronizer_status
        .last_known_block_hashes
        .first()
        .map(|h| h.to_hex())
        .unwrap_or_default();
    let mut o = Obj::new();
    o.set("blockCount", status.wallet_block_count);
    o.set("knownBlockCount", status.network_block_count);
    o.set("localDaemonBlockCount", status.local_daemon_block_count);
    o.set("lastBlockHash", last);
    o.set("peerCount", open.peer_count);
    Ok(o.build())
}

// ---------------------------------------------------------------------------
// blocks and transactions
// ---------------------------------------------------------------------------

/// `handleGetBlockHashes` -> `WalletService::getBlockHashes`. Both members are
/// required.
///
/// The C++ reads them out of a WalletGreen container, which holds every block
/// hash it ever synced. A modern container holds the last 100 plus a sparse
/// checkpoint, so anything older is asked of the daemon — one
/// `/getwalletsyncdata` call per window, which is what the wallet's own sync
/// uses. `blockCount` is capped at [`MAX_BLOCK_COUNT`].
fn get_block_hashes(state: &ServiceState, p: &Json) -> Result<Json> {
    let first = need_u64(p, "firstBlockIndex")?;
    let count = need_u64(p, "blockCount")?;
    let hashes = block_hashes(state, first, count)?;
    let mut o = Obj::new();
    o.set("blockHashes", Json::Array(hashes.into_iter().map(Json::from).collect()));
    Ok(o.build())
}

/// The block hashes of `[first, first + count)` in height order, from the
/// container where it has them and from the daemon where it does not.
///
/// The container's read lock is held across the daemon calls, so a wide window
/// keeps the sync thread waiting for as long as they take. That is why
/// [`MAX_BLOCK_COUNT`] is small: the bound on the round trips is the bound on
/// the stall.
pub fn block_hashes(state: &ServiceState, first: u64, count: u64) -> Result<Vec<String>> {
    if count > MAX_BLOCK_COUNT {
        return Err(AppError {
            application_code: WErr::WrongParameters as i64,
            message: format!("blockCount {count} is above the maximum this service answers, {MAX_BLOCK_COUNT}"),
        });
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(count as usize);
    let mut at = first;
    let open = state.read();
    while (out.len() as u64) < count {
        let want = count - out.len() as u64;
        let data = fetch_block_hashes(&open, at, want)?;
        if data.is_empty() {
            break;
        }
        for (height, hash) in data {
            if height >= first + count {
                break;
            }
            out.push(hash);
            at = height + 1;
        }
    }
    Ok(out)
}

/// One `/getwalletsyncdata` window, reduced to `(height, hash)`.
fn fetch_block_hashes(
    open: &wrkz_wallet::api::OpenWallet,
    start_height: u64,
    block_count: u64,
) -> Result<Vec<(u64, String)>> {
    use wrkz_wallet::sync::SyncDaemon;
    let request = wrkz_wallet::daemon::SyncRequest {
        block_hash_checkpoints: Vec::new(),
        start_height,
        start_timestamp: 0,
        block_count,
        // Only the hashes are wanted, so ask for as little body as the daemon
        // will send: no coinbases, no key offsets, and empty blocks kept
        // because their hashes are part of the answer.
        skip_coinbase_transactions: true,
        skip_input_key_offsets: Some(true),
        skip_empty_blocks: Some(false),
        encoding: None,
        end_height: None,
    };
    let data = open
        .sync
        .daemon()
        .wallet_sync_data(&request)
        .map_err(|e| AppError { application_code: WErr::InternalWalletError as i64, message: e.to_string() })?;
    Ok(data.items.into_iter().map(|b| (b.block_height, b.block_hash)).collect())
}

/// `TransactionsInBlockInfoFilter` (`WalletService.cpp:216`): an optional
/// payment id and an optional address set, both of which a transaction has to
/// match to be reported.
struct Filter {
    payment_id: Option<String>,
    addresses: Vec<String>,
}

impl Filter {
    fn of(p: &Json) -> Result<Filter> {
        let addresses = str_array(p, "addresses");
        need_valid_addresses(&addresses)?;
        let payment_id = match opt_str(p, "paymentId") {
            Some(id) if !id.is_empty() => {
                need_valid_payment_id(&id)?;
                Some(id.to_lowercase())
            }
            _ => None,
        };
        Ok(Filter { payment_id, addresses })
    }

    /// `checkTransaction` (`:246`). The wallet core already stores the payment
    /// id in the form this service reports — a long one read from `tx_extra`,
    /// a short one decrypted with our view key when we were the receiver — so
    /// the comparison is against that, exactly as the C++ compares against
    /// `getPaymentIdString`.
    fn matches(&self, tx: &Transaction, addresses_of: &dyn Fn(&Transaction) -> Vec<String>) -> bool {
        if let Some(want) = &self.payment_id {
            if &tx.payment_id.to_lowercase() != want {
                return false;
            }
        }
        if self.addresses.is_empty() {
            return true;
        }
        addresses_of(tx).iter().any(|a| self.addresses.contains(a))
    }
}

/// The addresses a transaction touched, resolved from the spend keys its
/// transfers name.
fn transfer_addresses(wallet: &wrkz_wallet::file::Wallet, tx: &Transaction) -> Vec<String> {
    tx.transfers.iter().filter_map(|t| wallet.sub_wallet(&t.public_key).map(|s| s.address.clone())).collect()
}

/// `convertTransactionWithTransfersToTransactionRpcInfo` (`WalletService.cpp:377`).
fn transaction_json(wallet: &wrkz_wallet::file::Wallet, tx: &Transaction) -> Json {
    let transfers: Vec<Json> = tx
        .transfers
        .iter()
        .map(|t| {
            let address = wallet.sub_wallet(&t.public_key).map(|s| s.address.clone()).unwrap_or_default();
            let mut o = Obj::new();
            // `WalletTransferType::USUAL` is 0, and every transfer this
            // container stores is one: the C++'s DONATION and CHANGE types are
            // WalletGreen bookkeeping that the modern container does not keep.
            o.set("type", 0u64);
            o.set("address", address);
            o.set("amount", t.amount);
            o.build()
        })
        .collect();

    let mut o = Obj::new();
    // `WalletTransactionState::SUCCEEDED` is 0. A transaction this container
    // holds is either confirmed or still in the pool, and neither is FAILED,
    // CANCELLED or DELETED — those are WalletGreen states for a send it could
    // not relay, which this wallet never records in the first place.
    o.set("state", 0u64);
    o.set("transactionHash", tx.hash.to_hex());
    o.set("blockIndex", tx.block_height);
    o.set("timestamp", tx.timestamp);
    o.set("isBase", tx.is_coinbase_transaction);
    o.set("unlockTime", tx.unlock_time);
    o.set("amount", Json::I64(tx.total_amount()));
    o.set("fee", tx.fee);
    o.set("transfers", Json::Array(transfers));
    // The C++ hex-encodes the raw `tx_extra`. The modern container keeps the
    // payment id rather than the extra bytes, so a transaction it did not
    // build has no extra to report; one it built does, through the payment id.
    o.set("extra", "");
    o.set("paymentId", tx.payment_id.clone());
    o.build()
}

/// The block window a `getTransactions` / `getTransactionHashes` request names:
/// either `firstBlockIndex` or `blockHash`, never both and never neither
/// (`PaymentServiceJsonRpcMessages.cpp:178`: the two serializer calls must
/// disagree, so `a == b` throws).
fn window(state: &ServiceState, p: &Json) -> Result<(u64, u64)> {
    let by_hash = opt_str(p, "blockHash");
    let by_index = opt_u64(p, "firstBlockIndex");
    if by_hash.is_some() == by_index.is_some() {
        return Err(AppError::request());
    }
    let count = need_u64(p, "blockCount")?;
    let first = match (by_hash, by_index) {
        (Some(hash), _) => {
            parse_hash(&hash)?;
            height_of_block(state, &hash)?
        }
        (_, Some(index)) => index,
        _ => unreachable!("one of the two is set"),
    };
    Ok((first, count))
}

/// The height of a block this container knows, for the `blockHash` form.
///
/// `WalletService::getTransactions(blockHash, ...)` reads the container's own
/// block-hash list and throws `OBJECT_NOT_FOUND` when the hash is not in it;
/// so does this, over the hashes the modern container keeps.
fn height_of_block(state: &ServiceState, hash: &str) -> Result<u64> {
    let open = state.read();
    let status = &open.wallet().wallet_synchronizer.transaction_synchronizer_status;
    let wanted = hash.to_lowercase();
    // `last_known_block_hashes` is newest first and ends at
    // `last_known_block_height`.
    let position = status.last_known_block_hashes.iter().position(|h| h.to_hex() == wanted);
    match position {
        Some(back) => Ok(status.last_known_block_height.saturating_sub(back as u64)),
        None => Err(AppError::service(Service::ObjectNotFound)),
    }
}

/// `handleGetTransactionHashes` -> `WalletService::getRpcTransactionHashes`.
fn get_transaction_hashes(state: &ServiceState, p: &Json) -> Result<Json> {
    let items = blocks_with_transactions(state, p, false)?;
    let mut o = Obj::new();
    o.set("items", Json::Array(items));
    Ok(o.build())
}

/// `handleGetTransactions` -> `WalletService::getRpcTransactions`.
fn get_transactions(state: &ServiceState, p: &Json) -> Result<Json> {
    let items = blocks_with_transactions(state, p, true)?;
    let mut o = Obj::new();
    o.set("items", Json::Array(items));
    Ok(o.build())
}

/// Both of the above: the container's transactions inside the window, grouped
/// by block, each group carrying that block's hash.
///
/// `filterTransactions` (`WalletService.cpp:338`) keeps a block in the answer
/// when the *unfiltered* block had transactions, so a block whose only
/// transaction the filter rejected still appears, with an empty list. That
/// reads as a bug and is reproduced: a caller walking blocks counts on the
/// items lining up with the blocks it asked about.
fn blocks_with_transactions(state: &ServiceState, p: &Json, full: bool) -> Result<Vec<Json>> {
    let (first, count) = window(state, p)?;
    let filter = Filter::of(p)?;
    if count > MAX_BLOCK_COUNT {
        return Err(AppError {
            application_code: WErr::WrongParameters as i64,
            message: format!("blockCount {count} is above the maximum this service answers, {MAX_BLOCK_COUNT}"),
        });
    }

    // Group this container's transactions by height, inside the window.
    let mut by_height: std::collections::BTreeMap<u64, Vec<Transaction>> = std::collections::BTreeMap::new();
    {
        let open = state.read();
        let wallet = open.wallet();
        let addresses_of = |tx: &Transaction| transfer_addresses(wallet, tx);
        for tx in wallet.transactions() {
            if tx.block_height < first || tx.block_height >= first.saturating_add(count) {
                continue;
            }
            let entry = by_height.entry(tx.block_height).or_default();
            if filter.matches(tx, &addresses_of) {
                entry.push(tx.clone());
            }
        }
    }
    if by_height.is_empty() {
        // `getTransactions` throws OBJECT_NOT_FOUND on an empty result rather
        // than answering with an empty list (`WalletService.cpp:2100`).
        return Err(AppError::service(Service::ObjectNotFound));
    }

    // Only the blocks that carry something need a hash, so this is one daemon
    // round trip per such block rather than one per block in the window.
    let heights: Vec<u64> = by_height.keys().copied().collect();
    let mut items = Vec::with_capacity(heights.len());
    let open = state.read();
    let wallet = open.wallet();
    for height in heights {
        let hash = fetch_block_hashes(&open, height, 1)?
            .into_iter()
            .find(|(h, _)| *h == height)
            .map(|(_, hash)| hash)
            .unwrap_or_default();
        let transactions = by_height.remove(&height).unwrap_or_default();
        let mut o = Obj::new();
        o.set("blockHash", hash);
        if full {
            let list: Vec<Json> = transactions.iter().map(|tx| transaction_json(wallet, tx)).collect();
            o.set("transactions", Json::Array(list));
        } else {
            let list: Vec<Json> = transactions.iter().map(|tx| Json::from(tx.hash.to_hex())).collect();
            o.set("transactionHashes", Json::Array(list));
        }
        items.push(o.build());
    }
    Ok(items)
}

/// `handleGetTransaction` -> `WalletService::getTransaction`.
fn get_transaction(state: &ServiceState, p: &Json) -> Result<Json> {
    let text = need_str(p, "transactionHash")?;
    let hash = parse_hash(&text)?;
    let open = state.read();
    let wallet = open.wallet();
    let tx = wallet
        .transaction(&hash.into())
        .or_else(|| wallet.unconfirmed_transactions().iter().find(|t| t.hash.0 == hash))
        .ok_or_else(|| AppError::wallet(WErr::ObjectNotFound))?;
    let mut o = Obj::new();
    o.set("transaction", transaction_json(wallet, tx));
    Ok(o.build())
}

/// `handleGetUnconfirmedTransactionHashes` ->
/// `WalletService::getUnconfirmedTransactionHashes`. The address filter
/// applies; there is no payment id filter on this one.
fn get_unconfirmed_transaction_hashes(state: &ServiceState, p: &Json) -> Result<Json> {
    let addresses = str_array(p, "addresses");
    need_valid_addresses(&addresses)?;
    let open = state.read();
    let wallet = open.wallet();
    let hashes: Vec<Json> = wallet
        .unconfirmed_transactions()
        .iter()
        .filter(|tx| addresses.is_empty() || transfer_addresses(wallet, tx).iter().any(|a| addresses.contains(a)))
        .map(|tx| Json::from(tx.hash.to_hex()))
        .collect();
    let mut o = Obj::new();
    o.set("transactionHashes", Json::Array(hashes));
    Ok(o.build())
}

// ---------------------------------------------------------------------------
// sending
// ---------------------------------------------------------------------------

/// `SendTransaction::Request::serialize` and
/// `CreateDelayedTransaction::Request::serialize`
/// (`PaymentServiceJsonRpcMessages.cpp:296`, `:341`), which are the same
/// parameters under two names: the sources are `addresses` in both, and only
/// `transfers` is required.
fn send_params(state: &ServiceState, p: &Json) -> Result<SendParams> {
    let orders = p.get("transfers").and_then(|v| v.as_array()).ok_or_else(AppError::request)?;
    let mut destinations = Vec::with_capacity(orders.len());
    for order in orders {
        // `WalletRpcOrder::serialize` requires both members.
        let address = order.get("address").and_then(|v| v.as_str()).ok_or_else(AppError::request)?;
        let amount = order.get("amount").and_then(|v| v.as_u64()).ok_or_else(AppError::request)?;
        destinations.push((address.to_string(), amount));
    }

    // `fee` wins over `feePerByte`, and neither means the network minimum.
    let fee = match (opt_u64(p, "fee"), opt_f64(p, "feePerByte")) {
        (Some(fixed), _) => FeeType::FixedFee(fixed),
        (None, Some(rate)) => FeeType::FeePerByte(rate),
        (None, None) => FeeType::MinimumFee,
    };

    // `extra` and `paymentId` together is a RequestSerializationError.
    let extra = opt_str(p, "extra");
    let payment_id = opt_str(p, "paymentId");
    if extra.is_some() && payment_id.is_some() {
        return Err(AppError::request());
    }
    let extra_data = match &extra {
        Some(hex_text) => hex::decode(hex_text).map_err(|_| AppError::wallet(WErr::BadTransactionExtra))?,
        None => Vec::new(),
    };

    let open = state.read();
    let network_height = open.network_height();
    // `getDefaultMixin` reads the tier at the daemon's own top block, not at
    // the height peers claim (`WalletService.cpp:2065`, as C++ `0b58b035` has it).
    let daemon_height = open.daemon_height();
    let default_mixin = wrkz_primitives::mixins::mixin_allowable_range(daemon_height).default;
    let mixin = opt_u64(p, "anonymity").unwrap_or(default_mixin);

    let sources = str_array(p, "addresses");
    let change_address = opt_str(p, "changeAddress").unwrap_or_default();
    need_valid_addresses(&sources)?;
    if !change_address.is_empty() {
        need_valid_addresses(std::slice::from_ref(&change_address))?;
    }

    Ok(SendParams {
        destinations,
        mixin,
        fee,
        payment_id: payment_id.unwrap_or_default(),
        addresses_to_take_from: sources,
        change_address,
        unlock_time: opt_u64(p, "unlockTime").unwrap_or(0),
        extra_data,
        send_all: false,
        network_height,
        daemon_height,
        // The transaction proof of work is searched on every core the host has
        // when the fee escape does not apply; a service is not interactive and
        // has nothing better to do with them.
        pow_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
    })
}

/// `handleSendTransaction` -> `WalletService::sendTransaction`.
///
/// `anonymity` in the answer is the ring size actually built, which may be
/// below the one asked for when the denominations spent had too few outputs on
/// chain — the C++ logs a warning and reports the same.
fn send_transaction(state: &ServiceState, p: &Json) -> Result<Json> {
    let params = send_params(state, p)?;
    let mut open = state.write();
    let (wallet, daemon) = open.sync.split_for_transfer();
    let mut random = transfer::SystemRandom;
    let sent = transfer::send_transaction_advanced(wallet, daemon, &params, &mut random)?;
    // Relayed: `tx` fires now, whatever the save below makes of it
    // (`WalletService.cpp:899`).
    state.notify.sent(&open, &sent.transaction_hash);
    let mut o = Obj::new();
    o.set("transactionHash", sent.transaction_hash.to_hex());
    o.set("fee", sent.fee);
    o.set("anonymity", sent.mixin);
    // A service that crashed between the relay and a save would forget the
    // spend and could offer the same inputs again, so the container is written
    // before the answer goes out.
    open.save()?;
    Ok(o.build())
}

/// `handleCreateDelayedTransaction` -> `WalletService::createDelayedTransaction`.
///
/// Built but not relayed. The C++ keeps it as an uncommitted `WalletGreen`
/// transaction; here it joins the container's prepared transactions, which is
/// the same idea under the modern wallet's name, and
/// [`send_delayed_transaction`] re-checks the inputs before relaying it.
fn create_delayed_transaction(state: &ServiceState, p: &Json) -> Result<Json> {
    let params = send_params(state, p)?;
    let mut open = state.write();
    let (wallet, daemon) = open.sync.split_for_transfer();
    let mut random = transfer::SystemRandom;
    let prepared = transfer::prepare_transaction(wallet, daemon, &params, &mut random)?;
    let mut o = Obj::new();
    o.set("transactionHash", prepared.transaction_hash.to_hex());
    o.set("fee", prepared.fee);
    open.prepared.push(prepared);
    Ok(o.build())
}

/// `handleGetDelayedTransactionHashes` ->
/// `WalletService::getDelayedTransactionHashes`.
fn get_delayed_transaction_hashes(state: &ServiceState, _p: &Json) -> Result<Json> {
    let open = state.read();
    let hashes: Vec<Json> = open.prepared.iter().map(|t| Json::from(t.transaction_hash.to_hex())).collect();
    let mut o = Obj::new();
    o.set("transactionHashes", Json::Array(hashes));
    Ok(o.build())
}

/// `handleDeleteDelayedTransaction` ->
/// `WalletService::deleteDelayedTransaction`. An unknown hash is
/// `OBJECT_NOT_FOUND` from the *service* category.
fn delete_delayed_transaction(state: &ServiceState, p: &Json) -> Result<Json> {
    let text = need_str(p, "transactionHash")?;
    let hash = parse_hash(&text)?;
    let mut open = state.write();
    let before = open.prepared.len();
    open.prepared.retain(|t| t.transaction_hash.0 != hash);
    if open.prepared.len() == before {
        return Err(AppError::service(Service::ObjectNotFound));
    }
    Ok(empty())
}

/// `handleSendDelayedTransaction` -> `WalletService::sendDelayedTransaction`.
fn send_delayed_transaction(state: &ServiceState, p: &Json) -> Result<Json> {
    let text = need_str(p, "transactionHash")?;
    let hash = parse_hash(&text)?;
    let mut open = state.write();
    let Some(at) = open.prepared.iter().position(|t| t.transaction_hash.0 == hash) else {
        return Err(AppError::service(Service::ObjectNotFound));
    };
    let prepared = open.prepared.remove(at);
    let network_height = open.network_height();
    let (wallet, daemon) = open.sync.split_for_transfer();
    match transfer::send_prepared_transaction(wallet, daemon, prepared, network_height) {
        Ok(sent) => {
            // The delayed transaction leaves `CREATED` here, which is when the
            // C++ announces it (`WalletService.cpp:899`).
            state.notify.sent(&open, &sent.transaction_hash);
            open.save()?;
            Ok(empty())
        }
        Err(e) => Err(map_wallet_error(&e)),
    }
}
