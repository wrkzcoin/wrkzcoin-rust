# Verifying a chain state

`wrkz-verify-state` rebuilds a chain state someone else built (a snapshot, a copy of another node's `DIR/state`) from its own block bodies and compares it record for record, so a node can start from someone else's state without trusting them; this page is how to make such a snapshot, publish it, and use it safely.

Syncing 4.2 million blocks from peers takes a day or two; importing a C++ database with [`wrkz-replay`](replay.md) takes hours and needs a C++ node first. A third way is to start from **someone else's already-built state**, a snapshot, and to check it before trusting it.

The C++ node has no counterpart (its tools are in the C++ documentation's [Other tools](https://docs.wrkz.work/guides/other-tools/)). This is a full chain state. A lite node's snapshot is a different file with its own format; see [Lite node snapshots](../node/lite-snapshots.md).

## What a snapshot is

A snapshot is simply a copy of a node's `DIR/state` directory, taken while the node is stopped, packed into one archive. There is no special format: the daemon opens it exactly as it opens its own state, and it carries its own schema version and mode tag, which the daemon checks at start-up (a windowed replay's state is refused).

Use a state built **with block bodies**: a daemon's own, or a `wrkz-replay --store-raw` import. Without bodies it can be neither served nor verified.

## Making one

```sh
sudo systemctl stop wrkz-node            # or `exit` on the console; SIGTERM flushes the state
tar -C /var/lib/wrkz-rust -cf - state | zstd -T0 -19 > wrkz-state-4214000.tar.zst
sha256sum wrkz-state-4214000.tar.zst > wrkz-state-4214000.tar.zst.sha256
sudo systemctl start wrkz-node
```

Publish, next to the archive: its SHA-256, the top height and the top block hash (`curl -s localhost:17856/info` before stopping: `height` and `top_block_hash`), and the commit of the build that wrote it (`wrkz-node --version`).

## Why it has to be checked

Everything a node decides (whether an output exists, whether a key image is spent, what global index an output has) it reads from its state. A state built by a buggy or hostile node can hold records no honest node would have written, and a daemon running on it would accept or reject the wrong blocks and hand wallets the wrong outputs. The archive's checksum only proves you got what was published, not that what was published is right.

## Verifying it

`wrkz-verify-state` rebuilds the chain from the snapshot's **own block bodies** into a fresh state, through this port's full validation and the compiled-in checkpoints, and after every block compares every record that block wrote (block info, hash index, transaction list and index, each output record, each spent key image, payment-id entries, the body) byte for byte against the snapshot. At the end it compares the output count of every amount.

```sh
cargo build --release -p wrkz-chain --features rocksdb --bin wrkz-verify-state
mkdir -p /srv/snap && tar -C /srv/snap -xf wrkz-state-4214000.tar.zst --zstd
./target/release/wrkz-verify-state --given /srv/snap/state --state /var/lib/wrkz-rust/state
```

The program is in every release archive too, so the `cargo build` line is needed only when building from source.

The first record that differs stops it with the block and the record named (`VERIFY FAILED: block 3: the output record of amount … differs …`) and exit status 1. `VERIFY OK` means:

- the rebuilt state (`--state`) is correct by construction: nothing but block bodies went into it, and the bodies passed validation and the checkpoints;
- the snapshot holds exactly the same records, so it is correct too.

Either can be served. The rebuilt one is ready as it is: it carries every block body and every per-block output list; point the daemon's `--data-dir` at its parent.

It is resumable (run it again on the same `--state`), Ctrl-C stops it cleanly at a block boundary with `VERIFY INCOMPLETE` and exit status 0, and it runs at import speed: `--threads` spreads the signature and proof-of-work checks above the last checkpoint. The snapshot is opened read-only.

**What this does not buy you** is time: verifying costs about what an import from a C++ database costs, because it is the same work. The trade is that it needs no C++ node and no network. If you trust the publisher enough to run on the snapshot at once, you can start a daemon on one copy and verify a second copy alongside; stop and rebuild if the verification fails.

## Options

```text
wrkz-verify-state --given <state-dir> --state <new-state-dir>
                  [--threads N] [--progress N] [--to H] [--batch-blocks N] [--sync-every N]
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--given DIR` | required | The state to check, opened read-only |
| `--state DIR` | required | Where the rebuilt state goes; must not be `--given` |
| `--threads N` | detected parallelism, at most 32 | Threads for the signature and proof-of-work checks |
| `--progress N` | 10000 | A progress line every *N* blocks |
| `--to H` | the given state's top | Stop at this block index |
| `--batch-blocks N` | 1000 | Blocks committed in one write batch |
| `--sync-every N` | 250000 | Make the rebuilt state durable every *N* blocks |

## A state built by `wrkz-replay` before 2026-09-10

Imports made before then kept the per-block output lists (`/get_global_indexes_for_range`) for only the last 512 blocks. The daemon now rebuilds a missing list from the block body on demand, so such a state serves wallets correctly without re-importing, and `wrkz-verify-state` compares those outputs in full all the same. New `--store-raw` imports keep every list.
