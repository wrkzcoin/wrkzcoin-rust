# 09 - Daemon RPC and the wallet sync contract

Source files: `src/rpc/RpcServer.cpp` (`setupRoutes` line 173, handlers
from line 900), `src/rpc/CoreRpcServerCommandsDefinitions.h`,
`src/nigel/Nigel.cpp` (the wallet's client), `src/cryptonotecore/Core.cpp`
(`getWalletSyncData` line 908, `getRawBlocks` line 1123,
`getBlockTemplate` line 2313), `include/WalletTypes.h` (JSON shapes).
Existing reference documentation: `docs/docs/daemon-rpc/http-endpoints.md`,
`docs/docs/daemon-rpc/json-rpc.md`, `docs/docs/daemon-rpc/auth-and-security.md`,
`docs/docs/guides/daemon-rpc-cookbook.md`. Live samples in `vectors/mainnet_*.json`.

The daemon RPC is HTTP with JSON bodies on port 17856. It is not consensus,
but every wallet, pool and miner speaks it, so a replacement daemon must
serve it and a replacement wallet must consume it. This document lists the
minimum a port needs and the exact JSON. The seed node
`http://node-fin.wrkz.work:17856` answers all of it over plain HTTP, which
is how the samples were taken.

## Transport rules (`RpcServer::middleware`, line 507)

- `Content-Type: application/json` bodies; responses always carry `status`
  (`"OK"` on success) except JSON-RPC, which uses `result`/`error`.
- Optional access token in `X-API-Key` or `Authorization: Bearer`.
- Per-IP rate limit, default 240 requests per minute; `429` when exceeded.
  Wallets treat `429` as "back off 20 s" (`Nigel.cpp:569`).
- Max body 2 MiB. `400` for a malformed body or an over-limit `blockCount`
  (the wallet halves its batch and retries, `Nigel.cpp:548`).
- `/sendrawtransaction` requires the node to be synced; others do not.
- Explorer-mode routes (`f_*`, `/queryblocksdetailed`) are off by default.
- CORS: `OPTIONS` on every path; `--enable-cors`.
- gzip response compression when built with zlib; `/info` reports
  `"compression"`.

## `GET /info` (also `/getinfo`)

Live sample (fields in alphabetical order as the daemon emits them):

```json
{"alt_blocks_count":0,"compression":"gzip","difficulty":52006338,
 "grey_peerlist_size":112,"hashrate":866772,"height":4213546,
 "incoming_connections_count":8,"last_known_block_index":4213544,
 "last_seed_bootstrap":1788797165,"lite":false,"lite_start_height":0,
 "major_version":7,"minor_version":0,"network_height":4213546,
 "outgoing_connections_count":3,"prune_capability_active":false,
 "prune_depth":10080,"pruned":false,"seed_nodes_count":4,
 "start_time":1788704062,"status":"OK","supported_height":4500000,
 "sync_active_peers":0,"sync_avg_batch_size":120,"sync_demoted_peers":0,
 "sync_features":["skipEmptyBlocks","base64","heightRange"],"synced":true,
 "top_block_hash":"2cbffaca2d4287c68967bcb379e8d2d206cb75b5efe810721c454f7e9d3da5b3",
 "tx_count":3510834,"tx_pool_size":0,
 "upgrade_heights":[1,40000,...,4500000],"version":"0.4.8","white_peerlist_size":10}
```

`height` and `network_height` are counts (top index + 1); the wallet
subtracts one (`Nigel.cpp:867`). `hashrate` is `difficulty / 60`. The
wallet reads `height`, `network_height`, both connection counts,
`difficulty`, `lite_start_height`, `sync_features`, `compression`, and an
optional `isCacheApi` (third-party caches only).

`GET /height` (also `/getheight`): `{"height":N,"network_height":N,"status":"OK"}`.
`/getheight` MUST NOT include a `hash` field: xmrig uses its absence to
detect a CryptoNote daemon. `GET /peers`: peer lists.

## JSON-RPC (`POST /json_rpc`)

Request `{"jsonrpc":"2.0","id":..,"method":..,"params":{..}}`; the `id` is
echoed. Methods: `getblockcount`, `getlastblockheader`, `getblockheaderbyhash`,
`getblockheaderbyheight`, `getblocktemplate`, `submitblock`, plus the
explorer-only `f_blocks_list_json`, `f_block_json`, `f_transaction_json`,
`f_on_transactions_pool_json`, `f_transactions_by_payment_id_json`.
Unknown methods return HTTP 404.

### `getblockheaderbyheight` — block header vectors

`params: {"height": index}` (a block index, not a count). Result
`{"block_header": {...}, "status":"OK"}`. Live headers at the version
boundaries; these are conformance data for stages 1 and 3:

| index | major | hash | prev_hash | nonce | timestamp | difficulty | reward | size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 1 | `877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce` | `00…00` | 70 | 0 | 1 | 1500000000000 | 197 |
| 1 | 1 | `93bb1fd850d9e904ca810cdb57935b6df45cd75fc3a86358a421e126c1ae7b51` | block 0 | 271363011 | 1529831318 | 1 | 11563301 | 333 |
| 2 | 2 | `4fc480b6507b6df08a92496f3af83dd16b5b44ea1ba76792bd4e6381696c29c3` | block 1 | 798020427 | 1529831318 | 1 | 11563298 | 442 |
| 3 | 3 | `e2c36c96876cec05e1e9b0f488eef4a0e1487ba38a2f52a3054123bab9bff5de` | block 2 | 2550627478 | 1529831318 | 60 | 11563295 | 442 |
| 4 | 4 | `bc9ecbdcde0fc6ca467025af49ba239e49148702af9503bce8627714f6974a31` | block 3 | 1935153120 | 1529831327 | 3660 | 11563292 | 442 |
| 5 | 4 | `513e3cbe87ff9ca63ee30197ac358de63ac30216b68359231917c1e10169e1cb` | block 4 | 2928845235 | 1529831456 | 24806 | 11563290 | 408 |
| 302400 | 4 | `12ef860f79ca76288333fa0227cfc0e4a9012ab382a44f437fbdbe8b846607ba` | `a2ef2414…eb67` | 113554 | 1548322468 | 7624528 | 10759190 | 4020 |
| 302401 | 5 | `e9e99274c55fe07f96ed18c6292f44aa570dfa543114b759716b572324c0f765` | block 302400 | 13994 | 1548325067 | 7767351 | 10758997 | 1978 |
| 600000 | 5 | `887c60169f8627bd7e8d128cd32b57259731b33560c20b8e9a1b5c4fb979267a` | `bd752b08…c726` | 429497261 | 1566359370 | 65771133 | 10022402 | 13426 |
| 600001 | 6 | `331f2464aa1a4abb6505802643d1e6a259c4eee9cc0305c1eedd7618bfad755b` | block 600000 | 2469645125 | 1566359399 | 57719958 | 10022204 | 655 |
| 1000000 | 6 | `3d2d2dd60aec0f03b53d9b4bd4ef47902114fca5749bcda95eb45a8d9a1c0682` | `28021607…5698` | 3580721724 | 1590601445 | 107917495 | 9113541 | 462 |
| 1000001 | 7 | `38b8983c2fe4953dfd1857232702b3ab83ae25139a864d6e1160b1739633f144` | block 1000000 | 32173218 | 1590601653 | 110104052 | 9113539 | 971 |
| 4213000 | 7 | `c59f3ff8bd3ff4c8db795d707287241ed4904e9b8af2151c0a767acb04ed4604` | `3d3c595b…d678` | 7822 | 1788894799 | 24880685 | 1000000 | 211 |

`size` here is `block_size` as reported (the stored cumulative size). The
raw blobs for 0–5, 302401, 600001, 1000001 and 4213000 are in `vectors/`.
`num_txes` counts the coinbase (1 for an empty block).

### `getblocktemplate` / `submitblock`

`params: {"wallet_address": "...", "reserve_size": n}` or
`{"wallet_address": "...", "extra_nonce": hex}`. Result
`{"blocktemplate_blob": hex, "difficulty": d, "height": h, "reserved_offset": o, "status":"OK"}`.
`reserved_offset` is the byte offset of the reserved bytes inside the blob
(`RpcServer.cpp:1670`: position of the tx public key + 32 + 2). Miners MUST
apply the merge-mining tag fix-up from `07-blocks-consensus.md` before
hashing. `submitblock` takes `params: [hex]` and answers
`{"status":"OK"}` or an error `-7 Block not accepted`.

## Wallet sync endpoints

### `POST /getwalletsyncdata`

Request as the wallet sends it (`Nigel.cpp:489`):

```json
{"blockHashCheckpoints": ["<hash>", ...],   // newest first: up to 50 recent block hashes it holds,
                                             // then hashes of blocks it processed, then one hash per 5000 blocks
 "startHeight": 0,                           // scan start index, used when checkpoints match nothing
 "startTimestamp": 0,                        // alternative to startHeight for new wallets; 0 = unused
 "blockCount": 100,                          // 100 .. 1000 (--rpc-max-block-count), daemon caps at 10000
 "skipCoinbaseTransactions": false,
 "skipInputKeyOffsets": true,                // optional; daemon may omit key_offsets in inputs
 "skipEmptyBlocks": true,                    // only after /info advertises it, and only with skipCoinbaseTransactions
 "encoding": "base64",                       // only after /info advertises it
 "endHeight": 0}                             // exclusive bound; only after /info advertises heightRange
```

Start resolution (`Core::resolveWalletSyncStartIndex` via `getWalletSyncData`):
the first checkpoint hash the daemon knows on its main chain wins; the
response starts at the block **after** it. If none matches, `startHeight`
is used, or the first block at or after `startTimestamp` if a timestamp was
given. A wallet with no checkpoints and `startHeight 0` therefore gets
block 0 first.

Response (`WalletTypes.h:581-663`, live sample in
`vectors/mainnet_getwalletsyncdata_4213000.json`):

```json
{"items": [
   {"blockHash": hex, "blockHeight": 4213000, "blockTimestamp": 1788894799,
    "coinbaseTX": {"hash": hex, "outputs": [{"amount": 1000000, "key": hex}],
                   "txPublicKey": hex, "unlockTime": 4213040},          // absent when skipCoinbaseTransactions
    "transactions": [
       {"hash": hex, "outputs": [{"amount": n, "key": hex}], "txPublicKey": hex, "unlockTime": n,
        "paymentID": "" | 16 or 64 hex chars,
        "inputs": [{"amount": n, "k_image": hex, "key_offsets": [...]}]}]}],
 "scannedToHeight": 4213001,     // highest index the daemon looked at (may exceed the last item)
 "synced": false,                // true when the wallet is at the daemon's top
 "topBlock": {"hash": hex, "height": n},   // present when synced, so the wallet records the tip
 "status": "OK"}
```

`paymentID` is the plaintext long id, or the 16-hex-char ciphertext of an
encrypted short id (the daemon cannot decrypt it), or empty. The daemon
never reports legacy plaintext short ids. Outputs carry no global indexes
here; the wallet fetches those separately. A response is capped at 8 MiB
of assembled blocks; at least one block is always returned.

### `POST /getrawblocks`

Same request fields (checkpoints, `startHeight`, `startTimestamp`,
`blockCount`, `skipCoinbaseTransactions`). Response
`{"items": [{"block": hex, "transactions": [hex]}], "synced": bool, "topBlock": {...}, "status":"OK"}`.
The wallet uses this path only when configured to (`m_useRawBlocks`,
`Nigel.cpp:519`) and falls back to `/getwalletsyncdata` on 404/500.
For a port this is the endpoint to pull *conformance data* from: it
returns the exact block and transaction bytes.

### `POST /get_global_indexes_for_range`

Request `{"startHeight": a, "endHeight": b}` (indexes, `b` exclusive, at
most `--rpc-max-global-index-range` = 5000 apart). Response
`{"indexes": [{"key": <tx hash>, "value": [global index per output]}], "status":"OK"}`
(sample: `vectors/mainnet_get_global_indexes_for_range.json`). The wallet
asks for a 10-block window around the block of interest
(`GLOBAL_INDEXES_OBSCURITY`) so the daemon cannot tell which transaction it
owns.

### `POST /getrandom_outs`

Request `{"amounts": [a1, a2, ...], "outs_count": n}` with one entry per
input the wallet will spend (duplicates allowed) and `n = mixin + 1`.
Response
`{"outs": [{"amount": a, "outs": [{"global_amount_index": i, "out_key": hex}, ...]}, ...], "status":"OK"}`
(sample: `vectors/mainnet_getrandom_outs.json`). The daemon returns up to
`n` unlocked outputs per amount chosen uniformly over the whole history of
that denomination (`DatabaseBlockchainCache::getRandomOutsByAmount`), and
returns fewer when fewer exist; an older daemon answers `400` with
`errorCode` `CANT_GET_FAKE_OUTPUTS` instead. The wallet may receive its
own output among them and skips it.

### `POST /sendrawtransaction`

Request `{"tx_as_hex": hex}`. Response `{"status":"OK"}` or
`{"status":"Failed","error": "..."}` (HTTP 200 either way). The daemon runs
pool admission (`06-transactions.md`) and relays on success.

### `POST /get_transactions_status`

Request `{"transactionHashes": [hex]}`. Response
`{"transactionsInPool": [...], "transactionsInBlock": [...], "transactionsUnknown": [...], "status":"OK"}`.
Used by the wallet to detect its own unconfirmed sends being dropped.

### Others

`/queryblockslite`, `/get_pool_changes_lite`, `/get_o_indexes`,
`/queryblocksdetailed` serve the legacy `WalletGreen`/`wrkz-service` stack
and explorers; shapes in `CoreRpcServerCommandsDefinitions.h`. A port that
does not ship the legacy wallet service can omit them.

## Wallet API and wallet service

The wallet-facing HTTP API (`wrkz-wallet-api`) and the legacy JSON-RPC
service (`wrkz-service`) are application contracts, fully documented at
`docs/docs/wallet-api/endpoints.md` and
`docs/docs/wallet-service-json-rpc/methods.md`. A port of the wallet
library should reproduce the C API in `10-wallet.md` first; these two
servers are thin layers over it (or over `WalletGreen`).

## Acceptance for this document

1. Stage 2 (wallet): against the seed node, `/info` parses, a fresh wallet
   syncs from `startHeight = 4213000` with checkpoints and receives the
   blocks in `vectors/mainnet_getwalletsyncdata_4213000.json`, then
   `/get_global_indexes_for_range` for `[4213000, 4213001)` returns
   `[3808773]` for the coinbase hash shown in the sample.
2. Stage 3 (daemon): the port's `/info`, `/height`, `/getheight`,
   `getblockheaderbyheight`, `getblocktemplate`, `submitblock`,
   `/getwalletsyncdata`, `/getrawblocks`, `/get_global_indexes_for_range`,
   `/getrandom_outs`, `/sendrawtransaction` and `/get_transactions_status`
   produce byte-comparable JSON (ignoring key order and the node-specific
   fields of `/info`) to the C++ daemon for the same requests, and the
   unmodified C++ CLI wallet (`wrkz-wallet`, built from `src/zedwallet++`) syncs against it and sends a
   transaction through it.
3. xmrig in solo mode mines a block through the port. Against this chain that
   means the built-in stratum server (`--stratum-bind-port`,
   `docs/DAEMON.md#mining`): xmrig's `--daemon` mode cannot read a Forknote
   template, against the C++ daemon or this one.
