# Lite node snapshots

A lite node snapshot packs the region below a lite node's height into one file,
so a node can start at that height after a file transfer instead of rebuilding
the region from genesis; `wrkz-node` reads and writes the C++ `Wrkzd`'s format
in both directions.

A lite node below its lite height holds exactly three tables that a network
cannot hand it quickly: block info, spent key images and key outputs. The C++
packs that region `[0, H)` into a **lite node base snapshot**
(`src/daemon/LiteSnapshot.h`; the C++ guide is
[Lite Snapshots](https://docs.wrkz.work/guides/lite-snapshots/)). A snapshot
`Wrkzd` exported imports here, and one exported here imports into `Wrkzd` and
carries the digest a C++ node computes for the same chain and `H`. See
[Lite node](reduced-modes.md#lite-node) for the mode itself.

```text
wrkz-node --snapshot-info wrkz-lite-base-h4000000-v1.litesnap
wrkz-node --data-dir DIR --lite --lite-height 4000000 --import-lite-snapshot wrkz-lite-base-h4000000-v1.litesnap
wrkz-node --data-dir DIR --lite --lite-height 4000000
> snapshot_export [start [height] [path] | status | cancel]
wrkz-node --data-dir DIR --snapshot-stats
```

A snapshot is a pure function of the chain and `H`: two nodes at different tips
produce the same payload, so its digest can be published and compiled in. The
digest is the whole security of an import — everything below `H` is taken on
trust, because it cannot be checked without the block bodies the importing node
does not have — so **only a digest compiled into the binary is importable**, and
there is no flag to override that, here or in the C++. This build knows one:

| `H` | payload digest |
| --- | --- |
| 4,000,000 | `4601d802d990fa26b876ed7fdaffc00953cff6ca6b77299fd1c6981ef94fe09e` |

## The file

```text
header   128 bytes, little-endian, written last over a reserved copy
frame*   u32 rawLen | u32 compLen | compLen bytes of one zstd frame (level 10)
end      u32 0 | u32 0
```

| offset | width | field |
| --- | --- | --- |
| 0 | 8 | magic `WRKZLITE` |
| 8 | u32 | format version, 1; any other is refused |
| 12 | 32 | genesis hash |
| 44 | u32 | lite height `H` |
| 48 | u64 | block info records (`H`) |
| 56 | u64 | key image records |
| 64 | u64 | amount count records (never set: always 0) |
| 72 | u64 | key output records |
| 80 | u64 | transactions count: `alreadyGeneratedTransactions` of block `H - 1` |
| 88 | u64 | distinct key output amounts |
| 96 | 32 | payload digest |

A raw frame holds `LEB128(keyLen) key LEB128(valueLen) value` records, keys
strictly ascending across the file; the writer closes a frame after the record
that brings it to 4 MiB or more, and a reader refuses a length above 64 MiB. The
digest chains over the **raw** frames — `running = cn_fast_hash(running ‖
cn_fast_hash(frame))` from 32 zero bytes — so it depends on the records and the
4 MiB rule and not on zstd: two builds on different zstd versions may write
different files for the same chain, and always the same digest. (This build
links zstd 1.5.7; a file is byte-identical to the C++'s only when the C++ build
linked the same.)

The records are the C++ database's own KV documents for three tables, in the
order RocksDB iterates them:

| table | key | value | filter |
| --- | --- | --- | --- |
| `6` block info | block index, u32 LE (35 bytes) | `CachedBlockInfo`, verbatim (203 bytes) | index `< H` |
| `7` key image | the image (64 bytes) | the spending block, u32 (17 bytes) | spent `< H` |
| `j` key output | amount u64 LE, global index u32 LE (59 bytes) | `KeyOutputInfo`, `transactionHash` zeroed (164 bytes) | created `< H` |

## Importing one

`--import-lite-snapshot FILE` needs `--lite` and the `--lite-height` the file was
made at, runs on a data directory holding nothing but genesis, and **exits**
either way — status 0 when it imported, 1 when it did not — exactly as `Wrkzd`
does. Start the daemon again without the flag and it syncs on from `H`.

It refuses, before writing anything and in the C++'s words: a database that
holds more than genesis, another chain's genesis, another height, a digest not
compiled in. Then it reads the whole file once without writing — every record
of a table a snapshot may carry, the digest the header claims, exactly `H`
distinct block infos all below `H`, cumulative difficulty, generated coins and
transaction count never falling, every compiled-in checkpoint below `H`, and the
header's counts — and only then reads it again to write. Progress goes to stdout
in the C++'s machine-readable form, one line per two million records:

```text
WRKZ-IMPORT {"phase":"verify","done":2000000,"total":148728732,"percent":1.3}
```

Each record is transcoded into this port's own key namespace
(`wrkz_chain::keys`) and written through the daemon's batching store, so memory
is bounded by `--batch-bytes`, not by the 5 GiB file: `6` becomes the block info
and its hash → index record, `7` the spent key image, `j` the output record, and
the per-amount output counts are derived from the outputs, as the C++ derives
its `b` and `h`. The state is then tagged **`lite-snapshot`**, and its tip is
`H - 1`.

Where this importer is stricter than the C++ — every one of these is a check a
file the C++ exporter wrote always passes, and all of them run in the verifying
pass:

- the distinct-amount count is compared with the header **before** anything is
  written (the C++ compares it after, and then says the database must be deleted);
- each amount's outputs must be the global indexes `0 .. n` without a gap (the
  C++ counts them and trusts the rest; a gap would put every output this node
  writes afterwards at the wrong index);
- a key image spent, or an output created, at or above `H` is refused;
- block 0 and its outputs must be this daemon's own genesis; genesis is left as
  the node constructed it, raw block and real transaction hashes included;
- the state is tagged `lite-snapshot-importing` before the first record is
  written, so an import that is killed half way leaves a directory the daemon
  refuses to serve and a second import refuses to finish, rather than one that
  looks like a fresh chain with key images already spent. Delete it and import
  again;
- keys out of order, and bytes after the terminator, are refused when reading.

## What an imported node can and cannot answer

An imported state is a lite node: it opens only with the matching `--lite
--lite-height`, validates every block from `H` exactly as a full node does, and
refuses reorganisations below `H`. Below `H` it holds no transaction records at
all — no transaction index, no per-block transaction list, no payment ids, and
a zero transaction hash in every output — which a lite node that synced its own
region does keep (see [Follow-ups](internals.md#follow-ups)). Every question
that would need them is refused with the lite error rather than answered
wrongly:

| request | synced lite node | imported lite node |
| --- | --- | --- |
| block headers, bodies, wallet sync, `/getrawblocks`, P2P blocks below `H` | refused / clamped to `H` | refused / clamped to `H` |
| `/get_global_indexes_for_range` below `H` | 400, as the C++ | 400, as the C++ |
| `/get_o_indexes` for a transaction the node cannot find | 500 "Failed to getTransactionGlobalIndexes" | **400** "This node is a lite node and stores no transaction data below height H" |
| `print_tx`, `f_transaction_json` for one it cannot find | "not found" | **the lite refusal**: it cannot tell a transaction below `H` from one never mined |
| payment-id lookups | answered | **refused** |
| `/getrandom_outs`, ring member resolution, double-spend checks | answered | answered (public key, unlock time and block index are all carried) |
| `/get_transactions_status` | in block / pool / unknown | a transaction mined below `H` reads **unknown**, as on a C++ lite node |

`/get_transactions_status` keeps the C++'s answer on purpose: its three lists
have no place for "cannot say", a wallet only asks about its own recent sends,
and those are always above a line that is at least 20,160 blocks deep.
`prune_status` and `db_status` print `Transaction Records From: H`, and the
start-up log says the node was imported.

## Exporting one

```text
> snapshot_export start [height] [path]
> snapshot_export status
> snapshot_export cancel
```

The height defaults to a lite node's own lite height and must match it; a full
node has to name one. The export refuses when the node is not yet at least
20,160 blocks (`MIN_LITE_FULL_BLOCK_DEPTH`) above the height, when the output
file exists, and when the free space where it would go is less than the database
it comes from. The default path is `wrkz-lite-base-h<H>-v1.litesnap` in the
parent of the data directory; a directory argument gets that name appended.

It runs on its own thread while the node carries on. `status` shows the table,
the records kept of those scanned and the elapsed time, and after it finishes
the path, the record count and the **digest**, which is also logged; `cancel`
stops it and removes the partial file, and so does shutting the daemon down.
A full node, a lite node that synced, and a lite node imported from a snapshot
all export the same payload for the same chain and height: outputs are written
with their transaction hash zeroed whatever the state holds.

Where the C++ walks its database under one RocksDB snapshot, this reads the
chain in batches — a few thousand records per read lock — and releases the lock
before compressing or writing, so a block is never kept waiting on the export.
Nothing below `H` can change meanwhile: a lite node refuses any reorganisation
below its line, and the depth refusal keeps the region 20,160 blocks under a
tip that can reorganise 180. The walk reproduces the C++'s RocksDB order itself
— block indexes and global indexes in the byte order of their little-endian
encoding, amounts likewise — because this port keys the same records
big-endian.

## Measuring a database

`--snapshot-stats` walks every table of the state, prints records and logical
megabytes per table, the total, the on-disk size and the compression ratio, and
exits; on a synced chain it takes minutes. The tables are this port's, not the
C++'s, so the rows differ: the `(snapshot)` rows are the three a snapshot
carries, `(derived)` the two an import rebuilds, `(lite drops)` the block bodies
a lite node never keeps and `(import drops)` the records an imported node does
not have below its height. One extra row, `C++ snapshot payload`, is what the
snapshot tables come to in the C++ encoding the file holds — the number the
C++'s own "Snapshot payload" row reports for the same chain.

Where this all differs from the C++ is gathered in
[Lite node snapshots: divergences](internals.md#snapshot-divergences), and how
the format was proven against the real published file is in
[Acceptance on a real chain](internals.md#snapshot-acceptance).
