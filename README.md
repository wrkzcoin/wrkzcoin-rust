# WrkzCoin (Rust)

A Rust port of the WrkzCoin node and wallets. It follows the protocol
specification in [`spec/`](spec/README.md), which was taken from the C++
reference, [wrkzcoin/wrkzcoin](https://github.com/wrkzcoin/wrkzcoin) at commit
`8d89d7bf`, and it is tested against that code.

> **Not ready for production.** Nothing here should serve a public wallet or
> accept mining until the block-by-block dual run against the live C++ daemon
> (`scripts/dual-run.py`) has passed. [What is proven so far](#what-is-proven-so-far)
> lists what has been checked.

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
instead), `wrkz-netmon`, `wallet-upgrader`, `cryptotest` and the `wallet_capi`
C library.

## Layout

| Crate | Contents | Spec |
| --- | --- | --- |
| `crates/wrkz-pow` | Keccak, the five proofs of work (the CryptoNight family on AES-NI, ARMv8 or table AES; Chukwa on argon2id), tree hash and `check_hash` in Rust, ported from wrkzcoin `8d89d7bf` and tested against its C; every `crypto.cpp` primitive through the C shim of `wrkz-pow-ref` | 02, 03 |
| `crates/wrkz-pow-ref` | The reference C of wrkzcoin `8d89d7bf`, vendored unchanged under `c/`: the ref10 curve code and Keccak that `wrkz-pow::curve` still calls, and (feature `pow`, tests and fuzzing only) the proof-of-work C that `wrkz-pow` is compared against | 02, 03 |
| `crates/wrkz-primitives` | Constants, varint, base58/addresses, mnemonics, binary serialization, transactions, blocks and hashing blobs, tx_extra, difficulty, fees, mixins, KV binary | 01, 04, 05, 06, 07 |
| `crates/wrkz-storage` | The C++ node's RocksDB key layout and record encodings, a typed chain reader with ring-member resolution, the RocksDB engine (feature `rocksdb`, ZSTD) and `wrkz-db-inspect` | 11 |
| `crates/wrkz-chain` | Chain state, block and transaction validation in the C++ order (signatures and proofs of work checked in parallel), reward with the size penalty, checkpoints, alternative chains and reorganisation, `wrkz-replay` and `wrkz-verify-state` | 06, 07, 11 |
| `crates/wrkz-mempool` | The transaction pool and block templates, byte-comparable with the C++ daemon's `getblocktemplate` | 06, 07, 09 |
| `crates/wrkz-p2p` | Levin framing, handshake, timed sync, ping, every block and transaction notification, and `wrkz-p2p-probe` — the connectivity diagnostic that needs no data directory and no state: it dials any peer, handshakes, downloads a few blocks and exits non-zero if it cannot, which is how an operator answers "can this box reach the network at all" before there is a node to ask | 08 |
| `crates/wrkz-node` | The daemon: peer manager, white and gray lists, back ping, the block sync state machine, batched commits, the console, and the C++ configuration file and option names | 08, 09 |
| `crates/wrkz-rpc` | The daemon HTTP and JSON-RPC surface, gzip, the wallet sync cache and `/metrics`; the logger, the IPC listener and the `--*-notify` hook runner the daemon and the wallet programs share; `wrkz-rpc-diff` | 09 |
| `crates/wrkz-wallet` | Wallet file format, the daemon client, synchronization, balances, transaction construction, and the `wrkz-wallet`, `wrkz-wallet-api`, `wrkz-wallet-sync` and `wrkz-wallet-send` programs | 03, 09, 10 |
| `crates/wrkz-service` | `wrkz-service`: the JSON-RPC wallet service of `src/walletservice`, method for method, over the modern container | 09, 10 |
| `crates/wrkz-txpow-server` | `wrkz-txpow-server`: computes the transaction proof of work for wallets that would rather not — phones and browsers | 06 |
| `apps/pluton` | Rust Pluton Wallet: the wallet with a window, for Windows, macOS, Linux, Android and the browser, on `wrkz-wallet`. Its own workspace, so nothing here pulls in a GUI toolkit | 09, 10 |

`fuzz/` holds libFuzzer targets for every parser that reads bytes from a
peer, a daemon or a file. See [`fuzz/README.md`](fuzz/README.md).

## Build and test

Needs a Rust toolchain and a C compiler (for the curve code in
`wrkz-pow-ref`; the hashing is Rust). On Windows, MinGW-w64 GCC or MSVC.
The `rocksdb` feature additionally needs a C++ compiler and libclang, so it
is off by default and every test runs against an in-memory store.

    cargo test --workspace
    cargo test --workspace -- --ignored        # live checks against a seed node
    scripts/ci.sh                              # fmt, clippy, tests, docs, Pluton, RocksDB
    scripts/ci.sh clippy tests                 # or only the sections named

On Ubuntu, once: `scripts/ubuntu-setup.sh`. Then the full pass, including
the RocksDB engine, inspection of a C++ database and a live P2P sync:

    WRKZ_DB=$HOME/.WRKZCoin/DB scripts/ubuntu-test.sh

Point `WRKZ_DB` at the daemon's own `DB` directory with `Wrkzd` stopped, or
at a copy of it.

## Cross-compiling

One Ubuntu (22.04/24.04, x86_64) host builds every platform — no Mac, no
Windows machine and no Apple SDK:

    scripts/cross-setup.sh                   # once: all toolchains, pinned and checksum-verified
    scripts/release.sh                       # every platform below, into dist/

| Target | Runs on | Built with |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86_64 | cargo |
| `aarch64-unknown-linux-gnu` | Linux arm64 (Raspberry Pi 4/5 64-bit, Graviton, Ampere), glibc ≥ 2.28 | zig |
| `x86_64-pc-windows-gnu` | Windows 10/11 x64, no DLLs needed | MinGW-w64, static |
| `x86_64-apple-darwin` | macOS 13+ on Intel | zig |
| `aarch64-apple-darwin` | macOS 13+ on Apple silicon | zig |
| `aarch64-linux-android` | Android 7.0+ (Termux, `adb`) | Android NDK |
| `x86_64-linux-android` | Android emulator, x86 Chromebooks | Android NDK |

Each archive, `wrkzcoin-cli-<version>-<commit>-<os>-<arch>` (`.zip` for
Windows, `.tar.gz` otherwise, e.g. `wrkzcoin-cli-1.0.0-97f3ab1-linux-arm64.tar.gz`),
holds `wrkz-node`, `wrkz-replay`, `wrkz-p2p-probe`, `wrkz-service`,
`wrkz-txpow-server` and the four wallet programs, next to a `SHA256SUMS` that
two hosts building the same commit reproduce.

Pick platforms by OS (`linux`, `windows`, `macos`, `android`), or build one
binary while developing:

    scripts/release.sh windows macos
    scripts/cross.sh aarch64-apple-darwin --release -p wrkz-wallet --bins
    scripts/cross.sh x86_64-pc-windows-gnu --release --features rocksdb -p wrkz-node --bin wrkz-node

Without a configured host, the same toolchains come as an image:

    docker build -f Dockerfile.cross -t wrkz-cross .
    docker run --rm -v "$PWD":/src -v wrkz-cargo:/usr/local/cargo/registry -u "$(id -u):$(id -g)" wrkz-cross scripts/release.sh

(append OS names to build fewer; the PowerShell form for Docker Desktop is at
the top of `Dockerfile.cross`).

Before trusting a release on a new architecture, run `cargo test -p wrkz-pow`
once on it (a Mac, a Raspberry Pi, Termux): it checks the hashing and curve
code against the C++ reference vectors. The toolchain choices and the knobs
are described at the top of `scripts/cross-setup.sh` and `scripts/cross.sh`.

## Running a node

Build with persistent storage and run:

    cargo build --release -p wrkz-node --features rocksdb
    ./target/release/wrkz-node --data-dir ~/.wrkz-rust

A `Wrkzd` configuration file works as it is (`wrkz-node -c wrkz.json`), with
the same option names; `--dump-config` prints the effective settings and
`--help` lists every option.

The fastest way to a synced node is to import a database you already have
rather than syncing millions of blocks from peers:

    cargo run --release -p wrkz-chain --features rocksdb --bin wrkz-replay -- \
        --store-raw --db /path/to/copy/of/DB --state ~/.wrkz-rust/state

`--store-raw` keeps the block and transaction bytes, and a node you intend to
run needs them. Without it the state is about a quarter of the size and is
still fine for verifying the chain, but the daemon cannot serve any block
below the import to a peer or a wallet. A state someone else built can be
checked with `wrkz-verify-state` first.

With Docker:

    docker build -t wrkz-rust .
    docker run -d --name wrkz -v wrkz-data:/data -p 17855:17855 -p 127.0.0.1:17856:17856 wrkz-rust

## Verifying consensus

`wrkz-replay` validates real blocks from a C++ database against our
implementation. The default acceptance run checks only the heights where
consensus rules change, with checkpoints disabled so proof of work and
signatures actually run, which takes minutes rather than a full pass over
the whole chain:

    wrkz-replay --db /path/to/copy/of/DB --state /tmp/replay-forks --windows forks --window 500

`--sample N` takes random windows from a printed seed, and the default
linear mode replays everything, resumably, applying checkpoints exactly as
the C++ node does.

## What is proven so far

The tests replay every vector in `spec/vectors/`, and these results came
from real data rather than fixtures:

- block templates match the live C++ daemon's `getblocktemplate` field for
  field, including the whole blob with the random keys masked;
- the node synced thousands of blocks from real mainnet peers over Levin,
  and serves a C++-shaped client that dials in;
- wallet files written here open in the C++ `wrkz-wallet` CLI and re-export
  byte-identically, and files it writes open here;
- transactions built by the wallet pass our port of the C++ validator with
  the rings resolved against real chain outputs and every signature checked;
- the storage layer opened a real 4.2 million block C++ database and
  verified its headers, proofs of work and ring signatures;
- the 4,300,000 fork (rings of up to eight) is tested at the block level:
  blocks up to and including 4,300,000 refuse a ring of three, 4,300,001
  takes it, and the pool agrees with the next block at every height.

Not yet: the dual run against the C++ daemon, a block mined by a stock xmrig
through the stratum port, and a cross-compiled build run against the chain.

## Licence

WrkzCoin (Rust) is licensed as the C++ WrkzCoin is: under the GNU General
Public License, version 3 or later, keeping the notices of the projects the
code grew from — the CryptoNote developers and the Bytecoin developers
(LGPL-3.0), the Monero Project (BSD-3-Clause) and the TurtleCoin developers
(GPL-3.0). [`LICENSE`](LICENSE) is the C++ repository's own file, unchanged.

`crates/wrkz-pow-ref/c/` is C code copied unchanged from the C++ repository,
under the same notices and each file's header; its argon2 is MIT
([`crates/wrkz-pow-ref/c/argon2/LICENSE`](crates/wrkz-pow-ref/c/argon2/LICENSE)).

Rust Pluton Wallet draws with [Slint](https://slint.dev) under Slint's
royalty-free licence, which asks that the application show the "About Slint"
notice; Pluton's About page does.
