// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The plain (non-JSON-RPC) handlers, one per `RpcServer.cpp` handler, in the
//! order that file declares them.
//!
//! Each builds the same members with the same names and the same types as its
//! C++ counterpart; the line number is on the function. Where the C++ reads a
//! parameter with `getUint64FromJSON` and friends (`include/JsonHelper.h`), so
//! does this, including which parameters are optional and what they default to.

use crate::api::{ApiError, DetailedBlock, DetailedInput, DetailedTransaction, NodeApi, SyncRequest};
use crate::base64;
use crate::http::Response;
use crate::json::{hash_array, u64_array, Json, Obj};
use crate::server::ServerConfig;
use crate::sync_cache::{SyncCache, SyncCacheKey};
use wrkz_primitives::constants::{BLOCKS_SYNCHRONIZING_DEFAULT_COUNT, FORK_HEIGHTS, SOFTWARE_SUPPORTED_FORK_INDEX};
use wrkz_primitives::Hash;

/// `CryptoNote::SyncFeatures` (`config/CryptoNoteConfig.h:515`), in the order
/// `/info` pushes them.
pub const SYNC_FEATURES: [&str; 3] = ["skipEmptyBlocks", "base64", "heightRange"];

/// `ErrorCode::CANT_GET_FAKE_OUTPUTS` (`src/errors/Errors.h:114`).
pub const CANT_GET_FAKE_OUTPUTS: i64 = 27;

/// `DIFFICULTY_TARGET`, the divisor `/info`'s `hashrate` uses. Both operands
/// are `uint64_t` in the C++, so the `round()` around the division never sees a
/// fraction — the arithmetic is integer, and stays integer here.
const DIFFICULTY_TARGET: u64 = wrkz_primitives::constants::DIFFICULTY_TARGET;

// ---------------------------------------------------------------------------
// response shapes shared with the JSON-RPC half
// ---------------------------------------------------------------------------

/// `RpcServer::failRequest` (`RpcServer.cpp:842`): `{"error":…,"status":"Failed"}`.
pub fn fail_request(status: u16, body: &str) -> Response {
    let mut j = Obj::new();
    j.set("status", "Failed").set("error", body);
    Response::json(status, j.build().to_string())
}

/// How much of a malformed body [`fail_json_body`] echoes back. The C++
/// echoes all of it, which turns one 2 MiB request into a 2 MiB error for the
/// same price; the start of the body is all a caller debugging it needs.
pub const MAX_ECHOED_BODY: usize = 256;

/// `RpcServer::getJsonBody`'s parse failure (`RpcServer.cpp:748`). The message
/// is assembled from the same two pieces, with the body echoed back when it was
/// not empty — reproduced because a caller looking at the text is looking at
/// exactly this. A body longer than [`MAX_ECHOED_BODY`] is cut there and
/// followed by `...[N more bytes]`; a shorter one is echoed whole, as before.
pub fn fail_json_body(body: &[u8]) -> Response {
    let mut message = String::new();
    if !body.is_empty() {
        message
            .push_str("Warning: received body is not JSON encoded!\nKey/value parameters are NOT supported.\nBody:\n");
        let shown = &body[..body.len().min(MAX_ECHOED_BODY)];
        message.push_str(&String::from_utf8_lossy(shown));
        if body.len() > MAX_ECHOED_BODY {
            message.push_str(&format!("...[{} more bytes]", body.len() - MAX_ECHOED_BODY));
        }
    }
    message.push_str("Failed to parse request body as JSON");
    fail_request(400, &message)
}

/// The middleware's rendering of a handler that returned an `Error`
/// (`RpcServer.cpp:614`): `{"errorCode":…,"errorMessage":…}`.
pub fn fail_error(status: u16, code: i64, message: &str) -> Response {
    let mut j = Obj::new();
    j.set("errorCode", code).set("errorMessage", message);
    Response::json(status, j.build().to_string())
}

/// A successful body.
pub fn ok(json: Json) -> Response {
    Response::json(200, json.to_string())
}

// ---------------------------------------------------------------------------
// parameter reading (include/JsonHelper.h)
// ---------------------------------------------------------------------------

/// What went wrong reading a parameter, and which C++ exception path it is.
pub enum ParamError {
    /// `std::invalid_argument("Missing JSON parameter: '<key>'")`, which the
    /// middleware turns into a 400 with that exact text (`RpcServer.cpp:640`).
    Missing(String),
    /// A `nlohmann::json::type_error`, which the middleware catches as a plain
    /// `std::exception` and answers 500 with "Internal server error: …". The
    /// text after the colon is nlohmann's; ours names the key and the type
    /// instead, which is the one message in this crate that is not byte-identical.
    WrongType(String, &'static str),
}

impl ParamError {
    pub fn response(&self) -> Response {
        match self {
            ParamError::Missing(key) => fail_request(400, &format!("Missing JSON parameter: '{key}'")),
            ParamError::WrongType(key, want) => {
                fail_request(500, &format!("Internal server error: JSON parameter '{key}' must be {want}"))
            }
        }
    }
}

type Param<T> = std::result::Result<T, ParamError>;

fn value<'a>(body: &'a Json, key: &str) -> Param<&'a Json> {
    body.get(key).ok_or_else(|| ParamError::Missing(key.to_string()))
}

pub fn u64_of(body: &Json, key: &str) -> Param<u64> {
    value(body, key)?.as_u64().ok_or_else(|| ParamError::WrongType(key.to_string(), "an unsigned integer"))
}

