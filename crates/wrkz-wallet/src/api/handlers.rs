// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! One function per `ApiDispatcher` handler, in the order the header declares
//! them (`ApiDispatcher.h:110`).
//!
//! Each returns the C++'s `std::tuple<Error, uint16_t>` as an [`Outcome`] or an
//! [`Abort`], and sets the same body. A missing or mistyped request parameter
//! is [`Abort::BadJson`], which the middleware turns into a **400** with no
//! body — the C++'s `catch (const json::exception &)`.
//!
//! The handlers come in two kinds, and
//! [`RouteSpec::write`](super::route::RouteSpec::write) picks between them:
//!
//! - [`handle_read`] is handed the published [`WalletView`] and nothing else.
//!   It cannot reach the working wallet, the transaction lock or the daemon,
//!   so a read-only route never waits for any of them.
//! - [`handle_write`] gets the state. It takes the transaction lock first,
//!   holds the working wallet only while it applies the change, and publishes
//!   a new view before it returns. A send builds its transaction and relays it
//!   from the view, and holds the working wallet only to record the send.

use wrkz_rpc::http::Request;
use wrkz_rpc::json::{Json, Obj};

use super::route::{Abort, HandlerResult, Outcome, Route};
use super::{
    address_for_spend_key, delete_sub_wallet, import_view_sub_wallet, mnemonic_seed_for_address, spend_key_of,
    spend_keys, to_hex, transactions_range, ApiState, OpenWallet, WalletView, RPC_DEFAULT_PORT,
};
use crate::file::{Hex32, SecretKey, Transaction, Wallet, WalletError};
use crate::sync::Synchronizer;
use crate::transfer::{self, FeeType, PreparedTransaction, SendParams, SystemRandom};

use zeroize::Zeroizing;

////////////////////////
/* ENTRY              */
////////////////////////

/// Run a read-only route against the published view: `None` when no wallet
/// is open, which only a route that needs none is ever asked with.
pub fn handle_read(view: Option<&WalletView>, body: &Json, route: Route) -> HandlerResult {
    match &route {
        Route::ValidateAddress => return validate_address(body),
        Route::CreateIntegratedAddress(a, p) => return create_integrated_address(a, p),
        _ => {}
    }

    // The middleware has already answered 403 when none is open, so `None`
    // here can only be a close racing this request.
    let view = view.ok_or(Abort::Status(403))?;

    match route {
        // --- addresses --------------------------------------------------
        Route::GetAddresses => get_addresses(view),
        Route::GetPrimaryAddress => get_primary_address(view),

        // --- keys --------------------------------------------------------
        Route::GetPrivateViewKey => get_private_view_key(view),
        Route::GetSpendKeys(a) => get_spend_keys(view, &a),
        Route::GetMnemonicSeed(a) => get_mnemonic_seed(view, &a),

        // --- status, node, maintenance -----------------------------------
        Route::GetStatus => get_status(view),
        Route::GetNodeInfo => get_node_info(view),
        Route::SaveWallet => save_wallet(view),
        Route::ExportToJson => export_to_json(view, body),

        // --- balances -----------------------------------------------------
        Route::GetBalance => get_balance(view),
        Route::GetBalanceForAddress(a) => get_balance_for_address(view, &a),
        Route::GetBalances => get_balances(view),

        // --- transactions -------------------------------------------------
        Route::GetTransactions => get_transactions(view),
        Route::GetUnconfirmedTransactions => get_unconfirmed_transactions(view),
        Route::GetUnconfirmedTransactionsForAddress(a) => get_unconfirmed_transactions_for_address(view, &a),
        Route::GetTransactionsFromHeight(s) => get_transactions_from_height(view, &s),
        Route::GetTransactionsFromHeightToHeight(s, e) => get_transactions_from_height_to_height(view, &s, &e),
        Route::GetTransactionsFromHeightWithAddress(a, s) => get_transactions_from_height_with_address(view, &a, &s),
        Route::GetTransactionsFromHeightToHeightWithAddress(a, s, e) => {
            get_transactions_from_height_to_height_with_address(view, &a, &s, &e)
        }
        Route::GetTransactionDetails(h) => get_transaction_details(view, &h),
        Route::GetTransactionsByPaymentId(p) => get_transactions_by_payment_id(view, &p),
        Route::GetTransactionsWithPaymentId => get_transactions_with_payment_id(view),
        Route::GetTxPrivateKey(h) => get_tx_private_key(view, &h),

        // A route that changes the wallet reaching here is a route table
        // bug, and a 500 says so rather than serving it from a copy.
        _ => Err(Abort::Status(500)),
    }
}

