# RPC interfaces

This port serves the same three RPC interfaces as the C++ WrkzCoin (the daemon RPC, the wallet API and the wallet service's JSON-RPC) with the same routes, methods and JSON; this page gives their addresses and authentication, what the port adds, and where it deliberately differs.

The request and response of every method are documented once, on the C++ site ([daemon RPC](https://docs.wrkz.work/daemon-rpc/overview/), [wallet API](https://docs.wrkz.work/wallet-api/endpoints/), [wallet service](https://docs.wrkz.work/wallet-service-json-rpc/methods/)), and apply here unchanged unless this page says otherwise. The wire contract a port must keep, with live samples, is [spec 09](../spec/09-rpc-and-wallet-sync.md).

## The three surfaces

| Interface | Program | Default address | Authentication | Reference |
| --- | --- | --- | --- | --- |
| Daemon RPC | `wrkz-node` | `127.0.0.1:17856` | None by default; with `--rpc-access-token`, `X-API-Key` or `Authorization: Bearer` | [Overview](https://docs.wrkz.work/daemon-rpc/overview/), [JSON-RPC](https://docs.wrkz.work/daemon-rpc/json-rpc/), [HTTP endpoints](https://docs.wrkz.work/daemon-rpc/http-endpoints/), [Auth and security](https://docs.wrkz.work/daemon-rpc/auth-and-security/) |
| Wallet API | `wrkz-wallet-api` | `127.0.0.1:7856` | `X-API-KEY` header, the required `--rpc-password` | [Endpoints](https://docs.wrkz.work/wallet-api/endpoints/) |
| Wallet service | `wrkz-service` | `127.0.0.1:7856`, `POST /json_rpc` | A `password` member in every request, the required `--rpc-password` | [Methods](https://docs.wrkz.work/wallet-service-json-rpc/methods/) |

The wallet API and the wallet service both default to port 7856, as their C++ counterparts do (`SERVICE_DEFAULT_PORT`, `src/config/CryptoNoteConfig.h:539`), so running both on one host needs `-p` on one or `--bind-port` on the other. The transaction proof-of-work server is a fourth, smaller HTTP interface of this port's own; see [Transaction PoW server](../wallets/txpow-server.md).

Each program listens on loopback unless told otherwise, and each can also serve on a local IPC socket (not on Windows): `--rpc-ipc-path` for the daemon and the wallet API, `--bind-ipc-path` (instead of the port) for the service.

## Parity with the C++

The daemon RPC is route for route `src/rpc/RpcServer.cpp`, the wallet API route for route `src/walletapi/ApiDispatcher.cpp`, and the service method for method `src/walletservice/PaymentServiceJsonRpcServer.cpp` and `src/walletservice/WalletService.cpp`. Beyond the set of routes:

- **The same bytes.** The C++ builds every body with `nlohmann::json`, which emits object keys in ascending byte order; this port sorts the same way, so for the same values it produces the same bytes, key order included. Amounts, difficulties, rewards and global indexes are 64-bit integers end to end and never pass through a float.
- **The same status codes and errors**: 401, 403, 413, 429 and 503 where the C++ returns them, the C++'s error bodies and messages (including its wording where it is wrong, such as `f_transaction_json` saying "Block hash" of a transaction hash), and the daemon's own JSON-RPC error codes.
- **The same heights.** Which fields are counts (`/info` `height`, `getblockcount`, `getblocktemplate` `height`) and which are indexes (`getblockheaderbyheight`, every wallet sync field) is kept exactly, because getting it wrong desynchronises every wallet.
- **The same version.** `wrkz-node --version` says which C++ release its RPC answers as, currently WrkzCoin 0.4.8.

Routes that other CryptoNote daemons have and `src/rpc/RpcServer.cpp` does not (`/fee`, `/getblocks`, `/gettransactions`, `on_getblockhash`, `getblock` and the like) are a 404 here, as they are on the live seed node.

`wrkz-rpc-diff` checks the claim against a running C++ daemon ([Diagnostics](../tools/diagnostics.md#wrkz-rpc-diff)).

## Daemon RPC

The routes, all registered in `RpcServer::setupRoutes` (`src/rpc/RpcServer.cpp:173`):

| Method | Path |
| --- | --- |
| `GET` | `/info`, `/getinfo`, `/height`, `/getheight`, `/peers` |
| `GET`, `POST` | `/json_rpc` |
| `POST` | `/sendrawtransaction`, `/getrandom_outs`, `/getwalletsyncdata`, `/getrawblocks`, `/get_global_indexes_for_range`, `/get_transactions_status`, `/queryblockslite`, `/get_pool_changes_lite`, `/get_o_indexes` |
| `POST` | `/queryblocksdetailed` (explorer mode only) |
| `POST` | `/console` (the IPC socket only; see [Console and IPC](../node/console-and-ipc.md)) |
| `OPTIONS` | any path, for CORS |

JSON-RPC methods: `getblocktemplate`, `submitblock`, `getblockcount`, `getlastblockheader`, `getblockheaderbyhash` and `getblockheaderbyheight`; with `--daemon-mode explorer`, also `f_blocks_list_json`, `f_block_json`, `f_transaction_json`, `f_on_transactions_pool_json` and `f_transactions_by_payment_id_json`. An explorer method or route on a standard daemon is a 403 that names `--daemon-mode explorer`.

The limits, each an option of `wrkz-node`. All but `--rpc-max-connections-per-ip` and `--rpc-workers` are the C++'s options, with its names and defaults:

| Option | Default | Past it |
| --- | --- | --- |
| `--rpc-max-rpm` | 240 requests a minute per address, loopback exempt | 429 |
| `--rpc-max-connections-per-ip` | 8 open connections per address, loopback and `--rpc-trust-proxy` exempt | 429 |
| `--rpc-max-body-bytes` | 2 MiB | 413 |
| `--rpc-max-block-count` | 1000 blocks per sync request | 400 |
| `--rpc-max-global-index-range` | 5000 | 400 |
| `--rpc-read-timeout`, `--rpc-write-timeout` | 10 seconds | |
| `--rpc-workers` | 16 worker threads; a full queue is a 503 | |

`/sendrawtransaction` needs the node synced and is a 503 before. `/getwalletsyncdata` answers are cached for the next wallet asking for the same range (`--rpc-sync-cache-size`, 64 MiB). `wrkz-node --help` lists every RPC option, and [Configuration](../node/configuration.md) describes them.

### Compression

A response is gzipped when the client sends `Accept-Encoding: gzip`, the body is JSON and it is at least 1024 bytes long: what `cpp-httplib` does when the C++ is built with zlib, which is how public nodes are built. `/info` reports `"compression":"gzip"` accordingly. The wallet API gzips the same way. The wallet service never does, as the C++ service's server does not.

### `/metrics` and `/health`

Two routes are this port's own, and exist only when asked for; otherwise each is the 404 of any unrouted path.

`GET /metrics` (`--enable-metrics`)
:   The node's state and the RPC server's own counters in the Prometheus text format: height, network height, difficulty, connections, pool size, sync state, requests answered, responses by status class, gzipped responses, and the sync cache's size, hits and misses. The metric names start with `wrkz_`.

`GET /health` (`--enable-health`)
:   `200` with `{"status":"OK","synced":true,…}` once the node is synced, `503` with `"status":"SYNCING"` before; the body also carries `height`, `network_height` and `peers`.

Both go through the same access token and rate limit as every route, so a scraper sends the token as `Authorization: Bearer`, which Prometheus supports natively.

```sh
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:17856/metrics
```

## Differences from the C++

Each of these is deliberate, and each is either off by default or changes nothing a correct client relies on.

### Daemon RPC

- **`/metrics` and `/health`** are this port's own (above), off by default.
- **`--rpc-max-connections-per-ip`** (8 open connections per address, then 429) and **`--rpc-workers`** (a fixed pool of 16, then 503) are this port's own bounds on the server; the C++ has neither option. Loopback and `--rpc-trust-proxy` are exempt from the first; raise it for an explorer back end on another host.
- **The access token is checked before the body is parsed** on `/json_rpc`; the C++ parses first. Without the token every body is a 401, rather than some being a parse error.
- **`/getrandom_outs` is bounded**: `outs_count` at most 100, `amounts` at most 10,000 entries and 100,000 decoys in all; past any of them is a 400 in the shape of a bad parameter. The C++ narrows `outs_count` to 16 bits (`src/rpc/RpcServer.cpp:1171`) and looks up whatever it was asked for. Wallets ask for at most eight per input.
- **`f_transaction_json`'s `result.tx.extra`** is the hex of the transaction's extra bytes. The C++ (`src/rpc/RpcServer.cpp:2274`) hexes the `std::vector`'s own control block instead and emits 48 characters of heap pointers, which leaks process addresses and says nothing about the transaction.
- **`/get_global_indexes_for_range` entries** come in block order, then transaction order. The C++ walks an `unordered_map` (`src/rpc/RpcServer.cpp:1520`), so its order is whatever that container gave; a client that depends on it is already broken against the C++.
- **A block or explorer lookup below a lite or pruned node's floor** is a 500 whose message names the mode and the height the node's data starts at. The C++ reaches an unguarded `std::map::at` (`src/cryptonotecore/DatabaseBlockchainCache.cpp:2395`) and answers 500 `"Internal server error: map::at"`. See [Lite, pruned and explorer](../node/reduced-modes.md).
- **`--rpc-stream-threshold KB`** compresses a body that large straight into the socket with `Transfer-Encoding: chunked`, where the C++ always sends `Content-Length`. Off (0) by default.
- **`Content-Encoding: zstd`** is offered ahead of gzip by a daemon built with the `zstd` feature, to a client that asks for it; only this port's wallet asks. Releases are built without it.
- **`--decoy-selection recent`** makes `/getrandom_outs` favour recent outputs. The default, `uniform`, is what every C++ node does.

### Wallet API

- **Coinbase transactions are scanned by default.** The C++ skipped them unless `--scan-coinbase-transactions` was given; that flag is still accepted and changes nothing, and `--skip-coinbase-transactions` turns scanning off.
- **`--threads` defaults to one per core, at most 16**; the C++ default is every core.
- **Nothing secret is logged.** Each request is logged with its peer, method, path, status and time. The C++ prints every request body, and on a wrong key prints the expected key next to it (`src/walletapi/ApiDispatcher.cpp`).
- **`--sync-windows` and `--sync-max-blocks`** are this port's own options, for a faster first sync from a daemon that offers the `heightRange` and `skipEmptyBlocks` sync features.

### Wallet service

- **The container is the modern one.** The API is the C++ service's, but `wrkz-service` opens the `WalletBackend` container that `wrkz-wallet`, `wrkz-wallet-api` and Pluton write, not WalletGreen's. A WalletGreen container from the C++ service must be converted with the C++ `wrkz-walletupgrader` first. It is the one thing that is not drop-in; see [Wallet service](../wallets/service.md).
- **`getBlockHashes` and the block-ranged `getTransactions` and `getTransactionHashes`** fetch from the daemon the block hashes a modern container does not keep (it holds the last 100 and sparse checkpoints), and `blockCount` is capped at 1000.
- **`export`** writes the modern container's JSON.
- **Transaction notifications:** an incoming transaction is announced when it is mined, not while it is in the pool, because the modern container does not follow other people's pool transactions; and a send still waiting for its block when the service starts gets its `--tx-confirmed-notify`, where the C++ would say nothing.
- **The password is compared in constant time**, where `src/walletservice/PaymentServiceJsonRpcServer.cpp` uses `!=`. Both compare a slow hash of it, so no answer changes.

The wallet API's `nodeFee` and the service's `getFeeInfo` and `getNodeFeeInfo` always report no fee, as they do in the C++ against a WrkzCoin daemon, which serves no `/fee` route.