pub fn str_of<'a>(body: &'a Json, key: &str) -> Param<&'a str> {
    value(body, key)?.as_str().ok_or_else(|| ParamError::WrongType(key.to_string(), "a string"))
}

pub fn bool_of(body: &Json, key: &str) -> Param<bool> {
    value(body, key)?.as_bool().ok_or_else(|| ParamError::WrongType(key.to_string(), "a boolean"))
}

pub fn array_of<'a>(body: &'a Json, key: &str) -> Param<&'a [Json]> {
    value(body, key)?.as_array().ok_or_else(|| ParamError::WrongType(key.to_string(), "an array"))
}

/// `hasMember(body, key) ? getUint64FromJSON(body, key) : default`.
fn u64_or(body: &Json, key: &str, default: u64) -> Param<u64> {
    if body.has(key) {
        u64_of(body, key)
    } else {
        Ok(default)
    }
}

fn bool_or(body: &Json, key: &str, default: bool) -> Param<bool> {
    if body.has(key) {
        bool_of(body, key)
    } else {
        Ok(default)
    }
}

/// `Common::podFromHex`: exactly 64 hex characters.
pub fn hash_from_hex(s: &str) -> Option<Hash> {
    if s.len() != 64 {
        return None;
    }
    let bytes = hex::decode(s).ok()?;
    bytes.try_into().ok()
}

/// A `Response` from a `ParamError`, so a handler can `?` its way out.
macro_rules! param {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return e.response(),
        }
    };
}

/// `ApiError` → the status the C++ gives when its own read fails.
fn api_failure(e: ApiError) -> Response {
    match e {
        ApiError::Busy(m) => fail_request(503, &m),
        ApiError::Internal(m) => fail_request(500, &format!("Internal server error: {m}")),
        ApiError::Unsupported(m) => fail_request(500, &format!("Internal server error: {m}")),
        // `RpcServer.cpp:1509-1518` answers this one with a 400 and the
        // message as the whole body, not wrapped in "Internal server error".
        ApiError::NotHeld(m) => fail_request(400, &m),
    }
}

// ---------------------------------------------------------------------------
// GET /info, GET /getinfo (RpcServer.cpp:891)
// ---------------------------------------------------------------------------

/// `RpcServer::info`.
///
/// `height` and `network_height` are **counts** (top index + 1); the wallet
/// subtracts one (spec/09). `last_known_block_index` is an **index**.
/// `hashrate` is `difficulty / DIFFICULTY_TARGET` in integer arithmetic.
pub fn info(api: &dyn NodeApi, cfg: &ServerConfig) -> Response {
    let i = match api.info() {
        Ok(i) => i,
        // `RpcServer.cpp:988`: any exception inside the handler becomes this.
        Err(_) => {
            let mut j = Obj::new();
            j.set("status", "BUSY").set("error", "Chain is reorganizing, please retry shortly");
            return Response::json(503, j.build().to_string());
        }
    };
    let mut j = Obj::new();
    j.set("height", i.height)
        .set("top_block_hash", hex::encode(i.top_block_hash))
        .set("difficulty", i.difficulty)
        .set("tx_count", i.tx_count)
        .set("tx_pool_size", i.tx_pool_size)
        .set("alt_blocks_count", i.alt_blocks_count)
        .set("outgoing_connections_count", i.outgoing_connections_count)
        .set("incoming_connections_count", i.incoming_connections_count)
        .set("white_peerlist_size", i.white_peerlist_size)
        .set("grey_peerlist_size", i.grey_peerlist_size)
        .set("seed_nodes_count", i.seed_nodes_count)
        .set("last_seed_bootstrap", i.last_seed_bootstrap)
        .set("last_known_block_index", i.last_known_block_index)
        .set("network_height", i.network_height)
        .set("upgrade_heights", u64_array(FORK_HEIGHTS.iter().copied()))
        .set("supported_height", FORK_HEIGHTS.get(SOFTWARE_SUPPORTED_FORK_INDEX).copied().unwrap_or(0))
        .set("hashrate", i.difficulty / DIFFICULTY_TARGET)
        .set("synced", i.height == i.network_height)
        .set("pruned", i.pruned)
        .set("prune_depth", i.prune_depth)
        .set("prune_capability_active", i.prune_capability_active)
        .set("lite", i.lite_start_height != 0)
        .set("lite_start_height", i.lite_start_height)
        .set("sync_active_peers", i.sync_active_peers)
        .set("sync_avg_batch_size", i.sync_avg_batch_size)
        .set("sync_demoted_peers", i.sync_demoted_peers)
        .set("major_version", i.major_version)
        .set("minor_version", i.minor_version)
        .set("version", cfg.version.as_str())
        .set("status", "OK")
        .set("start_time", i.start_time)
        .set("compression", cfg.compression.as_str())
        .set("sync_features", Json::Array(SYNC_FEATURES.iter().map(|f| Json::Str((*f).into())).collect()));
    ok(j.build())
}

// ---------------------------------------------------------------------------
// GET /height, GET /getheight (RpcServer.cpp:1002)
// ---------------------------------------------------------------------------

/// `RpcServer::height`. Deliberately three members and no `hash`: xmrig reads
/// the absence of `hash` as "this is a CryptoNote daemon" (`RpcServer.cpp:294`).
pub fn height(api: &dyn NodeApi) -> Response {
    let h = api.height();
    let mut j = Obj::new();
    j.set("height", h.height).set("network_height", h.network_height).set("status", "OK");
    ok(j.build())
}

