# Testing

The tests are layered: `cargo test` replays the vectors with no network, `scripts/ci.sh` is the pass every push must clear, `scripts/ubuntu-test.sh` adds the live network and a real C++ database, `scripts/e2e.sh` runs the daemon and the wallet tools together on one machine, and `scripts/dual-run.py` compares a running node with the C++ daemon block by block; this page describes each, the CI jobs and the dependency audit.

Fuzzing has its own page: [Fuzzing](fuzzing.md).

## Unit and vector tests

```sh
cargo test --workspace
```

The tests replay every vector in `spec/vectors/` and need no network and no database: every test runs against an in-memory store, because the `rocksdb` feature is off by default. The vector tests hash whole blocks with CryptoNight, so the dev profile keeps the hashing optimised even in test builds.

### Live checks

```sh
cargo test --workspace -- --ignored
```

Most ignored tests talk to a seed node (`node-fin.wrkz.work`) and need outbound TCP to port 17856; they are not part of CI. A few ignored tests are measurements rather than checks: they assert only that two paths agree and print timings, for example `cargo test --release -p wrkz-pow -- --ignored --nocapture parallel`.

## `scripts/ci.sh`

The continuous-integration pass, the same one the GitHub workflow runs, and runnable by hand on the Ubuntu host:

```sh
scripts/ci.sh                        # every section
scripts/ci.sh clippy tests           # only the sections named
WRKZ_DB=/path/DB scripts/ci.sh       # plus the RocksDB inspection
WRKZ_CI_DEBUG_TESTS=1 scripts/ci.sh  # plus a debug-profile test run
```

The sections, in the order they run:

| Section | What it runs |
| --- | --- |
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings`: warnings are errors |
| `tests` | `cargo test --workspace --release`; with `WRKZ_CI_DEBUG_TESTS=1`, the debug profile too |
| `docs` | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS="-D warnings"` |
| `pluton` | Clippy and the tests of Rust Pluton Wallet, which is its own workspace and which `--workspace` never reaches |
| `rocksdb` | Clippy with `--features rocksdb`, and the tests of `wrkz-storage`, `wrkz-chain`, `wrkz-rpc` and `wrkz-node` with it |

It fails on the first problem, and everything passes `--locked`: CI proving a build that quietly resolved newer dependencies than `Cargo.lock` names is CI proving the wrong build.

The release profile turns `debug_assert!` and overflow checks off, so a release-only run executes neither. Both matter here (the difficulty and reward arithmetic is deliberately wrapping in some places and deliberately checked in others), so the debug profile is a separate pass, off by default because it rebuilds everything; the workflow runs it as its own job.

The `rocksdb` section needs libclang (`apt install libclang-dev`). Where there is none it says so out loud rather than passing silently, and fails if the section was named on the command line, as the workflow's `rocksdb` job names it. `WRKZ_SKIP_ROCKSDB=1` makes skipping it deliberate. With `WRKZ_DB` set, `wrkz-db-inspect` checks the top 1000 blocks and 100 rings of that C++ database at the end.

Not part of `ci.sh`: the network tests (`--ignored`) and the P2P probe, which `scripts/ubuntu-test.sh` runs, and the fuzz smoke run and the dependency audit, which the workflow runs as jobs of their own because each needs a toolchain the script does not assume.

## CI jobs

[`.github/workflows/ci.yml`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/.github/workflows/ci.yml) runs on every push to `development` and `main` and on every pull request. The jobs run in parallel, so a failure names itself. Each is pinned to `ubuntu-24.04` and to rustc 1.98.1 (the toolchain a release is cut with), because rustfmt's output and clippy's lint set both move between releases.

| Job | What it checks |
| --- | --- |
| `fmt` | `scripts/ci.sh fmt` |
| `test` | `scripts/ci.sh clippy tests docs` |
| `pluton` | `scripts/ci.sh pluton`, with the fontconfig development package Slint links |
| `rocksdb` | `scripts/ci.sh rocksdb`, with clang, libclang and cmake. Its own job because `librocksdb-sys` is the longest build in the tree |
| `debug` | `cargo test --workspace` in the debug profile: `debug_assert!` and overflow checks on |
| `fuzz` | On nightly, every libFuzzer target still builds and gets a short run (20,000 inputs or two minutes). Not a fuzzing campaign; see [Fuzzing](fuzzing.md) |
| `deny` | `cargo deny` over the workspace, then over Rust Pluton Wallet's own manifest with the same `deny.toml` |
| `cross` | macOS (both architectures), arm64 Linux and Windows, built from the Linux runner with `scripts/cross.sh` and `RUSTFLAGS=-D warnings`: the platform-only code the other jobs never compile. RocksDB and Android are left out |

