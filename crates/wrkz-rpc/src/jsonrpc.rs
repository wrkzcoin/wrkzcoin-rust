// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `POST /json_rpc` and `GET /json_rpc` — the `jsonRpc` lambda of
//! `RpcServer::setupRoutes` (`RpcServer.cpp:194`) and the handlers it routes to.
//!
//! The order of operations is the C++'s, and it matters — with one deliberate
//! exception, step 0:
//!
//! 0. the body cap and the access token are checked **before** anything is
//!    parsed. The C++ parses first, so on a daemon that requires a token a
//!    caller without one could still make it parse two megabytes of JSON and
//!    read its own body back in the error. Here that caller gets the 401. A
//!    caller *with* the token sees exactly the C++ order below;
//! 1. the body is parsed before the rest of the middleware runs, so a body
//!    that is not JSON is a 400 before the rate limit is consulted;
//! 2. a missing `method` is a 400 with the C++'s wording;
//! 3. an unknown method is a bare **404** with no body — this is where the
//!    methods other CryptoNote daemons serve (`on_getblockhash`,
//!    `getblocksbyheights`, `getblockdetailsbyheight`, `getblock`, `getblocks`,
//!    `gettransaction`, `gettransactionspool`, `getcurrencyid`) end up, because
//!    `RpcServer.cpp` routes none of them;
//! 4. only then the middleware (token, rate limit, permissions, sync gate);
//! 5. and finally the request's `id` is copied into any 200 response that is a
//!    JSON object without one (`RpcServer.cpp:262`).
//!
//! Errors use `failJsonRpcRequest` (`RpcServer.cpp:855`): HTTP **200** with
//! `{"error":{"code":n,"message":s},"jsonrpc":"2.0"}`. The codes are the
//! daemon's own small set (−1 … −9), not the JSON-RPC 2.0 codes of
//! `src/rpc/JsonRpc.h`, which only the wallet service uses.

use crate::api::{ApiError, BlockHeaderInfo, NodeApi, SubmitOutcome};
use crate::handlers::{self, fail_request, hash_from_hex, str_of, u64_of};
use crate::http::{Request, Response};
use crate::json::{Json, Obj};
use crate::server::{Context, RpcMode};
use wrkz_primitives::base58::{self, Base58Error};
use wrkz_primitives::constants::{
    full_reward_zone, INTEGRATED_ADDRESS_LENGTH, INTEGRATED_ADDRESS_LENGTH_LONG, SHORT_PAYMENT_ID_LENGTH,
    STANDARD_ADDRESS_LENGTH,
};
use wrkz_primitives::tx::Input;

/// `WalletConfig::addressPrefix` (`src/config/WalletConfig.h:15`).
const ADDRESS_PREFIX: &str = "Wrkz";

/// `RpcServer::failJsonRpcRequest`: HTTP 200, an `error` object, no `result`.
pub fn fail(code: i64, message: &str) -> Response {
    let mut e = Obj::new();
    e.set("message", message).set("code", code);
    let mut j = Obj::new();
    j.set("jsonrpc", "2.0").set("error", e.build());
    Response::json(200, j.build().to_string())
}

/// A `result` envelope.
fn result(value: Json) -> Response {
    let mut j = Obj::new();
    j.set("jsonrpc", "2.0").set("result", value);
    Response::json(200, j.build().to_string())
}

/// The 503 `getlastblockheader` answers with while the chain reorganises
/// (`RpcServer.cpp:1822`).
fn reorganizing() -> Response {
    let mut e = Obj::new();
    e.set("code", -9i64).set("message", "Chain is reorganizing, please retry shortly");
    let mut j = Obj::new();
    j.set("jsonrpc", "2.0").set("error", e.build());
    Response::json(503, j.build().to_string())
}

/// What a method needs before it runs — the four arguments `router(...)` takes
/// in `RpcServer.cpp:198`.
#[derive(Clone, Copy)]
struct MethodRoute {
    permissions: RpcMode,
    sync_required: bool,
}

fn method_route(method: &str) -> Option<MethodRoute> {
    let std = MethodRoute { permissions: RpcMode::Standard, sync_required: false };
    let exp = MethodRoute { permissions: RpcMode::Explorer, sync_required: false };
    match method {
        "getblocktemplate"
        | "submitblock"
        | "getblockcount"
        | "getlastblockheader"
        | "getblockheaderbyhash"
        | "getblockheaderbyheight" => Some(std),
        "f_blocks_list_json"
        | "f_block_json"
        | "f_transaction_json"
        | "f_on_transactions_pool_json"
        | "f_transactions_by_payment_id_json" => Some(exp),
        _ => None,
    }
}