// ---------------------------------------------------------------------------
// GET /peers (RpcServer.cpp:1016)
// ---------------------------------------------------------------------------

/// `RpcServer::peers`: `ip:port` strings, white list under `peers`, gray under
/// `peers_gray`.
pub fn peers(api: &dyn NodeApi) -> Response {
    let p = api.peers();
    let strings = |v: Vec<String>| Json::Array(v.into_iter().map(Json::Str).collect());
    let mut j = Obj::new();
    j.set("peers", strings(p.white)).set("peers_gray", strings(p.gray)).set("status", "OK");
    ok(j.build())
}

// ---------------------------------------------------------------------------
// POST /sendrawtransaction (RpcServer.cpp:1085)
// ---------------------------------------------------------------------------

/// `RpcServer::sendTransaction`. Always HTTP 200; `status` is `OK` or `Failed`.
///
/// `transactionHash` is added only once the hex parsed — a body whose
/// `tx_as_hex` is not hex gets `status`/`error` and nothing else.
pub fn send_transaction(api: &dyn NodeApi, body: &Json) -> Response {
    let raw = param!(str_of(body, "tx_as_hex"));
    let mut j = Obj::new();
    match hex::decode(raw) {
        Err(_) => {
            j.set("status", "Failed").set("error", "Failed to parse transaction from hex buffer");
        }
        Ok(blob) => {
            j.set("transactionHash", hex::encode(wrkz_pow::cn_fast_hash(&blob)));
            match api.add_transaction_to_pool(&blob) {
                Ok(()) => {
                    j.set("status", "OK").set("error", "");
                }
                Err(e) => {
                    j.set("status", "Failed").set("error", e);
                }
            }
        }
    }
    ok(j.build())
}

// ---------------------------------------------------------------------------
// POST /getrandom_outs (RpcServer.cpp:1152)
// ---------------------------------------------------------------------------

/// Most decoys one `/getrandom_outs` entry may ask for. Wallets ask for
/// `mixin + 1` (`Transfer.cpp:832`, `WalletGreen.cpp:3189`, and this port's
/// `get_ring_participants`): 2 today (`MAXIMUM_MIXIN_V5` = 1), 8 from
/// `MIXIN_LIMITS_V6_HEIGHT` (`MAXIMUM_MIXIN_V6` = 7), and 31 at the most any
/// WrkzCoin mixin range has ever allowed (`MAXIMUM_MIXIN_V1` = 30).
pub const MAX_RANDOM_OUTS_COUNT: u64 = 100;

/// Most entries `amounts` may have. A wallet sends one per input it spends,
/// repeats included. A wallet transaction is at most `getMaxTxSize`, 124,400
/// bytes, and an input at the smallest ring that asks for decoys (size 2)
/// costs at least 180 of them (`estimateTransactionSize`), so a transaction
/// that can be sent has about 690 inputs at most. The C++ wallet selects
/// inputs and fetches their decoys (`Transfer.cpp:1402`) before it checks the
/// size (`isTransactionPayloadTooBig`, `Transfer.cpp:464`), so a doomed
/// attempt from a wallet full of dust can ask for more than that and is then
/// refused by the wallet itself; this leaves room for that too.
pub const MAX_RANDOM_OUTS_AMOUNTS: usize = 10_000;

/// Most decoys one request may ask for in all, `amounts × outs_count`. This is
/// what bounds the answer — about 100 bytes of JSON per decoy, so 10 MB — and
/// the lookups behind it. The largest request a wallet makes for a
/// transaction it can send is 8 × 690 = 5,520.
pub const MAX_RANDOM_OUTS_TOTAL: u64 = 100_000;

/// `RpcServer::getRandomOuts`.
///
/// `outs_count` is narrowed to `uint16_t` by the C++ (`RpcServer.cpp:1171`).
/// Here it is first held to [`MAX_RANDOM_OUTS_COUNT`], `amounts` to
/// [`MAX_RANDOM_OUTS_AMOUNTS`] and the two together to
/// [`MAX_RANDOM_OUTS_TOTAL`]; past any of them is a 400 in the
/// `failRequest` shape a bad parameter gets, where the C++ would look up
/// whatever it was asked for — 65,535 decoys for each of a million amounts
/// (65,537 wraps to one). No wallet asks for anything near.
///
/// Finding fewer outputs than asked for is **not** an error (`Core.cpp:2054`);
/// only a failure inside the lookup is, and that is a 400 carrying
/// `errorCode` 27 (`CANT_GET_FAKE_OUTPUTS`).
pub fn get_random_outs(api: &dyn NodeApi, body: &Json) -> Response {
    let requested = param!(u64_of(body, "outs_count"));
    let amounts = param!(array_of(body, "amounts"));
    if requested > MAX_RANDOM_OUTS_COUNT {
        return fail_request(400, &format!("outs_count exceeds the maximum of {MAX_RANDOM_OUTS_COUNT}"));
    }
    if amounts.len() > MAX_RANDOM_OUTS_AMOUNTS {
        return fail_request(400, &format!("amounts has more than the maximum of {MAX_RANDOM_OUTS_AMOUNTS} entries"));
    }
    if requested.saturating_mul(amounts.len() as u64) > MAX_RANDOM_OUTS_TOTAL {
        return fail_request(
            400,
            &format!("amounts × outs_count exceeds the maximum of {MAX_RANDOM_OUTS_TOTAL} outputs per request"),
        );
    }
    // Lossless: `requested` is at most `MAX_RANDOM_OUTS_COUNT`.
    let count = requested as u16;
    let mut outs = Vec::with_capacity(amounts.len());
    for entry in amounts {
        let Some(amount) = entry.as_u64() else {
            return ParamError::WrongType("amounts".into(), "an array of unsigned integers").response();
        };
        let found = match api.random_outputs(amount, count) {
            Ok(Ok(v)) => v,
            Ok(Err(message)) => return fail_error(400, CANT_GET_FAKE_OUTPUTS, &message),
            Err(e) => return api_failure(e),
        };
        let mut amount_outs = Vec::with_capacity(found.len());
        for (global_index, key) in found {
            let mut o = Obj::new();
            o.set("global_amount_index", global_index).set("out_key", hex::encode(key));
            amount_outs.push(o.build());
        }
        let mut o = Obj::new();
        o.set("amount", amount).set("outs", Json::Array(amount_outs));
        outs.push(o.build());
    }
    let mut j = Obj::new();
    j.set("outs", Json::Array(outs)).set("status", "OK");
    ok(j.build())
}

