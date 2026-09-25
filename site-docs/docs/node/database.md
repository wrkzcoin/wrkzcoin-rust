# The database

With `--features rocksdb` the chain state is a RocksDB database in
`--data-dir/state`, opened with the C++ node's options at the C++ daemon's
defaults; this page covers tuning it and how it is compacted. A build without
the feature keeps the chain in memory and loses it on exit. The C++ `db-*`
options are in its [Configuration Reference](https://docs.wrkz.work/guides/daemon-configuration/).

## Database tuning

None of the `--db-*` options is recorded in the database, and none changes what
the chain accepts: a database written under one set opens under any other.
Block size, compression and filters apply to the SST files written from then
on; `compact_db force` rewrites the rest. The options and their defaults are in
[Configuration](configuration.md#command-line).

- **Read cache.** `--db-read-buffer-size` (256 MB) is split in two: a **row
  cache**, which answers a repeated point lookup from the finished key and
  value without touching a block, and a **block cache** for everything else,
  index and filter blocks included. The row cache is an eighth by default (32 MB
  of 256), or `--db-row-cache-percent` of the whole, at most 90. The two add up
  to what was asked for.
- **Write buffer.** `--db-write-buffer-size` (64 MB) is the memtable size. SST
  files are half of it (at least 8 MB) and level 1 four times it (at least
  64 MB).
- **Blocks.** `--db-block-size` (4 KB) is the uncompressed data block. Larger
  compresses better and makes every point lookup read and decompress more.
- **Compression.** ZSTD from level 2 down, and none on levels 0 and 1, where
  new data lands and compaction reads most. `--db-enable-compression=false`
  turns it off. `--db-compression-level` sets the level for the bottommost
  level, which holds nearly all the data; `--db-compression-dict-bytes` trains a
  dictionary per SST file on a hundred times that much data. Both cost
  compaction time and nothing else.
- **Bottommost filters.** By default the bottommost level is written without
  bloom filters and read without consulting them: right for a lookup that
  finds its key, which most do, and a block read for one that does not. Spent
  key image checks are lookups meant to miss, and `--db-bottom-filters` buys
  their filters back at the cost of space.
- The engine's own `LOG` file in the database directory carries warnings and
  errors only.

Where this differs from `Wrkzd`: bloom filters are sized with
`optimize_filters_for_memory`, which RocksDB 10.10 (the C++ node's) turns on by
default and the bundled 8.10 does not, so it is set explicitly;
`--db-threads 0`, `--db-write-buffer-size 0` and negative sizes are refused
rather than handed to RocksDB; a dictionary too large for RocksDB's `int`
saturates instead of wrapping; and a configuration file's `db-*` values are not
reset by a second pass over the command line
([Configuration file](configuration.md#configuration-file)).

`wrkz-replay` and `wrkz-verify-state` open their databases with their own
profiles and none of this reaches them: they keep 16 KB blocks, the whole read
cache as block cache, filters on every level (an import's lookups are misses)
and RocksDB's default log. `wrkz-db-inspect` opens a `Wrkzd` database with the
daemon's options, which are that database's own; see
[Diagnostics](../tools/diagnostics.md).

## Database compaction

A compaction rewrites SST files to drop overwritten and deleted records and to
move data down the levels. RocksDB compacts on its own as data arrives; these
are the **full** compactions the C++ daemon runs on top, and `compact_db`.

- **At start-up** a full compaction starts in the background, on every start.
  `--skip-boot-compaction` skips it.
- **Periodically** a scheduler looks every 60 seconds — every 30 minutes once
  the node has been within two blocks of the network for three looks running,
  and every 60 seconds again once it falls 20 behind — and starts one when none
  is running, the state directory's filesystem has
  `--auto-compaction-min-free-bytes` free (8 GiB), at least
  `--auto-compaction-min-gap-blocks` blocks (720, about twelve hours of chain)
  have arrived since the last one started or finished, and at least 30 minutes
  have passed since then too. That last rule is what stops a syncing node, which
  passes 720 blocks in seconds, from rewriting its database every minute.
  `--auto-compaction-min-gap-blocks 0` turns the schedule off.
- **On request**, on the console:

    ```text
    > compact_db
    DB compaction started in background. Use `compact_db status` or `compact_db wait`.
    > compact_db status
    DB compaction status: running (512s elapsed)
    Started by: manual console request
    > compact_db wait
    Waiting for DB compaction to complete...
    DB compaction completed.
    ```

    `compact_db start` is the same as `compact_db`. `compact_db force` also
    rewrites the bottommost level, which an ordinary compaction leaves alone
    because after the first one it holds nearly the whole database. Forcing it
    is slow and needs free space about the size of the database; it is how
    changed compression, block size and filter settings reach data already
    written.

The node keeps syncing and serving throughout: writes and RocksDB's own
compactions carry on beside a full one. `db_status` prints the compaction state
beside RocksDB's own counters: whether a compaction is pending, how many are
running, the estimated bytes still to compact, the live SST size and the
background errors.

**The marker.** `state/.compact_db_in_progress` is written when a compaction
starts and removed only when one completes. A start that finds it logs
`Detected unfinished DB compaction marker from previous run` and compacts again;
`compact_db status` and `db_status` report it.

**Shutdown.** A running compaction is stopped at shutdown, keeps its marker, and
the next start resumes the work. rocksdb 0.22 cannot cancel one compaction the
way the C++ does: the only way to stop it stops all of the engine's background
work, flushes included, until the database is reopened. So the daemon does that
last. The scheduler stops as soon as the shutdown begins; a running compaction
is stopped only once the chain state has been committed.

### Where this differs from `Wrkzd`, and why {#compaction-differences}

- With `--skip-boot-compaction` the C++ scheduler has seen no compaction, so it
  starts one about a minute after start-up anyway. Here the periodic rules count
  from start-up.
- The C++ `compact_db wait` holds the compaction lock while it waits, which
  stalls the scheduler, `compact_db status` and shutdown for the whole
  compaction. Here it does not.
- The C++ block gap applies only once the height has risen past the last
  compaction, so a node whose chain has stalled compacts every half hour over no
  new data. Here no new blocks is fewer blocks than the gap.
- Low free space is logged once when it happens and once when it clears, not at
  every check, and no scheduler runs at all with the gap at `0`.
- A failure is RocksDB's background-error counter rising during the pass,
  because rocksdb 0.22's `compact_range_opt` returns no status. The engine's
  `LOG` file names the error.
- The marker lives in `state/`, where this port's database is, rather than `DB/`.