/// The whole `/json_rpc` route, from the raw request to the finished response.
pub fn handle(ctx: &Context, req: &Request, peer: &str) -> Response {
    // 0. The body cap and the token, before a byte of the body is parsed
    //    (see the module docs). Step 4 checks them again, which is two
    //    comparisons for an authorised caller and nothing it can observe.
    if let Some(early) = crate::server::gate_auth(ctx, req, peer) {
        return early;
    }

    // 1. `getJsonBody(req, res, true)`.
    let body = match crate::json::parse(
        &req.body,
        crate::json::ParseLimits { max_bytes: ctx.config.limits.max_body, max_depth: ctx.config.max_json_depth },
    ) {
        Ok(v) => v,
        Err(_) => return handlers::fail_json_body(&req.body),
    };

    // 2. `hasMember(*body, "method")`.
    let Some(method) = body.get("method") else {
        return fail_request(400, "Missing JSON parameter: 'method'");
    };
    let Some(method) = method.as_str() else {
        // `getStringFromJSON` on a non-string throws a type error, which the
        // outer lambda does not catch — the middleware does, as a 500.
        return fail_request(500, "Internal server error: JSON parameter 'method' must be a string");
    };

    // 3. unknown method: `res.status = 404` with no body, and no `id`.
    let Some(route) = method_route(method) else {
        return Response::new(404);
    };

    // 4. the middleware.
    if let Some(early) = crate::server::gate(ctx, req, peer, route.permissions, route.sync_required) {
        return early;
    }

    let api = ctx.api.as_ref();
    let mut res = match method {
        "getblocktemplate" => get_block_template(api, &body),
        "submitblock" => submit_block(api, &body),
        "getblockcount" => get_block_count(api),
        "getlastblockheader" => get_last_block_header(api),
        "getblockheaderbyhash" => get_block_header_by_hash(api, &body),
        "getblockheaderbyheight" => get_block_header_by_height(api, &body),
        "f_blocks_list_json" => get_blocks_by_height(api, &body),
        "f_block_json" => get_block_details_by_hash(api, &body),
        "f_transaction_json" => get_transaction_details_by_hash(api, &body),
        "f_on_transactions_pool_json" => get_transactions_in_pool(api),
        "f_transactions_by_payment_id_json" => {
            get_transaction_hashes_by_payment_id(api, &body, ctx.config.max_block_count)
        }
        _ => Response::new(404),
    };

    // 5. `RpcServer.cpp:262`: echo the request's `id` into any 200 object
    //    response that does not already carry one. JSON-RPC 2.0 requires it and
    //    xmrig drops an answer without it.
    if res.status == 200 && !res.body.is_empty() {
        if let Some(id) = body.get("id") {
            if let Ok(Json::Object(mut members)) = crate::json::parse(&res.body, crate::json::ParseLimits::default()) {
                if !members.iter().any(|(k, _)| k == "id") {
                    members.push(("id".into(), id.clone()));
                    res.body = Json::Object(members).to_string().into_bytes();
                }
            }
        }
    }
    res
}

// ---------------------------------------------------------------------------
// headers
// ---------------------------------------------------------------------------

/// The `block_header` object (`RpcServer.cpp:1800`, `:1878`, `:1938` — the same
/// thirteen members in all three handlers).
///
/// `depth` is `topHeight - height` in **`uint32_t` arithmetic**: for an
/// alternative block above the tip the C++ wraps, and so does this.
fn block_header_object(h: &BlockHeaderInfo, top_index: u64) -> Json {
    let mut o = Obj::new();
    o.set("major_version", h.major_version)
        .set("minor_version", h.minor_version)
        .set("timestamp", h.timestamp)
        .set("prev_hash", hex::encode(h.prev_hash))
        .set("nonce", h.nonce)
        .set("orphan_status", h.orphan_status)
        .set("height", h.height)
        .set("depth", u64::from((top_index as u32).wrapping_sub(h.height as u32)))
        .set("hash", hex::encode(h.hash))
        .set("difficulty", h.difficulty)
        .set("reward", h.reward)
        .set("num_txes", h.num_txes)
        .set("block_size", h.block_size);
    o.build()
}