// ---------------------------------------------------------------------------
// POST /getwalletsyncdata (RpcServer.cpp:1207)
// ---------------------------------------------------------------------------

/// The request fields both sync endpoints share, parsed and checked exactly as
/// `RpcServer::getWalletSyncData` / `getRawBlocks` do.
struct ParsedSync {
    request: SyncRequest,
    skip_input_key_offsets: bool,
    base64: bool,
}

fn parse_sync_request(cfg: &ServerConfig, body: &Json, wallet: bool) -> std::result::Result<ParsedSync, Response> {
    let mut checkpoints = Vec::new();
    if body.has("blockHashCheckpoints") {
        let arr = array_of(body, "blockHashCheckpoints").map_err(|e| e.response())?;
        for entry in arr {
            let Some(s) = entry.as_str() else {
                return Err(ParamError::WrongType("blockHashCheckpoints".into(), "an array of strings").response());
            };
            match hash_from_hex(s) {
                Some(h) => checkpoints.push(h),
                None => return Err(fail_request(400, "blockHashCheckpoints contains invalid hash")),
            }
        }
    }
    let start_height = u64_or(body, "startHeight", 0).map_err(|e| e.response())?;
    let start_timestamp = u64_or(body, "startTimestamp", 0).map_err(|e| e.response())?;
    let block_count = u64_or(body, "blockCount", 100).map_err(|e| e.response())?;
    if block_count > cfg.max_block_count {
        return Err(fail_request(400, "blockCount exceeds rpc-max-block-count"));
    }
    let skip_coinbase_transactions = bool_or(body, "skipCoinbaseTransactions", false).map_err(|e| e.response())?;

    let (skip_input_key_offsets, skip_empty_blocks, end_height, base64) = if wallet {
        let skip_offsets = bool_or(body, "skipInputKeyOffsets", false).map_err(|e| e.response())?;
        let skip_empty = bool_or(body, "skipEmptyBlocks", false).map_err(|e| e.response())?;
        let end = u64_or(body, "endHeight", 0).map_err(|e| e.response())?;
        let encoding = if body.has("encoding") { str_of(body, "encoding").map_err(|e| e.response())? } else { "hex" };
        if encoding != "hex" && encoding != "base64" {
            return Err(fail_request(400, "encoding must be either 'hex' or 'base64'"));
        }
        (skip_offsets, skip_empty, end, encoding == "base64")
    } else {
        // `/getrawblocks` reads none of these (`RpcServer.cpp:2966`).
        (false, false, 0, false)
    };

    Ok(ParsedSync {
        request: SyncRequest {
            block_hash_checkpoints: checkpoints,
            start_height,
            start_timestamp,
            block_count,
            skip_coinbase_transactions,
            skip_empty_blocks,
            end_height,
        },
        skip_input_key_offsets,
        base64,
    })
}

/// `encodePod` (`RpcServer.cpp:1300`).
fn encode_pod(pod: &[u8], as_base64: bool) -> String {
    if as_base64 {
        base64::encode(pod)
    } else {
        hex::encode(pod)
    }
}

