# WrkzCoin (Rust) Documentation

Operator guides, wallets, RPC and the protocol specification for the Rust port
of the WrkzCoin node and wallets. The port follows the
[protocol specification](spec/index.md), which was taken from the C++
reference, [wrkzcoin/wrkzcoin](https://github.com/wrkzcoin/wrkzcoin) at commit
`8d89d7bf`, and it is tested against that code. The source of truth is always
the code in [wrkzcoin/wrkzcoin-rust](https://github.com/wrkzcoin/wrkzcoin-rust).

## The C++ reference

The C++ WrkzCoin is the reference this port follows, and every claim about how
the network behaves can be checked against it:

| | |
| --- | --- |
| [docs.wrkz.work](https://docs.wrkz.work/) | The C++ documentation: `Wrkzd`, its wallets, and the full daemon RPC, Wallet API and wallet service reference |
| [wrkzcoin/wrkzcoin](https://github.com/wrkzcoin/wrkzcoin) | The C++ source code; the specification was taken from commit [`8d89d7bf`](https://github.com/wrkzcoin/wrkzcoin/commit/8d89d7bf92866e6f2985db149e49eb33eac61918) |

Where the two behave the same, these pages link to the C++ documentation rather
than repeat it, and say where the Rust port differs. A C++ file cited on any
page, such as `src/cryptonotecore/Core.cpp:1465`, links to that line at
`8d89d7bf`.

## Start here

| I want to&nbsp;… | Go to |
| --- | --- |
| Know whether it is ready | [Project Status](getting-started/status.md) |
| Get a binary | [Installing a Release](getting-started/install.md), or [Building from Source](getting-started/build.md) |
| Run a node | [Running a Node](node/index.md) |
| Get a synced node quickly | [Bringing a Chain Up](node/bringing-a-chain-up.md) |
| Look up a daemon option | [Configuration](node/configuration.md) |
| Run a node on a small disk | [Lite, Pruned and Explorer](node/reduced-modes.md) and [Lite Node Snapshots](node/lite-snapshots.md) |
| Mine | [Mining](node/mining.md) |
| Build on the RPC | [RPC Interfaces](rpc/index.md) |
| Use a wallet | [Wallet CLI](wallets/wallet-cli.md) or [Rust Pluton Wallet](wallets/pluton.md) |
| Run a payment integration | [Wallet API](wallets/wallet-api.md) or [Wallet Service](wallets/service.md) |
| Check a node against the C++ | [Replay and Consensus Checks](tools/replay.md) and [Diagnostics](tools/diagnostics.md) |
| Build for another platform | [Cross-compiling](building/cross-compile.md) or [Docker](building/docker.md) |
| Understand the protocol | [Protocol Specification](spec/index.md) |
| See what changed | [Changelog](changelog.md) |

## Programs

| Program | In the C++ | What it does |
| --- | --- | --- |
| `wrkz-node` | `Wrkzd` | The daemon: P2P sync and validation, the daemon RPC, block templates and a stratum port for miners |
| `wrkz-wallet` | `wrkz-wallet` | The command-line wallet |
| `wrkz-wallet-api` | `wrkz-wallet-api` | The wallet's HTTP API |
| `wrkz-service` | `wrkz-service` | The JSON-RPC wallet service, method for method; it opens the modern wallet file, not WalletGreen containers |
| `wrkz-txpow-server` | `wrkz-txpow-server` | Computes the transaction proof of work for phones and browsers |
| `rust-pluton-wallet` | — | Rust Pluton Wallet, the wallet with a window, for desktop, Android and the browser |
| `wrkz-replay` | — | Imports a C++ node's database, validating every block on the way |
| `wrkz-verify-state` | — | Rebuilds a chain state someone else built from its own blocks and compares it record for record |
| `wrkz-db-inspect` | — | Reads a C++ database and checks its headers, proofs of work and ring signatures |
| `wrkz-p2p-probe` | — | Dials one peer, handshakes and downloads a few blocks: can this machine reach the network at all |
| `wrkz-rpc-diff` | — | Puts the same requests to two daemons and compares the answers |
| `wrkz-wallet-sync`, `wrkz-wallet-send` | — | Sync an address, or build one transaction by hand, to compare with the C++ wallet |

`wrkz-replay`, `wrkz-verify-state` and `wrkz-db-inspect` need the `rocksdb`
feature. Not ported: the C++ `miner` (the node's stratum port is for xmrig
instead), `wrkz-netmon`, `wallet-upgrader` and `cryptotest`.

## Default ports

| Port | Service | Listens on |
| --- | --- | --- |
| `17855` | P2P | all interfaces |
| `17856` | Daemon RPC | `127.0.0.1` |
| `17857` | ZMQ publisher, on by default | `127.0.0.1` |
| — | Stratum, off until a port is given | `127.0.0.1` |
| `17870` | `wrkz-txpow-server`, if you run one | `127.0.0.1` |
| `7856` | `wrkz-wallet-api` **and** `wrkz-service` | `127.0.0.1` |

These are the C++ defaults too ([docs.wrkz.work](https://docs.wrkz.work/)).
`wrkz-wallet-api` and `wrkz-service` share the same default port, as they do in
the C++. Give one of them a different port if you run both on one machine.

## Versions

The site at the root always describes the newest release. Each release is also
kept at its own path, `/v1.0.1/` for example, and the version selector in the
header moves between them. A page there describes that release as it shipped,
so a link to one stays true after the code moves on.

## For AI agents and scripts

Every page is published as plain Markdown as well as HTML, so a model or a
script can read the docs without parsing the rendered site.

| URL | What it is |
| --- | --- |
| [`/llms.txt`](https://docs-rust.wrkz.work/llms.txt) | An index in the [llmstxt.org](https://llmstxt.org) format: every page, grouped by section, one line of description each |
| [`/llms-full.txt`](https://docs-rust.wrkz.work/llms-full.txt) | The entire documentation set concatenated as one Markdown file |
| `<any page URL> + .md` | The Markdown source of that page — e.g. [`/node/reduced-modes.md`](https://docs-rust.wrkz.work/node/reduced-modes.md) |

All three are regenerated on every build, so they never lag the site. **They
describe the documented behaviour, not your node**: version-specific answers
still need `wrkz-node --version` and the [changelog](changelog.md).