fn header_result(h: &BlockHeaderInfo, top_index: u64) -> Response {
    let mut r = Obj::new();
    r.set("status", "OK").set("block_header", block_header_object(h, top_index));
    result(r.build())
}

/// `RpcServer::getBlockCount` (`RpcServer.cpp:1764`). `count` is a **count**.
fn get_block_count(api: &dyn NodeApi) -> Response {
    let mut r = Obj::new();
    r.set("status", "OK").set("count", api.top_index() + 1);
    result(r.build())
}

/// `RpcServer::getLastBlockHeader` (`RpcServer.cpp:1779`). `depth` is 0.
fn get_last_block_header(api: &dyn NodeApi) -> Response {
    let top = api.top_index();
    match api.block_header_by_index(top) {
        Ok(Some(h)) => header_result(&h, top),
        // The C++ throws "Top block hash is null during chain reorganization"
        // and answers -9 / 503.
        _ => reorganizing(),
    }
}

/// `RpcServer::getBlockHeaderByHash` (`RpcServer.cpp:1832`).
fn get_block_header_by_hash(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    let hash_str = handlers::param!(str_of(params, "hash"));
    let Some(hash) = hash_from_hex(hash_str) else {
        return fail(-1, "Block hash specified is not a valid hex!");
    };
    match api.block_header_by_hash(&hash) {
        Ok(Some(h)) => header_result(&h, api.top_index()),
        Ok(None) => fail(-5, "Block hash specified does not exist!"),
        Err(e) => internal(e),
    }
}

/// `RpcServer::getBlockHeaderByHeight` (`RpcServer.cpp:1902`). `height` is a
/// block **index**, and one above the tip is `-2`.
fn get_block_header_by_height(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    let height = handlers::param!(u64_of(params, "height"));
    let top = api.top_index();
    if height > top {
        return fail(
            -2,
            &format!(
                "Requested block header for a height that is higher than the current \
                 blockchain height! Current height: {top}"
            ),
        );
    }
    match api.block_header_by_index(height) {
        Ok(Some(h)) => header_result(&h, top),
        Ok(None) => fail(-5, "Block hash specified does not exist!"),
        Err(e) => internal(e),
    }
}

fn internal(e: ApiError) -> Response {
    fail_request(500, &format!("Internal server error: {e}"))
}

// ---------------------------------------------------------------------------
// mining
// ---------------------------------------------------------------------------

/// `validateAddresses({address}, false)` (`src/errors/ValidateParameters.cpp`),
/// reduced to the four outcomes `getblocktemplate` can reach, with the C++
/// messages verbatim. The stratum login checks its address with the same call
/// (`StratumServer.cpp:431`).
pub fn validate_address(address: &str) -> Result<(), String> {
    let len = address.chars().count();
    if len != STANDARD_ADDRESS_LENGTH && len != INTEGRATED_ADDRESS_LENGTH && len != INTEGRATED_ADDRESS_LENGTH_LONG {
        return Err(format!(
            "The address given is the wrong length. It should be {STANDARD_ADDRESS_LENGTH} chars, \
             {INTEGRATED_ADDRESS_LENGTH} chars, or {INTEGRATED_ADDRESS_LENGTH_LONG} chars, but it is {len} chars."
        ));
    }
    if !address.starts_with(ADDRESS_PREFIX) {
        return Err("The address does not have the correct prefix corresponding to this coin - it appears to be an \
                    address for another cryptocurrency."
            .into());
    }
    if base58::is_integrated_address(address) {
        // `integratedAddressesAllowed` is false for this route.
        return Err(format!(
            "The address given ({address}) is an integrated address, but integrated addresses aren't valid for this \
             parameter."
        ));
    }
    match base58::parse_address(address) {
        Ok(_) => Ok(()),
        Err(Base58Error::InvalidCharacter { .. }) => {
            Err("The address contains invalid characters, that are not in the base58 set.".into())
        }
        Err(_) => Err("The address given is not valid. Possibly invalid checksum. Most likely a typo.".into()),
    }
}

/// `std::search(blob.begin(), blob.end(), needle...)`: the offset of the first
/// occurrence, or `blob.len()` (`end()`) when there is none — which is what
/// makes the "not enough space" check below fire.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.len();
    }
    haystack.windows(needle.len()).position(|w| w == needle).unwrap_or(haystack.len())
}