/// `RpcServer::getWalletSyncData`, with the response cache of
/// `RpcServer.cpp:1305-1337` in front of it when `cache` is given
/// ([`crate::sync_cache`]). A cached answer is the exact body an uncached
/// call built, so the cache changes the speed of an answer and nothing else.
pub fn get_wallet_sync_data(api: &dyn NodeApi, cfg: &ServerConfig, body: &Json, cache: Option<&SyncCache>) -> Response {
    let parsed = match parse_sync_request(cfg, body, true) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Read before the answer is built, as the C++ does: storing compares the
    // answer's last block against a tip that can only have risen since.
    let top_index = api.top_index();
    let mut cache_key = None;
    if let Some(cache) = cache.filter(|c| c.enabled()) {
        cache.observe_tip(top_index);
        if let Ok(Some(start_index)) = api.wallet_sync_start_index(&parsed.request) {
            let key = SyncCacheKey {
                start_index,
                block_count: parsed.request.block_count,
                end_height: parsed.request.end_height,
                skip_coinbase_transactions: parsed.request.skip_coinbase_transactions,
                skip_input_key_offsets: parsed.skip_input_key_offsets,
                skip_empty_blocks: parsed.request.skip_empty_blocks,
                base64: parsed.base64,
            };
            if let Some(cached) = cache.lookup(&key) {
                let mut res = Response::json(200, String::new());
                res.body = cached.to_vec();
                return res;
            }
            cache_key = Some(key);
        }
    }
    let data = match api.wallet_sync_data(&parsed.request) {
        Ok(d) => d,
        // `Core::getWalletSyncData` returning false is a bare 500 with no body
        // (`RpcServer.cpp:1329`).
        Err(_) => return Response::new(500),
    };
    let b64 = parsed.base64;

    let encode_tx = |t: &crate::api::SyncTransaction, coinbase: bool| -> Json {
        let mut outputs = Vec::with_capacity(t.outputs.len());
        for o in &t.outputs {
            let mut oo = Obj::new();
            oo.set("key", encode_pod(&o.key, b64)).set("amount", o.amount);
            if let Some(g) = o.global_index {
                oo.set("globalIndex", g);
            }
            outputs.push(oo.build());
        }
        let mut tx = Obj::new();
        tx.set("outputs", Json::Array(outputs))
            .set("hash", encode_pod(&t.hash, b64))
            .set("txPublicKey", encode_pod(&t.tx_public_key, b64))
            .set("unlockTime", t.unlock_time);
        if !coinbase {
            let mut inputs = Vec::with_capacity(t.inputs.len());
            for i in &t.inputs {
                let mut io = Obj::new();
                io.set("amount", i.amount).set("k_image", encode_pod(&i.key_image, b64));
                if !parsed.skip_input_key_offsets {
                    io.set("key_offsets", u64_array(i.key_offsets.iter().copied()));
                }
                inputs.push(io.build());
            }
            tx.set("paymentID", t.payment_id.as_str()).set("inputs", Json::Array(inputs));
        }
        tx.build()
    };

    let mut items = Vec::with_capacity(data.items.len());
    for block in &data.items {
        let mut b = Obj::new();
        if let Some(cb) = &block.coinbase {
            b.set("coinbaseTX", encode_tx(cb, true));
        }
        b.set("transactions", Json::Array(block.transactions.iter().map(|t| encode_tx(t, false)).collect()))
            .set("blockHeight", block.block_height)
            .set("blockHash", encode_pod(&block.block_hash, b64))
            .set("blockTimestamp", block.block_timestamp);
        items.push(b.build());
    }

    let mut j = Obj::new();
    j.set("items", Json::Array(items));
    if let Some(top) = data.top_block {
        let mut t = Obj::new();
        t.set("hash", encode_pod(&top.hash, b64)).set("height", top.height);
        j.set("topBlock", t.build());
    }
    if data.scanned_to_height != 0 {
        j.set("scannedToHeight", data.scanned_to_height);
    }
    j.set("synced", data.items.is_empty()).set("status", "OK");
    let res = ok(j.build());
    if let (Some(cache), Some(key), Some(first), Some(last)) = (cache, cache_key, data.items.first(), data.items.last())
    {
        cache.store(key, &res.body, first.block_height, last.block_height, top_index);
    }
    res
}

// ---------------------------------------------------------------------------
// POST /get_global_indexes_for_range (RpcServer.cpp:1459)
// ---------------------------------------------------------------------------

/// `RpcServer::getGlobalIndexes`. `startHeight` and `endHeight` are block
/// **indexes**, `endHeight` exclusive, at most `--rpc-max-global-index-range`
/// apart.
pub fn get_global_indexes(api: &dyn NodeApi, cfg: &ServerConfig, body: &Json) -> Response {
    let start = param!(u64_of(body, "startHeight"));
    let end = param!(u64_of(body, "endHeight"));
    if end < start {
        return fail_request(400, "endHeight must be >= startHeight");
    }
    if end - start >= cfg.max_global_index_range {
        return fail_request(400, "Requested range exceeds rpc-max-global-index-range");
    }
    let indexes = match api.global_indexes_for_range(start, end) {
        Ok(v) => v,
        // `RpcServer.cpp:1500`: a 500 whose body is `{"status":"Failed"}` and
        // nothing else — no `error` member.
        Err(_) => {
            let mut j = Obj::new();
            j.set("status", "Failed");
            return Response::json(500, j.build().to_string());
        }
    };
    let mut arr = Vec::with_capacity(indexes.len());
    for (hash, values) in indexes {
        let mut e = Obj::new();
        e.set("key", hex::encode(hash)).set("value", u64_array(values));
        arr.push(e.build());
    }
    let mut j = Obj::new();
    j.set("indexes", Json::Array(arr)).set("status", "OK");
    ok(j.build())
}

// ---------------------------------------------------------------------------
// POST /queryblockslite (RpcServer.cpp:2404)
// ---------------------------------------------------------------------------

