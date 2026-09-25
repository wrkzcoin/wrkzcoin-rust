# Internals and differences from the C++

This page is for readers who want to know how `wrkz-node` is built inside, how
fast it syncs and why, every place it deliberately behaves differently from the
C++ `Wrkzd`, how it has been checked against the C++, and what it does not do
yet. The operator pages link here rather than repeat it.

## How it is put together

One process, three parts:

- the **P2P engine** (`wrkz_node::Node`), single-threaded, the only *writer* of
  the chain state;
- the **RPC server** (`wrkz_rpc`), a fixed, bounded pool of worker threads that
  only ever *read* the chain;
- one **chain state** behind an `RwLock` and one **transaction pool** behind a
  `Mutex`, shared by both.

`RwLock` and not a channel to the engine, for the reason the C++ uses a
`std::shared_mutex` for `Core::m_chainMutex`: every read path takes it shared
and only `addBlock` takes it exclusively. So

- several RPC calls answer at once and none blocks another;
- a block arriving waits only for the reads already in flight, and holds the
  exclusive guard for exactly one write batch;
- an RPC call never waits for the engine's event loop, which is what a command
  channel would have made it do.

The lock is never held across a socket write: a handler assembles its value,
drops the guard, and the server serialises the response afterwards. The two
locks are always taken chain-then-pool, everywhere, so they cannot deadlock.

One **pool**, not two: what the RPC accepts on `/sendrawtransaction` is what the
engine relays to peers and what `getblocktemplate` mines. Transactions the RPC
accepted are drained into the engine once per loop and relayed with
`NOTIFY_NEW_TRANSACTIONS`, which is what `src/rpc/RpcServer.cpp:1142` does.

The design of the whole port is in
[Architecture](../spec/00-architecture.md), and the on-disk layout in
[Storage](../spec/11-storage.md).

## Sync throughput

Two measurements, both syncing from the seed nodes into `MemStore` with the
RPC server running in the same process:

| Where | Blocks/s |
| --- | --- |
| Linux host, earlier build | ~53 |
| Windows dev host, release, blocks 1 – 28,000 | 115 – 153 |

The second number is flattered by *which* blocks those are: the first 30,000
blocks of this chain are tiny and almost all empty, so they cost little to
validate and little to write. Take the lower figure as the planning number for
the long middle of the chain, where blocks carry transactions and ring
signatures.

At 50 blocks/s the full 4.2-million-block chain is roughly a day, and with one
write batch per block RocksDB is *slower* than `MemStore`, not faster: every
block costs a WAL append and a fsync-class round trip that a `BTreeMap` insert
does not. Expect **30–50 blocks/s** with RocksDB on an SSD, worse on spinning
disk or on a small VPS — one to two days for a full sync.

This is why the **replay-import path is the primary route**, not a shortcut
(see [Bringing a chain up](bringing-a-chain-up.md)):

- `wrkz-replay` reads the C++ database sequentially and validates as it goes,
  with no network round trips and no peer stalls;
- afterwards the daemon has only the few thousand blocks since the import to
  pull over P2P, which takes minutes.

What this crate does on its own side of the line: a downloaded batch is applied
in one pass without re-parsing, blobs already held are not re-serialised, and
the RPC lock is off the apply path entirely (the engine takes the write guard
only inside `add_block`, and the pool bookkeeping afterwards runs without it).

**Batched commits.** The figures above were taken with one RocksDB write batch
per block. The daemon now puts the import's `BatchStore` in front of the
engine: the blocks of one downloaded batch (120–600 blocks) accumulate in an
in-memory overlay that every read consults first, and reach RocksDB as **one**
write batch when the engine has finished with that event. A relayed block at
the tip is still committed on its own, and a block mined through
`submitblock` is committed before the call returns. The resume height travels
in the same atomic batch, so a crash leaves a whole number of blocks and the
next run re-syncs the rest. `--batch-blocks` and `--batch-bytes` bound what is
held back; `--batch-blocks 1` restores the old behaviour. **Not yet measured on
the Linux host** — re-take the table above with `--features rocksdb` before
quoting a number.

