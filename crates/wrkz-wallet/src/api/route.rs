// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The route table and the middleware — `ApiDispatcher::setupRoutes`
//! (`ApiDispatcher.cpp:121`) and `ApiDispatcher::middleware` (`:501`).
//!
//! Routes are tried in registration order and the first whose method and path
//! match wins, which is what httplib does. A path matching nothing is **404**
//! with an empty body, and never reaches authentication — httplib answers a
//! missing route itself.

use wrkz_rpc::http::{Request, Response};
use wrkz_rpc::json::{Json, Obj};

use super::{handlers, pretty, ApiState};
use crate::file::WalletError;

////////////////////////
/* HANDLER PROTOCOL   */
////////////////////////

/// What a handler produces: `{SUCCESS, statusCode}` plus the content it set.
pub struct Outcome {
    pub status: u16,
    pub body: Option<Json>,
}

impl Outcome {
    /// `return {SUCCESS, code}` with no `set_content`.
    pub fn status(status: u16) -> Outcome {
        Outcome { status, body: None }
    }

    /// `res.set_content(j.dump(4) + "\n", …); return {SUCCESS, code}`.
    pub fn json(status: u16, body: Json) -> Outcome {
        Outcome { status, body: Some(body) }
    }

    /// The common `200` with a body.
    pub fn ok(body: impl Into<Json>) -> Outcome {
        Outcome::json(200, body.into())
    }
}

/// The three ways a handler stops early, matching the three the C++ middleware
/// distinguishes.
pub enum Abort {
    /// `catch (const json::exception &)`: **400**, *empty* body. This is what a
    /// missing or mistyped request parameter produces (`ApiDispatcher.cpp:596`).
    BadJson,
    /// `if (error)`: **400** with `{"errorCode", "errorMessage"}`.
    Error(WalletError),
    /// `return {SUCCESS, code}` on a failure path — this status, empty body.
    Status(u16),
}

impl From<WalletError> for Abort {
    fn from(e: WalletError) -> Self {
        Abort::Error(e)
    }
}

pub type HandlerResult = std::result::Result<Outcome, Abort>;

////////////////////////
/* ROUTE TABLE        */
////////////////////////

/// `WalletState` (`ApiDispatcher.h:20`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalletState {
    MustBeOpen,
    MustBeClosed,
    DoesntMatter,
}

/// One matched route, with the path parameters already extracted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    OpenWallet,
    KeyImportWallet,
    SeedImportWallet,
    ImportViewWallet,
    CreateWallet,
    CreateAddress,
    ImportAddress,
    ImportDeterministicAddress,
    ImportViewAddress,
    ValidateAddress,
    SendPreparedTransaction,
    PrepareBasicTransaction,
    SendBasicTransaction,
    PrepareAdvancedTransaction,
    SendAdvancedTransaction,
    SendSweepTransaction,
    SendSweepAllTransaction,
    ExportToJson,
    CloseWallet,
    DeleteAddress(String),
    DeletePreparedTransaction(String),
    SaveWallet,
    ResetWallet,
    SetNodeInfo,
    RefreshSync,
    GetNodeInfo,
    GetPrivateViewKey,
    GetSpendKeys(String),
    GetMnemonicSeed(String),
    GetStatus,
    GetAddresses,
    GetPrimaryAddress,
    CreateIntegratedAddress(String, String),
    GetTransactions,
    GetUnconfirmedTransactions,
    GetUnconfirmedTransactionsForAddress(String),
    /// The path segment as written; parsed by the handler so an overflow is the
    /// C++'s **400** rather than a 404.
    GetTransactionsFromHeight(String),
    GetTransactionsFromHeightToHeight(String, String),
    GetTransactionsFromHeightWithAddress(String, String),
    GetTransactionsFromHeightToHeightWithAddress(String, String, String),
    GetTxPrivateKey(String),
    GetTransactionDetails(String),
    GetTransactionsByPaymentId(String),
    GetTransactionsWithPaymentId,
    GetBalance,
    GetBalanceForAddress(String),
    GetBalances,
}