/// `RpcServer::getBlockTemplate` (`RpcServer.cpp:1553`).
///
/// `extra_nonce` wins over `reserve_size` (the `else if` at `:1593`).
/// `reserved_offset` is the position of the coinbase transaction public key in
/// the blob, plus 32 for the key and 2 for the extra-nonce tag and its length
/// byte (`:1673`). `height` is a **count**.
fn get_block_template(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };

    let reserve: Vec<u8> = if params.has("extra_nonce") {
        let nonce = handlers::param!(str_of(params, "extra_nonce"));
        if nonce.len() > 255 * 2 {
            return fail(-3, "Too big extra nonce, maximum allowed is 255 bytes");
        }
        match hex::decode(nonce) {
            Ok(v) => v,
            Err(_) => return fail(-3, "Given extra nonce is not hex!"),
        }
    } else if params.has("reserve_size") {
        let size = handlers::param!(u64_of(params, "reserve_size"));
        if size > 255 {
            return fail(-3, "Too big reserved size, maximum allowed is 255");
        }
        vec![0u8; size as usize]
    } else {
        Vec::new()
    };
    let reserve_size = reserve.len();

    let address = handlers::param!(str_of(params, "wallet_address"));
    if let Err(message) = validate_address(address) {
        return fail(-4, &message);
    }

    let answer = match api.block_template(address, &reserve) {
        Ok(Ok(a)) => a,
        Ok(Err(message)) => return fail(-5, &format!("Failed to create block template: {message}")),
        Err(e) => return internal(e),
    };

    let mut reserved_offset: u64 = 0;
    if reserve_size > 0 {
        let at = find_subslice(&answer.blob, &answer.tx_public_key);
        reserved_offset = (at + std::mem::size_of::<wrkz_primitives::Hash>() + 2) as u64;
        if reserved_offset as usize + reserve_size > answer.blob.len() {
            return fail(-5, "Internal error: failed to create block template, not enough space for reserved bytes");
        }
    }

    let mut r = Obj::new();
    r.set("height", answer.height)
        .set("difficulty", answer.difficulty)
        .set("reserved_offset", reserved_offset)
        .set("blocktemplate_blob", hex::encode(&answer.blob))
        .set("status", "OK");
    result(r.build())
}