/// `RpcServer::queryBlocksLite`, the legacy `WalletGreen` sync path. Not
/// documented in spec/09 beyond a mention; the shape here is
/// `RpcServer.cpp:2450-2500` and `CoreRpcServerCommandsDefinitions.h`.
pub fn query_blocks_lite(api: &dyn NodeApi, body: &Json) -> Response {
    let timestamp = param!(u64_or(body, "timestamp", 0));
    let mut known = Vec::new();
    if body.has("blockIds") {
        for entry in param!(array_of(body, "blockIds")) {
            let Some(s) = entry.as_str() else {
                return ParamError::WrongType("blockIds".into(), "an array of strings").response();
            };
            match hash_from_hex(s) {
                Some(h) => known.push(h),
                None => return fail_request(400, "Block hash specified is not a valid hex string!"),
            }
        }
    }
    let q = match api.query_blocks_lite(&known, timestamp) {
        Ok(q) => q,
        Err(_) => return fail_request(500, "Internal error: failed to queryblockslite"),
    };
    let mut items = Vec::with_capacity(q.items.len());
    for block in &q.items {
        let mut e = Obj::new();
        e.set("blockShortInfo.block", Json::Array(block.block.iter().map(|b| Json::U64(*b as u64)).collect()))
            .set("blockShortInfo.blockId", hex::encode(block.block_id))
            .set("blockShortInfo.txPrefixes", Json::Array(block.tx_prefixes.iter().map(tx_prefix_entry).collect()));
        items.push(e.build());
    }
    let mut j = Obj::new();
    j.set("fullOffset", q.full_offset)
        .set("currentHeight", q.current_index)
        .set("startHeight", q.start_index)
        .set("items", Json::Array(items))
        .set("status", "OK");
    ok(j.build())
}

/// One `transactionPrefixInfo.*` entry, shared by `/queryblockslite` and
/// `/get_pool_changes_lite` (`RpcServer.cpp:2492` and `:2707`, identical).
fn tx_prefix_entry(info: &crate::api::TxPrefixInfo) -> Json {
    let mut e = Obj::new();
    e.set("transactionPrefixInfo.txHash", hex::encode(info.hash))
        .set("transactionPrefixInfo.txPrefix", tx_prefix_object(&info.prefix));
    e.build()
}

/// The `txPrefix` object: `extra`, `unlock_time`, `version`, `vin`, `vout`.
fn tx_prefix_object(prefix: &wrkz_primitives::tx::TransactionPrefix) -> Json {
    use wrkz_primitives::tx::Input;
    let mut vin = Vec::with_capacity(prefix.inputs.len());
    for input in &prefix.inputs {
        let mut value = Obj::new();
        let type_tag = match input {
            Input::Base { block_index } => {
                value.set("height", *block_index);
                "ff"
            }
            Input::Key { amount, key_offsets, key_image } => {
                value
                    .set("k_image", hex::encode(key_image))
                    .set("amount", *amount)
                    .set("key_offsets", u64_array(key_offsets.iter().copied()));
                "02"
            }
        };
        let mut o = Obj::new();
        o.set("type", type_tag).set("value", value.build());
        vin.push(o.build());
    }
    let mut vout = Vec::with_capacity(prefix.outputs.len());
    for output in &prefix.outputs {
        let mut data = Obj::new();
        data.set("key", hex::encode(output.key));
        let mut target = Obj::new();
        target.set("data", data.build()).set("type", "02");
        let mut o = Obj::new();
        o.set("amount", output.amount).set("target", target.build());
        vout.push(o.build());
    }
    let mut p = Obj::new();
    p.set("extra", hex::encode(&prefix.extra))
        .set("unlock_time", prefix.unlock_time)
        .set("version", prefix.version)
        .set("vin", Json::Array(vin))
        .set("vout", Json::Array(vout));
    p.build()
}

// ---------------------------------------------------------------------------
// POST /get_transactions_status (RpcServer.cpp:2560)
// ---------------------------------------------------------------------------

/// `RpcServer::getTransactionsStatus`. Every requested hash lands in exactly
/// one of the three arrays.
pub fn get_transactions_status(api: &dyn NodeApi, body: &Json) -> Response {
    let mut hashes = Vec::new();
    for entry in param!(array_of(body, "transactionHashes")) {
        let Some(s) = entry.as_str() else {
            return ParamError::WrongType("transactionHashes".into(), "an array of strings").response();
        };
        match hash_from_hex(s) {
            Some(h) => hashes.push(h),
            None => return fail_request(400, "Transaction hash specified is not a valid hex string!"),
        }
    }
    let status = match api.transactions_status(&hashes) {
        Ok(s) => s,
        Err(_) => return fail_request(500, "Internal error: failed to getTransactionsStatus"),
    };
    let mut j = Obj::new();
    j.set("transactionsInBlock", hash_array(status.in_block.iter()))
        .set("transactionsInPool", hash_array(status.in_pool.iter()))
        .set("transactionsUnknown", hash_array(status.unknown.iter()))
        .set("status", "OK");
    ok(j.build())
}

// ---------------------------------------------------------------------------
// POST /get_pool_changes_lite (RpcServer.cpp:2621)
// ---------------------------------------------------------------------------

/// `RpcServer::getPoolChanges`. Not in spec/09 beyond a mention; the shape is
/// `RpcServer.cpp:2718`.
pub fn get_pool_changes(api: &dyn NodeApi, body: &Json) -> Response {
    let tail = param!(str_of(body, "tailBlockId"));
    let Some(tail) = hash_from_hex(tail) else {
        return fail_request(400, "tailBlockId specified is not a valid hex string!");
    };
    let mut known = Vec::new();
    for entry in param!(array_of(body, "knownTxsIds")) {
        let Some(s) = entry.as_str() else {
            return ParamError::WrongType("knownTxsIds".into(), "an array of strings").response();
        };
        match hash_from_hex(s) {
            Some(h) => known.push(h),
            None => return fail_request(400, "Transaction hash specified is not a valid hex string!"),
        }
    }
    let changes = match api.pool_changes_lite(&tail, &known) {
        Ok(c) => c,
        Err(e) => return api_failure(e),
    };
    let mut j = Obj::new();
    j.set("addedTxs", Json::Array(changes.added.iter().map(tx_prefix_entry).collect()))
        .set("deletedTxsIds", hash_array(changes.deleted.iter()))
        .set("isTailBlockActual", changes.is_tail_block_actual)
        .set("status", "OK");
    ok(j.build())
}