/// The middleware arguments a route was registered with.
#[derive(Clone, Copy, Debug)]
pub struct RouteSpec {
    pub state: WalletState,
    /// `viewWalletsAllowed` / `viewWalletsBanned`.
    pub view_permitted: bool,
    /// Whether the route changes the wallet.
    ///
    /// `false`: [`handlers::handle_read`] is handed the published
    /// [`WalletView`](super::WalletView) and nothing else, so the route never
    /// waits for the sync thread, a send or the daemon. `true`:
    /// [`handlers::handle_write`] runs it under the transaction lock, holds the
    /// working wallet only while it applies, and publishes a new view before it
    /// answers (see [`crate::api`], "Locking"). The C++'s `writeOp` is narrower:
    /// only opening and closing take its mutex exclusively.
    pub write: bool,
}

const fn spec(state: WalletState, view_permitted: bool, write: bool) -> RouteSpec {
    RouteSpec { state, view_permitted, write }
}

use WalletState::{DoesntMatter, MustBeClosed, MustBeOpen};

/// Match a request against the route table, in `setupRoutes` order.
pub fn match_route(method: &str, path: &str) -> Option<(Route, RouteSpec)> {
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let seg: Vec<&str> = if path == "/" { Vec::new() } else { seg };

    match method {
        "POST" => match seg.as_slice() {
            ["wallet", "open"] => Some((Route::OpenWallet, spec(MustBeClosed, true, true))),
            ["wallet", "import", "key"] => Some((Route::KeyImportWallet, spec(MustBeClosed, true, true))),
            ["wallet", "import", "seed"] => Some((Route::SeedImportWallet, spec(MustBeClosed, true, true))),
            ["wallet", "import", "view"] => Some((Route::ImportViewWallet, spec(MustBeClosed, true, true))),
            ["wallet", "create"] => Some((Route::CreateWallet, spec(MustBeClosed, true, true))),
            ["addresses", "create"] => Some((Route::CreateAddress, spec(MustBeOpen, false, true))),
            ["addresses", "import"] => Some((Route::ImportAddress, spec(MustBeOpen, false, true))),
            ["addresses", "import", "deterministic"] => {
                Some((Route::ImportDeterministicAddress, spec(MustBeOpen, false, true)))
            }
            ["addresses", "import", "view"] => Some((Route::ImportViewAddress, spec(MustBeOpen, true, true))),
            ["addresses", "validate"] => Some((Route::ValidateAddress, spec(DoesntMatter, true, false))),
            ["transactions", "send", "prepared"] => {
                Some((Route::SendPreparedTransaction, spec(MustBeOpen, false, true)))
            }
            ["transactions", "prepare", "basic"] => {
                Some((Route::PrepareBasicTransaction, spec(MustBeOpen, false, true)))
            }
            ["transactions", "send", "basic"] => Some((Route::SendBasicTransaction, spec(MustBeOpen, false, true))),
            ["transactions", "prepare", "advanced"] => {
                Some((Route::PrepareAdvancedTransaction, spec(MustBeOpen, false, true)))
            }
            ["transactions", "send", "advanced"] => {
                Some((Route::SendAdvancedTransaction, spec(MustBeOpen, false, true)))
            }
            ["transactions", "send", "sweep"] => Some((Route::SendSweepTransaction, spec(MustBeOpen, false, true))),
            ["transactions", "send", "sweep", "all"] => {
                Some((Route::SendSweepAllTransaction, spec(MustBeOpen, false, true)))
            }
            ["export", "json"] => Some((Route::ExportToJson, spec(MustBeOpen, true, false))),
            _ => None,
        },

        "DELETE" => match seg.as_slice() {
            ["wallet"] => Some((Route::CloseWallet, spec(MustBeOpen, true, true))),
            ["addresses", a] if is_address(a) => {
                Some((Route::DeleteAddress((*a).to_string()), spec(MustBeOpen, true, true)))
            }
            ["transactions", "prepared", h] if is_hash(h) => {
                Some((Route::DeletePreparedTransaction((*h).to_string()), spec(MustBeOpen, false, true)))
            }
            _ => None,
        },

        "PUT" => match seg.as_slice() {
            ["save"] => Some((Route::SaveWallet, spec(MustBeOpen, true, false))),
            ["reset"] => Some((Route::ResetWallet, spec(MustBeOpen, true, true))),
            ["node"] => Some((Route::SetNodeInfo, spec(MustBeOpen, true, true))),
            ["sync", "refresh"] => Some((Route::RefreshSync, spec(MustBeOpen, true, true))),
            _ => None,
        },

        "GET" => match seg.as_slice() {
            ["node"] => Some((Route::GetNodeInfo, spec(MustBeOpen, true, false))),
            ["keys"] => Some((Route::GetPrivateViewKey, spec(MustBeOpen, true, false))),
            ["keys", "mnemonic", a] if is_address(a) => {
                Some((Route::GetMnemonicSeed((*a).to_string()), spec(MustBeOpen, false, false)))
            }
            ["keys", a] if is_address(a) => {
                Some((Route::GetSpendKeys((*a).to_string()), spec(MustBeOpen, false, false)))
            }
            ["status"] => Some((Route::GetStatus, spec(MustBeOpen, true, false))),
            ["addresses"] => Some((Route::GetAddresses, spec(MustBeOpen, true, false))),
            ["addresses", "primary"] => Some((Route::GetPrimaryAddress, spec(MustBeOpen, true, false))),
            ["addresses", a, p] if is_address(a) && is_payment_id(p) => Some((
                Route::CreateIntegratedAddress((*a).to_string(), (*p).to_string()),
                spec(MustBeOpen, true, false),
            )),
            ["transactions"] => Some((Route::GetTransactions, spec(MustBeOpen, true, false))),
            ["transactions", "unconfirmed"] => Some((Route::GetUnconfirmedTransactions, spec(MustBeOpen, true, false))),
            ["transactions", "unconfirmed", a] if is_address(a) => {
                Some((Route::GetUnconfirmedTransactionsForAddress((*a).to_string()), spec(MustBeOpen, true, false)))
            }
            ["transactions", "address", a, s] if is_address(a) && is_digits(s) => Some((
                Route::GetTransactionsFromHeightWithAddress((*a).to_string(), (*s).to_string()),
                spec(MustBeOpen, true, false),
            )),
            ["transactions", "address", a, s, e] if is_address(a) && is_digits(s) && is_digits(e) => Some((
                Route::GetTransactionsFromHeightToHeightWithAddress(
                    (*a).to_string(),
                    (*s).to_string(),
                    (*e).to_string(),
                ),
                spec(MustBeOpen, true, false),
            )),
            ["transactions", "privatekey", h] if is_hash(h) => {
                Some((Route::GetTxPrivateKey((*h).to_string()), spec(MustBeOpen, false, false)))
            }
            ["transactions", "hash", h] if is_hash(h) => {
                Some((Route::GetTransactionDetails((*h).to_string()), spec(MustBeOpen, true, false)))
            }
            ["transactions", "paymentid", h] if is_hash(h) => {
                Some((Route::GetTransactionsByPaymentId((*h).to_string()), spec(MustBeOpen, true, false)))
            }
            ["transactions", "paymentid"] => Some((Route::GetTransactionsWithPaymentId, spec(MustBeOpen, true, false))),
            ["transactions", s] if is_digits(s) => {
                Some((Route::GetTransactionsFromHeight((*s).to_string()), spec(MustBeOpen, true, false)))
            }
            ["transactions", s, e] if is_digits(s) && is_digits(e) => Some((
                Route::GetTransactionsFromHeightToHeight((*s).to_string(), (*e).to_string()),
                spec(MustBeOpen, true, false),
            )),
            ["balance"] => Some((Route::GetBalance, spec(MustBeOpen, true, false))),
            ["balance", a] if is_address(a) => {
                Some((Route::GetBalanceForAddress((*a).to_string()), spec(MustBeOpen, true, false)))
            }
            ["balances"] => Some((Route::GetBalances, spec(MustBeOpen, true, false))),
            _ => None,
        },

        _ => None,
    }
}