/// `RpcServer::submitBlock` (`RpcServer.cpp:1691`). `params` is an array of
/// exactly one hex blob.
fn submit_block(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params").and_then(Json::as_array) {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    if params.len() != 1 {
        return fail(-1, &format!("You must submit one and only one block blob! (Found {})", params.len()));
    }
    let Some(hex_blob) = params[0].as_str() else {
        return fail_request(500, "Internal server error: JSON parameter 'params[0]' must be a string");
    };
    let Ok(blob) = hex::decode(hex_blob) else {
        return fail(-6, "Submitted block blob is not hex!");
    };
    match api.submit_block(&blob) {
        Ok(SubmitOutcome::Added { .. }) => {
            let mut r = Obj::new();
            r.set("status", "OK");
            result(r.build())
        }
        Ok(SubmitOutcome::NotAccepted) => fail(-7, "Block not accepted"),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// explorer methods
// ---------------------------------------------------------------------------

/// `RpcServer::getBlocksByHeight`, routed as `f_blocks_list_json`
/// (`RpcServer.cpp:1975`): the 31 blocks from `height` down to `height - 30`.
fn get_blocks_by_height(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    let height = handlers::param!(u64_of(params, "height"));
    let top = api.top_index();
    if height > top {
        return fail(
            -2,
            &format!(
                "Requested block header for a height that is higher than the current \
                 blockchain height! Current height: {top}"
            ),
        );
    }
    let entries = match api.block_list(height) {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    let mut blocks = Vec::with_capacity(entries.len());
    for e in entries {
        let mut o = Obj::new();
        o.set("cumul_size", e.cumul_size)
            .set("difficulty", e.difficulty)
            .set("hash", hex::encode(e.hash))
            .set("height", e.height)
            .set("timestamp", e.timestamp)
            .set("tx_count", e.tx_count);
        blocks.push(o.build());
    }
    let mut r = Obj::new();
    r.set("status", "OK").set("blocks", Json::Array(blocks));
    result(r.build())
}

/// `RpcServer::getBlockDetailsByHash`, routed as `f_block_json`
/// (`RpcServer.cpp:2032`).
///
/// `hash` may be a 64-character hash **or** a decimal height, and in the second
/// case the C++ looks up `height - 1` — a long-standing off-by-one that a block
/// explorer built against it depends on, so it is reproduced.
fn get_block_details_by_hash(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    let hash_str = handlers::param!(str_of(params, "hash"));
    let top = api.top_index();

    let hash = if hash_str.len() == 64 {
        match hash_from_hex(hash_str) {
            Some(h) => h,
            None => return fail(-1, "Block hash specified is not a valid hex!"),
        }
    } else {
        let Ok(height) = hash_str.parse::<u64>() else {
            return fail(-1, "Block hash specified is not valid!");
        };
        match api.block_hash_by_index(height.wrapping_sub(1)) {
            Ok(Some(h)) => h,
            Ok(None) => {
                return fail(
                    -2,
                    &format!(
                        "Requested hash for a height that is higher than the current \
                         blockchain height! Current height: {top}"
                    ),
                )
            }
            Err(e) => return internal(e),
        }
    };

    let details = match api.block_details(&hash) {
        Ok(Some(d)) => d,
        // `getBlockByHash` throws for an unknown hash and the middleware turns
        // it into a 500 with the exception's text (`Core.cpp:1310`).
        Ok(None) => return fail_request(500, "Internal server error: Requested hash wasn't found in main blockchain"),
        Err(e) => return internal(e),
    };

    let mut txs = Vec::with_capacity(details.transactions.len());
    for t in &details.transactions {
        let mut o = Obj::new();
        o.set("hash", hex::encode(t.hash)).set("fee", t.fee).set("amount_out", t.amount_out).set("size", t.size);
        txs.push(o.build());
    }

    let effective_size_median = details.size_median.max(full_reward_zone(details.header.major_version) as u64);

    let h = &details.header;
    let mut b = Obj::new();
    b.set("major_version", h.major_version)
        .set("minor_version", h.minor_version)
        .set("timestamp", h.timestamp)
        .set("prev_hash", hex::encode(h.prev_hash))
        .set("nonce", h.nonce)
        .set("orphan_status", h.orphan_status)
        .set("height", h.height)
        .set("depth", u64::from((top as u32).wrapping_sub(h.height as u32)))
        .set("hash", hex::encode(h.hash))
        .set("difficulty", h.difficulty)
        .set("reward", h.reward)
        .set("blockSize", h.block_size)
        .set("transactionsCumulativeSize", details.transactions_cumulative_size)
        // A string, not a number: `std::to_string` at `RpcServer.cpp:2145`.
        .set("alreadyGeneratedCoins", details.already_generated_coins.to_string())
        .set("alreadyGeneratedTransactions", details.already_generated_transactions)
        .set("sizeMedian", details.size_median)
        .set("baseReward", details.base_reward)
        .set("penalty", Json::F64(details.penalty))
        .set("effectiveSizeMedian", effective_size_median)
        .set("transactions", Json::Array(txs))
        .set("totalFeeAmount", details.total_fee_amount);

    let mut r = Obj::new();
    r.set("status", "OK").set("block", b.build());
    result(r.build())
}

/// `RpcServer::getTransactionDetailsByHash`, routed as `f_transaction_json`
/// (`RpcServer.cpp:2171-2309`).
///
/// Three objects: `block`, the block the transaction was mined in in the short
/// shape `f_blocks_list_json` uses; `tx`, the transaction as it was serialised;
/// and `txDetails`, the derived values.
///
/// **Pool transactions are not visible here**, in this port as in the C++:
/// `Core::getTransactions` walks the chain segments and never the pool
/// (`Core.cpp:1356-1406`), so a transaction that has not been mined answers
/// `-1 "Block hash specified does not exist!"`. The two error messages say
/// "Block hash" for a transaction hash; that is the C++'s wording and a client
/// matching on it would break if it were corrected.
///
/// # One deliberate divergence
///
/// `result.tx.extra` here is the hex of the transaction's extra **bytes**. The
/// C++ prints `Common::podToHex(transaction.extra)` (`RpcServer.cpp:2273`),
/// and `extra` is a `std::vector<uint8_t>` while `podToHex` has no vector
/// overload — so it hexes `sizeof(std::vector)` bytes of the vector's own
/// control block and emits 48 characters of heap pointers. That is not a
/// format to be compatible with: it is uninitialised-adjacent process memory,
/// it leaks ASLR addresses to anyone who can reach the RPC, and it answers a
/// question about the transaction with a fact about the server. The field name
/// and the field's obvious meaning agree with each other and not with the C++,
/// so this emits what the name says.
fn get_transaction_details_by_hash(api: &dyn NodeApi, body: &Json) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    let hash_str = handlers::param!(str_of(params, "hash"));
    // `podFromHex` wants exactly 64 valid hex characters; the message says
    // "Block hash" for a transaction (`RpcServer.cpp:2183-2187`).
    let Some(hash) = hash_from_hex(hash_str) else {
        return fail(-1, "Block hash specified is not a valid hex!");
    };
    let details = match api.transaction_details(&hash) {
        Ok(Some(d)) => d,
        // `rawTXs.size() != 1` (`RpcServer.cpp:2200-2204`).
        Ok(None) => return fail(-1, "Block hash specified does not exist!"),
        Err(ApiError::Unsupported(_)) => return fail(-1, "Block hash specified does not exist!"),
        Err(e) => return internal(e),
    };

    let b = &details.block;
    let mut block = Obj::new();
    block
        .set("cumul_size", b.cumul_size)
        .set("difficulty", b.difficulty)
        .set("hash", hex::encode(b.hash))
        .set("height", b.height)
        .set("timestamp", b.timestamp)
        .set("tx_count", b.tx_count);

    // `vin` (`RpcServer.cpp:2228-2255`): `type` is the tag as a *string*, and a
    // key input's `key_offsets` are the **relative** offsets the transaction
    // carries, not absolute global indexes.
    let mut vin = Vec::with_capacity(details.transaction.prefix.inputs.len());
    for input in &details.transaction.prefix.inputs {
        let mut entry = Obj::new();
        match input {
            Input::Base { block_index } => {
                let mut value = Obj::new();
                value.set("height", *block_index);
                entry.set("type", "ff").set("value", value.build());
            }
            Input::Key { amount, key_offsets, key_image } => {
                let mut value = Obj::new();
                value
                    .set("k_image", hex::encode(key_image))
                    .set("amount", *amount)
                    .set("key_offsets", Json::Array(key_offsets.iter().map(|o| Json::U64(*o)).collect()));
                entry.set("type", "02").set("value", value.build());
            }
        }
        vin.push(entry.build());
    }

    // `vout` (`:2257-2271`). Every output on this chain is a key output; the
    // C++ `std::get<KeyOutput>` would throw for anything else, and the parser
    // this port uses cannot produce anything else.
    let mut vout = Vec::with_capacity(details.transaction.prefix.outputs.len());
    for output in &details.transaction.prefix.outputs {
        let mut data = Obj::new();
        data.set("key", hex::encode(output.key));
        let mut target = Obj::new();
        target.set("data", data.build()).set("type", "02");
        let mut entry = Obj::new();
        entry.set("amount", output.amount).set("target", target.build());
        vout.push(entry.build());
    }

    let mut tx = Obj::new();
    tx.set("extra", hex::encode(&details.transaction.prefix.extra))
        .set("publicKey", details.extra_public_key.map(hex::encode).unwrap_or_default())
        // `Common::toHex(txDetails.extra.nonce)`: the nonce field's bytes.
        .set("nonce", hex::encode(&details.extra_nonce))
        .set("unlock_time", details.transaction.prefix.unlock_time)
        .set("version", details.transaction.prefix.version)
        .set("vin", Json::Array(vin))
        .set("vout", Json::Array(vout));

    let mut tx_details = Obj::new();
    tx_details
        .set("hash", hex::encode(details.hash))
        .set("amount_out", details.amount_out)
        .set("fee", details.fee)
        .set("mixin", details.mixin)
        .set("paymentId", details.payment_id.as_str())
        .set("paymentIdEncrypted", details.payment_id_encrypted)
        .set("size", details.size);

    let mut r = Obj::new();
    r.set("status", "OK").set("block", block.build()).set("tx", tx.build()).set("txDetails", tx_details.build());
    result(r.build())
}

/// `RpcServer::getTransactionsInPool`, routed as `f_on_transactions_pool_json`
/// (`RpcServer.cpp:2298`).
fn get_transactions_in_pool(api: &dyn NodeApi) -> Response {
    let entries = match api.pool_transactions() {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    let mut txs = Vec::with_capacity(entries.len());
    for t in entries {
        let mut o = Obj::new();
        o.set("hash", hex::encode(t.hash)).set("fee", t.fee).set("amount_out", t.amount_out).set("size", t.size);
        txs.push(o.build());
    }
    let mut r = Obj::new();
    r.set("status", "OK").set("transactions", Json::Array(txs));
    result(r.build())
}

/// `RpcServer::getTransactionHashesByPaymentId`, routed as
/// `f_transactions_by_payment_id_json` (`RpcServer.cpp:2341`).
fn get_transaction_hashes_by_payment_id(api: &dyn NodeApi, body: &Json, max_hashes: u64) -> Response {
    let params = match body.get("params") {
        Some(p) => p,
        None => return fail_request(400, "Missing JSON parameter: 'params'"),
    };
    let payment_id_str = handlers::param!(str_of(params, "paymentId"));
    if payment_id_str.len() == SHORT_PAYMENT_ID_LENGTH {
        return fail(
            -1,
            "Short payment IDs are encrypted to their receiver and cannot be looked up. \
             Only the sender and the receiver can read one.",
        );
    }
    let Some(payment_id) = hash_from_hex(payment_id_str) else {
        return fail(-1, "Payment ID specified is not 64 valid hex characters!");
    };
    match api.transaction_hashes_by_payment_id(&payment_id) {
        Ok(mut hashes) => {
            // `RpcServer.cpp:2404-2415`: a reused payment id — a shared exchange
            // deposit id, say — can name an unbounded number of transactions, so
            // the answer is cut at `--rpc-max-block-count` and the caller is told
            // it was. `totalCount` is the count **before** the cut, and the cut
            // keeps the head of chain-then-pool, so pool hashes go first.
            let total = hashes.len() as u64;
            let truncated = total > max_hashes;
            if truncated {
                hashes.truncate(max_hashes as usize);
            }
            let mut r = Obj::new();
            r.set("status", "OK")
                .set("transactionHashes", crate::json::hash_array(hashes.iter()))
                .set("totalCount", total)
                .set("truncated", truncated);
            result(r.build())
        }
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_envelope_is_the_cpp_one() {
        let r = fail(-7, "Block not accepted");
        assert_eq!(r.status, 200);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"error":{"code":-7,"message":"Block not accepted"},"jsonrpc":"2.0"}"#
        );
    }

    #[test]
    fn only_the_cpp_methods_route() {
        for m in [
            "getblocktemplate",
            "submitblock",
            "getblockcount",
            "getlastblockheader",
            "getblockheaderbyhash",
            "getblockheaderbyheight",
            "f_blocks_list_json",
            "f_block_json",
            "f_transaction_json",
            "f_on_transactions_pool_json",
            "f_transactions_by_payment_id_json",
        ] {
            assert!(method_route(m).is_some(), "{m} is routed by RpcServer.cpp");
        }
        // Methods other CryptoNote daemons serve; this one 404s them.
        for m in [
            "on_getblockhash",
            "getblocksbyheights",
            "getblockdetailsbyheight",
            "getblock",
            "getblocks",
            "gettransaction",
            "gettransactionspool",
            "getcurrencyid",
            "",
        ] {
            assert!(method_route(m).is_none(), "{m} is not routed by RpcServer.cpp");
        }
        assert_eq!(method_route("f_block_json").unwrap().permissions, RpcMode::Explorer);
    }

    #[test]
    fn address_validation_reports_the_cpp_messages() {
        let good = "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
        assert_eq!(validate_address(good), Ok(()));
        assert!(validate_address("short").unwrap_err().starts_with("The address given is the wrong length."));
        let wrong_prefix = format!("Xrkz{}", &good[4..]);
        assert!(validate_address(&wrong_prefix).unwrap_err().contains("another cryptocurrency"));
        let integrated = "x".repeat(INTEGRATED_ADDRESS_LENGTH);
        let integrated = format!("Wrkz{}", &integrated[4..]);
        assert!(validate_address(&integrated).unwrap_err().contains("is an integrated address"));
        let typo = format!("{}X", &good[..good.len() - 1]);
        assert!(validate_address(&typo).is_err());
    }

    #[test]
    fn the_reserved_offset_search_matches_std_search() {
        assert_eq!(find_subslice(b"abcdef", b"cd"), 2);
        // Not found is `end()`, which the C++ then turns into an offset past the
        // blob and refuses with -5.
        assert_eq!(find_subslice(b"abcdef", b"xy"), 6);
        assert_eq!(find_subslice(b"ab", b"abc"), 2);
    }
}
