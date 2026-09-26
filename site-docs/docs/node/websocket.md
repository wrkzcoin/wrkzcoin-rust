# WebSocket events

With `--enable-websocket`, `wrkz-node` serves `GET /ws` on its RPC port: what
happens to the chain and the pool, as a WebSocket stream. It carries the same
topics and the same JSON as the [ZMQ socket](zmq-and-hooks.md#zmq), on a port
a browser, a reverse proxy and TLS can all reach. The wallets in this
repository follow it, so a synced wallet hears of a block the moment its node
has it instead of asking every two seconds.

The C++ `Wrkzd` has no WebSocket; this is the port's own, and off by default.

```sh
wrkz-node --data-dir ./wrkz-rust --enable-websocket
# ws://127.0.0.1:17856/ws
```

| Option | Default | What it does |
| --- | --- | --- |
| `--enable-websocket` | off | serve `GET /ws` on the RPC port. Off, `/ws` is the 404 of any unrouted path |
| `--ws-max-clients N` | 128 | subscribers at once; past it an upgrade is a `503` |
| `--ws-max-clients-per-ip N` | 4 | from one address, loopback and the IPC socket exempt; past it a `429`. `0` is no per-address cap |

The three are also configuration-file keys, `enable-websocket`,
`ws-max-clients` and `ws-max-clients-per-ip`.

## Messages

Every message is a text frame holding one JSON object,
`{"topic":"…","data":{…}}`. `data` is, byte for byte, the body the ZMQ socket
publishes under the same topic; `height` is a block's index.

| Topic | `data` | Sent when |
| --- | --- | --- |
| `hello` | `{"height":N,"hash":"…","topics":[…]}` | first, once: the tip, and the topics this stream carries |
| `hashblock` | `{"height":N,"hash":"…"}` | a block joins the main chain |
| `chain_main` | `{"height":N,"hash":"…","transaction_hashes":[…]}` | straight after it; the coinbase first |
| `hashblock_alt` | `{"height":N,"hash":"…"}` | a block is kept on an alternative chain |
| `chainswitch` | `{"common_root_height":R,"hashes":[…]}` | a reorganisation; the common root first |
| `txpool_add` | `{"hashes":["…"]}` | a transaction enters the pool |
| `txpool_del` | `{"hashes":[…],"reason":"InBlock"}`, or `Outdated`, `NotActual` | transactions leave it |
| `heartbeat` | `{}` | every 30 seconds |

`?topics=` picks topics by prefix, comma-separated, as a ZMQ subscription
does: `/ws?topics=hashblock,chainswitch` brings `hashblock`, `hashblock_alt`
and `chainswitch`. A prefix that matches no topic is a `400`, so a misspelling
is not a silent stream. Without `?topics=` every topic is sent; `hello` and
`heartbeat` always are.

Treat the stream as a notification, not a ledger. It never replays what was
missed: after connecting, read `hello` and catch up over the ordinary RPC.

## Limits and liveness

- The upgrade goes through the same middleware as every route: the access
  token (`X-API-Key` or `Authorization: Bearer`) and the rate limit, where it
  counts as one request.
- An upgraded connection leaves the RPC's worker pool at once, so subscribers
  never hold a worker; it keeps its address's RPC connection slot while it is
  open. Each subscriber costs two threads and a queue of 1000 messages.
- A subscriber that falls 1000 messages behind is **disconnected**, not
  skipped, so a stream that looks complete is complete.
- The node sends a ping and a `heartbeat` every 30 seconds. A subscriber that
  sends nothing — a pong answers the ping — for 90 seconds is dropped.
- A subscriber may send pongs, pings, a close, and messages of at most 4 KiB,
  which are read and ignored. An unmasked frame is a protocol error (close
  1002).
- When the node stops, every subscriber gets a close with code 1001.

## Browsers

A page cannot set headers on a WebSocket, so it cannot present an access
token: a node with `--rpc-access-token` streams only to programs. And a page
may subscribe only where it may already call the RPC: a request that carries
an `Origin` is refused with a `403` unless `--enable-cors` allows that origin
(or `*`).

## Wallets

`wrkz-wallet`, `wrkz-wallet-api`, `wrkz-service` and Rust Pluton Wallet — on
the desktop, on Android, and in the browser — follow their node's stream by
themselves. There is nothing to configure:

- on `hello`, a block, a reorganisation or a pool change, the wallet syncs at
  once, and asks `/info` again so the network height moves with the block;
- while the stream is live, a synced wallet polls every 30 seconds instead of
  every 2 (Pluton: instead of every 10), only in case a message was lost;
- a node that does not serve `/ws` — every C++ node, or this one without
  `--enable-websocket` — answers the upgrade with something other than `101`.
  The wallet then leaves that node alone for ten minutes and polls exactly as
  before. Nothing is logged above debug;
- `https://` nodes are followed over `wss://`, with the same root
  certificates as the HTTP client. A node over a local IPC socket is not
  followed.

The stream only ever wakes a wallet. Balances and history still come from the
ordinary sync, so a lost or late message costs at most the time until the next
poll.

## Watching it

Any WebSocket client works. With [websocat](https://github.com/vi/websocat):

```sh
websocat 'ws://127.0.0.1:17856/ws?topics=hashblock'
```