/// Run a route that changes the wallet, under the transaction lock.
pub fn handle_write(state: &ApiState, _req: &Request, body: &Json, route: Route) -> HandlerResult {
    // `m_transactionMutex`, for the whole change.
    let _serialised = state.lock_transactions();

    match route {
        // --- wallet lifecycle ------------------------------------------
        Route::OpenWallet => open_wallet(state, body),
        Route::KeyImportWallet => key_import_wallet(state, body),
        Route::SeedImportWallet => seed_import_wallet(state, body),
        Route::ImportViewWallet => import_view_wallet(state, body),
        Route::CreateWallet => create_wallet(state, body),
        Route::CloseWallet => close_wallet(state),

        // --- addresses --------------------------------------------------
        Route::CreateAddress => create_address(state),
        Route::ImportAddress => import_address(state, body),
        Route::ImportDeterministicAddress => import_deterministic_address(state, body),
        Route::ImportViewAddress => import_view_address(state, body),
        Route::DeleteAddress(a) => delete_address(state, &a),

        // --- node and maintenance -----------------------------------------
        Route::SetNodeInfo => set_node_info(state, body),
        Route::RefreshSync => refresh_sync(state),
        Route::ResetWallet => reset_wallet(state, body),

        // --- sending --------------------------------------------------------
        Route::PrepareBasicTransaction => make_basic_transaction(state, body, false),
        Route::SendBasicTransaction => make_basic_transaction(state, body, true),
        Route::PrepareAdvancedTransaction => make_advanced_transaction(state, body, false),
        Route::SendAdvancedTransaction => make_advanced_transaction(state, body, true),
        Route::SendPreparedTransaction => send_prepared_transaction(state, body),
        Route::SendSweepTransaction => send_sweep_transaction(state, body, false),
        Route::SendSweepAllTransaction => send_sweep_transaction(state, body, true),
        Route::DeletePreparedTransaction(h) => delete_prepared_transaction(state, &h),

        _ => Err(Abort::Status(500)),
    }
}

////////////////////////
/* JSON ACCESSORS     */
////////////////////////

/// `getJsonValue<T>(body, key)` (`ApiDispatcher.h:27`): a missing key, or one
/// of the wrong type, throws — which the middleware answers with a bare 400.
fn get_str(body: &Json, key: &str) -> Result<String, Abort> {
    body.get(key).and_then(Json::as_str).map(str::to_string).ok_or(Abort::BadJson)
}

fn get_u64(body: &Json, key: &str) -> Result<u64, Abort> {
    body.get(key).and_then(Json::as_u64).ok_or(Abort::BadJson)
}

fn opt_u64(body: &Json, key: &str) -> Result<Option<u64>, Abort> {
    match body.get(key) {
        None => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or(Abort::BadJson),
    }
}

fn opt_str(body: &Json, key: &str) -> Result<Option<String>, Abort> {
    match body.get(key) {
        None => Ok(None),
        Some(v) => v.as_str().map(|s| Some(s.to_string())).ok_or(Abort::BadJson),
    }
}

fn opt_bool(body: &Json, key: &str) -> Result<Option<bool>, Abort> {
    match body.get(key) {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or(Abort::BadJson),
    }
}

/// `getJsonValue<uint16_t>`: out of range throws, like every other cast.
fn opt_u16(body: &Json, key: &str) -> Result<Option<u16>, Abort> {
    match opt_u64(body, key)? {
        None => Ok(None),
        Some(v) => u16::try_from(v).map(Some).map_err(|_| Abort::BadJson),
    }
}

/// `getJsonValue<float>`: a JSON number, integral or not.
fn opt_f64(body: &Json, key: &str) -> Result<Option<f64>, Abort> {
    match body.get(key) {
        None => Ok(None),
        Some(Json::F64(v)) => Ok(Some(*v)),
        Some(Json::U64(v)) => Ok(Some(*v as f64)),
        Some(Json::I64(v)) => Ok(Some(*v as f64)),
        Some(_) => Err(Abort::BadJson),
    }
}

/// `getJsonValue<Crypto::SecretKey>`: 64 hex characters, or the `from_json`
/// throws.
fn get_secret_key(body: &Json, key: &str) -> Result<SecretKey, Abort> {
    SecretKey::from_hex(&get_str(body, key)?).ok_or(Abort::BadJson)
}

/// `getJsonValue<Crypto::PublicKey>` / `<Crypto::Hash>`.
fn get_hex32(body: &Json, key: &str) -> Result<Hex32, Abort> {
    Hex32::from_hex(&get_str(body, key)?).ok_or(Abort::BadJson)
}

/// `ApiDispatcher::getDefaultWalletParams` (`ApiDispatcher.cpp:2038`).
///
/// `filename` and `password` are required; the daemon triple defaults to
/// `127.0.0.1:RPC_DEFAULT_PORT` without TLS.
fn default_wallet_params(body: &Json) -> Result<(String, u16, bool, String, Zeroizing<String>), Abort> {
    let filename = get_str(body, "filename")?;
    let password = Zeroizing::new(get_str(body, "password")?);
    let host = opt_str(body, "daemonHost")?.unwrap_or_else(|| "127.0.0.1".to_string());
    let port = opt_u16(body, "daemonPort")?.unwrap_or(RPC_DEFAULT_PORT);
    let ssl = opt_bool(body, "daemonSSL")?.unwrap_or(false);
    Ok((host, port, ssl, filename, password))
}

////////////////////////
/* WALLET ACCESS      */
////////////////////////

/// Run `f` against the working wallet, then publish what it left — whether it
/// succeeded or not, since a failure may still have changed something. The
/// route table has already answered 403 when none is open, so `None` here can
/// only be a race with a close.
fn with_wallet_mut<T>(state: &ApiState, f: impl FnOnce(&mut OpenWallet) -> Result<T, Abort>) -> Result<T, Abort> {
    let mut guard = state.wallet.write().map_err(|_| Abort::Status(500))?;
    let open = guard.as_mut().ok_or(Abort::Status(403))?;
    let result = f(open);
    state.publish(Some(open));
    result
}

