# Checking a node

This page shows how to check a running `wrkz-node` by hand with curl, how to
point a wallet at it, and how to run the whole check end to end with one
script.

The C++ counterpart is the
[Daemon RPC Cookbook](https://docs.wrkz.work/guides/daemon-rpc-cookbook/) on docs.wrkz.work.

## Checking it with curl

```sh
# height and version; `height` is a COUNT (top index + 1)
curl -s http://127.0.0.1:17856/info | head -c 400; echo

# what a solo miner polls. Note there is deliberately no "hash" member:
# xmrig reads its absence as "this is a CryptoNote daemon".
curl -s http://127.0.0.1:17856/getheight

# JSON-RPC
curl -s -X POST http://127.0.0.1:17856/json_rpc \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"getblockcount"}'

curl -s -X POST http://127.0.0.1:17856/json_rpc \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"getlastblockheader"}'

# a block header by index (an INDEX, not a count)
curl -s -X POST http://127.0.0.1:17856/json_rpc \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"getblockheaderbyheight","params":{"height":4213000}}'

# what a wallet syncs with
curl -s -X POST http://127.0.0.1:17856/getwalletsyncdata \
  -H 'Content-Type: application/json' \
  -d '{"blockHashCheckpoints":[],"startHeight":0,"startTimestamp":0,"blockCount":2,"skipCoinbaseTransactions":false}'
```

With `--rpc-access-token TOKEN`, add `-H "X-API-Key: TOKEN"` (or
`-H "Authorization: Bearer TOKEN"`) to every one of those. Responses are
gzipped for a client that asks; `curl --compressed` shows it.

Every endpoint is described in [RPC interfaces](../rpc/index.md), and the C++
reference is the [daemon JSON-RPC](https://docs.wrkz.work/daemon-rpc/json-rpc/)
documentation. To compare a node's answers with a C++ daemon's, see
[Comparing with the C++ daemon](internals.md#comparing-with-the-c-daemon).

## Pointing a wallet at it

### This port's wallet tools

```sh
cargo build --release -p wrkz-wallet --bin wrkz-wallet-sync --bin wrkz-wallet-send

./target/release/wrkz-wallet-sync \
    --daemon http://127.0.0.1:17856 \
    --view-key <64 hex> --address Wrkz... \
    --scan-height 4200000 --out view.wallet

./target/release/wrkz-wallet-send \
    --wallet view.wallet --daemon http://127.0.0.1:17856 \
    --to Wrkz... --amount 1000 --dry-run
```

Run the same two commands against `http://node-fin.wrkz.work:17856` and the
results must agree.
[`scripts/e2e.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/e2e.sh)
does exactly that. Both tools are described in
[Diagnostics](../tools/diagnostics.md).

### A wallet CLI

```sh
./wrkz-wallet --remote-daemon 127.0.0.1:17856
```

or, from inside the C++ wallet, `set_daemon 127.0.0.1 17856`. This port's
[`wrkz-wallet`](../wallets/wallet-cli.md) takes the same `--remote-daemon`. The
wallet must be able to reach the RPC port; if the daemon runs on another
machine, bind the RPC there and use that address.

## End to end in one command

[`scripts/e2e.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/e2e.sh)
builds everything, starts the daemon, waits for the RPC, curls the endpoints,
runs `wrkz-rpc-diff` against the C++ seed node, syncs a wallet against our
daemon and against the C++ daemon and diffs the two, builds a transaction with
`--dry-run`, then stops the daemon and reports. Any failure exits non-zero; the
wallet steps are skipped with a message when the keys are not supplied.

```sh
# offline smoke test, no network, no wallet keys
WRKZ_STATE=empty WRKZ_REFERENCE= WRKZ_FEATURES= scripts/e2e.sh

# the real thing on the server, over an imported state
WRKZ_DATA_DIR=$HOME/.wrkz-rust WRKZ_STATE=import \
WRKZ_VIEW_KEY=<64 hex> WRKZ_ADDRESS=Wrkz... WRKZ_SCAN_HEIGHT=4200000 \
WRKZ_SEND_TO=Wrkz... \
  scripts/e2e.sh
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `WRKZ_DATA_DIR` | a temporary directory | where the daemon's state lives |
| `WRKZ_STATE` | `import` when `WRKZ_DATA_DIR/state` exists | `import` (the state is already there), `sync` (sync from the network up to `WRKZ_SYNC_TO`) or `empty` (start from genesis and do not sync) |
| `WRKZ_SYNC_TO` | 2000 | block index to sync to with `WRKZ_STATE=sync` |
| `WRKZ_RPC_PORT`, `WRKZ_P2P_PORT` | 17856, 17855 | the daemon's ports |
| `WRKZ_FEATURES` | `rocksdb` | cargo features; `""` on a host with no libclang, which then keeps the chain in memory |
| `WRKZ_VIEW_KEY`, `WRKZ_ADDRESS` | — | the private view key and its address, for the wallet sync |
| `WRKZ_SCAN_HEIGHT` | 0 | where the wallet starts scanning |
| `WRKZ_SEND_TO`, `WRKZ_SEND_AMOUNT` | —, 1000 | destination and atomic units for the dry-run build |
| `WRKZ_REFERENCE` | `http://node-fin.wrkz.work:17856` | a C++ daemon to compare against; `""` skips it |

The same checks run offline in CI as
`cargo test -p wrkz-node --test daemon_rpc`, which starts the daemon in process
over a synthetic chain and drives it with the **wallet's own client**
(`wrkz_wallet::daemon::Daemon`) — the code that drives the C++ daemon. See
[Testing](../contributing/testing.md).