[`.github/workflows/audit.yml`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/.github/workflows/audit.yml) runs the `deny` job every Monday as well, since advisories are published against crates this tree already locks, and can be run by hand from the Actions tab. Dependabot proposes grouped weekly updates for both Cargo workspaces and the actions, leaving the `=`-pinned `rocksdb` and Slint alone.

## Dependency audit

[`deny.toml`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/deny.toml) is cargo-deny's policy, over the whole dependency tree with every feature on (the RocksDB engine and the zstd paths are optional and would otherwise never be looked at):

```sh
cargo install cargo-deny --locked
cargo deny --workspace --all-features check
cargo deny --manifest-path apps/pluton/Cargo.toml --config deny.toml --all-features check
```

- **Advisories:** a published advisory stops the build, and so does a yanked crate. The ignore list is empty, and should stay that way.
- **Licences:** permissive licences only, from an allow list written from this tree's own lock file, so a new entry is a deliberate decision. The workspace's own crates, which are GPL-3.0-or-later and never published, are skipped.
- **Bans:** a `*` version requirement is refused, and so are async runtimes and web frameworks (`tokio`, `hyper`, `axum` and the like): the daemon, the wallet API and the service all speak HTTP/1.1 through `wrkz_rpc::http` on a thread pool, deliberately. Two versions of one crate is a warning.
- **Sources:** crates.io only; unknown registries and git sources are refused.

## `scripts/ubuntu-test.sh`

The full pass on the Linux host, after `scripts/ubuntu-setup.sh` once:

```sh
WRKZ_DB=$HOME/.WRKZCoin/DB scripts/ubuntu-test.sh
```

Point `WRKZ_DB` at the daemon's own `DB` directory with `Wrkzd` stopped, or at a copy of it: a RocksDB read-only open still needs the directory to be consistent. The C++ database is only ever opened read-only.

1. **Workspace tests**: `cargo test --workspace --release`.
2. **Live RPC**: the ignored tests of `wrkz-wallet`, against a seed node (outbound 17856).
3. **Live P2P probe**: `wrkz-p2p-probe node-fin.wrkz.work:17855 5` (outbound 17855).
4. **RocksDB engine**: builds `wrkz-db-inspect`, and with `WRKZ_DB` checks that database's headers, hashes, proofs of work and ring signatures.
5. **Offline replay**, with `WRKZ_DB`: `wrkz-replay` through the port's own consensus code, by default the rule-change windows with checkpoints off, a few minutes ([Replay](../tools/replay.md)).
6. **Live P2P sync**: a fresh `wrkz-node` syncs `WRKZ_SYNC_TO` blocks from the seed nodes over Levin alone, validating every one, and exits 0 at the height. It is bounded twice, by `--sync-to` and by `timeout`, so a seed node being down fails the script instead of hanging it.

| Variable | Default | Meaning |
| --- | --- | --- |
| `WRKZ_DB` | unset | A C++ database; without it steps 4 and 5 only build |
| `WRKZ_REPLAY_WINDOW` | 500 | Window size, blocks each side of a rule change |
| `WRKZ_REPLAY_STATE` | `target/replay-state` | Base of the replay's state directories, one per mode; delete one to start that pass over |
| `WRKZ_REPLAY_SAMPLE` | unset | Also replay *N* random windows |
| `WRKZ_REPLAY_SEED` | unset | Repeat a sampled run |
| `WRKZ_REPLAY_FULL` | unset | `1`: also the full linear pass, genesis to the tip with checkpoints as the C++ has them. Hours against a 40 GB database, but resumable |
| `WRKZ_SYNC_TO` | 2000 | Block index the live sync stops at |
| `WRKZ_SYNC_DIR` | `/tmp/wrkz-node-test` | The live sync's data directory, emptied first |
| `WRKZ_SYNC_TIMEOUT` | 600 | Seconds before the live sync is given up |
| `WRKZ_SYNC_PORT` | 0 | P2P port for the live sync; 0 takes a free one, so it cannot collide with a `Wrkzd` on the same host |

## `scripts/e2e.sh`

End to end on one machine: this port's daemon, its wallet sync and its transaction build.