/// [`with_wallet_mut`] for a change no read-only route can see — the prepared
/// transactions — which has nothing to publish.
fn with_working_wallet<T>(state: &ApiState, f: impl FnOnce(&mut OpenWallet) -> Result<T, Abort>) -> Result<T, Abort> {
    let mut guard = state.wallet.write().map_err(|_| Abort::Status(500))?;
    f(guard.as_mut().ok_or(Abort::Status(403))?)
}

/// The published view, for a change that starts from it.
fn published(state: &ApiState) -> Result<std::sync::Arc<WalletView>, Abort> {
    state.view().ok_or(Abort::Status(403))
}

////////////////////////
/* POST: LIFECYCLE    */
////////////////////////

/// Put a freshly opened/created/imported container in place of whatever was
/// there, with its synchronizer, and publish it.
fn install(
    state: &ApiState,
    wallet: Wallet,
    filename: String,
    password: Zeroizing<String>,
    host: String,
    port: u16,
    ssl: bool,
) -> Result<(), Abort> {
    // The daemon, the synchronizer and the first `/info`: shared with
    // `wrkz-service`, which opens its one container the same way. Built before
    // the working wallet is taken, so the sync thread is not held up by it.
    let open = state
        .open_container(wallet, filename, password, host, port, ssl)
        .map_err(|e| Abort::Error(WalletError::DaemonOffline(e)))?;

    let mut guard = state.wallet.write().map_err(|_| Abort::Status(500))?;
    *guard = Some(open);
    state.publish(guard.as_ref());
    Ok(())
}

