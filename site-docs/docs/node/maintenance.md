# Maintenance

`wrkz-node` has four one-off maintenance actions under the C++ `Wrkzd`'s names —
`--resync`, `--rewind-to-height`, `--export-blockchain` and
`--import-blockchain` — each of which runs once, before the node starts. Each
runs after the configuration is read and before the engine, the RPC or the
console start, and under the data-directory lock, so none of them can touch a
directory another `wrkz-node` is running on. They are command-line options only,
as in the C++: one set in a configuration file draws a warning and is ignored,
since a file read at every start is no place for `--resync`. The C++ options
are in its [Configuration Reference](https://docs.wrkz.work/guides/daemon-configuration/).

## `--resync` {#resync}

```sh
wrkz-node --data-dir DIR --resync
```

Deletes the chain state (`DIR/state`), `p2pstate.wrkz.bin` and
`p2panchors.wrkz.txt`, then starts at genesis and syncs from the network. There
is no prompt, as in the C++.

- **The ban list stays.** `p2pbans.wrkz.txt` is not a cache: it is the
  operator's `ban add`, or a peer that misbehaved badly enough to be shut out
  for a day, and a node syncing from genesis is the node that most needs to
  keep those peers out. The C++ has no such file.
- **The anchors go** with the peer state they are part of. A resync is how an
  operator starts over from a chain they no longer trust, and the peers that
  served it are the last ones to dial first.
- **The lock is taken before anything is deleted.** The C++ deletes without
  looking; here a `--resync` aimed at a directory another daemon holds exits
  with the lock message and deletes nothing. The lock covers `wrkz-node` only:
  stop a `wrkz-replay` writing into `DIR/state` before resyncing over it.
- A deletion that fails exits 1 with the C++'s `Could not delete data path`
  and the reason.

## `--rewind-to-height N` {#rewind-to-height}

```sh
wrkz-node --data-dir DIR --rewind-to-height 4200000
```

Removes every block from height `N` up — the block at `N - 1` becomes the top,
so the chain is `N` blocks tall — then starts as usual and syncs forward again.
For a node stuck on a fork, or to fetch a stretch of the chain again.

- **One atomic write.** Interrupted, the state is at the old top or at the new
  one, never in between. Everything the removed blocks wrote is taken back out:
  the state is record for record what it was when it was `N` blocks tall, which
  [`crates/wrkz-chain/tests/rewind.rs`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/crates/wrkz-chain/tests/rewind.rs)
  checks 600 blocks deep, and applying the same blocks again gives back the
  state from before the rewind.
- **The C++'s limits.** `N` at most 4,320 blocks below the top index
  (`MAX_BLOCK_ALLOWED_TO_REWIND`, three days), and not below a lite node's
  `--lite-height`. `0` is refused in favour of `--resync`. A height at or above
  the chain's leaves nothing to do, and the node starts.
- **A rewind that cannot be done exits 1 and changes nothing.** The C++ logs
  the depth limit at `INFO` and starts anyway, on the chain the operator asked
  to leave.
- **`N` is always the height afterwards.** The C++'s early return compares a
  count with an index, so `N` equal to the top index does nothing there while
  `N - 1` removes two blocks. Here the first removes the top block.
- **A short chain can be rewound.** The C++ refuses any rewind on a chain of
  fewer than 4,320 blocks, which protects nothing.
- **No block bodies needed.** The daemon keeps, for every block, the records
  that undo it: its outputs, key images, transaction and payment-id entries. A
  state imported with a short unwind history (`wrkz-replay` before 2026-09-10
  kept 512 blocks of output records) rebuilds the missing ones from the bodies;
  a state with neither refuses, naming the block. So a pruned node can rewind
  past its window, which the C++ cannot; the bodies it pruned on the way up do
  not come back, and re-syncing the removed blocks prunes the same ones again.

## `--export-blockchain` and `--import-blockchain` {#export-and-import}

```sh
wrkz-node --data-dir DIR    --export-blockchain --dump-file /backup/wrkz.dump
wrkz-node --data-dir NEWDIR --import-blockchain --dump-file /backup/wrkz.dump
```

Write the chain to a dump file, or apply one to the state, and exit: `0` with
`Time to export N seconds.` (or `import`) on stdout, or `1` with
`Failed to export blockchain: <reason>` (or `import`). Given both, the import
runs. `--dump-file` defaults to `blockchain.dump` in the **current directory**,
not the data directory, as in the C++.

### Export

**Export** refuses when the file exists (and never overwrites one that appears
meanwhile), when the chain is shorter than 1,000 blocks or
`--max-export-blocks` is below that, on a lite node, and on a state with no
block body at height 1 — a pruned node, or a `wrkz-replay` import made without
`--store-raw`. `--max-export-blocks N` exports a chain of `N` blocks: genesis
and heights 1 to `N - 1`, the C++'s arithmetic, so the same option makes the
same file; a larger `N` than the chain holds exports all of it, with a note. A
failed or interrupted (Ctrl-C) export deletes the partial file, since a dump
that stops early imports cleanly up to where it stops.

### Import

**Import** does not need an empty state. Records at or below the state's top
when it starts are stepped over without being read, so a rerun resumes, and a
dump can be applied to a node that has already synced part of the chain. The
first record applied must be the next block and name the state's top as its
parent.

**The file is never trusted**, and this is the one real departure from the
C++: there, an import without `--import-validate` pushes each block straight
into the database with no proof-of-work, signature or double-spend check. Here
every block goes through the path a peer's block takes, under the same
checkpoints: as fast as a peer sync below the last checkpoint, whose hash
vouches for each block, and every rule above it. `--import-validate` is
accepted so a C++ command line runs, and changes nothing; add
`--no-checkpoints` to verify proof of work and ring signatures below the last
checkpoint as well, which is much slower.

The import commits as the node syncs: in batches of `--batch-blocks` and
`--batch-bytes`, with the write-ahead log off unless `--wal`, and the resume
height inside every batch. It stops at the first record that is wrong — cut
short, out of order, unparseable, or refused by a consensus rule — naming the
height, and commits every whole block before it, so fixing or re-fetching the
file and running the same command resumes from there. Ctrl-C stops it at a
block boundary the same way, exiting 1. A lite or pruned node keeps the bodies
of imported blocks exactly as a sync would.

Smaller differences from the C++ reader: a height or length must be plain
decimal digits (`std::stoull` also takes a sign and stops at the first
non-digit; the writer produces neither), and a file that ends after a height
but before its length is an error, where the C++ reports success at the wrong
height. The body's second transaction count is read and not used, as the C++
reads it. Progress lines, every 10,000 blocks, print UTC rather than local
time.

### The dump file

The file is the C++ daemon's, byte for byte, so a dump written by either
daemon imports into the other. It is a flat run of records with no header,
version, checksum or compression:

```text
<height> SP <length> SP <length bytes of body> SP
```

`height` (the block index, from 1: genesis is constructed, never written) and
`length` are ASCII decimal, and `SP` is one space (0x20). The body is the C++
`RawBlock` through its binary serializer, whose integers are unsigned LEB128
varints:

```text
varint block_size,  block_size bytes: the block blob
varint tx_count,    varint tx_count again (the array header repeats it)
tx_count times:     varint tx_size, tx_size bytes: a transaction blob
```

That is neither the C++ database's raw block record nor this port's `W r`
record.
[`wrkz_chain::dump`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/crates/wrkz-chain/src/dump.rs)
reads and writes it.