// ---------------------------------------------------------------------------
// POST /queryblocksdetailed (RpcServer.cpp:2731)
// ---------------------------------------------------------------------------

/// `RpcServer::queryBlocksDetailed`, an explorer route that is off by default
/// (a 403 in `Standard` mode, before this is reached): the chain from a
/// caller's known blocks, each block with every transaction in
/// `Core::getBlockDetails`'s detail — the ring size and the ring's last member
/// per input, the global index per output.
pub fn query_blocks_detailed(api: &dyn NodeApi, body: &Json) -> Response {
    let timestamp = param!(u64_or(body, "timestamp", 0));
    let mut known = Vec::new();
    if body.has("blockIds") {
        for entry in param!(array_of(body, "blockIds")) {
            let Some(s) = entry.as_str() else {
                return ParamError::WrongType("blockIds".into(), "an array of strings").response();
            };
            match hash_from_hex(s) {
                Some(h) => known.push(h),
                None => return fail_request(400, "Block hash specified is not a valid hex string!"),
            }
        }
    }
    // A `uint64_t` handed to `Core::queryBlocksDetailed`'s `uint32_t`
    // parameter: truncated, as the C++ does.
    let block_count = param!(u64_or(body, "blockCount", BLOCKS_SYNCHRONIZING_DEFAULT_COUNT as u64)) as u32;
    let q = match api.query_blocks_detailed(&known, timestamp, block_count) {
        Ok(q) => q,
        // `RpcServer.cpp:2780`: the lite route's message, as the C++ has it.
        Err(_) => return fail_request(500, "Internal error: failed to queryblockslite"),
    };
    let blocks: Vec<Json> = q.blocks.iter().map(detailed_block_json).collect();
    let mut j = Obj::new();
    j.set("fullOffset", q.full_offset)
        .set("currentHeight", q.current_index)
        .set("startHeight", q.start_index)
        .set("blocks", blocks)
        .set("status", "OK");
    ok(j.build())
}

/// One `blocks` entry (`RpcServer.cpp:2900-2917`).
fn detailed_block_json(b: &DetailedBlock) -> Json {
    let mut o = Obj::new();
    o.set("major_version", b.major_version)
        .set("minor_version", b.minor_version)
        .set("timestamp", b.timestamp)
        .set("prevBlockHash", hex::encode(b.prev_hash))
        .set("index", b.index)
        .set("hash", hex::encode(b.hash))
        .set("difficulty", b.difficulty)
        .set("reward", b.reward)
        .set("blockSize", b.block_size)
        // `std::to_string`: a string, as in `f_block_json`.
        .set("alreadyGeneratedCoins", b.already_generated_coins.to_string())
        .set("alreadyGeneratedTransactions", b.already_generated_transactions)
        .set("sizeMedian", b.size_median)
        .set("baseReward", b.base_reward)
        .set("nonce", b.nonce)
        .set("totalFeeAmount", b.total_fee_amount)
        .set("transactionsCumulativeSize", b.transactions_cumulative_size)
        .set("transactions", b.transactions.iter().map(|t| detailed_transaction_json(b, t)).collect::<Vec<_>>());
    o.build()
}

/// One `transactions` entry (`RpcServer.cpp:2787-2897`). `blockHash` and
/// `blockIndex` are the enclosing block's, which is where the C++ takes them
/// from.
fn detailed_transaction_json(block: &DetailedBlock, t: &DetailedTransaction) -> Json {
    let mut extra = Obj::new();
    extra
        .set("nonce", u64_array(t.extra_nonce.iter().map(|b| u64::from(*b))))
        .set("publicKey", hex::encode(t.extra_public_key))
        .set("raw", hex::encode(&t.extra_raw));

    let mut inputs = Vec::with_capacity(t.inputs.len());
    for input in &t.inputs {
        let mut data = Obj::new();
        let kind = match input {
            DetailedInput::Base { block_index, amount } => {
                let mut inner = Obj::new();
                inner.set("height", *block_index);
                data.set("amount", *amount).set("input", inner.build());
                "ff"
            }
            DetailedInput::Key { amount, key_offsets, key_image, mixin, output_transaction_hash, output_number } => {
                let mut inner = Obj::new();
                inner
                    .set("amount", *amount)
                    .set("k_image", hex::encode(key_image))
                    .set("key_offsets", u64_array(key_offsets.iter().copied()));
                let mut output = Obj::new();
                output.set("transactionHash", hex::encode(output_transaction_hash)).set("number", *output_number);
                data.set("input", inner.build()).set("mixin", *mixin).set("output", output.build());
                "02"
            }
        };
        let mut e = Obj::new();
        e.set("type", kind).set("data", data.build());
        inputs.push(e.build());
    }

    let mut outputs = Vec::with_capacity(t.outputs.len());
    for o in &t.outputs {
        let mut key = Obj::new();
        key.set("key", hex::encode(o.key));
        let mut target = Obj::new();
        target.set("data", key.build()).set("type", "02");
        let mut inner = Obj::new();
        inner.set("amount", o.amount).set("target", target.build());
        let mut e = Obj::new();
        e.set("globalIndex", o.global_index).set("output", inner.build());
        outputs.push(e.build());
    }

    // `first` is the input's position, `second` one signature of its ring.
    let mut signatures = Vec::new();
    for (i, ring) in t.signatures.iter().enumerate() {
        for sig in ring {
            let mut s = Obj::new();
            s.set("first", i).set("second", hex::encode(sig));
            signatures.push(s.build());
        }
    }

    let mut o = Obj::new();
    o.set("blockHash", hex::encode(block.hash))
        .set("blockIndex", block.index)
        .set("extra", extra.build())
        .set("fee", t.fee)
        .set("hash", hex::encode(t.hash))
        // Every transaction here came out of a block.
        .set("inBlockchain", true)
        .set("inputs", inputs)
        .set("mixin", t.mixin)
        .set("outputs", outputs)
        .set("paymentId", hex::encode(t.payment_id))
        .set("signatures", signatures)
        .set("signaturesSize", t.signatures.len())
        .set("size", t.size)
        .set("timestamp", t.timestamp)
        .set("totalInputsAmount", t.total_inputs_amount)
        .set("totalOutputsAmount", t.total_outputs_amount)
        .set("unlockTime", t.unlock_time);
    o.build()
}