/// `ApiDispatcher::openWallet` (`ApiDispatcher.cpp:642`).
fn open_wallet(state: &ApiState, body: &Json) -> HandlerResult {
    let (host, port, ssl, filename, password) = default_wallet_params(body)?;
    let wallet = Wallet::open(&filename, &password)?;
    install(state, wallet, filename, password, host, port, ssl)?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::keyImportWallet` (`:657`).
fn key_import_wallet(state: &ApiState, body: &Json) -> HandlerResult {
    let (host, port, ssl, filename, password) = default_wallet_params(body)?;
    let view = get_secret_key(body, "privateViewKey")?;
    let spend = get_secret_key(body, "privateSpendKey")?;
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);

    crate::file::check_new_wallet_filename(&filename)?;
    let wallet = Wallet::import_from_keys(&spend, &view, scan_height)?;
    wallet.save(&filename, &password)?;
    install(state, wallet, filename, password, host, port, ssl)?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::seedImportWallet` (`:692`).
fn seed_import_wallet(state: &ApiState, body: &Json) -> HandlerResult {
    let (host, port, ssl, filename, password) = default_wallet_params(body)?;
    let seed = Zeroizing::new(get_str(body, "mnemonicSeed")?);
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);

    crate::file::check_new_wallet_filename(&filename)?;
    let wallet = Wallet::import_from_mnemonic(&seed, scan_height)?;
    wallet.save(&filename, &password)?;
    install(state, wallet, filename, password, host, port, ssl)?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::importViewWallet` (`:720`).
fn import_view_wallet(state: &ApiState, body: &Json) -> HandlerResult {
    let (host, port, ssl, filename, password) = default_wallet_params(body)?;
    let address = get_str(body, "address")?;
    let view = get_secret_key(body, "privateViewKey")?;
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);

    crate::file::check_new_wallet_filename(&filename)?;
    let wallet = Wallet::import_view_only(&view, &address, scan_height)?;
    wallet.save(&filename, &password)?;
    install(state, wallet, filename, password, host, port, ssl)?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::createWallet` (`:757`).
fn create_wallet(state: &ApiState, body: &Json) -> HandlerResult {
    let (host, port, ssl, filename, password) = default_wallet_params(body)?;

    crate::file::check_new_wallet_filename(&filename)?;

    // `WalletBackend::createWallet` folds the daemon's height into the scan
    // start, so the daemon has to answer before the container exists.
    let daemon = state.daemon(&host, port, ssl).map_err(|e| Abort::Error(WalletError::DaemonOffline(e)))?;
    let network_height =
        crate::sync::SyncDaemon::info(&daemon).map(|i| i.network_height.saturating_sub(1)).unwrap_or(0);

    let wallet = Wallet::create_new(network_height)?;
    wallet.save(&filename, &password)?;
    install(state, wallet, filename, password, host, port, ssl)?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::closeWallet` (`:1220`): save and drop.
fn close_wallet(state: &ApiState) -> HandlerResult {
    let mut guard = state.wallet.write().map_err(|_| Abort::Status(500))?;
    if let Some(open) = guard.as_ref() {
        // The C++ destructor saves; a failure there is not reported either.
        let _ = open.save();
    }
    *guard = None;
    state.publish(None);
    Ok(Outcome::status(200))
}

////////////////////////
/* ADDRESSES          */
////////////////////////

/// `ApiDispatcher::createAddress` (`:772`).
fn create_address(state: &ApiState) -> HandlerResult {
    with_wallet_mut(state, |open| {
        let (address, wallet_index) = open.wallet_mut().add_sub_wallet()?;
        let (public_spend_key, private_spend_key, _) = spend_keys(open.wallet(), &address)?;

        let mut o = Obj::new();
        o.set("address", address)
            .set("privateSpendKey", private_spend_key.to_hex().to_string())
            .set("publicSpendKey", public_spend_key.to_hex())
            .set("walletIndex", wallet_index);
        Ok(Outcome::json(201, o.build()))
    })
}

/// `ApiDispatcher::importAddress` (`:786`).
fn import_address(state: &ApiState, body: &Json) -> HandlerResult {
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);
    let key = get_secret_key(body, "privateSpendKey")?;

    with_wallet_mut(state, |open| {
        let address = open.wallet_mut().import_sub_wallet_from_key(&key, scan_height)?;
        let mut o = Obj::new();
        o.set("address", address);
        Ok(Outcome::json(201, o.build()))
    })
}

/// `ApiDispatcher::importDeterministicAddress` (`:813`).
fn import_deterministic_address(state: &ApiState, body: &Json) -> HandlerResult {
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);
    let wallet_index = get_u64(body, "walletIndex")?;

    with_wallet_mut(state, |open| {
        let address = open.wallet_mut().import_sub_wallet_at_index(wallet_index, scan_height)?;
        let mut o = Obj::new();
        o.set("address", address);
        Ok(Outcome::json(201, o.build()))
    })
}

/// `ApiDispatcher::importViewAddress` (`:840`).
fn import_view_address(state: &ApiState, body: &Json) -> HandlerResult {
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);
    let public_spend_key = get_hex32(body, "publicSpendKey")?;

    with_wallet_mut(state, |open| {
        let address = import_view_sub_wallet(open.wallet_mut(), public_spend_key, scan_height)?;
        let mut o = Obj::new();
        o.set("address", address);
        Ok(Outcome::json(201, o.build()))
    })
}

/// `ApiDispatcher::validateAddress` (`:868`): integrated addresses allowed.
fn validate_address(body: &Json) -> HandlerResult {
    let address = get_str(body, "address")?;

    // `validateAddresses({address}, true)`, in the C++'s order.
    let parsed = crate::transfer::validate_address(&address, true)?;
    let is_integrated = wrkz_primitives::base58::is_integrated_address(&address);
    let payment_id = parsed.payment_id.clone().unwrap_or_default();
    let actual_address = wrkz_primitives::base58::standard_address(&parsed.spend_public_key, &parsed.view_public_key);

    let mut o = Obj::new();
    o.set("isIntegrated", is_integrated)
        .set("paymentID", payment_id)
        .set("actualAddress", actual_address)
        .set("publicSpendKey", to_hex(&parsed.spend_public_key))
        .set("publicViewKey", to_hex(&parsed.view_public_key));
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::deleteAddress` (`:1229`).
fn delete_address(state: &ApiState, address: &str) -> HandlerResult {
    with_wallet_mut(state, |open| {
        delete_sub_wallet(open.wallet_mut(), address)?;
        open.sync.refresh_key_image_owners();
        Ok(Outcome::status(200))
    })
}

/// `ApiDispatcher::getAddresses` (`:1511`).
fn get_addresses(view: &WalletView) -> HandlerResult {
    let addresses: Vec<Json> = view.wallet().addresses().map(|a| Json::Str(a.to_string())).collect();
    let mut o = Obj::new();
    o.set("addresses", Json::Array(addresses));
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::getPrimaryAddress` (`:1521`).
fn get_primary_address(view: &WalletView) -> HandlerResult {
    let mut o = Obj::new();
    o.set("address", view.wallet().primary_address().unwrap_or_default());
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::createIntegratedAddress` (`:1531`). Needs no wallet state
/// beyond being open — the address comes from the path.
fn create_integrated_address(address: &str, payment_id: &str) -> HandlerResult {
    let parsed = crate::transfer::validate_address(address, false)?;
    let integrated =
        wrkz_primitives::base58::integrated_address(&parsed.spend_public_key, &parsed.view_public_key, payment_id)
            .map_err(WalletError::InvalidAddress)?;

    let mut o = Obj::new();
    o.set("integratedAddress", integrated);
    Ok(Outcome::ok(o.build()))
}

////////////////////////
/* KEYS               */
////////////////////////

/// `ApiDispatcher::getPrivateViewKey` (`:1411`).
fn get_private_view_key(view: &WalletView) -> HandlerResult {
    let mut o = Obj::new();
    o.set("privateViewKey", view.wallet().private_view_key().to_hex().to_string());
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::getSpendKeys` (`:1422`).
fn get_spend_keys(view: &WalletView, address: &str) -> HandlerResult {
    let (public, private, index) = spend_keys(view.wallet(), address)?;
    let mut o = Obj::new();
    o.set("publicSpendKey", public.to_hex())
        .set("privateSpendKey", private.to_hex().to_string())
        .set("walletIndex", index);
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::getMnemonicSeed` (`:1447`).
fn get_mnemonic_seed(view: &WalletView, address: &str) -> HandlerResult {
    let seed = mnemonic_seed_for_address(view.wallet(), address)?;
    let mut o = Obj::new();
    o.set("mnemonicSeed", seed.to_string());
    Ok(Outcome::ok(o.build()))
}

////////////////////////
/* STATUS AND NODE    */
////////////////////////

/// `ApiDispatcher::getStatus` (`:1471`).
fn get_status(view: &WalletView) -> HandlerResult {
    let status = view.sync_status();
    let daemon_synced = status.local_daemon_block_count + 1 >= status.network_block_count;
    let wallet_synced = status.wallet_block_count + 10 >= status.network_block_count;
    let (gap_covered_to, gap_serves_from) = view.sync_gap.unwrap_or((0, 0));

    let mut o = Obj::new();
    o.set("walletBlockCount", status.wallet_block_count)
        .set("localDaemonBlockCount", status.local_daemon_block_count)
        .set("networkBlockCount", status.network_block_count)
        .set("isDaemonSynced", daemon_synced)
        .set("isWalletSynced", wallet_synced)
        .set("isOutOfSync", !(daemon_synced && wallet_synced))
        .set("peerCount", view.peer_count)
        .set("hashrate", view.hashrate)
        .set("isViewWallet", view.wallet().is_view_wallet())
        .set("subWalletCount", view.wallet().sub_wallets.sub_wallet.len() as u64)
        .set("daemonLiteStartHeight", view.lite_start_height())
        .set("isSyncStalledByLiteNode", view.sync_gap.is_some())
        .set("syncGapCoveredTo", gap_covered_to)
        .set("syncGapDaemonServesFrom", gap_serves_from);
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::getNodeInfo` (`:1385`).
///
/// `nodeFee` and `nodeAddress` are always `0` and `""`: `Nigel` reads them from
/// a `/fee` route no WrkzCoin daemon serves.
fn get_node_info(view: &WalletView) -> HandlerResult {
    let mut o = Obj::new();
    o.set("daemonHost", view.daemon_host.clone())
        .set("daemonPort", view.daemon_port)
        .set("daemonSSL", view.daemon_ssl)
        .set("nodeFee", 0u64)
        .set("nodeAddress", "");
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::setNodeInfo` (`:1330`): `WalletBackend::swapNode`.
fn set_node_info(state: &ApiState, body: &Json) -> HandlerResult {
    let host = get_str(body, "daemonHost")?;
    let port = opt_u16(body, "daemonPort")?.unwrap_or(RPC_DEFAULT_PORT);
    let ssl = opt_bool(body, "daemonSSL")?.unwrap_or(false);

    swap_node(state, host, port, ssl)?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::refreshSync` (`:1357`): swap to the node already in use,
/// which is how the C++ forces a reconnect.
fn refresh_sync(state: &ApiState) -> HandlerResult {
    let view = published(state)?;
    let (host, port, ssl) = (view.daemon_host.clone(), view.daemon_port, view.daemon_ssl);

    swap_node(state, host.clone(), port, ssl)?;

    let mut o = Obj::new();
    o.set("status", "OK")
        .set("message", "Wallet sync refresh triggered")
        .set("daemonHost", host)
        .set("daemonPort", port)
        .set("daemonSSL", ssl);
    Ok(Outcome::ok(o.build()))
}

/// Rebuild the synchronizer around a new daemon, keeping the container and its
/// sync progress (`WalletBackend::swapNode`).
fn swap_node(state: &ApiState, host: String, port: u16, ssl: bool) -> Result<(), Abort> {
    let daemon = state.daemon(&host, port, ssl).map_err(|e| Abort::Error(WalletError::DaemonOffline(e)))?;

    with_wallet_mut(state, |open| {
        let config = open.sync.config().clone();
        let wallet = std::mem::take(open.sync.wallet_mut());
        open.sync = Synchronizer::with_config(daemon, wallet, config);
        open.daemon_host = host;
        open.daemon_port = port;
        open.daemon_ssl = ssl;
        open.sync_gap = None;
        open.refresh_info();
        Ok(())
    })
}

/// `ApiDispatcher::saveWallet` (`:1275`), of the published view: the state
/// after the last completed change, never a sync step half applied.
fn save_wallet(view: &WalletView) -> HandlerResult {
    view.save()?;
    Ok(Outcome::status(200))
}

/// `ApiDispatcher::resetWallet` (`:1288`), including the lite-node refusal.
fn reset_wallet(state: &ApiState, body: &Json) -> HandlerResult {
    let scan_height = opt_u64(body, "scanHeight")?.unwrap_or(0);

    with_wallet_mut(state, |open| {
        let (lite_start_height, transactions_lost) = open.lite_rescan_impact(scan_height);
        if transactions_lost != 0 {
            return Err(Abort::Error(WalletError::LiteNodeCannotRescanThatLow(format!(
                "The daemon this wallet is connected to holds no block data below height {lite_start_height}. \
                 Rescanning from {scan_height} would start at {lite_start_height} instead, losing \
                 {transactions_lost} transaction(s) this wallet already holds from below that height. Connect a \
                 daemon holding the whole chain, or pass a scanHeight of at least {lite_start_height}."
            ))));
        }

        open.wallet_mut().reset(scan_height);
        open.sync.refresh_key_image_owners();
        open.sync_gap = None;
        open.save()?;
        Ok(Outcome::status(200))
    })
}

/// `ApiDispatcher::exportToJSON` (`:1194`): the wallet's plaintext JSON,
/// written where the caller asked.
fn export_to_json(view: &WalletView, body: &Json) -> HandlerResult {
    let path = get_str(body, "filename")?;

    let json = view.wallet().to_json_bytes()?;
    let mut contents = json.to_vec();
    contents.push(b'\n');
    std::fs::write(&path, &contents).map_err(WalletError::InvalidWalletFilename)?;
    Ok(Outcome::status(200))
}

////////////////////////
/* BALANCES           */
////////////////////////

/// `ApiDispatcher::getBalance` (`:1841`).
fn get_balance(view: &WalletView) -> HandlerResult {
    let (unlocked, locked) = view.total_balance();
    Ok(Outcome::ok(balance_object(unlocked, locked)))
}

/// `ApiDispatcher::getBalanceForAddress` (`:1854`).
fn get_balance_for_address(view: &WalletView, address: &str) -> HandlerResult {
    let (unlocked, locked) = view
        .wallet()
        .balance_for_address(address, view.network_height())
        .ok_or_else(|| WalletError::AddressNotInWallet(address.to_string()))?;
    Ok(Outcome::ok(balance_object(unlocked, locked)))
}

/// `ApiDispatcher::getBalances` (`:1874`): a **bare array**, not an object.
fn get_balances(view: &WalletView) -> HandlerResult {
    let entries: Vec<Json> = view
        .wallet()
        .address_balances(view.network_height())
        .into_iter()
        .map(|b| {
            let mut o = Obj::new();
            o.set("address", b.address).set("unlocked", b.unlocked).set("locked", b.locked);
            o.build()
        })
        .collect();
    Ok(Outcome::ok(Json::Array(entries)))
}

fn balance_object(unlocked: u64, locked: u64) -> Json {
    let mut o = Obj::new();
    o.set("unlocked", unlocked).set("locked", locked);
    o.build()
}

////////////////////////
/* TRANSACTIONS       */
////////////////////////

/// `WalletTypes::to_json(Transaction)` (`JsonSerialization.cpp:26`) with
/// `ApiDispatcher::publicKeysToAddresses` already applied: each transfer's
/// `publicKey` is replaced by the `address` that owns it (`:2094`).
fn transaction_json(wallet: &Wallet, tx: &Transaction) -> Json {
    let transfers: Vec<Json> = tx
        .transfers
        .iter()
        .map(|t| {
            let mut o = Obj::new();
            o.set("amount", t.amount).set("address", address_for_spend_key(wallet, &t.public_key));
            o.build()
        })
        .collect();

    let mut o = Obj::new();
    o.set("transfers", Json::Array(transfers))
        .set("hash", tx.hash.to_hex())
        .set("fee", tx.fee)
        .set("blockHeight", tx.block_height)
        .set("timestamp", tx.timestamp)
        .set("paymentID", tx.payment_id.clone())
        .set("unlockTime", tx.unlock_time)
        .set("isCoinbaseTransaction", tx.is_coinbase_transaction);
    o.build()
}

/// `{"transactions": [...]}`.
fn wrap_transactions(wallet: &Wallet, txs: &[&Transaction]) -> Json {
    let list: Vec<Json> = txs.iter().map(|t| transaction_json(wallet, t)).collect();
    let mut o = Obj::new();
    o.set("transactions", Json::Array(list));
    o.build()
}

/// `ApiDispatcher::getTransactions` (`:1555`).
fn get_transactions(view: &WalletView) -> HandlerResult {
    let txs: Vec<&Transaction> = view.wallet().transactions().iter().collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getUnconfirmedTransactions` (`:1567`).
fn get_unconfirmed_transactions(view: &WalletView) -> HandlerResult {
    let txs: Vec<&Transaction> = view.wallet().unconfirmed_transactions().iter().collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getUnconfirmedTransactionsForAddress` (`:1579`).
fn get_unconfirmed_transactions_for_address(view: &WalletView, address: &str) -> HandlerResult {
    let spend_key = spend_key_of(address)?;
    let txs: Vec<&Transaction> = view
        .wallet()
        .unconfirmed_transactions()
        .iter()
        .filter(|t| t.transfers.iter().any(|x| x.public_key == spend_key))
        .collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getTransactionsFromHeight` (`:1616`): a thousand blocks.
fn get_transactions_from_height(view: &WalletView, start: &str) -> HandlerResult {
    let start = parse_height(start)?;
    let txs = transactions_range(view.wallet(), start, start.saturating_add(1000));
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getTransactionsFromHeightToHeight` (`:1646`).
fn get_transactions_from_height_to_height(view: &WalletView, start: &str, end: &str) -> HandlerResult {
    let (start, end) = (parse_height(start)?, parse_height(end)?);
    if start >= end {
        return Err(Abort::Status(400));
    }
    let txs = transactions_range(view.wallet(), start, end);
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getTransactionsFromHeightWithAddress` (`:1694`).
fn get_transactions_from_height_with_address(view: &WalletView, address: &str, start: &str) -> HandlerResult {
    let start = parse_height(start)?;
    let spend_key = spend_key_of(address)?;
    let txs: Vec<&Transaction> = transactions_range(view.wallet(), start, start.saturating_add(1000))
        .into_iter()
        .filter(|t| t.transfers.iter().any(|x| x.public_key == spend_key))
        .collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getTransactionsFromHeightToHeightWithAddress` (`:1751`).
fn get_transactions_from_height_to_height_with_address(
    view: &WalletView,
    address: &str,
    start: &str,
    end: &str,
) -> HandlerResult {
    let (start, end) = (parse_height(start)?, parse_height(end)?);
    if start >= end {
        return Err(Abort::Status(400));
    }
    let spend_key = spend_key_of(address)?;
    let txs: Vec<&Transaction> = transactions_range(view.wallet(), start, end)
        .into_iter()
        .filter(|t| t.transfers.iter().any(|x| x.public_key == spend_key))
        .collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `std::stoull` on a path segment the regex already limited to digits: an
/// overflow is `{SUCCESS, 400}` — a 400 with no body.
fn parse_height(s: &str) -> Result<u64, Abort> {
    s.parse::<u64>().map_err(|_| Abort::Status(400))
}

/// `ApiDispatcher::getTransactionDetails` (`:1822`): `{"transaction": {...}}`,
/// or **404** when this wallet has never seen the hash.
fn get_transaction_details(view: &WalletView, hash: &str) -> HandlerResult {
    let hash = Hex32::from_hex(hash).ok_or(Abort::Status(404))?;
    let Some(tx) = view.wallet().transactions().iter().find(|t| t.hash == hash) else {
        return Err(Abort::Status(404));
    };
    let mut o = Obj::new();
    o.set("transaction", transaction_json(view.wallet(), tx));
    Ok(Outcome::ok(o.build()))
}

/// `ApiDispatcher::getTransactionsByPaymentId` (`:1867`).
fn get_transactions_by_payment_id(view: &WalletView, payment_id: &str) -> HandlerResult {
    let txs: Vec<&Transaction> = view.wallet().transactions().iter().filter(|t| t.payment_id == payment_id).collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getTransactionsWithPaymentId` (`:1891`): every transaction
/// that carries one at all.
fn get_transactions_with_payment_id(view: &WalletView) -> HandlerResult {
    let txs: Vec<&Transaction> = view.wallet().transactions().iter().filter(|t| !t.payment_id.is_empty()).collect();
    Ok(Outcome::ok(wrap_transactions(view.wallet(), &txs)))
}

/// `ApiDispatcher::getTxPrivateKey` (`:1900`).
fn get_tx_private_key(view: &WalletView, hash: &str) -> HandlerResult {
    let hash = Hex32::from_hex(hash).ok_or(Abort::Error(WalletError::HashInvalid))?;
    let key = view.wallet().tx_private_key(&hash).ok_or(WalletError::TxPrivateKeyNotFound)?;
    let mut o = Obj::new();
    o.set("transactionPrivateKey", key.to_hex().to_string());
    Ok(Outcome::ok(o.build()))
}

////////////////////////
/* SENDING            */
////////////////////////

/// `{transactionHash, fee, relayedToNetwork, mixin}` — the body every
/// prepare/send route answers with (`ApiDispatcher.cpp:987`).
fn send_response(prepared: &PreparedTransaction, relayed: bool) -> Json {
    let mut o = Obj::new();
    o.set("transactionHash", prepared.transaction_hash.to_hex())
        .set("fee", prepared.fee)
        .set("relayedToNetwork", relayed)
        .set("mixin", prepared.mixin);
    o.build()
}

/// `ApiDispatcher::makeBasicTransaction` (`:947`).
fn make_basic_transaction(state: &ApiState, body: &Json, send: bool) -> HandlerResult {
    let destination = get_str(body, "destination")?;
    let amount = get_u64(body, "amount")?;
    let payment_id = opt_str(body, "paymentID")?.unwrap_or_default();

    let view = published(state)?;
    let params = SendParams {
        pow_threads: pow_threads(),
        ..SendParams::basic(&destination, amount, &payment_id, view.network_height(), view.daemon_height())
    };
    build_and_maybe_send(state, &view, params, send)
}

/// `ApiDispatcher::makeAdvancedTransaction` (`:1005`).
fn make_advanced_transaction(state: &ApiState, body: &Json, send: bool) -> HandlerResult {
    let destinations_json = body.get("destinations").and_then(Json::as_array).ok_or(Abort::BadJson)?;
    let mut destinations = Vec::with_capacity(destinations_json.len());
    for d in destinations_json {
        destinations.push((get_str(d, "address")?, get_u64(d, "amount")?));
    }

    let mixin = opt_u64(body, "mixin")?;

    // `fee` wins over `feePerByte`; neither means the network minimum.
    let fee = match (opt_u64(body, "fee")?, opt_f64(body, "feePerByte")?) {
        (Some(f), _) => FeeType::FixedFee(f),
        (None, Some(rate)) => FeeType::FeePerByte(rate),
        (None, None) => FeeType::MinimumFee,
    };

    let source_addresses = match body.get("sourceAddresses") {
        None => Vec::new(),
        Some(Json::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(item.as_str().ok_or(Abort::BadJson)?.to_string());
            }
            out
        }
        Some(_) => return Err(Abort::BadJson),
    };

    let payment_id = opt_str(body, "paymentID")?.unwrap_or_default();
    let change_address = opt_str(body, "changeAddress")?.unwrap_or_default();
    let unlock_time = opt_u64(body, "unlockTime")?.unwrap_or(0);

    let extra_data = match opt_str(body, "extra")? {
        None => Vec::new(),
        Some(hex) => super::from_hex(&hex).ok_or(Abort::Error(WalletError::InvalidExtraData))?,
    };

    let view = published(state)?;
    let network_height = view.network_height();
    // The default ring is the tier at the daemon's own top block, where its
    // pool judges the transaction (`ApiDispatcher.cpp:1030`, C++ `0b58b035`).
    let daemon_height = view.daemon_height();
    let params = SendParams {
        destinations,
        mixin: mixin.unwrap_or_else(|| wrkz_primitives::mixins::mixin_allowable_range(daemon_height).default),
        fee,
        payment_id,
        addresses_to_take_from: source_addresses,
        change_address,
        unlock_time,
        extra_data,
        send_all: false,
        network_height,
        daemon_height,
        pow_threads: pow_threads(),
    };
    build_and_maybe_send(state, &view, params, send)
}

/// Build from the published view; relay when asked, otherwise remember the
/// transaction for `POST /transactions/send/prepared`.
///
/// Neither the proof of work nor either daemon call holds the working wallet,
/// so the sync thread and every read-only route carry on meanwhile. The caller
/// holds the transaction lock, so no other send builds from these inputs
/// before this one is recorded, and the view it builds from already holds
/// every earlier send.
fn build_and_maybe_send(state: &ApiState, view: &WalletView, params: SendParams, send: bool) -> HandlerResult {
    let mut random = SystemRandom;
    let prepared = transfer::prepare_transaction(view.wallet(), view.daemon(), &params, &mut random)?;

    if send {
        transfer::relay_prepared_transaction(view.daemon(), &prepared)?;
        record_sent(state, &prepared)?;
        return Ok(Outcome::json(201, send_response(&prepared, true)));
    }

    let body = send_response(&prepared, false);
    with_working_wallet(state, |open| {
        open.prepared.retain(|p| p.transaction_hash != prepared.transaction_hash);
        open.prepared.push(prepared);
        Ok(())
    })?;
    Ok(Outcome::json(201, body))
}

/// Record a relayed send in the working wallet — what `relay_and_store` does
/// after the relay (`Transfer.cpp:487-513`) — and publish it.
fn record_sent(state: &ApiState, prepared: &PreparedTransaction) -> Result<(), Abort> {
    with_wallet_mut(state, |open| {
        // In the moment between the relay and this, the sync thread may have
        // seen the transaction mined; recording it again would book the spend
        // twice.
        if !open.wallet().transactions().iter().any(|t| t.hash == prepared.transaction_hash) {
            transfer::apply_sent_transaction(open.wallet_mut(), prepared);
        }
        Ok(())
    })
}

/// `ApiDispatcher::sendPreparedTransaction` (`:918`).
fn send_prepared_transaction(state: &ApiState, body: &Json) -> HandlerResult {
    let hash = get_hex32(body, "transactionHash")?;

    let prepared = with_working_wallet(state, |open| {
        let Some(index) = open.prepared.iter().position(|p| p.transaction_hash == hash) else {
            return Err(Abort::Error(WalletError::PreparedTransactionNotFound));
        };
        Ok(open.prepared.remove(index))
    })?;

    // `SendTransaction::sendPreparedTransaction` (`Transfer.cpp:519`): every
    // input still spendable, then relay and record.
    let view = published(state)?;
    let now = crate::platform::now_seconds();
    if prepared.inputs.iter().any(|i| !view.wallet().have_spendable_input_at(&i.input, view.network_height(), now)) {
        return Err(Abort::Error(WalletError::PreparedTransactionExpired));
    }
    transfer::relay_prepared_transaction(view.daemon(), &prepared)?;
    record_sent(state, &prepared)?;

    let mut o = Obj::new();
    o.set("transactionHash", prepared.transaction_hash.to_hex());
    Ok(Outcome::json(201, o.build()))
}

/// `ApiDispatcher::deletePreparedTransaction` (`:1249`): **200** when it was
/// there, **404** when it was not — never an error body.
fn delete_prepared_transaction(state: &ApiState, hash: &str) -> HandlerResult {
    let Some(hash) = Hex32::from_hex(hash) else {
        return Ok(Outcome::status(404));
    };
    with_working_wallet(state, |open| {
        let before = open.prepared.len();
        open.prepared.retain(|p| p.transaction_hash != hash);
        Ok(Outcome::status(if open.prepared.len() < before { 200 } else { 404 }))
    })
}

/// `ApiDispatcher::sendSweepTransaction` (`:1106`) and `sendSweepAllTransaction`
/// (`:1152`): one **200** carrying a per-batch result array.
///
/// `sweepToAddress` records each batch before it builds the next, so the sweep
/// runs over a copy of the published wallet, and each batch is recorded in the
/// working wallet — and published — the moment it has been relayed.
fn send_sweep_transaction(state: &ApiState, body: &Json, all: bool) -> HandlerResult {
    let destination = get_str(body, "destination")?;
    let payment_id = opt_str(body, "paymentID")?.unwrap_or_default();
    let amount = if all { 0 } else { opt_u64(body, "amount")?.unwrap_or(0) };

    let view = published(state)?;
    let mut scratch = view.wallet().clone();
    let mut random = SystemRandom;
    let mut record = |prepared: &PreparedTransaction| {
        // It has been relayed; the only way to fail here is a poisoned lock.
        let _ = record_sent(state, prepared);
    };
    let results = transfer::sweep_to_address_reporting(
        &mut scratch,
        view.daemon(),
        &destination,
        &payment_id,
        amount,
        view.daemon_height(),
        &mut random,
        &mut record,
    );

    let list: Vec<Json> = results
        .iter()
        .map(|r| {
            let mut o = Obj::new();
            match r {
                Ok(hash) => {
                    o.set("success", true).set("transactionHash", hash.to_hex());
                }
                Err(e) => {
                    o.set("success", false).set("errorCode", e.code()).set("errorMessage", e.to_string());
                }
            }
            o.build()
        })
        .collect();

    let mut o = Obj::new();
    o.set("transactions", Json::Array(list));
    Ok(Outcome::ok(o.build()))
}

/// The C++ wallet API searches the transaction proof of work on every core.
fn pow_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}
