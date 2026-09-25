# Diagnostics

Five small programs answer one question each (is this C++ database sound, can this machine reach the network, does this daemon answer like the C++ one, what does this address hold, what would this send look like) and a script watches a daemon's ZMQ feed; this page says what each is for and how to run it.

None of the five has a C++ counterpart; the C++ node's own tools, its ZMQ test script among them, are in the C++ documentation's [Other tools](https://docs.wrkz.work/guides/other-tools/). All five programs ship in every release archive. From source:

```sh
cargo build --release -p wrkz-storage --features rocksdb --bin wrkz-db-inspect
cargo build --release -p wrkz-p2p --bin wrkz-p2p-probe
cargo build --release -p wrkz-rpc --bin wrkz-rpc-diff
cargo build --release -p wrkz-wallet --bin wrkz-wallet-sync --bin wrkz-wallet-send
```

Each prints its usage with `--help`. Each exits 0 when everything it checked passed and non-zero otherwise, so they can be scripted.

## `wrkz-db-inspect`

Opens a C++ node's RocksDB read-only and checks it with the port's own code: for every block in range, that the stored hash is the hash of the stored raw block, the previous-block links, the major version rule, and the proof of work against the difficulty derived from the cumulative difficulties. Outside the checkpoint zone that is what the node itself verified.

```sh
wrkz-db-inspect ~/.WRKZCoin/DB --count 1000 --rings 100
```

| Option | Default | Meaning |
| --- | --- | --- |
| `<db-dir>` | required, first | The C++ `DB` directory. Stop `Wrkzd` first, or use a copy |
| `--from N` | | First block index to check |
| `--count N` | 1000 | Blocks to check; without `--from`, the top *N* |
| `--rings N` | 0 | Also resolve the ring members of the last *N* non-coinbase transactions in range and verify their ring signatures |
| `--quiet` | off | No line per block |

It ends with `INSPECT OK`, or `INSPECT FAILED:` and the reason with exit status 1. It needs the `rocksdb` feature. The whole chain, rather than a range, is what [`wrkz-replay`](replay.md) checks.

## `wrkz-p2p-probe`

The operator's connectivity check. It needs no data directory and no state: it dials one peer, handshakes, asks for the chain from genesis, downloads a few blocks over Levin and validates them (ids, previous-block links, versions, coinbase height, transaction hashes, proof of work). It is how to answer "can this machine reach the network at all" before there is a node to ask.

```sh
wrkz-p2p-probe                              # node-fin.wrkz.work:17855, 5 blocks
wrkz-p2p-probe 203.0.113.7:17855 20
```

Both arguments are positional and optional: the peer (`host:port`, default `node-fin.wrkz.work:17855`) and the number of blocks to fetch (default 5). It prints the peer's version, height and a few of its peers, and ends with `PROBE OK`, or `PROBE FAILED:` and exit status 1. It needs outbound TCP to the peer's P2P port.

## `wrkz-rpc-diff`

Puts the same requests to two daemons and compares the answers. It is the check behind the claim that the daemon RPC matches the C++ ([RPC interfaces](../rpc/index.md)).

```sh
wrkz-rpc-diff --reference http://node-fin.wrkz.work:17856 --ours http://127.0.0.1:17856
```

By default it compares **shapes**: every key present on one side must be present on the other, with the same type. That is the comparison that means anything between two nodes on different chains. `--values` compares values as well, skipping a list of fields that legitimately differ (clocks, peer identities, connection counts, and anything that follows from the chain's height); use it only when both sides hold the same chain.

Without `--height` it probes the highest block index both daemons hold, so a node still catching up does not report every range endpoint as a difference.

| Option | Default | Meaning |
| --- | --- | --- |
| `--reference URL` | required | The daemon to compare against, usually a C++ one |
| `--ours URL` | required | The daemon under test |
| `--endpoints a,b,c` | all | Only the probes named; `--list` shows the names |
| `--values` | off | Compare values too, not only shapes |
| `--height N` | the common top | The block index the probes ask about |
| `--range-start N` | as `--height` | The start of the ranged probes |
| `--timeout SECONDS` | 30 | Per request |
| `--list` | | Print every probe's name, method and path, and exit |

Each probe prints `ok` or `DIFF` with the differences under it, then a count of clean probes. Exit status is 0 when every probe is clean, 1 otherwise, 2 for a bad argument.

## `wrkz-wallet-sync`

View-syncs an address against a daemon and prints its transactions and balance, so the result can be diffed against the C++ `wrkz-wallet` CLI on the same keys.

```sh
wrkz-wallet-sync --daemon http://node-fin.wrkz.work:17856 \
    --view-key <64 hex> --address Wrkz... --scan-height 4200000
```

The wallet it builds is view-only: it finds incoming outputs and their amounts, but it holds no spend key, so it cannot compute key images and cannot see those outputs being spent. The transaction list and balance it prints are the incoming side only, which is exactly what a C++ view wallet on the same keys shows.

| Option | Default | Meaning |
| --- | --- | --- |
| `--daemon URL` | required | Daemon base URL |
| `--view-key HEX` | required | The private view key, 64 hex characters |
| `--address ADDR` | required | The standard address the view key belongs to |
| `--scan-height N` | 0 | Block index to start scanning from |
| `--out FILE` | | Write the synced wallet to *FILE*, as a normal wallet file the C++ CLI can open |
| `--password PASS` | empty | Password for `--out` |
| `--max-steps N` | unlimited | Stop after *N* sync rounds |
| `--skip-coinbase` | off | Do not scan coinbase transactions |
| `--sync-windows` | off | Ask for four height windows a round far below the tip (with `--skip-coinbase`, from a daemon offering `skipEmptyBlocks`) |
| `--sync-max-blocks N` | 1000 | Most blocks one request asks for, at most 10000; above 1000 needs a daemon with a higher `--rpc-max-block-count` |
| `--quiet` | off | No progress lines on stderr |

## `wrkz-wallet-send`

Builds, and optionally relays, one transaction from a wallet file, so a send can be made by hand and inspected before it goes out.

```sh
wrkz-wallet-send --wallet mine.wallet --password secret \
    --daemon http://node-fin.wrkz.work:17856 \
    --to Wrkz... --amount 1000 --dry-run
```

`--dry-run` stops after building: it prints the hex, the fee, the size, the proof of work and every input the transaction spends, and touches neither the daemon nor the wallet file. Without it the transaction is relayed through `/sendrawtransaction` after a confirmation prompt, and the wallet file is written back with the inputs locked, the change recorded as unconfirmed and the transaction private key stored.

The wallet must already be synced (with `wrkz-wallet-sync`, or any wallet): this tool does not scan. It refuses to relay when the wallet's height is more than a block behind the daemon's, because inputs it has not seen spent would be double spends.

| Option | Default | Meaning |
| --- | --- | --- |
| `--wallet FILE` | required | The wallet file to spend from |
| `--password PASS` | empty | Its password |
| `--daemon URL` | required | Daemon base URL |
| `--to ADDR` | required | Destination, standard or integrated |
| `--amount N` | required unless `--send-all` | Atomic units to send |
| `--payment-id HEX` | | 16 or 64 hex characters |
| `--fee N` | | A fixed fee in atomic units |
| `--fee-per-byte RATE` | the network minimum | Atomic units per byte |
| `--mixin N` | the tier default | Ring size minus one |
| `--change-address A` | the primary address | Where change goes |
| `--from ADDR` | all | A subwallet to spend from; repeatable |
| `--unlock-time N` | network height + 20 + 15 | Block index or Unix time |
| `--send-all` | off | Send the whole balance, taking the fee out of the amount |
| `--pow-threads N` | all cores | Threads for the transaction proof-of-work search |
| `--seed HEX` | | A 32-byte seed for a reproducible build; testing only |
| `--dry-run` | off | Build and print; do not relay and do not save |
| `--yes` | off | Relay without the confirmation prompt |

## `scripts/zmq-subscribe.py`

Watches a daemon's ZMQ feed: every block, reorganisation and pool change, one line each (the topic and its JSON body), as the daemon publishes them. It works against the C++ `Wrkzd` and `wrkz-node` alike, because it is a plain libzmq SUB socket. It needs pyzmq (`pip install pyzmq`).

```sh
scripts/zmq-subscribe.py                              # tcp://127.0.0.1:17857
scripts/zmq-subscribe.py --topic hashblock --topic chainswitch
scripts/zmq-subscribe.py --count 1 --timeout 180 tcp://127.0.0.1:17857
```

Topics match by prefix, as ZMQ matches them, so `--topic hashblock` also brings `hashblock_alt`; with no `--topic` every topic comes through. `--count N` exits after *N* messages, `--timeout S` gives up after *S* quiet seconds. Exit status: 0 once `--count` messages arrived (or on Ctrl-C), 1 if `--timeout` passed with nothing new, 2 without pyzmq. The topics and the publisher are described in [ZMQ and notify hooks](../node/zmq-and-hooks.md).

The block-by-block comparison of two daemons, `scripts/dual-run.py`, is on [Testing](../contributing/testing.md#dual-run).