// ---------------------------------------------------------------------------
// POST /get_o_indexes (RpcServer.cpp:2938)
// ---------------------------------------------------------------------------

/// `RpcServer::getGlobalIndexesDeprecated`: the global indexes of one
/// transaction's outputs, by transaction hash.
pub fn get_global_indexes_deprecated(api: &dyn NodeApi, body: &Json) -> Response {
    let txid = param!(str_of(body, "txid"));
    let Some(hash) = hash_from_hex(txid) else {
        return fail_request(400, "txid specified is not a valid hex string!");
    };
    match api.transaction_global_indexes(&hash) {
        Ok(Some(indexes)) => {
            let mut j = Obj::new();
            j.set("o_indexes", u64_array(indexes.into_iter().map(u64::from))).set("status", "OK");
            ok(j.build())
        }
        // `Core::getTransactionGlobalIndexes` answering false, which is what an
        // unknown transaction produces (`RpcServer.cpp:2957`).
        Ok(None) => fail_request(500, "Internal error: Failed to getTransactionGlobalIndexes"),
        // A lite snapshot import that cannot tell whether the transaction is
        // below its line: the 400 of every other lite refusal, not a 500.
        Err(ApiError::NotHeld(m)) => fail_request(400, &m),
        Err(_) => fail_request(500, "Internal error: Failed to getTransactionGlobalIndexes"),
    }
}

// ---------------------------------------------------------------------------
// POST /getrawblocks (RpcServer.cpp:2966)
// ---------------------------------------------------------------------------

/// `RpcServer::getRawBlocks`: the exact block and transaction bytes.
pub fn get_raw_blocks(api: &dyn NodeApi, cfg: &ServerConfig, body: &Json) -> Response {
    let parsed = match parse_sync_request(cfg, body, false) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let blocks = match api.raw_blocks(&parsed.request) {
        Ok(b) => b,
        Err(_) => return Response::new(500),
    };
    let mut items = Vec::with_capacity(blocks.items.len());
    for item in &blocks.items {
        let mut o = Obj::new();
        o.set("block", hex::encode(&item.block))
            .set("transactions", Json::Array(item.transactions.iter().map(|t| Json::Str(hex::encode(t))).collect()));
        items.push(o.build());
    }
    let mut j = Obj::new();
    j.set("items", Json::Array(items)).set("synced", blocks.items.is_empty()).set("status", "OK");
    if let Some(top) = blocks.top_block {
        let mut t = Obj::new();
        t.set("hash", hex::encode(top.hash)).set("height", top.height);
        j.set("topBlock", t.build());
    }
    ok(j.build())
}

pub(crate) use param;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_bodies_match_the_cpp() {
        let r = fail_request(400, "endHeight must be >= startHeight");
        assert_eq!(r.status, 400);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"error":"endHeight must be >= startHeight","status":"Failed"}"#
        );
        let r = fail_error(400, CANT_GET_FAKE_OUTPUTS, "Output is locked");
        assert_eq!(String::from_utf8(r.body).unwrap(), r#"{"errorCode":27,"errorMessage":"Output is locked"}"#);
    }

    #[test]
    fn the_json_parse_failure_echoes_the_body_like_the_cpp() {
        let r = fail_json_body(b"not json");
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("Warning: received body is not JSON encoded!"));
        assert!(body.contains("Body:\\nnot jsonFailed to parse request body as JSON"));
        // An empty body carries only the second half.
        let r = fail_json_body(b"");
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"error":"Failed to parse request body as JSON","status":"Failed"}"#
        );
    }

    #[test]
    fn missing_parameters_report_the_cpp_message() {
        let e = ParamError::Missing("startHeight".into()).response();
        assert_eq!(e.status, 400);
        assert_eq!(
            String::from_utf8(e.body).unwrap(),
            r#"{"error":"Missing JSON parameter: 'startHeight'","status":"Failed"}"#
        );
    }

    #[test]
    fn hashes_must_be_exactly_64_hex_characters() {
        assert!(hash_from_hex(&"a".repeat(64)).is_some());
        assert!(hash_from_hex(&"a".repeat(63)).is_none());
        assert!(hash_from_hex(&"a".repeat(65)).is_none());
        assert!(hash_from_hex(&"z".repeat(64)).is_none());
    }
}