**No write-ahead log during initial sync.** Until a peer first confirms we hold
its top block, chain state is written with RocksDB's write-ahead log off, as
`wrkz-replay` runs an import. That is safe for the same reason batching is: the
resume height travels in the same atomic batch as its blocks and RocksDB
flushes memtables in order, so a crash loses the blocks since the last memtable
flush — re-downloaded on the next start — and never leaves an inconsistent
state. On synchronizing, the memtables are flushed to SST files first and the
log is switched back on for good; the log lines `initial sync: … without the
write-ahead log` and `synchronized: … through the write-ahead log again` mark
both switches. `--wal` keeps the log on throughout.

**Proof of work off the critical path.** Above the last checkpoint every block
costs a CryptoNight hash (about 0.8 ms for `cn_upx`) and every transaction
another. Both now run on the validation threads (`--threads`): a downloaded
batch's block hashes are computed across them before the batch is applied, and
a block's transaction proofs of work join the same parallel pass as its ring
signatures. Which blocks are accepted, and which rule a rejection names, are
unchanged at every thread count; a transaction arriving for the pool still
pays for its proof of work up front.

**Wallet bandwidth.** Responses are gzipped for clients that send
`Accept-Encoding: gzip` (this port's wallet does, and so does a C++ wallet
built with zlib), and `/getwalletsyncdata` answers far enough behind the tip
are cached (`--rpc-sync-cache-size`), so a public node serving many wallets
builds each range once. `curl --compressed` shows the first.

## Divergences from the C++, in one place {#divergences}

Each of these is deliberate, and each is described where it matters on the
operator pages; this is the list.

### Configuration {#configuration-divergences}

See [Configuration](configuration.md).

- A `--load-checkpoints` file that names a different hash at an index the
  compiled-in table covers is refused.
- `--no-upnp` is this port's own; the C++ always tries UPnP.
- A configuration file's `db-*` values are not reset by a second pass over the
  command line.
- `--decoy-selection recent` and `--rpc-stream-threshold` exist only here, and
  both default to the C++'s behaviour.

### Networking {#networking-divergences}

See [Networking](networking.md).

