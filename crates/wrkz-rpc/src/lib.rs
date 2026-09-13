// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The daemon's HTTP and JSON-RPC surface (spec/09-rpc-and-wallet-sync.md;
//! `src/rpc/RpcServer.cpp`).
//!
//! Every wallet, pool, miner and explorer speaks this API, so a replacement
//! daemon has to serve it field for field. This crate is that surface, and
//! nothing else: it holds no chain state and takes no locks of its own. The
//! node it serves is behind [`NodeApi`], so the handlers are pure functions of
//! a snapshot, a test can drive them from a fake, and the P2P layer supplies
//! its own numbers to `/info` without this crate depending on it.
//!
//! - [`json`] — a strict JSON value, parser and writer. Keys are emitted in
//!   ascending byte order, which is what `nlohmann::json` does, and `u64`
//!   never passes through `f64`.
//! - [`http`] — HTTP/1.1 request parsing and response writing, plus a minimal
//!   client for the comparison tool. No framework.
//! - [`server`] — the listener, a bounded worker pool, the route table and the
//!   middleware.
//! - [`console`] — `POST /console` on the IPC socket, for `wrkz-node attach`,
//!   and the [`console::ConsoleSlot`] the daemon installs its console in.
//! - [`api`] — [`NodeApi`] and the value types the handlers print.
//! - [`node`] — [`ChainNode`], [`NodeApi`] over `wrkz-chain` and `wrkz-mempool`.
//! - [`handlers`] / [`jsonrpc`] — one function per C++ handler.
//! - [`diff`] / [`probe`] — the structural comparison behind `wrkz-rpc-diff`.
//! - [`notify`] — the `--*-notify` hook runner the daemon, `wrkz-wallet-api`
//!   and `wrkz-service` share (`common/Notifier.cpp`).
//!
//! # Field order
//!
//! **Guaranteed.** The C++ builds every body with `nlohmann::json`, whose
//! default object is a `std::map<std::string, …>`; `dump()` therefore emits
//! keys in ascending byte order regardless of the order the handler set them
//! in. [`json::Json::to_string`] sorts the same way, so for the same values
//! this crate produces the same bytes — key order included — and every sample
//! in `spec/vectors` confirms the C++ side of that. Arrays keep the order the
//! handler built them in, on both sides.
//!
//! One exception is inherent: `/get_global_indexes_for_range` walks an
//! `unordered_map` in the C++ (`RpcServer.cpp:1508`), so the *order of its
//! entries* is whatever that container happened to give. Ours is block order,
//! then transaction order. A caller that depends on the order of those entries
//! is already broken against the C++.
//!
//! # Endpoints
//!
//! Every route below is one `RpcServer.cpp` routes; nothing else is served.
//!
//! | Route | C++ | spec/09 |
//! | --- | --- | --- |
//! | `GET /info`, `GET /getinfo` | `RpcServer::info`, `:891`; routed `:290`, `:299` | yes |
//! | `GET /height`, `GET /getheight` | `RpcServer::height`, `:1002`; routed `:291`, `:300` | yes |
//! | `GET /peers` | `RpcServer::peers`, `:1016` | mentioned |
//! | `GET`/`POST /json_rpc` | the `jsonRpc` lambda, `:194` | yes |
//! | `POST /sendrawtransaction` | `RpcServer::sendTransaction`, `:1085` | yes |
//! | `POST /getrandom_outs` | `RpcServer::getRandomOuts`, `:1152` | yes |
//! | `POST /getwalletsyncdata` | `RpcServer::getWalletSyncData`, `:1207` | yes |
//! | `POST /get_global_indexes_for_range` | `RpcServer::getGlobalIndexes`, `:1459` | yes |
//! | `POST /queryblockslite` | `RpcServer::queryBlocksLite`, `:2404` | mentioned only |
//! | `POST /get_transactions_status` | `RpcServer::getTransactionsStatus`, `:2560` | yes |
//! | `POST /get_pool_changes_lite` | `RpcServer::getPoolChanges`, `:2621` | mentioned only |
//! | `POST /queryblocksdetailed` | `RpcServer::queryBlocksDetailed`, `:2731` (explorer) | mentioned only |
//! | `POST /get_o_indexes` | `RpcServer::getGlobalIndexesDeprecated`, `:2938` | mentioned only |
//! | `POST /getrawblocks` | `RpcServer::getRawBlocks`, `:2966` | yes |
//! | `POST /console` | `RpcServer::console`, `:1062`; routed `:325`, **IPC socket only** | no |
//! | `OPTIONS *` | `RpcServer::handleOptions`, `:864` | yes (CORS) |
//!
//! JSON-RPC methods (`RpcServer.cpp:206-256`):
//!
//! | Method | C++ | Mode |
//! | --- | --- | --- |
//! | `getblocktemplate` | `RpcServer::getBlockTemplate`, `:1553` | standard |
//! | `submitblock` | `RpcServer::submitBlock`, `:1691` | standard |
//! | `getblockcount` | `RpcServer::getBlockCount`, `:1764` | standard |
//! | `getlastblockheader` | `RpcServer::getLastBlockHeader`, `:1779` | standard |
//! | `getblockheaderbyhash` | `RpcServer::getBlockHeaderByHash`, `:1832` | standard |
//! | `getblockheaderbyheight` | `RpcServer::getBlockHeaderByHeight`, `:1902` | standard |
//! | `f_blocks_list_json` | `RpcServer::getBlocksByHeight`, `:1975` | explorer |
//! | `f_block_json` | `RpcServer::getBlockDetailsByHash`, `:2032` | explorer |
//! | `f_transaction_json` | `RpcServer::getTransactionDetailsByHash`, `:2166` | explorer |
//! | `f_on_transactions_pool_json` | `RpcServer::getTransactionsInPool`, `:2298` | explorer |
//! | `f_transactions_by_payment_id_json` | `RpcServer::getTransactionHashesByPaymentId`, `:2341` | explorer |
//!
//! ## Routes the C++ does not have
//!
//! `/fee`, `/getpeers`, `/getblocks`, `/gettransactions`, `/queryblocks`,
//! `/get_pool_changes`, and the JSON-RPC methods `on_getblockhash`,
//! `getblocksbyheights`, `getblockdetailsbyheight`, `getblock`, `getblocks`,
//! `gettransaction`, `gettransactionspool` and `getcurrencyid` exist on other
//! CryptoNote daemons. `RpcServer.cpp` routes none of them, and the live seed
//! node answers **404** for every one (verified). Serving them would be a
//! divergence, so this crate answers 404 too, and
//! `server::tests::the_route_table_is_the_cpp_route_table` pins that.
//!
//! One route exists only on the local IPC socket, in the C++ and here:
//! `POST /console` (`RpcServer.cpp:318-326`; [`console`]). Over TCP that path
//! is a 404 like any unrouted one, token or no token.
//!
//! # Heights and indexes
//!
//! spec/09 is explicit about which is which, and getting it wrong desynchronises
//! every wallet:
//!
//! | Field | Kind |
//! | --- | --- |
//! | `/info` `height`, `network_height`, `supported_height` | **count** (top index + 1) |
//! | `/info` `last_known_block_index`, `lite_start_height` | index |
//! | `/height` `height`, `network_height` | **count** |
//! | `getblockcount` `count` | **count** |
//! | `getblocktemplate` `height` | **count** |
//! | `getblockheaderbyheight` `params.height`, `block_header.height` | index |
//! | `/getwalletsyncdata` `startHeight`, `endHeight`, `blockHeight`, `scannedToHeight`, `topBlock.height` | index |
//! | `/get_global_indexes_for_range` `startHeight`, `endHeight` | index, `endHeight` exclusive |
//! | `queryblockslite` `startHeight`, `currentHeight`, `fullOffset` | index |
//!
//! Amounts, difficulties, rewards and global indexes are `u64` end to end;
//! nothing in this crate converts one to `f64`. The single floating-point
//! field in the whole surface is `f_block_json`'s `penalty`, which is a `double`
//! in the C++ too (`RpcServer.cpp:2148`).
//!
//! # Transport
//!
//! - `Content-Type: application/json` on every response (`RpcServer.cpp:534`).
//! - `X-API-Key`, else `Authorization: Bearer` (`:548`); a mismatch is **401**
//!   `{"status":"Failed","error":"Unauthorized RPC request"}`.
//! - Per-IP rate limit, default 240/minute, loopback exempt (`:570`); over it is
//!   **429**. Wallets read 429 as "back off 20 s" (spec/09).
//! - Body cap, 2 MiB by default (`:536`): **413**. A malformed body is **400**
//!   with the C++'s two-part message; an over-limit `blockCount` is **400**
//!   `"blockCount exceeds rpc-max-block-count"`, which the wallet answers by
//!   halving its batch.
//!   Only the first 256 bytes of a malformed body are echoed back.
//! - `/json_rpc` checks the access token *before* it parses the body (the C++
//!   parses first), so without the token every body is **401**.
//! - `/getrandom_outs`: `outs_count` at most 100, `amounts` at most 10,000
//!   entries and 100,000 decoys in all; past any of them is **400** in the
//!   `failRequest` shape. Wallets ask for `mixin + 1` ≤ 8 per input.
//! - Connections: one address may hold 8 at once (loopback, and everyone
//!   behind `--rpc-trust-proxy`, exempt); the next is **429** from the
//!   acceptor. A full queue is **503** with `Connection: close`. The request
//!   head must arrive within the read timeout, and the body within the read
//!   timeout plus its length at 32 KiB/s.
//! - `/sendrawtransaction` needs the node synced (`:576`): **503** otherwise.
//! - Explorer routes on a `Standard` daemon: **403** with the
//!   "--daemon-mode explorer" message.
//! - CORS: `OPTIONS` on every path, and `Access-Control-Allow-Origin` on every
//!   response, only when `--enable-cors` set a header (`:529`, `:864`).
//! - Response size: `/getwalletsyncdata` and `/getrawblocks` are capped at 8 MiB
//!   of assembled blocks, checked *after* appending so at least one block always
//!   goes out ([`node::MAX_RESPONSE_BYTES`]).
//! - Status strings: `"OK"`, `"Failed"`, and `"BUSY"` — the last only from
//!   `/info` while the chain reorganises (`RpcServer.cpp:988`), with HTTP 503.
//! - JSON-RPC errors are HTTP **200** with `{"error":{"code","message"},"jsonrpc"}`
//!   and the daemon's own codes: −1 bad parameter, −2 height above the tip,
//!   −3 reserve too big, −4 bad address, −5 template failed / block not found,
//!   −6 blob not hex, −7 block not accepted, −9 reorganising (503). The
//!   JSON-RPC 2.0 codes of `src/rpc/JsonRpc.h` (−32600 …) belong to the wallet
//!   service and appear nowhere in the daemon.
//! - gzip: a response is compressed when the client sends
//!   `Accept-Encoding: gzip`, the body is JSON and at least
//!   [`http::MIN_GZIP_BYTES`] long — what `cpp-httplib` does when the C++ is
//!   built with zlib, which is how public nodes are built. `/info` reports
//!   `"compression":"gzip"` accordingly (`RpcServer.cpp:66`).
//! - `/getwalletsyncdata` answers are cached, as the C++ caches them
//!   (`--rpc-sync-cache-size`, 64 MiB by default; [`sync_cache`]).
//! - `GET /metrics` ([`metrics`]) is this port's own and exists only with
//!   `--enable-metrics`; otherwise it is a 404 like any unrouted path.
//!
//! # Lite and pruned nodes
//!
//! A daemon started with `--lite` or `--prune` keeps block bodies only for part
//! of the chain ([`api::BodyPolicy`]). Consensus is untouched — it validates
//! every block it applies exactly as a full node does — but three groups of
//! endpoints have to answer differently, and each does what the C++ does:
//!
//! - **Wallet sync** (`/getwalletsyncdata`, `/getrawblocks`, `queryblockslite`)
//!   raises the caller's start height to the floor and answers from there, with
//!   no error (`RpcServer.cpp:1248-1253`, `:3006-3011`). A wallet has been told
//!   the floor by `/info`'s `lite_start_height` and clamps its own scan to it.
//! - **`/get_global_indexes_for_range`** on a *lite* node is refused outright
//!   below the lite height: HTTP **400**, `"This node is a lite node and stores
//!   no transaction data below height H"` (`:1509-1518`). A pruned node is not
//!   refused — pruning drops bodies and keeps every index.
//! - **Block and explorer lookups** below the floor report an error rather than
//!   an answer. The C++ reaches an unguarded `std::map::at` here and returns
//!   **500** `"Internal server error: map::at"`
//!   (`DatabaseBlockchainCache.cpp:2395`); this returns the same status with a
//!   message that names the mode and the height its data starts at, because a
//!   caller cannot act on the C++'s.
//!
//! `/info` reports `lite`, `lite_start_height`, `pruned`, `prune_depth` and
//! `prune_capability_active` from the state's own configuration, so they
//! describe the database that is running rather than a default.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use wrkz_chain::{ChainState, Checkpoints};
//! use wrkz_mempool::TransactionPool;
//! use wrkz_rpc::node::{serving_config, ChainNode};
//! use wrkz_rpc::server::{self, ServerConfig};
//! use wrkz_storage::MemStore;
//!
//! let chain = ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet())?;
//! let node = Arc::new(ChainNode::standalone(chain, TransactionPool::new(Default::default())));
//! let server = server::start(node, ServerConfig { bind: "127.0.0.1:0".into(), ..Default::default() })?;
//! println!("listening on {}", server.local_addr());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod api;
pub mod base64;
pub mod console;
pub mod diff;
pub mod events;
pub mod handlers;
pub mod http;
pub mod ipc;
pub mod json;
pub mod jsonrpc;
pub mod log;
pub mod metrics;
pub mod node;
pub mod notify;
pub mod probe;
pub mod server;
pub mod sync_cache;

pub use api::{ApiError, NodeApi};
pub use json::Json;
pub use node::{ChainNode, MinedBlock, P2pSnapshot};
pub use server::{start, RpcMode, RunningServer, ServerConfig};

/// What `/info` reports as `version`, and what the seed node reports today.
///
/// The C++ takes it from `PROJECT_VERSION` (`version.h`). A port that claims a
/// version it does not implement would mislead the "out of date" warning every
/// wallet shows, so this tracks the C++ release this surface was written
/// against.
pub const DAEMON_VERSION: &str = "0.4.8";
