// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The JSON-RPC envelope and the method table.
//!
//! `JsonRpcServer::processRequest` (`jsonrpcserver/JsonRpcServer.cpp:57`) and
//! `PaymentServiceJsonRpcServer::processJsonRpcRequest`
//! (`walletservice/PaymentServiceJsonRpcServer.cpp:174`), which between them
//! decide everything about the wire shape:
//!
//! - **only `/json_rpc`** is routed; every other path is a bare 404;
//! - a body that is not JSON is **200** with a `-32700` "Parse error" envelope
//!   whose `id` is `null` — not a 400;
//! - `id` is echoed back exactly as it came, of whatever type, and is absent
//!   from the answer when it was absent from the request;
//! - `jsonrpc` is always `"2.0"` in the answer, and the request's own
//!   `jsonrpc` member is never looked at;
//! - the password is a **member of the request object**, not a header, and is
//!   checked before the method is even read;
//! - a handler failure is still **200**, with the error in the envelope
//!   ([`crate::errors`]).
//!
//! Notifications (a request with no `id`) get a full answer here, as they do
//! from the C++: it never treats one specially.

use wrkz_rpc::json::{self, Json, Obj};

use crate::errors::{AppError, ERR_INVALID_PASSWORD, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE_ERROR};

/// The request members this layer reads, once the body has parsed.
pub struct Envelope<'a> {
    /// `req("id")`, echoed verbatim. `None` when the request had none.
    pub id: Option<&'a Json>,
    /// `req("password")`.
    pub password: Option<&'a Json>,
    /// `req("method")`.
    pub method: Option<&'a Json>,
    /// `req("params")`, defaulted to an empty object as the C++ does
    /// (`PaymentServiceJsonRpcServer.cpp:227`).
    pub params: Json,
}

impl<'a> Envelope<'a> {
    pub fn of(req: &'a Json) -> Envelope<'a> {
        Envelope {
            id: req.get("id"),
            password: req.get("password"),
            method: req.get("method"),
            params: req.get("params").cloned().unwrap_or_else(|| Json::Object(Vec::new())),
        }
    }
}

/// `prepareJsonResponse` (`JsonRpcServer.cpp:107`): the `id` if there was one,
/// then `jsonrpc`.
fn prepared(id: Option<&Json>) -> Obj {
    let mut o = Obj::new();
    if let Some(id) = id {
        o.set("id", id.clone());
    }
    o.set("jsonrpc", "2.0");
    o
}

/// `fillJsonResponse` (`:194`).
pub fn result(id: Option<&Json>, value: Json) -> Json {
    let mut o = prepared(id);
    o.set("result", value);
    o.build()
}

/// `makeGenericErrorReponse` (`:141`): `code` and `message`, and no `data`.
pub fn generic_error(id: Option<&Json>, code: i64, message: &str) -> Json {
    let mut e = Obj::new();
    e.set("code", code);
    e.set("message", message);
    let mut o = prepared(id);
    o.set("error", e.build());
    o.build()
}

/// `makeErrorResponse` (`:118`): the `-32700` code every application error
/// carries, the category's message, and `data.application_code`.
pub fn app_error(id: Option<&Json>, e: &AppError) -> Json {
    if e.is_request_error() {
        // A `RequestSerializationError` escapes the handler and is caught by
        // `processJsonRpcRequest` as a generic error, so it has no `data`.
        return generic_error(id, ERR_PARSE_ERROR, &e.message);
    }
    let mut data = Obj::new();
    data.set("application_code", e.application_code);
    let mut err = Obj::new();
    err.set("code", ERR_PARSE_ERROR);
    err.set("message", e.message.as_str());
    err.set("data", data.build());
    let mut o = prepared(id);
    o.set("error", err.build());
    o.build()
}

/// `makeMethodNotFoundResponse` (`:170`).
pub fn method_not_found(id: Option<&Json>) -> Json {
    generic_error(id, ERR_METHOD_NOT_FOUND, "Method not found")
}

/// `makeInvalidPasswordResponse` (`:182`).
pub fn invalid_password(id: Option<&Json>) -> Json {
    generic_error(id, ERR_INVALID_PASSWORD, "Invalid or no rpc password")
}

/// `makeGenericErrorReponse(resp, "Invalid Request", -3600)` (`:207`).
pub fn invalid_request(id: Option<&Json>) -> Json {
    generic_error(id, ERR_INVALID_REQUEST, "Invalid Request")
}

/// `makeJsonParsingErrorResponse` (`:199`): a fresh envelope with `id` **null**
/// rather than absent, and no `id` echo, because nothing parsed.
pub fn parse_error() -> Json {
    let mut e = Obj::new();
    e.set("code", ERR_PARSE_ERROR);
    e.set("message", "Parse error");
    let mut o = Obj::new();
    o.set("id", Json::Null);
    o.set("jsonrpc", "2.0");
    o.set("error", e.build());
    o.build()
}

/// The limits a request body is parsed under. The C++ has none; these are this
/// port's, and the same ones the daemon's JSON-RPC uses.
pub fn parse_limits() -> json::ParseLimits {
    json::ParseLimits::default()
}