- Priority and exclusive nodes take hostnames and IPv6 literals, an entry that
  does not resolve stops the start, a failing priority node is backed off, and
  `--add-peer` never overwrites an existing white-list entry
  ([details](networking.md#priority-differences)).
- The IPv6 RPC listener starts on `--rpc-bind-ipv6-address` alone; the C++
  also wants `--rpc-use-ipv6`, which is accepted here and changes nothing.
- UPnP does not hold up the start, can be turned off, is skipped where it
  cannot help, and its mapping is removed on a clean shutdown
  ([details](networking.md#upnp-differences)).

### Maintenance {#maintenance-divergences}

See [Maintenance](maintenance.md).

- `--resync` keeps the ban list and takes the data-directory lock before
  deleting anything.
- `--rewind-to-height` exits 1 and changes nothing when it cannot be done,
  always leaves `N` blocks, works on a chain shorter than 4,320 blocks, and
  needs no block bodies.
- `--import-blockchain` validates every block whether or not
  `--import-validate` is given, and its reader is stricter about malformed
  heights and lengths.

### Reduced modes {#reduced-mode-divergences}

See [Lite, pruned and explorer](reduced-modes.md).

- A prune depth below 181 is refused in `wrkz-chain`, below the 10,080 clamp.
- The catch-up prune pass keeps a resume point instead of rescanning from
  height 0.
- Explorer lookups below a pruned node's window return an error naming the
  mode instead of `map::at`.
- A lite node keeps the transaction records the C++ drops below its height
  ([Follow-ups](#follow-ups)).

### Lite node snapshots {#snapshot-divergences}

See [Lite node snapshots](lite-snapshots.md).

- the stricter import checks listed under
  [Importing one](lite-snapshots.md#importing-one), all in the verifying pass;
- keys out of order and trailing bytes refused when reading;
- `--snapshot-info` escapes its error message, where the C++ pastes it into the
  JSON line unescaped (a Windows path made that line invalid JSON);
- an export refuses to overwrite a file even if one appears after the check;
- an interrupted import is marked, and refused by the daemon;
- an imported node refuses transaction lookups it cannot answer rather than
  reporting "not found";
- `--snapshot-stats` measures this port's tables, not the C++'s, and adds the
  C++ payload estimate.

### Database {#database-divergences}

See [The database](database.md).

- Bloom filters are sized with `optimize_filters_for_memory` explicitly, and
  zero or negative sizes are refused.
- Compaction: `--skip-boot-compaction` really skips, `compact_db wait` does not
  hold the lock, a stalled chain does not compact every half hour, and the
  marker lives in `state/` ([details](database.md#compaction-differences)).

### Console and attach {#console-divergences}

See [Console and IPC](console-and-ipc.md).

- `print_block`, `print_tx` and `print_pl` print labelled fields rather than raw
  JSON; `print_bc` and `log_tail` are this port's own.
- The attach prompt is drawn only on a terminal, and Ctrl+C leaves with status 0
  even in the middle of a command.

### ZMQ and hooks {#zmq-divergences}

See [ZMQ and notify hooks](zmq-and-hooks.md).

- `txpool_del` with `InBlock` lists what the block mined, and an empty one is
  not sent.
- No libzmq: the daemon speaks ZMTP 3.1 itself; a bracketed IPv6 address binds,
  and an `ipc://` socket is owner-only.
- A hook's standard input is empty, and a failing hook is logged on its first
  failure, every hundredth after and on recovery.

## Where the behaviour comes from in the C++ {#cpp-references}

The operator pages describe behaviour; these are the places in the C++ WrkzCoin
source, at commit `8d89d7bf`, that each part follows, for anyone checking one
against the other.

| Area | C++ source |
| --- | --- |
| Command-line names | `src/daemon/DaemonConfiguration.h`; the configuration file reader `src/daemon/DaemonConfiguration.cpp:1598`; the second command-line pass that resets `db-*` values `src/daemon/DaemonConfiguration.cpp:650-653` |
| IPv6 options | `src/daemon/DaemonConfiguration.cpp:332-345`; the P2P listener enabled by `m_enableIPv6 = !m_bind_ipv6.empty()` (`src/p2p/NetNode.cpp:491`); the RPC one by `m_ipv6Host`, which the C++ sets only with `--rpc-use-ipv6` as well as an address (`src/rpc/RpcServer.cpp:97`) |
| IPv6 listener | `acceptLoopIPv6` (`src/p2p/NetNode.cpp:2656`, spawned at `src/p2p/NetNode.cpp:770`); `m_bindPortIpv6 = (p2pBindPortIpv6 > 0) ? p2pBindPortIpv6 : port` (`src/p2p/NetNodeConfig.cpp:104`); `set_ipv6_v6only(true)` (`src/rpc/RpcServer.cpp:139`); the bind log line `src/p2p/NetNode.cpp:742` |
| IPv6 peer exchange | `P2P_IPV6_CAPABILITY_VERSION` (`src/config/CryptoNoteConfig.h:556`); `src/p2p/NetNode.cpp:904` and `src/p2p/NetNode.cpp:2335` |
| `--p2p-external-port` | `my_port` (`src/p2p/NetNode.cpp:1960-1967`); its range check `src/daemon/Daemon.cpp:522-526` |
| Priority and exclusive nodes | `NodeServer::connections_maker` (`src/p2p/NetNode.cpp:1537`): exclusive-only dialling `src/p2p/NetNode.cpp:1541-1549`, priority dialling `src/p2p/NetNode.cpp:1564`; `is_addr_connected` (`src/p2p/NetNode.cpp:1037`); the address parser `src/common/StringTools.cpp:431`; the start failure `src/daemon/Daemon.cpp:1050` |
| UPnP | `addPortMapping`, `src/p2p/NetNode.cpp:74-135` |
| `--resync` | `src/daemon/Daemon.cpp:494-513` |
| `--rewind-to-height` | `src/daemon/Daemon.cpp:869-887`, `Core::rewind`; the count/index comparison `src/cryptonotecore/DatabaseBlockchainCache.cpp:941` |
| Export and import | `src/daemon/Daemon.cpp:768-811`; the `RawBlock` serializer `src/serialization/CryptoNoteSerialization.cpp:547`; the unvalidated import `src/cryptonotecore/Core.cpp:3727-3755` |
| `--load-checkpoints` | `src/daemon/Daemon.cpp:634-656` |
| Lite mode | the help text `src/daemon/DaemonConfiguration.cpp:106`; the refusals `src/daemon/Daemon.cpp:211-325`, the explorer one `src/daemon/Daemon.cpp:281-292`; the depth check's four samples `src/cryptonoteprotocol/CryptoNoteProtocolHandler.cpp:48` (this port's is `wrkz_node::daemon::LiteDepthCheck`) |
| Pruning | "Pruning records no height it pruned to", `src/cryptonotecore/Core.cpp:2987` |
| Lite node snapshots | `src/daemon/LiteSnapshot.h`, and [`LITESNAPSHOT.md`](https://github.com/wrkzcoin/wrkzcoin/blob/8d89d7bf/LITESNAPSHOT.md) in the C++ tree |
| Console | `src/daemon/DaemonCommandsHandler.cpp`, one handler per command |
| Attach | `src/daemon/AttachConsole.cpp` |
| Database options | `getDBOptions`, `src/cryptonotecore/RocksDBWrapper.cpp:551-670`, at the defaults of `src/daemon/DaemonConfiguration.h:62-67`; compaction cancelling `CompactRangeOptions::canceled` |
| Stratum | `src/daemon/StratumServer.cpp` |
| ZMQ | `src/daemon/ZmqPublisher.cpp`; the `txpool_del` list reserved and never filled `src/cryptonotecore/Core.cpp:1657-1686` |
| Notify hooks | `src/daemon/ChainNotifier.cpp` |
| Pool relay | `src/rpc/RpcServer.cpp:1142` |

## Comparing with the C++ daemon {#comparing-with-the-c-daemon}

`wrkz-rpc-diff` puts the same requests to two daemons and diffs the JSON
structurally (see also [Diagnostics](../tools/diagnostics.md)):

```sh
cargo run --release -p wrkz-rpc --bin wrkz-rpc-diff -- \
    --reference http://node-fin.wrkz.work:17856 \
    --ours      http://127.0.0.1:17856
```

Without `--height` it asks both daemons how far they are and probes the highest
block index **both** hold, so a node still catching up is compared on ground it
actually has.

By default it compares **shapes** — every key and every type, on both sides,
plus the HTTP status — which is the comparison that means anything between two
nodes on different chains. `--values` compares values too (skipping clocks,
peer identities and anything height-dependent); use it when both sides hold the
same block, which after an import they do. `--list` prints the probe names,
`--endpoints a,b,c` selects some.

Two results from this port, against `node-fin.wrkz.work:17856`:

- **shape**, our node syncing at block 16,818: 22 of 23 probes identical. The
  one difference is `/sendrawtransaction`, where we answer the 503 sync gate
  because no peer has confirmed us at the top yet and the reference answers 200
  because it is synced. Both are the correct answer for their node.
- **values**, both daemons at block 15,000:
  `--endpoints info,height,getlastblockheader,getblockheaderbyheight,getwalletsyncdata,getrawblocks`
  → **6 of 6 identical**, values included. That is acceptance 2 of
  [RPC and wallet sync](../spec/09-rpc-and-wallet-sync.md#acceptance-for-this-document)
  for a real range of the real chain.

Three probes cannot agree on values and are expected to differ:
`getrandom_outs` (decoys are random), `get_global_indexes_for_range` (the C++
walks an `unordered_map`, so its *entry order* is arbitrary — the sets are the
same, permuted), and `/sendrawtransaction` while we are behind.

## Lite node snapshots: acceptance on a real chain {#snapshot-acceptance}

The tests prove the [snapshot format](lite-snapshots.md) against hand-built
bytes and a round trip over a chain built from genesis — export, import, export
again gives the same file, the imported state applies the blocks above its line
exactly as the original did, and an export's bytes are the C++ walk's — but only
the real file proves interoperability. On the operator's host, with the RocksDB
build:

```sh
cargo build --release -p wrkz-node --features rocksdb
```

1. **Read the published file.**
   `wrkz-node --snapshot-info wrkz-lite-base-h4000000-v1.litesnap` must print
   `"liteHeight":4000000,"records":148728732,…,"transactionsCount":7469434,…,"digest":"4601d802…f94fe09e","accepted":true`.
2. **Import it.**
   `wrkz-node --data-dir /srv/wrkz-imported --lite --lite-height 4000000 --import-lite-snapshot wrkz-lite-base-h4000000-v1.litesnap`
   must exit 0 after `Imported 148728732 records`. Then
   `wrkz-node --data-dir /srv/wrkz-imported --lite --lite-height 4000000` must
   open at height 4,000,000 and sync on; `curl -s localhost:17856/info` shows
   `"lite_start_height":4000000`.
3. **Reproduce the digest from the imported state.** Once that node is past
   4,020,160: `snapshot_export start` on its console, then `snapshot_export
   status` until it reports `Digest: 4601d802d990fa26b876ed7fdaffc00953cff6ca6b77299fd1c6981ef94fe09e`.
4. **Reproduce it from a state that never saw the file** — the proof that
   matters. A full state from `wrkz-replay --store-raw --db <C++ DB> --state
   /srv/wrkz-full/state` (or any node synced from genesis), served by
   `wrkz-node --data-dir /srv/wrkz-full`: `snapshot_export start 4000000` must
   report the same digest. `cmp` it against the published file: identical when
   the zstd versions agree, and the digests agree regardless.
5. **The other direction.** On a fresh data directory,
   `Wrkzd --lite --lite-height 4000000 --import-lite-snapshot <the file step 4 wrote>`
   must accept it (its digest is the compiled-in one) and exit 0.

## Follow-ups

- The C++ also drops the transaction records, the block's transaction-hash list
  and the payment-id entries below the lite height, and zeroes the transaction
  hash on key outputs there. This port keeps all of them: they cost little, they
  cannot affect consensus, and keeping them is why a lite node here can still
  answer `f_transaction_json` and the global-index endpoints for the region the
  C++ cannot. The one place that is deliberately *not* exploited is
  `/get_global_indexes_for_range`, which reproduces the C++'s 400 below the lite
  height so one client can be written against both. A lite node **imported from
  a snapshot** is the exception: the file never carried those records, so below
  its height it has none of them and refuses those lookups instead; see
  [What an imported node can and cannot answer](lite-snapshots.md#what-an-imported-node-can-and-cannot-answer).

## What is not covered

Honest list of what this daemon does **not** do yet, and why. Everything in the
node section has been run except the RocksDB paths, which need libclang and were
built and exercised on the Linux host only.

**RPC endpoints.** Everything the C++ `src/rpc/RpcServer.cpp` routes is served,
the explorer methods and `/queryblocksdetailed` included.

The seven endpoints the wallet uses — `/info`, `/getwalletsyncdata`,
`/getrawblocks`, `/get_global_indexes_for_range`, `/getrandom_outs`,
`/sendrawtransaction`, `/get_transactions_status` — are complete and are driven
in CI by the wallet's own client.

**Other gaps.**

- **`/fee` and `/getpeers` do not exist**, and neither do the JSON-RPC methods
  `on_getblockhash`, `getblocksbyheights`, `getblockdetailsbyheight`,
  `getblock`, `getblocks`, `gettransaction`, `gettransactionspool` and
  `getcurrencyid`. That is not an omission: `src/rpc/RpcServer.cpp` routes none of them
  and the live seed node answers 404 for every one, verified.
- **The RocksDB paths are built with `--features rocksdb` on Ubuntu**, which is
  the first thing `scripts/e2e.sh` does. A Windows host with MinGW builds them
  too, given two things: libclang, which `pip install libclang --target DIR`
  provides as `DIR/clang/native/libclang.dll` for `LIBCLANG_PATH`; and, with a
  recent GCC (16.1 was used), `CXXFLAGS="-include cstdint"`, because RocksDB
  8.10's headers use `uint64_t` without including `<cstdint>`.
- **The maintenance actions have run on `MemStore` only.** `--resync`,
  `--rewind-to-height` and the dump import and export have no RocksDB-specific
  code, but on RocksDB three things have not been seen yet: a rewind's single
  write batch at full depth (a few hundred thousand operations), the
  write-ahead log switched off for an import, and the sync that makes an
  import durable at its end.
- **Cross-compiled builds have not run a node yet.** The Windows, macOS,
  Android and arm64 Linux builds of [Cross-compiling](../building/cross-compile.md)
  link and pass the lints, but none has yet been built with RocksDB or run
  against the chain, and the CryptoNight and curve vectors have not yet been
  run on ARM hardware. Do both (`cargo test -p wrkz-pow` on the device) before
  trusting a node on a new platform.

What has been proven so far, across the whole port, is summarised in
[Project status](../getting-started/status.md).
