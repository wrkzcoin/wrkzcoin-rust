# Bringing a chain up

There are three ways to give a node its chain — import a C++ node's database
with `wrkz-replay`, sync from the network, or import a dump file — and on a
machine that already has a C++ node the first is **much** faster.

The C++ counterpart is [Running a Node](https://docs.wrkz.work/guides/running-daemon/) on
docs.wrkz.work.

## Recommended: import the C++ database with `wrkz-replay` {#import-with-wrkz-replay}

`wrkz-replay` reads a C++ node's RocksDB read-only, validates every block
through this port's own consensus code, and writes **our** chain state into a
second directory. That directory is exactly what the daemon serves. Importing a
40 GB database and then catching up the last few thousand blocks over P2P is
far quicker than pulling 4.2 million blocks from peers, and it validates the
chain on the way in. [Replay and consensus checks](../tools/replay.md) covers
the tool itself.

```sh
cargo build --release -p wrkz-chain --bin wrkz-replay --features rocksdb

# Stop Wrkzd first, or copy its DB directory. The replay opens it read-only,
# but a live node writing under it will make the read inconsistent.
sudo systemctl stop wrkzd     # or however it is run

./target/release/wrkz-replay --store-raw \
    --db    "$HOME/.WRKZCoin/DB" \
    --state "$HOME/.wrkz-rust/state"
```

!!! warning "`--store-raw` is not optional for a node you intend to run"

    Without it the replay writes the indexes, block records, outputs, key
    images and the transaction index, but **not the block and transaction
    bytes** — which is why a body-less state is about a quarter of the size of
    the source. Such a state is perfectly good for *verifying* the chain, and a
    daemon on it will sync, validate and follow the tip, but for every height
    below the import it cannot serve a block to a syncing peer, cannot answer
    `/getrawblocks`, `getwalletsyncdata`, `gettransaction` or the block
    explorer methods, and would report a reorganisation as unavailable. A state
    cannot be topped up with bodies it never wrote: adding them means importing
    again into a new directory. The daemon says so at start-up, naming the
    height below which it holds no bodies.

    Leave it off only for a verification pass, and give that its own `--state`
    directory.

Expect the resulting state to be roughly the size of the source database, since
the bodies are the bulk of it. Once the daemon is running on it, the C++
database is redundant.

A `--store-raw` import keeps every block's list of the outputs it created,
which `/get_global_indexes_for_range` reads for any height a wallet asks
about. Imports made before 2026-09-10 kept only the last 512; the daemon
rebuilds a missing list from the block body when asked, so such a state serves
wallets correctly without being imported again.

A state built elsewhere — a snapshot — can be checked before it is trusted:
see [Verifying a chain state](../tools/verify-state.md).

The replay is **resumable**: run it again and it continues from the height it
reached, so it can go overnight in pieces. Ctrl-C or `SIGTERM` stops it
cleanly at a block boundary, commits and exits 0.

### Import tuning

The import is bound by round trips to the two databases, not by the CPU. The
defaults already batch both sides; these are the knobs if a run on your disk
says otherwise. Every progress line prints a phase breakdown — `source read`,
`decode`, `validate`, `commit`, `cross-check` — so start by reading which one
dominates.

| Option | Default | What it does |
| --- | --- | --- |
| `--threads N` | detected cores, capped at 32 | ring signature verification. Irrelevant inside the checkpoint zone, where signatures are not checked |
| `--batch-blocks N` | 1000 | blocks sharing one state write batch. `1` is the old per-block behaviour |
| `--batch-bytes MB` | 64 | the other bound on a batch, since blocks are not uniform |
| `--source-cache MB` | 512 | block cache for the source database |
| `--no-read-ahead` | off | one point lookup per record, as before |
| `--sync-every N` | 250000 | explicit flush, bounding what a crash can lose |
| `--wal` | off during import | put the write-ahead log back if you would rather not re-run a batch after a crash |
| `--compact` | off | compact the state once at the end |
| `--db-threads N` | the `--threads` value | RocksDB background jobs |

Raising `--batch-blocks` trades memory for fewer commits. The write-ahead log
is off during an import because the resume height is written inside the same
atomic batch as the blocks it covers, so a crash leaves a whole number of
blocks and a height that matches them; re-running continues from there.

### Pointing the daemon at it

Point the daemon at the *parent* of that directory:

```sh
./target/release/wrkz-node --data-dir "$HOME/.wrkz-rust"
```

The daemon expects the state under `--data-dir/state`, which is where
`wrkz-replay --state ~/.wrkz-rust/state` put it.

### State tags

`wrkz-replay` labels the state directory with the kind of run that produced
it, and the daemon checks that label:

| Tag | Written by | The daemon |
| --- | --- | --- |
| *(none)* | a fresh state, or one this daemon built by syncing | serves it |
| `linear` | `wrkz-replay --db … --state …`, genesis to the tip | serves it |
| `windows` | `wrkz-replay` in windowed mode (`--window`, `--sample`) | **refuses it** |

A windowed replay deliberately seeds block infos it never validated, so the
chain in such a directory has holes. Serving one would hand wallets blocks that
were never checked, so the daemon refuses with a message naming the fix. Use a
separate directory for windowed replays.

A state loaded from a lite node snapshot carries tags of its own; see
[Lite node snapshots](lite-snapshots.md#importing-one).

## Or: sync from the network {#sync-from-the-network}

With no state directory the daemon starts at genesis and syncs from the
compiled-in seed nodes:

```sh
./target/release/wrkz-node --data-dir "$HOME/.wrkz-rust"
```

This works, and it is the only option on a machine with no C++ database, but it
is slow — see [Sync throughput](internals.md#sync-throughput). For a smoke test,
stop at a low height:

```sh
./target/release/wrkz-node --data-dir /tmp/wrkz --sync-to 2000
```

## Or: import a dump file {#import-a-dump-file}

A `blockchain.dump` written by `wrkz-node --export-blockchain` or by a C++
`Wrkzd --export-blockchain` imports with `--import-blockchain`, every block
validated on the way in; see
[Maintenance](maintenance.md#export-and-import).