/// `ApiConstants::addressRegex`: the prefix, then 94 alphanumerics.
fn is_address(s: &str) -> bool {
    let prefix = wrkz_primitives::constants::ADDRESS_PREFIX;
    s.len() == wrkz_primitives::constants::STANDARD_ADDRESS_LENGTH
        && s.starts_with(prefix)
        && s[prefix.len()..].bytes().all(|c| c.is_ascii_alphanumeric())
}

/// `ApiConstants::hashRegex`: 64 hex characters.
fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// `ApiConstants::paymentIDRegex`: 16 or 64 hex characters.
fn is_payment_id(s: &str) -> bool {
    (s.len() == 16 || s.len() == 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

////////////////////////
/* MIDDLEWARE         */
////////////////////////

/// `ApiDispatcher::middleware` plus the route lookup httplib does first.
///
/// Every response this returns is complete: status, headers and body.
pub fn dispatch(state: &ApiState, req: &Request) -> Response {
    if req.method == "OPTIONS" {
        return handle_options(state, req);
    }

    let Some((route, spec)) = match_route(&req.method, &req.path) else {
        // httplib's own 404, which never runs the middleware.
        return with_cors(state, Response::new(404));
    };

    // 1. The CORS header goes on before anything can return.
    let cors = |r: Response| with_cors(state, r);

    // 2. `checkAuthenticated`.
    let Some(key) = req.header("X-API-KEY") else {
        return cors(Response::new(401));
    };
    if !state.password_matches(key) {
        return cors(Response::new(401));
    }

    // The body is parsed but a parse failure is not an error: many routes take
    // no body at all (`ApiDispatcher.cpp:527`).
    let body = wrkz_rpc::json::parse(&req.body, wrkz_rpc::json::ParseLimits::default()).ok();
    let body = body.filter(Json::is_object).unwrap_or_else(|| Obj::new().build());

    // 3. The wallet state, from the published view: finding out whether a
    // wallet is open never waits for the sync thread.
    let view = state.view();
    match spec.state {
        WalletState::MustBeOpen if view.is_none() => return cors(Response::new(403)),
        WalletState::MustBeClosed if view.is_some() => return cors(Response::new(403)),
        _ => {}
    }

    // 4. The view wallet ban.
    if !spec.view_permitted && view.as_ref().is_some_and(|v| v.wallet().is_view_wallet()) {
        return cors(error_response(&WalletError::IllegalViewWalletOperation));
    }

    // 5. The handler: a read-only route gets the view and nothing else.
    let outcome = if spec.write {
        handlers::handle_write(state, req, &body, route)
    } else {
        handlers::handle_read(view.as_deref(), &body, route)
    };
    match outcome {
        Ok(Outcome { status, body: None }) => cors(Response::new(status)),
        Ok(Outcome { status, body: Some(json) }) => cors(json_response(status, &json)),
        Err(Abort::BadJson) => cors(Response::new(400)),
        Err(Abort::Status(status)) => cors(Response::new(status)),
        Err(Abort::Error(e)) => cors(error_response(&e)),
    }
}

/// `ApiDispatcher::handleOptions` (`ApiDispatcher.cpp:1926`). Not passed
/// through the middleware, so no `X-API-KEY` is required.
fn handle_options(state: &ApiState, req: &Request) -> Response {
    let supported = if state.config.cors_header.is_empty() { "" } else { "OPTIONS, GET, POST, PUT, DELETE" };

    let mut res = Response::new(200);
    if req.header("Access-Control-Request-Method").is_some() {
        res.set_header("Access-Control-Allow-Methods", supported);
    } else {
        res.set_header("Allow", supported);
    }

    if !state.config.cors_header.is_empty() {
        res.set_header("Access-Control-Allow-Origin", &state.config.cors_header);
        res.set_header("Access-Control-Allow-Headers", "Origin, X-Requested-With, Content-Type, Accept, X-API-KEY");
    }

    res
}

fn with_cors(state: &ApiState, mut res: Response) -> Response {
    if !state.config.cors_header.is_empty() {
        res.set_header("Access-Control-Allow-Origin", &state.config.cors_header);
    }
    res
}

/// `{"errorCode": …, "errorMessage": …}` at **400**, pretty printed.
pub fn error_response(error: &WalletError) -> Response {
    let mut o = Obj::new();
    o.set("errorCode", error.code()).set("errorMessage", error.to_string());
    json_response(400, &o.build())
}

fn json_response(status: u16, body: &Json) -> Response {
    let mut res = Response::new(status);
    res.body = pretty::body(body).into_bytes();
    res.set_header("Content-Type", "application/json");
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: &str =
        "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
    const HASH: &str = "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f";

    fn route(method: &str, path: &str) -> Option<Route> {
        match_route(method, path).map(|(r, _)| r)
    }

    #[test]
    fn the_route_table_is_the_cpp_route_table() {
        assert_eq!(route("POST", "/wallet/open"), Some(Route::OpenWallet));
        assert_eq!(route("POST", "/addresses/import/deterministic"), Some(Route::ImportDeterministicAddress));
        assert_eq!(route("POST", "/transactions/send/sweep/all"), Some(Route::SendSweepAllTransaction));
        assert_eq!(route("DELETE", "/wallet"), Some(Route::CloseWallet));
        assert_eq!(route("PUT", "/sync/refresh"), Some(Route::RefreshSync));
        assert_eq!(route("GET", "/balances"), Some(Route::GetBalances));
        assert_eq!(route("GET", "/transactions/paymentid"), Some(Route::GetTransactionsWithPaymentId));
    }

    #[test]
    fn address_routes_need_a_real_looking_address() {
        assert_eq!(route("GET", &format!("/balance/{ADDRESS}")), Some(Route::GetBalanceForAddress(ADDRESS.into())));
        // Too short, wrong prefix, and an integrated address are all 404 —
        // httplib never matches the route at all.
        assert_eq!(route("GET", "/balance/Wrkz123"), None);
        assert_eq!(route("GET", "/balance/TRTLv1..."), None);
    }

    #[test]
    fn a_height_route_is_only_digits() {
        assert_eq!(route("GET", "/transactions/100"), Some(Route::GetTransactionsFromHeight("100".into())));
        assert_eq!(
            route("GET", "/transactions/100/200"),
            Some(Route::GetTransactionsFromHeightToHeight("100".into(), "200".into()))
        );
        assert_eq!(route("GET", "/transactions/abc"), None);
    }

    #[test]
    fn the_specific_transaction_routes_win_over_the_height_ones() {
        assert_eq!(route("GET", "/transactions/unconfirmed"), Some(Route::GetUnconfirmedTransactions));
        assert_eq!(
            route("GET", &format!("/transactions/hash/{HASH}")),
            Some(Route::GetTransactionDetails(HASH.into()))
        );
        assert_eq!(
            route("GET", &format!("/transactions/privatekey/{HASH}")),
            Some(Route::GetTxPrivateKey(HASH.into()))
        );
        assert_eq!(
            route("GET", &format!("/transactions/paymentid/{HASH}")),
            Some(Route::GetTransactionsByPaymentId(HASH.into()))
        );
    }

    #[test]
    fn the_integrated_address_route_takes_both_payment_id_lengths() {
        assert!(route("GET", &format!("/addresses/{ADDRESS}/0102030405060708")).is_some());
        assert!(route("GET", &format!("/addresses/{ADDRESS}/{HASH}")).is_some());
        assert!(route("GET", &format!("/addresses/{ADDRESS}/xyz")).is_none());
        // The literal route wins, as it is registered first.
        assert_eq!(route("GET", "/addresses/primary"), Some(Route::GetPrimaryAddress));
    }

    #[test]
    fn routes_the_cpp_does_not_have_are_not_served() {
        for (method, path) in [
            ("GET", "/wallet"),
            ("POST", "/save"),
            ("GET", "/save"),
            ("POST", "/addresses"),
            ("GET", "/transactions/prepared"),
            ("PUT", "/wallet/open"),
            ("PATCH", "/balance"),
        ] {
            assert!(route(method, path).is_none(), "{method} {path} should not be routed");
        }
    }

    #[test]
    fn view_wallet_permissions_match_the_registrations() {
        fn banned(method: &str, path: &str) -> bool {
            !match_route(method, path).expect("routed").1.view_permitted
        }
        assert!(banned("POST", "/addresses/create"));
        assert!(banned("POST", "/addresses/import"));
        assert!(banned("POST", "/transactions/send/basic"));
        assert!(banned("POST", "/transactions/send/sweep"));
        assert!(banned("GET", &format!("/keys/{ADDRESS}")));
        assert!(banned("GET", &format!("/keys/mnemonic/{ADDRESS}")));
        assert!(banned("GET", &format!("/transactions/privatekey/{HASH}")));
        assert!(banned("DELETE", &format!("/transactions/prepared/{HASH}")));

        // A view wallet may still import a view address, read balances and
        // export, which is what `viewWalletsAllowed` says on those routes.
        assert!(!banned("POST", "/addresses/import/view"));
        assert!(!banned("GET", "/balance"));
        assert!(!banned("POST", "/export/json"));
        assert!(!banned("DELETE", &format!("/addresses/{ADDRESS}")));
    }

    #[test]
    fn only_the_routes_that_change_the_wallet_are_writes() {
        let write = |m: &str, p: &str| match_route(m, p).unwrap_or_else(|| panic!("{m} {p} routed")).1.write;

        // What an integration polls, and what only reads the container, is
        // answered from the published view.
        for (m, p) in [
            ("GET", "/status".to_string()),
            ("GET", "/balance".to_string()),
            ("GET", format!("/balance/{ADDRESS}")),
            ("GET", "/balances".to_string()),
            ("GET", "/addresses".to_string()),
            ("GET", "/addresses/primary".to_string()),
            ("GET", "/keys".to_string()),
            ("GET", format!("/keys/{ADDRESS}")),
            ("GET", format!("/keys/mnemonic/{ADDRESS}")),
            ("GET", "/node".to_string()),
            ("GET", "/transactions".to_string()),
            ("GET", "/transactions/unconfirmed".to_string()),
            ("GET", format!("/transactions/hash/{HASH}")),
            ("GET", format!("/transactions/privatekey/{HASH}")),
            ("PUT", "/save".to_string()),
            ("POST", "/export/json".to_string()),
            ("POST", "/addresses/validate".to_string()),
        ] {
            assert!(!write(m, &p), "{m} {p} only reads");
        }

        for (m, p) in [
            ("POST", "/wallet/open".to_string()),
            ("POST", "/wallet/create".to_string()),
            ("POST", "/addresses/create".to_string()),
            ("POST", "/addresses/import/view".to_string()),
            ("POST", "/transactions/prepare/basic".to_string()),
            ("POST", "/transactions/send/advanced".to_string()),
            ("POST", "/transactions/send/prepared".to_string()),
            ("POST", "/transactions/send/sweep/all".to_string()),
            ("DELETE", "/wallet".to_string()),
            ("DELETE", format!("/addresses/{ADDRESS}")),
            ("DELETE", format!("/transactions/prepared/{HASH}")),
            ("PUT", "/reset".to_string()),
            ("PUT", "/node".to_string()),
            ("PUT", "/sync/refresh".to_string()),
        ] {
            assert!(write(m, &p), "{m} {p} changes the wallet");
        }
    }

    #[test]
    fn the_wallet_state_of_each_route_matches_the_registration() {
        let state = |m: &str, p: &str| match_route(m, p).expect("routed").1.state;
        assert_eq!(state("POST", "/wallet/open"), WalletState::MustBeClosed);
        assert_eq!(state("POST", "/wallet/create"), WalletState::MustBeClosed);
        assert_eq!(state("POST", "/addresses/validate"), WalletState::DoesntMatter);
        assert_eq!(state("GET", "/status"), WalletState::MustBeOpen);
        assert_eq!(state("DELETE", "/wallet"), WalletState::MustBeOpen);
    }
}
