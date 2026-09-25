# Replay and consensus checks

`wrkz-replay` reads a C++ node's database and validates its real blocks through this port's own consensus code, either to import the whole chain into a state a node can run on, or to check the consensus rules where they change; this page covers both uses and every option.

The C++ node has no counterpart; its own tools are in the C++ documentation's [Other tools](https://docs.wrkz.work/guides/other-tools/). `wrkz-replay` needs the `rocksdb` feature, and ships in every release archive:

```sh
cargo build --release -p wrkz-chain --features rocksdb --bin wrkz-replay
```

The C++ database is opened **read-only** and never written. Point `--db` at a copy, or stop `Wrkzd` first: a read-only open still needs the directory to be consistent. `--state` is a directory of this port's own, in its own key namespace.

After each block, the port's derived values are checked against the C++ records for that index (block hash, cumulative difficulty, coins generated so far, cumulative block size, transaction count and timestamp), and the run stops at the first mismatch, naming the index, the hash and what disagreed. A rejected block prints the index, the hash and the rule. Either way the exit status is non-zero. A clean run ends with `REPLAY OK`.

## Importing a C++ database

The fastest way to a synced node is to import a database you already have rather than syncing millions of blocks from peers:

```sh
wrkz-replay --store-raw --db /path/to/copy/of/DB --state ~/.wrkz-rust/state
```

then start the node on the parent directory:

```sh
wrkz-node --data-dir ~/.wrkz-rust
```

`--store-raw` keeps the block and transaction bytes, and a node you intend to run needs them. Without it the state is about a quarter of the size and is still fine for verifying the chain, but the daemon cannot serve any block below the import to a peer or a wallet. With `--store-raw` the import also keeps every block's output list, as the daemon itself does, so `/get_global_indexes_for_range` can answer at any height.

This is the default, **linear** mode: genesis to the top, in order, with checkpoints applied exactly as the C++ node applies them. Against a 40 GB database it takes hours. It is **resumable**: the applied height is part of the state, so a second run on the same `--state` continues from it, and the whole chain can be done in pieces overnight. Ctrl-C (or `SIGTERM`) stops it cleanly: it finishes the block it is on, commits the batch, makes the state durable, prints where it got to and exits 0. Killing it outright is safe too; the state is left at the last committed batch and the next run re-does that batch.

A state someone else built can be checked with [`wrkz-verify-state`](verify-state.md) before a node runs on it. [Bringing a chain up](../node/bringing-a-chain-up.md) compares the ways to get a synced node.

## Verifying consensus

The default acceptance run checks only the heights where consensus rules change, with checkpoints disabled so proof of work and signatures actually run. It takes minutes rather than a full pass over the whole chain:

```sh
wrkz-replay --db /path/to/copy/of/DB --state /tmp/replay-forks --windows forks --window 500
```

`--windows forks` replays a window around every height where a block version, a fee, a mixin tier, a difficulty algorithm, a size or unlock limit, or the transaction proof of work starts or stops applying, and the end of the checkpoint zone at 4,188,000. The list is taken from the port's own constants, so a fork height added there is covered here the day it lands. A window around height *H* is the *W* blocks ending at *H* and the *W* blocks after it, so the last block under the old rule and the first under the new one are both validated.

`--sample N` replays *N* random windows instead, also with checkpoints off. The seed is printed either way; pass it back with `--seed` to repeat a run exactly.

The linear pass is the only one that proves the emission and the cumulative difficulty of the whole chain. To run it with the proof of work and signatures checked below the checkpoints as well, add `--no-checkpoints-from H`.

A windowed run reseeds the state at each window, so it is not resumable and does not need to be. The two modes leave incompatible states behind (a windowed run seeds block records it never validated), so the state carries a mode tag and a run of one mode refuses a directory written by the other. Use one `--state` directory per mode. The daemon refuses a windowed state too.

## Options

Modes (pick one; the default is the linear pass):

| Option | Meaning |
| --- | --- |
| *(none)* | Genesis to the top, in order, checkpoints exactly as the C++ has them. Resumable |
| `--windows forks` | Only the blocks around every height where a rule changes, checkpoints off |
| `--sample N` | *N* random windows, checkpoints off |

Options:

| Option | Default | Meaning |
| --- | --- | --- |
| `--db DIR` | required | The C++ database, opened read-only |
| `--state DIR` | required | Where this port's state goes |
| `--window W` | 2000 | Window size, in blocks each side of a height (windowed modes) |
| `--seed S` | from the clock | The sampling seed; printed either way |
| `--from H`, `--to H` | | Restrict the linear pass |
| `--no-checkpoints-from H` | | Linear pass only: turn the checkpoint zone off from *H*, so proof of work and signatures run below it |
| `--progress N` | 10000 | A progress line every *N* blocks |
| `--store-raw` | off | Keep block bodies and every per-block output list in the state; needed by a node that will serve the chain |
| `--threads N` | detected parallelism, at most 32 | Verify a transaction's ring signatures on *N* threads. `--threads 1` is the sequential path; the result is the same at every value |
| `--legacy-transaction-list` | off | Below 600,000, check only that a block carries as many transactions as it names, as the C++ does. The strict check accepts the real chain; this exists so an import that ever meets an old block failing it can go on, and the block should be reported |

`--from`, `--to` and `--no-checkpoints-from` are refused in a windowed mode rather than silently ignored.

Import throughput. None of these can change what is accepted, what is rejected, or which rule a rejection names:

| Option | Default | Meaning |
| --- | --- | --- |
| `--batch-blocks N` | 1000 | Commit the state every *N* blocks in one write batch. `1` is one batch per block. The resume height is written in the same atomic batch, so a crash resumes at a batch boundary |
| `--batch-bytes MB` | 64 | Commit early when a batch reaches this size |
| `--sync-every N` | 250000 | Make the state durable every *N* blocks; matters only with the write-ahead log off |
| `--wal` | off | Keep the state's write-ahead log on during the import, trading throughput for a smaller loss window |
| `--no-read-ahead` | off | Read the source one record at a time rather than a window at a time |
| `--compact` | off | Compact the state database once the import is done (skipped if the run was interrupted) |
| `--db-threads N` | the validation thread count | Background threads and jobs for the state database |
| `--source-cache MB` | 512 | Block cache for the source database |

## From the test script

`scripts/ubuntu-test.sh` runs the replay when `WRKZ_DB` names a C++ database: the fork windows always (`WRKZ_REPLAY_WINDOW`, default 500), `WRKZ_REPLAY_SAMPLE=N` for random windows as well (`WRKZ_REPLAY_SEED` to repeat), and `WRKZ_REPLAY_FULL=1` for the full linear pass. The states go under `WRKZ_REPLAY_STATE` (default `target/replay-state`), one directory per mode. See [Testing](../contributing/testing.md).
