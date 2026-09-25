# Running a node

`wrkz-node` is the daemon: it syncs the chain over the WrkzCoin P2P protocol and
serves the daemon RPC (`/info`, `/getwalletsyncdata`, `getblocktemplate`, …) on
the same state, in one process. This section is what an operator needs to build
it, bring a chain up, run it unattended and check it.

Everything in it has been run except the RocksDB paths, which need libclang and
were built and exercised on the Linux host only; see
[What is not covered](internals.md#what-is-not-covered).

## Build

On Ubuntu 22.04 or 24.04:

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev pkg-config git curl
# Rust, if it is not there yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

git clone https://github.com/wrkzcoin/wrkzcoin-rust.git wrkz-rust && cd wrkz-rust
cargo build --release -p wrkz-node --bin wrkz-node --features rocksdb
```

`--features rocksdb` is what makes the chain state **persistent**. Without it
the daemon runs with the chain in memory and loses it on exit; it says so at
start-up. `clang`/`libclang-dev` are needed only for that feature (the RocksDB
bindings), which is why `cargo build -p wrkz-node` without it works anywhere.

A build from a git checkout, the Docker image included, is stamped with its
commit, which `--version` then prints (`wrkz-node <version> (<commit>), ...`);
`build.rs` reads it from `.git`. To stamp a build from a source export with no
`.git`, or to name another commit, set it yourself:

```sh
WRKZ_GIT_COMMIT=935a145 \
  cargo build --release -p wrkz-node --bin wrkz-node --features rocksdb
```

For Windows, macOS, Android or arm64 Linux, build on the same Ubuntu host and
copy the binary over:
[`scripts/cross-setup.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/cross-setup.sh)
once, then
[`scripts/release.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/release.sh)
`windows` (or no argument, for every platform). See
[Cross-compiling](../building/cross-compile.md), and
[Building from source](../getting-started/build.md) for the whole workspace.
Every option in this section is the same on every platform. `--data-dir` has no
default on any of them — the daemon refuses to start without one — so give it a
path in the platform's own form (`--data-dir C:\wrkz` on Windows).

## The quickest path to a running node

1. **Bring a chain up.** On a machine that already has a C++ `Wrkzd` database,
   import it with `wrkz-replay --store-raw`; that is far faster than a sync from
   genesis. Otherwise the daemon syncs from the seed nodes on its own. Both are
   in [Bringing a chain up](bringing-a-chain-up.md).
2. **Start it.**
   [`scripts/run-daemon.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/run-daemon.sh)
   starts it with server defaults, logging to a file, and prints a loud warning
   when the RPC is bound to a public address:

    ```sh
    WRKZ_DATA_DIR=$HOME/.wrkz-rust WRKZ_TOKEN=$(openssl rand -hex 16) scripts/run-daemon.sh
    ```

    The script binds the RPC to `0.0.0.0` unless `WRKZ_RPC_IP` says otherwise,
    which is why the token matters; set `WRKZ_RPC_IP=127.0.0.1` for a node only
    this machine uses. Or run the binary directly:

    ```sh
    ./target/release/wrkz-node --data-dir "$HOME/.wrkz-rust"
    ```

3. **Check it.** `curl -s http://127.0.0.1:17856/info` answers once the RPC is
   up; [Checking a node](checking.md) has the rest.
4. **Run it unattended** under [systemd](systemd.md).

The default ports are 17855 for P2P, 17856 for the RPC and 17857 for the ZMQ
publisher (loopback only).

## In this section

- [Bringing a chain up](bringing-a-chain-up.md) — import a C++ database with
  `wrkz-replay`, sync from the network, or import a dump file. **Read this
  first.**
- [Configuration](configuration.md) — every command-line option, the `Wrkzd`
  configuration file, and operating notes: status line, metrics, health,
  shutdown, exit codes.
- [Networking](networking.md) — IPv6, priority and exclusive nodes, UPnP port
  mapping.
- [Console and IPC](console-and-ipc.md) — the local IPC socket, the console, and
  attaching to a daemon already running.
- [Maintenance](maintenance.md) — `--resync`, `--rewind-to-height`,
  `--export-blockchain` and `--import-blockchain`.
- [Lite, pruned and explorer](reduced-modes.md) — the reduced modes, and how to
  tell which one a node is in.
- [Lite node snapshots](lite-snapshots.md) — starting a lite node from a
  snapshot file, and exporting one.
- [The database](database.md) — RocksDB tuning and compaction.
- [Mining](mining.md) — the built-in stratum server for xmrig.
- [ZMQ and notify hooks](zmq-and-hooks.md) — the ZMQ publisher and the
  `--block-notify`, `--reorg-notify` and `--tx-notify` hooks.
- [systemd](systemd.md) — a unit file with hardening.
- [Checking a node](checking.md) — curl, pointing a wallet at it, and end to end
  in one command.
- [Internals and differences](internals.md) — how it is put together, sync
  throughput, where it differs from the C++, and what is not covered.

The C++ daemon's own guide is at
[docs.wrkz.work](https://docs.wrkz.work/guides/running-daemon/).