1. build the daemon and the wallet tools;
2. bring a chain state up and start the daemon;
3. wait for the RPC to answer (and, with `WRKZ_STATE=sync`, sync to `WRKZ_SYNC_TO`);
4. call the endpoints an operator checks by hand (`/info`, `/getheight`, `/height`, `/peers`, `getblockcount`, `getlastblockheader`, `/getwalletsyncdata`, `/getrawblocks`, `/get_transactions_status`, `/getrandom_outs`), then compare their shapes against a C++ daemon with `wrkz-rpc-diff`;
5. sync a view wallet against the daemon (needs `WRKZ_VIEW_KEY` and `WRKZ_ADDRESS`);
6. build a transaction against the daemon, as a dry run (needs a synced wallet file and `WRKZ_SEND_TO`);
7. stop the daemon cleanly and report.

Steps 5 and 6 are skipped, with a message, when the keys are not supplied; every other step is required and any failure exits non-zero.

| Variable | Default | Meaning |
| --- | --- | --- |
| `WRKZ_DATA_DIR` | a temporary directory | Where the daemon's state lives |
| `WRKZ_STATE` | `import` when `WRKZ_DATA_DIR/state` exists | `import`: the state is already there; `sync`: sync from the network to `WRKZ_SYNC_TO`; `empty`: start from genesis offline |
| `WRKZ_SYNC_TO` | 2000 | Block index to sync to with `WRKZ_STATE=sync` |
| `WRKZ_RPC_PORT`, `WRKZ_P2P_PORT` | 17856, 17855 | The daemon's ports |
| `WRKZ_FEATURES` | `rocksdb` | Cargo features; `""` on a host with no libclang, which keeps the chain in memory |
| `WRKZ_VIEW_KEY`, `WRKZ_ADDRESS` | unset | The private view key and its standard address (step 5) |
| `WRKZ_SCAN_HEIGHT` | 0 | Where the wallet starts scanning |
| `WRKZ_SEND_TO`, `WRKZ_SEND_AMOUNT` | unset, 1000 | Destination and atomic units for the dry-run build (step 6) |
| `WRKZ_REFERENCE` | `http://node-fin.wrkz.work:17856` | A C++ daemon to compare shapes against; `""` skips it |

With an imported C++ database, the fast way:

```sh
cargo build --release -p wrkz-chain --bin wrkz-replay --features rocksdb
./target/release/wrkz-replay --store-raw --db ~/.WRKZCoin/DB --state ~/.wrkz-rust/state
WRKZ_DATA_DIR=~/.wrkz-rust WRKZ_STATE=import scripts/e2e.sh
```

## Dual run

`scripts/dual-run.py` compares a Rust node against the C++ daemon, block by block, for as long as you let it run. It is the acceptance tool for stage 3 step 6 of the [roadmap](../spec/12-roadmap.md#stage-3-daemon), the last gate before the port serves a public wallet or accepts mining.

```sh
scripts/dual-run.py --ours http://127.0.0.1:17856 \
                    --reference http://node-fin.wrkz.work:17856
```

Every `--interval` seconds it reads `/info` from both and records the two heights; for every block index both nodes now have and it has not yet compared, it fetches the header from each (`getblockheaderbyheight`) and compares the fields consensus fixes: hash, previous hash, major and minor version, nonce, timestamp, difficulty, reward and block size. It prints one line per poll, and on the first mismatch prints both headers in full, writes them to the report file and exits non-zero.

A divergence means one of the two nodes would fork off the network. It is never something to shrug at: capture the block index and both headers, and do not run the port against anything real until it is explained.

| Option | Default | Meaning |
| --- | --- | --- |
| `--ours URL` | required | RPC base URL of the Rust node |
| `--reference URL` | required | RPC base URL of the C++ daemon |
| `--from N` | near the common tip | First block index to compare |
| `--until N` | | Stop once this block index has been compared |
| `--interval S` | 30 | Seconds between polls |
| `--confirmations N` | 3 | Stay this many blocks behind both tips, since the top can reorganise |
| `--timeout S` | 20 | Per-request timeout |
| `--tolerate-outage S` | 300 | Seconds a node may stay unreachable before giving up |
| `--report FILE` | `dual-run-divergence.txt` | Where to write the divergence report; empty to disable |
| `--max-per-poll N` | 500 | Most blocks to compare in one poll, so catching up stays responsive |

Catching up from a low `--from` costs one request per block per node, so start it near the tip unless you mean to walk the whole chain. Exit status: 0 stopped cleanly (Ctrl-C or `--until` reached), 1 divergence found, 2 a node was unreachable for longer than `--tolerate-outage`. It needs only Python 3's standard library.
