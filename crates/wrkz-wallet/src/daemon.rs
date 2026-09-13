// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The daemon RPC client a wallet uses (`src/nigel/Nigel.cpp`,
//! spec/09-rpc-and-wallet-sync.md). Blocking HTTP with JSON bodies; every
//! request shape and every response field name is the wire contract.

use serde::{Deserialize, Serialize};
#[cfg(feature = "native")]
use std::io::Read;
#[cfg(feature = "native")]
use std::time::Duration;

/// Largest response body accepted from the daemon.
///
/// `ureq`'s `into_json` reads without any limit, so a hostile or broken daemon
/// could otherwise stream until the wallet runs out of memory. spec/09 caps an
/// assembled `/getwalletsyncdata` response at 8 MiB; this leaves headroom for
/// hex expansion and for `/getrawblocks`.
pub const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Whether this build can speak TLS. The `https` feature compiles in `ureq`'s
/// backend; without it an `https://` base URL is rejected up front rather than
/// failing later with an opaque transport error.
pub const HTTPS_SUPPORTED: bool = cfg!(feature = "https");

/// This client classifies daemon failures; it does **not** retry. Choosing how
/// to react — spec/09's "wait 20 s on 429" and "halve `blockCount` on 400" —
/// belongs to the sync loop that owns the batch size, so every variant here is
/// reported to the caller as-is.
#[derive(Debug)]
pub enum DaemonError {
    /// HTTP 429: the per-IP rate limit. The caller backs off (the C++ wallet
    /// waits 20 s and keeps its batch size).
    RateLimited,
    /// HTTP 400: malformed body, or `blockCount` over the daemon's limit. The
    /// caller halves its batch and retries.
    BadRequest(String),
    /// Endpoint missing. The caller's raw-blocks path falls back to
    /// `/getwalletsyncdata` on 404/500.
    NotFound,
    Http(u16, String),
    Transport(String),
    Json(String),
    /// The body exceeded [`MAX_RESPONSE_BYTES`].
    ResponseTooLarge,
    /// The base URL was not a scheme this build can speak.
    UnsupportedScheme(String),
    /// `status` was not `OK`.
    Status(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DaemonError {}

pub type Result<T> = std::result::Result<T, DaemonError>;

#[derive(Clone, Debug, Deserialize)]
pub struct Info {
    pub height: u64,
    pub network_height: u64,
    pub difficulty: u64,
    #[serde(default)]
    pub incoming_connections_count: u64,
    #[serde(default)]
    pub outgoing_connections_count: u64,
    #[serde(default)]
    pub lite_start_height: u64,
    #[serde(default)]
    pub sync_features: Vec<String>,
    #[serde(default)]
    pub compression: Option<String>,
    #[serde(default)]
    pub synced: bool,
    #[serde(default)]
    pub top_block_hash: Option<String>,
    #[serde(default)]
    pub supported_height: Option<u64>,
    #[serde(default)]
    pub upgrade_heights: Vec<u64>,
    #[serde(default)]
    pub version: Option<String>,
    pub status: String,
}

impl Info {
    pub fn supports(&self, feature: &str) -> bool {
        self.sync_features.iter().any(|f| f == feature)
    }
    /// `height` and `network_height` are counts; the wallet subtracts one (`Nigel.cpp:867`).
    pub fn top_index(&self) -> u64 {
        self.height.saturating_sub(1)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Height {
    pub height: u64,
    pub network_height: u64,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BlockHeader {
    pub block_size: u64,
    pub depth: u64,
    pub difficulty: u64,
    pub hash: String,
    pub height: u64,
    pub major_version: u8,
    pub minor_version: u8,
    pub nonce: u32,
    pub num_txes: u64,
    pub orphan_status: bool,
    pub prev_hash: String,
    pub reward: u64,
    pub timestamp: u64,
}

/// `POST /getwalletsyncdata` and `/getrawblocks` request (`Nigel.cpp:489`).
#[derive(Clone, Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncRequest {
    /// Newest first: recent hashes, processed hashes, then one per 5000 blocks.
    pub block_hash_checkpoints: Vec<String>,
    pub start_height: u64,
    pub start_timestamp: u64,
    pub block_count: u64,
    pub skip_coinbase_transactions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_input_key_offsets: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_empty_blocks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_height: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutput {
    pub amount: u64,
    pub key: String,
    /// The daemon never sends this; a third-party blockchain cache API does,
    /// under the key `globalIndex` (`WalletTypes.h:679`). When it is absent the
    /// wallet fills the index in from `/get_global_indexes_for_range`.
    #[serde(default, rename = "globalIndex")]
    pub global_index: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SyncInput {
    pub amount: u64,
    pub k_image: String,
    #[serde(default)]
    pub key_offsets: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncTransaction {
    pub hash: String,
    pub outputs: Vec<SyncOutput>,
    pub tx_public_key: String,
    pub unlock_time: u64,
    #[serde(default, rename = "paymentID")]
    pub payment_id: String,
    #[serde(default)]
    pub inputs: Vec<SyncInput>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncBlock {
    pub block_hash: String,
    pub block_height: u64,
    pub block_timestamp: u64,
    #[serde(default, rename = "coinbaseTX")]
    pub coinbase_tx: Option<SyncTransaction>,
    #[serde(default)]
    pub transactions: Vec<SyncTransaction>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TopBlock {
    pub hash: String,
    pub height: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletSyncData {
    #[serde(default)]
    pub items: Vec<SyncBlock>,
    #[serde(default)]
    pub scanned_to_height: Option<u64>,
    #[serde(default)]
    pub synced: bool,
    #[serde(default)]
    pub top_block: Option<TopBlock>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RawBlockItem {
    pub block: String,
    #[serde(default)]
    pub transactions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawBlocks {
    #[serde(default)]
    pub items: Vec<RawBlockItem>,
    #[serde(default)]
    pub synced: bool,
    #[serde(default)]
    pub top_block: Option<TopBlock>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GlobalIndexEntry {
    /// transaction hash
    pub key: String,
    /// global index per output
    pub value: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GlobalIndexes {
    #[serde(default)]
    pub indexes: Vec<GlobalIndexEntry>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RandomOut {
    pub global_amount_index: u64,
    pub out_key: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RandomOutsForAmount {
    pub amount: u64,
    #[serde(default)]
    pub outs: Vec<RandomOut>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RandomOuts {
    #[serde(default)]
    pub outs: Vec<RandomOutsForAmount>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionsStatus {
    #[serde(default)]
    pub transactions_in_pool: Vec<String>,
    #[serde(default)]
    pub transactions_in_block: Vec<String>,
    #[serde(default)]
    pub transactions_unknown: Vec<String>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SendResult {
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BlockTemplateResult {
    pub blocktemplate_blob: String,
    pub difficulty: u64,
    pub height: u64,
    pub reserved_offset: u64,
    pub status: String,
}

/// A response that carries its own `status` field.
#[cfg(feature = "native")]
trait HasStatus {
    fn status(&self) -> &str;
}

#[cfg(feature = "native")]
macro_rules! has_status {
    ($($t:ty),* $(,)?) => {
        $(impl HasStatus for $t {
            fn status(&self) -> &str {
                &self.status
            }
        })*
    };
}

#[cfg(feature = "native")]
has_status!(
    Info,
    Height,
    BlockTemplateResult,
    WalletSyncData,
    RawBlocks,
    GlobalIndexes,
    RandomOuts,
    TransactionsStatus
);

/// How the optional access token is presented (spec/09 "Transport rules"
/// accepts either form).
#[cfg(feature = "native")]
#[derive(Clone, Debug)]
enum Auth {
    ApiKey(String),
    Bearer(String),
}

#[cfg(feature = "native")]
impl Auth {
    fn apply(&self, req: ureq::Request) -> ureq::Request {
        match self {
            Auth::ApiKey(k) => req.set("X-API-Key", k),
            Auth::Bearer(t) => req.set("Authorization", &format!("Bearer {t}")),
        }
    }
}

/// Deliberately no `Debug`: it would print the access token.
///
/// Needs the `native` feature: a browser has no socket to give `ureq`.
#[cfg(feature = "native")]
pub struct Daemon {
    base: String,
    agent: ureq::Agent,
    auth: Option<Auth>,
}

#[cfg(feature = "native")]
impl Daemon {
    /// `base` like `http://node-fin.wrkz.work:17856` (no trailing slash).
    ///
    /// Only `http://` is accepted unless the crate is built with the `https`
    /// feature, which pulls in `ureq`'s TLS backend; without it an `https` URL
    /// would fail later with an opaque transport error.
    pub fn new(base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let scheme = base.split_once("://").map(|(s, _)| s).unwrap_or_default();
        if !(scheme == "http" || (scheme == "https" && HTTPS_SUPPORTED)) {
            return Err(DaemonError::UnsupportedScheme(base));
        }
        let agent =
            ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(60)).build();
        Ok(Self { base, agent, auth: None })
    }

    /// Send the token as `X-API-Key`.
    pub fn with_api_key(mut self, key: &str) -> Self {
        self.auth = Some(Auth::ApiKey(key.to_string()));
        self
    }

    /// Send the token as `Authorization: Bearer`.
    pub fn with_bearer_token(mut self, token: &str) -> Self {
        self.auth = Some(Auth::Bearer(token.to_string()));
        self
    }

    fn map_err(e: ureq::Error) -> DaemonError {
        match e {
            ureq::Error::Status(429, _) => DaemonError::RateLimited,
            ureq::Error::Status(400, r) => DaemonError::BadRequest(r.into_string().unwrap_or_default()),
            ureq::Error::Status(404, _) => DaemonError::NotFound,
            ureq::Error::Status(code, r) => DaemonError::Http(code, r.into_string().unwrap_or_default()),
            ureq::Error::Transport(t) => DaemonError::Transport(t.to_string()),
        }
    }

    /// What this build tells a daemon it can read.
    ///
    /// `ureq` adds `gzip` by itself and decodes it, which is all a C++ daemon
    /// offers. With the `zstd` feature the header is set here instead, naming
    /// both, and [`Daemon::read_json`] decodes the one the daemon picked; only
    /// this port's daemon, built with its own `zstd` feature, ever picks it.
    #[cfg(feature = "zstd")]
    const ACCEPT_ENCODING: &'static str = "zstd, gzip";

    /// Name the codings we can read, when that is not what `ureq` would say.
    fn accept_encoding(req: ureq::Request) -> ureq::Request {
        #[cfg(feature = "zstd")]
        {
            return req.set("Accept-Encoding", Self::ACCEPT_ENCODING);
        }
        #[cfg(not(feature = "zstd"))]
        req
    }

    /// Read a response body under [`MAX_RESPONSE_BYTES`] and parse it.
    ///
    /// The cap is on the bytes *after* decoding, so a compression bomb stops
    /// at the same place a plain oversized body does.
    fn read_json<T: for<'de> Deserialize<'de>>(resp: ureq::Response) -> Result<T> {
        #[cfg(feature = "zstd")]
        let zstd = resp.header("Content-Encoding").is_some_and(|e| e.eq_ignore_ascii_case("zstd"));
        let reader = resp.into_reader();
        #[cfg(feature = "zstd")]
        let reader: Box<dyn Read> = if zstd {
            Box::new(zstd::stream::read::Decoder::new(reader).map_err(|e| DaemonError::Transport(e.to_string()))?)
        } else {
            Box::new(reader)
        };
        let mut buf = Vec::new();
        reader.take(MAX_RESPONSE_BYTES + 1).read_to_end(&mut buf).map_err(|e| DaemonError::Transport(e.to_string()))?;
        if buf.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(DaemonError::ResponseTooLarge);
        }
        serde_json::from_slice(&buf).map_err(|e| DaemonError::Json(e.to_string()))
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let mut req = Self::accept_encoding(self.agent.get(&format!("{}{}", self.base, path)));
        if let Some(a) = &self.auth {
            req = a.apply(req);
        }
        Self::read_json(req.call().map_err(Self::map_err)?)
    }

    fn post<B: Serialize, T: for<'de> Deserialize<'de>>(&self, path: &str, body: &B) -> Result<T> {
        let mut req = Self::accept_encoding(
            self.agent.post(&format!("{}{}", self.base, path)).set("Content-Type", "application/json"),
        );
        if let Some(a) = &self.auth {
            req = a.apply(req);
        }
        let resp = req
            .send_json(serde_json::to_value(body).map_err(|e| DaemonError::Json(e.to_string()))?)
            .map_err(Self::map_err)?;
        Self::read_json(resp)
    }

    fn json_rpc<P: Serialize, T: for<'de> Deserialize<'de>>(&self, method: &str, params: P) -> Result<T> {
        #[derive(Serialize)]
        struct Req<'a, P> {
            jsonrpc: &'a str,
            id: &'a str,
            method: &'a str,
            params: P,
        }
        #[derive(Deserialize)]
        struct Resp<T> {
            result: Option<T>,
            error: Option<serde_json::Value>,
        }
        let r: Resp<T> = self.post("/json_rpc", &Req { jsonrpc: "2.0", id: "1", method, params })?;
        if let Some(e) = r.error {
            return Err(DaemonError::Status(e.to_string()));
        }
        r.result.ok_or_else(|| DaemonError::Json("missing result".into()))
    }

    /// For responses whose `status` sits beside the value being returned.
    fn ok<T>(status: &str, v: T) -> Result<T> {
        if status == "OK" {
            Ok(v)
        } else {
            Err(DaemonError::Status(status.to_string()))
        }
    }

    /// For responses that carry their own `status`.
    fn checked<T: HasStatus>(v: T) -> Result<T> {
        if v.status() == "OK" {
            Ok(v)
        } else {
            Err(DaemonError::Status(v.status().to_string()))
        }
    }

    pub fn info(&self) -> Result<Info> {
        Self::checked(self.get("/info")?)
    }

    pub fn height(&self) -> Result<Height> {
        Self::checked(self.get("/height")?)
    }

    pub fn block_header_by_height(&self, index: u64) -> Result<BlockHeader> {
        #[derive(Deserialize)]
        struct R {
            block_header: BlockHeader,
            status: String,
        }
        let r: R = self.json_rpc("getblockheaderbyheight", serde_json::json!({ "height": index }))?;
        Self::ok(&r.status, r.block_header)
    }

    pub fn last_block_header(&self) -> Result<BlockHeader> {
        #[derive(Deserialize)]
        struct R {
            block_header: BlockHeader,
            status: String,
        }
        let r: R = self.json_rpc("getlastblockheader", serde_json::json!({}))?;
        Self::ok(&r.status, r.block_header)
    }

    pub fn block_template(&self, wallet_address: &str, reserve_size: u64) -> Result<BlockTemplateResult> {
        Self::checked(self.json_rpc(
            "getblocktemplate",
            serde_json::json!({ "wallet_address": wallet_address, "reserve_size": reserve_size }),
        )?)
    }

    pub fn submit_block(&self, block_hex: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct R {
            status: String,
        }
        let r: R = self.json_rpc("submitblock", serde_json::json!([block_hex]))?;
        Self::ok(&r.status, ())
    }

    pub fn wallet_sync_data(&self, req: &SyncRequest) -> Result<WalletSyncData> {
        Self::checked(self.post("/getwalletsyncdata", req)?)
    }

    pub fn raw_blocks(&self, req: &SyncRequest) -> Result<RawBlocks> {
        Self::checked(self.post("/getrawblocks", req)?)
    }

    /// `[start, end)` block indexes, at most 5000 apart.
    pub fn global_indexes_for_range(&self, start: u64, end: u64) -> Result<GlobalIndexes> {
        Self::checked(
            self.post("/get_global_indexes_for_range", &serde_json::json!({ "startHeight": start, "endHeight": end }))?,
        )
    }

    /// One entry per input to spend (duplicates allowed), `outs_count = mixin + 1`.
    pub fn random_outs(&self, amounts: &[u64], outs_count: u64) -> Result<RandomOuts> {
        Self::checked(
            self.post("/getrandom_outs", &serde_json::json!({ "amounts": amounts, "outs_count": outs_count }))?,
        )
    }

    /// HTTP 200 either way; `status` is `OK` or `Failed` with `error`.
    pub fn send_raw_transaction(&self, tx_hex: &str) -> Result<SendResult> {
        self.post("/sendrawtransaction", &serde_json::json!({ "tx_as_hex": tx_hex }))
    }

    pub fn transactions_status(&self, hashes: &[String]) -> Result<TransactionsStatus> {
        Self::checked(self.post("/get_transactions_status", &serde_json::json!({ "transactionHashes": hashes }))?)
    }
}

/// `timestampToScanHeight(t) = (t − GENESIS_BLOCK_TIMESTAMP) / 60 − 10000`, floored at 0 (`WalletBackend::init`).
pub fn timestamp_to_scan_height(timestamp: u64) -> u64 {
    use wrkz_primitives::constants::{DIFFICULTY_TARGET, GENESIS_BLOCK_TIMESTAMP};
    if timestamp <= GENESIS_BLOCK_TIMESTAMP {
        return 0;
    }
    ((timestamp - GENESIS_BLOCK_TIMESTAMP) / DIFFICULTY_TARGET).saturating_sub(10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vectors() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
    }

    #[test]
    fn sample_responses_parse() {
        let s: WalletSyncData = serde_json::from_str(
            &std::fs::read_to_string(vectors().join("mainnet_getwalletsyncdata_4213000.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items[0].block_height, 4213000);
        let cb = s.items[0].coinbase_tx.as_ref().unwrap();
        assert_eq!(cb.hash, "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f");
        assert_eq!(cb.outputs[0].amount, 1_000_000);
        assert_eq!(cb.unlock_time, 4213040);
        assert_eq!(s.scanned_to_height, Some(4213001));

        let r: RawBlocks =
            serde_json::from_str(&std::fs::read_to_string(vectors().join("mainnet_rawblocks_302401_v5.json")).unwrap())
                .unwrap();
        assert_eq!(r.items[0].transactions.len(), 1);

        let g: GlobalIndexes = serde_json::from_str(
            &std::fs::read_to_string(vectors().join("mainnet_get_global_indexes_for_range.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(g.indexes[0].value, vec![3808773]);

        let o: RandomOuts =
            serde_json::from_str(&std::fs::read_to_string(vectors().join("mainnet_getrandom_outs.json")).unwrap())
                .unwrap();
        assert_eq!(o.outs.len(), 2);
        assert_eq!(o.outs[0].amount, 10000);
        assert_eq!(o.outs[0].outs.len(), 3);
    }

    #[test]
    fn sync_request_shape() {
        let req = SyncRequest {
            block_hash_checkpoints: vec!["aa".into()],
            start_height: 5,
            block_count: 100,
            skip_input_key_offsets: Some(true),
            ..Default::default()
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["blockHashCheckpoints"][0], "aa");
        assert_eq!(v["startHeight"], 5);
        assert_eq!(v["startTimestamp"], 0);
        assert_eq!(v["blockCount"], 100);
        assert_eq!(v["skipCoinbaseTransactions"], false);
        assert_eq!(v["skipInputKeyOffsets"], true);
        assert!(v.get("skipEmptyBlocks").is_none());
        assert_eq!(timestamp_to_scan_height(1529831318 + 60 * 20_000), 10_000);
        assert_eq!(timestamp_to_scan_height(0), 0);
    }

    #[cfg(feature = "native")]
    #[test]
    fn base_url_scheme_is_validated() {
        assert!(Daemon::new("http://node-fin.wrkz.work:17856").is_ok());
        assert!(matches!(Daemon::new("node-fin.wrkz.work:17856"), Err(DaemonError::UnsupportedScheme(_))));
        assert!(matches!(Daemon::new("ftp://node-fin.wrkz.work"), Err(DaemonError::UnsupportedScheme(_))));
        // https only when the TLS backend is compiled in
        assert_eq!(Daemon::new("https://node-fin.wrkz.work:17856").is_ok(), HTTPS_SUPPORTED);
    }

    /// A `status` other than `OK` becomes an error rather than a value.
    #[cfg(feature = "native")]
    #[test]
    fn non_ok_status_is_an_error() {
        let h: Height = serde_json::from_str(r#"{"height":1,"network_height":1,"status":"Failed"}"#).unwrap();
        assert!(matches!(Daemon::checked(h), Err(DaemonError::Status(s)) if s == "Failed"));
    }

    /// Live checks against the seed node (spec/09 acceptance 1 at stage-1 scale).
    /// Run with `cargo test -p wrkz-wallet -- --ignored`.
    #[cfg(feature = "native")]
    #[test]
    #[ignore]
    fn live_seed_node() {
        let d = Daemon::new("http://node-fin.wrkz.work:17856").unwrap();
        let info = d.info().unwrap();
        assert!(info.height > 4_213_000 && info.supports("heightRange"));
        let h = d.block_header_by_height(4213000).unwrap();
        assert_eq!(h.hash, "c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604");
        let s =
            d.wallet_sync_data(&SyncRequest { start_height: 4213000, block_count: 2, ..Default::default() }).unwrap();
        assert_eq!(s.items[0].block_height, 4213000);
        assert_eq!(
            s.items[0].coinbase_tx.as_ref().unwrap().hash,
            "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f"
        );
        let g = d.global_indexes_for_range(4213000, 4213001).unwrap();
        assert_eq!(g.indexes[0].key, "afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f");
        assert_eq!(g.indexes[0].value, vec![3808773]);
        let o = d.random_outs(&[10000, 50000], 3).unwrap();
        assert_eq!(o.outs.len(), 2);
        assert!(o.outs.iter().all(|a| a.outs.len() == 3));
        let st = d
            .transactions_status(&["afe062a7426f96d51f03b6ad8a81200f089139582ed5b176bdee96601346804f".into()])
            .unwrap();
        assert_eq!(st.transactions_in_block.len(), 1);
        let rb = d.raw_blocks(&SyncRequest { start_height: 4213000, block_count: 1, ..Default::default() }).unwrap();
        let b = wrkz_primitives::block::BlockTemplate::from_bytes(&hex::decode(&rb.items[0].block).unwrap()).unwrap();
        assert_eq!(hex::encode(b.hash().unwrap()), h.hash);
    }
}
