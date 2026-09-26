# Building from source

Building needs a Rust toolchain and a C compiler, plus a C++ compiler and libclang for the RocksDB engine a real node stores its chain in; this page covers the toolchain, each program's build command and the test pass.

For the C++ node, see the [Building](https://docs.wrkz.work/guides/building/) guide of the C++ documentation.

## Toolchain

The releases are built and tested with **rustc 1.98.1**. The workspace names it as its `rust-version` (`1.98`), CI installs exactly that version, and `scripts/release.sh` refuses to cut an archive with any other, because a different compiler produces different bytes. For development another compiler of at least that version will do; the pin is what makes a release reproducible.

```sh
rustup toolchain install 1.98.1
rustup override set 1.98.1     # in the checkout, if other projects want another
```

A **C compiler** is always needed, for the curve code in `wrkz-pow-ref` (the hashing is Rust). On Windows that is MinGW-w64 GCC or MSVC.

The **`rocksdb` feature** additionally needs a **C++ compiler and libclang** (bindgen generates the RocksDB bindings), and `cmake`. It is off by default, so the default build and every test run against an in-memory store. A node that keeps its chain across restarts needs it, and so do `wrkz-replay`, `wrkz-verify-state` and `wrkz-db-inspect`, which open RocksDB databases.

On Ubuntu 22.04 or 24.04 everything comes from one script, run once:

```sh
scripts/ubuntu-setup.sh
```

It installs `build-essential clang libclang-dev cmake pkg-config git curl` and, when `cargo` is missing, rustup with its default toolchain; run the `rustup` commands above afterwards to use the pinned one.

### Windows

The port is developed on Windows with MinGW-w64 (`x86_64-pc-windows-gnu`); MSVC is the other supported C compiler. The same `cargo` commands apply. For the `rocksdb` feature, libclang has to be where bindgen finds it; set `LIBCLANG_PATH` to its directory if it is not. RocksDB needs `std::thread`, so with MinGW use the *posix-threads* variant of the compiler. The local IPC sockets (`--rpc-ipc-path` and the like) are not available on Windows.

The Windows archives in a release are not built on Windows: they are cross-compiled from Linux, statically linked ([Cross-compiling](../building/cross-compile.md)).

## Building the programs

Release builds land in `target/release/`.

```sh
# The daemon, with persistent storage
cargo build --release -p wrkz-node --features rocksdb

# The import and state tools, and the database inspector
cargo build --release -p wrkz-chain --features rocksdb --bin wrkz-replay --bin wrkz-verify-state
cargo build --release -p wrkz-storage --features rocksdb --bin wrkz-db-inspect

# The wallets: wrkz-wallet, wrkz-wallet-api, wrkz-wallet-sync, wrkz-wallet-send
cargo build --release -p wrkz-wallet --bins

# The wallet service and the transaction proof-of-work server
cargo build --release -p wrkz-service --bin wrkz-service
cargo build --release -p wrkz-txpow-server --bin wrkz-txpow-server

# A private test network (see Tools, Simnet)
cargo build --release -p wrkz-simnet --bin wrkz-simnet

# The diagnostics that need no database
cargo build --release -p wrkz-p2p --bin wrkz-p2p-probe
cargo build --release -p wrkz-rpc --bin wrkz-rpc-diff
```

`wrkz-node` without `--features rocksdb` builds and runs, but keeps the chain in memory. `--version` on the daemon and the wallet programs prints the commit the binary was built from, when it was built from a git checkout.

Other optional features:

`zstd` (on `wrkz-node` and `wrkz-wallet`)
:   Offer `Content-Encoding: zstd` ahead of gzip on the daemon RPC, and ask for it from the wallet. Only this port's own programs speak it; a C++ daemon negotiates gzip only. Off by default because the zstd crate compiles C.

`https` (on `wrkz-wallet`, on by default)
:   Allow `https://` daemon URLs. `--no-default-features` builds the plain-HTTP client, which is all the seed nodes need.

Rust Pluton Wallet is its own workspace in `apps/pluton`, so nothing above pulls in a GUI toolkit and `--workspace` does not reach it:

```sh
cd apps/pluton && cargo run --release
```

See [Rust Pluton Wallet](../wallets/pluton.md) for its platforms.

## Testing

```sh
cargo test --workspace
cargo test --workspace -- --ignored        # live checks against a seed node
scripts/ci.sh                              # fmt, clippy, tests, docs, Pluton, RocksDB
scripts/ci.sh clippy tests                 # or only the sections named
```

The tests replay every vector in `spec/vectors/` and need no network; the `--ignored` ones talk to a seed node. On Ubuntu the full pass, including the RocksDB engine, inspection of a C++ database and a live P2P sync, is:

```sh
WRKZ_DB=$HOME/.WRKZCoin/DB scripts/ubuntu-test.sh
```

Point `WRKZ_DB` at the daemon's own `DB` directory with `Wrkzd` stopped, or at a copy of it. [Testing](../contributing/testing.md) describes every section of both scripts and the CI jobs.

## Next

- [Running a node](../node/index.md)
- [Cross-compiling](../building/cross-compile.md) for other platforms from one Linux host
- [Docker](../building/docker.md)
