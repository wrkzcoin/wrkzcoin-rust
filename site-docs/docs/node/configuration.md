# Configuration

`wrkz-node` is configured by command-line options, optionally read from a
`Wrkzd` configuration file; this page lists every option, explains the file,
and ends with the notes an operator needs to run a node unattended.

The C++ `Wrkzd`'s options are in its
[Configuration Reference](https://docs.wrkz.work/guides/daemon-configuration/); where the names
match, so does the meaning, and the differences are noted below and gathered in
[Internals](internals.md#configuration-divergences).

## Command line

Names follow the C++ `Wrkzd`'s where the C++ has one.

| Option | Default | Meaning |
| --- | --- | --- |
| `-c`, `--config-file PATH` | — | read settings from a `Wrkzd` configuration file ([Configuration file](#configuration-file)); command-line flags win |
| `--dump-config` | — | print the effective configuration as JSON and exit |
| `--save-config PATH` | — | write it to `PATH` and exit |
| `--data-dir DIR` | *(required)* | chain state (`DIR/state`), `p2pstate.wrkz.bin`, the pid lock |
| `--no-checkpoints` | off | validate every block fully; much slower |
| `--load-checkpoints VALUE` | `default` | `default` is the compiled-in table; a path is a CSV of `index,hash` lines that **replaces** it (the C++ format); an empty value means none. A file that names a different hash at any index the compiled-in table covers is refused: it cannot describe this chain |
| `--resync` | off | delete the chain state and the peer state (not the ban list), then sync from genesis ([Maintenance](maintenance.md#resync)) |
| `--rewind-to-height N` | — | remove every block from height `N` up, at most 4,320 below the top, then start; exits 1 if that cannot be done ([Maintenance](maintenance.md#rewind-to-height)) |
| `--export-blockchain` | off | write the chain to `--dump-file` and exit ([Maintenance](maintenance.md#export-and-import)) |
| `--import-blockchain` | off | apply `--dump-file`, every block validated as a peer's, and exit; a rerun resumes |
| `--dump-file PATH` | `blockchain.dump` | the dump written or read, relative to the **current** directory |
| `--max-export-blocks N` | the whole chain | export a chain of at most `N` blocks |
| `--import-validate` | off | accepted for C++ compatibility; an import is always validated |
| `--prune` | off | pruned-node mode ([Pruned node](reduced-modes.md#pruned-node)) |
| `--prune-depth N` | 10080 | block bodies kept behind the tip; a smaller value is **raised** to 10080 |
| `--lite` | off | lite-node mode. **Permanent for the database** ([Lite node](reduced-modes.md#lite-node)) |
| `--lite-height H` | *(required with `--lite`)* | store full block data from this height up |
| `--import-lite-snapshot FILE` | — | load a C++ lite node snapshot into a database holding only genesis, then exit (0 imported, 1 not). Needs `--lite` and the `--lite-height` the file was made at ([Lite node snapshots](lite-snapshots.md)) |
| `--snapshot-info FILE` | — | print what a snapshot file holds as one line of JSON, including whether this build accepts its digest, and exit. Needs no `--data-dir` |
| `--snapshot-stats` | — | walk every table of the state, print records and logical megabytes per table, and exit ([Measuring a database](lite-snapshots.md#measuring-a-database)) |
| `--auto-prune-min-gap-blocks N` | 120 | block gap between catch-up prune passes; `0` disables the schedule |
| `--auto-prune-min-free-bytes N` | 4 GiB | free space below which a prune pass is **forced** whatever the gap says |
| `--auto-compaction-min-gap-blocks N` | 720 | blocks between automatic database compactions; `0` turns periodic compaction off ([Database compaction](database.md#database-compaction)) |
| `--auto-compaction-min-free-bytes N` | 8 GiB | free space a periodic compaction needs before it starts |
| `--skip-boot-compaction` | off | do not compact the database at start-up; the periodic rules then count from start-up |
| `--p2p-bind-ip ADDR` | `0.0.0.0` | P2P listening address |
| `--p2p-bind-port PORT` | `17855` | P2P listening port; `0` picks a free one |
| `--p2p-external-port PORT` | `0` | the port peers are told to reach this node on (`my_port`), for a NAT that forwards a different port to the listener; `0` advertises the listening port. `--hide-my-port` wins over it. Outside 0–65535 it is refused with the C++'s message. The UPnP mapping is of the listening port either way |
| `--p2p-bind-ipv6-address ADDR` | *(unset)* | also listen on this IPv6 address (`::` for every interface). Unset means **no** IPv6 listener ([IPv6](networking.md#ipv6)) |
| `--p2p-bind-port-ipv6 PORT` | `0` | port for the IPv6 listener; `0` means `--p2p-bind-port` |
| `--no-listen` | off | outbound connections only |
| `--add-peer HOST:PORT` | — | put this peer on the white list and dial it at start, repeatable. Not dialled while exclusive nodes are configured |
| `--seed-node HOST:PORT` | — | an extra seed, repeatable |
| `--add-priority-node HOST:PORT` | — | keep a connection to this peer: dialled until connected, and again whenever it drops; repeatable ([Priority and exclusive nodes](networking.md#priority-and-exclusive-nodes)) |
| `--add-exclusive-node HOST:PORT` | — | connect to these peers and nothing else, repeatable ([Priority and exclusive nodes](networking.md#priority-and-exclusive-nodes)) |
| `--no-default-seeds` | off | skip the compiled-in seeds and DNS seeds |
| `--out-peers N` | 15 | outbound connection target |
| `--in-peers N` | 15 | inbound connection limit |
| `--sync-max-peers N` | 3 | connections that may pull the chain at once |
| `--sync-peer-failure-threshold N` | 2 | failures before a sync peer is demoted (at least 1) |
| `--sync-batch-min N` / `--sync-batch-max N` | 120 / 600 | the adaptive block request's floor and ceiling; the ceiling is raised to the floor, as the C++ does |
| `--block-sync-size N` | 600 | hard ceiling on blocks per request |
| `--block-sync-bytes N` | 16 MiB | bytes a request is sized against (at least 2 MiB) |
| `--allow-local-ip` | off | accept private and loopback peers |
| `--hide-my-port` | off | advertise `my_port = 0` |
| `--no-upnp` | off | do not ask the router to forward the P2P port ([UPnP port mapping](networking.md#upnp-port-mapping)). This port's own: the C++ always tries |
| `--p2p-reset-peerstate` | off | new peer id, empty peer lists |
| `--rpc-bind-ip ADDR` | `127.0.0.1` | RPC listening address |
| `--rpc-bind-port PORT` | `17856` | RPC listening port |
| `--rpc-bind-ipv6-address ADDR` | *(unset)* | also serve the RPC on this IPv6 address, on `--rpc-bind-port`. Unset means **no** IPv6 RPC listener |
| `--rpc-use-ipv6` | off | accepted for C++ command-line compatibility; `--rpc-bind-ipv6-address` already enables IPv6 |
| `--rpc-ipc-path PATH` | *(unset)* | also serve the RPC on a local socket at `PATH` (absolute), or `@name` in Linux's abstract namespace ([Local IPC socket](console-and-ipc.md#local-ipc-socket)). Not on Windows |
| `--rpc-ipc-mode MODE` | `0600` | octal permissions of the socket file; `0660` with `--rpc-ipc-group` to share it |
| `--rpc-ipc-group GROUP` | — | group to own the socket file |
| `--rpc-ipc-require-token` | off | also demand `--rpc-access-token` from socket callers |
| `--no-rpc` | off | do not start the RPC at all |
| `--enable-cors VALUE` | off | send `Access-Control-Allow-Origin: VALUE` |
| `--rpc-access-token TOKEN` | — | require `X-API-Key` or `Authorization: Bearer` |
| `--rpc-max-rpm N` | 240 | per-IP rate limit; `0` disables it. Also spelled `--rpc-max-requests-per-minute` |
| `--rpc-max-connections-per-ip N` | 8 | connections one remote address may hold open at once, waiting or being served; the next gets `429` before any worker sees it. `0` disables it. Loopback is exempt, and so is everyone behind `--rpc-trust-proxy`. Raise it for an explorer back end on another host |
| `--rpc-read-timeout SECS` / `--rpc-write-timeout SECS` | 10 / 10 | socket deadlines (at least 1) |
| `--rpc-max-body-bytes N` | 2 MiB | largest request body (at least 1024) |
| `--daemon-mode MODE` | `standard` | `explorer` also serves the `f_*` explorer JSON-RPC methods and `/queryblocksdetailed`; refused on a lite node ([Block explorer](reduced-modes.md#block-explorer)). The old `--enable-blockexplorer` is refused with a pointer here |
| `--rpc-max-block-count N` | 1000 | cap on `blockCount` |
| `--rpc-max-global-index-range N` | 5000 | cap on the global-index range |
| `--rpc-workers N` | 16 | RPC worker threads (fixed, bounded) |
| `--rpc-trust-proxy` | off | read the client IP from `X-Forwarded-For` |
| `--enable-metrics` | off | serve `GET /metrics` in the Prometheus text format on the RPC port, behind `--rpc-access-token` when one is set. Off, that path is a 404 |
| `--decoy-selection MODE` | `uniform` | how `/getrandom_outs` picks ring decoys. `uniform` is the C++'s pick over every unlocked output. `recent` draws each decoy's age from Monero's fitted gamma (ln of the age in seconds ~ Gamma(19.28, 1/1.61), median about 36 hours) so decoys look like real spends, which are mostly recent; any it cannot fill are picked uniformly. **Leave it at `uniform` until the C++ node and the network switch together**: a node handing out a different distribution from its peers makes its own users' rings recognisable |
| `--enable-health` | off | serve `GET /health` on the RPC port: **200** once synced, **503** while syncing, with `status`, `synced`, `height`, `network_height` and `peers` as JSON. Same token rule; off, a 404 |
| `--stratum-bind-port PORT` | `0` | serve the built-in stratum server on `PORT`, so a stock miner mines straight to this node ([Mining](mining.md)). `0` leaves it off |
| `--stratum-bind-ip ADDR` | `127.0.0.1` | the stratum server's listening address |
| `--stratum-share-difficulty N` | `0` | difficulty miners are given. `0` is the network difficulty, so a miner reports only blocks; a lower value makes it report progress too |
| `--stratum-max-connections N` | 32 | miners allowed at once; `0` is taken as 1 |
| `--zmq-pub ADDRESS` | `tcp://127.0.0.1:17857` | publish blocks, reorganisations and pool changes on a ZMQ PUB socket, `tcp://host:port` or `ipc://path`; empty is off ([ZMQ](zmq-and-hooks.md#zmq)) |
| `--no-zmq` | off | do not publish on ZMQ |
| `--block-notify CMD|URL` | — | run `CMD`, or POST JSON to `URL`, when a block joins the main chain ([Notify hooks](zmq-and-hooks.md#notify-hooks)) |
| `--reorg-notify CMD|URL` | — | the same on a reorganisation |
| `--tx-notify CMD|URL` | — | the same when a transaction enters the pool |
| `--notify-during-sync` | off | announce during the initial sync too; by default nothing is announced until the node is synced |
| `--rpc-sync-cache-size MB` | 64 | finished `/getwalletsyncdata` answers kept for the next wallet asking for the same range, as the C++ keeps them; only ranges 360 blocks behind the tip are kept, and a reorganisation drops them all. `0` disables |
| `--rpc-stream-threshold KB` | 0 (off) | compress a body this large or larger straight into the socket instead of building the compressed copy first. It saves a busy public node one copy of every large answer, per worker; it costs byte-identical framing with the C++, which always sends `Content-Length`, because a streamed answer is `Transfer-Encoding: chunked`. Every HTTP/1.1 client accepts chunked, but a client that compares our bytes with a C++ node's will see the difference |
| `--db-threads N` | 8 | background flush and compaction threads. This and every `--db-*` option below need `--features rocksdb` ([Database tuning](database.md#database-tuning)) |
| `--db-max-open-files N` | 4096 | open file limit; `-1` is none |
| `--db-read-buffer-size MB` | 256 | the read cache: row cache and block cache together |
| `--db-row-cache-percent N` | 0 | share of the read cache kept as a row cache, at most 90; `0` is an eighth |
| `--db-write-buffer-size MB` | 64 | memtable size, which also sizes SST files and level 1 |
| `--db-block-size KB` | 4 | uncompressed SST data block size; `0` is taken as 1 |
| `--db-enable-compression[=false]` | on | ZSTD from level 2 down; levels 0 and 1 are never compressed |
| `--db-compression-level N` | 0 | ZSTD level of the bottommost level; `0` is RocksDB's default |
| `--db-compression-dict-bytes N` | 0 | per-SST ZSTD dictionary size in bytes; `0` is none |
| `--db-bottom-filters[=true]` | off | keep bloom filters on the bottommost level |
| `--transaction-validation-threads N` | — | the C++ name for `--threads`; `0` keeps the default |
| `--threads N` | half the logical cores, at most 16 | ring signature verification threads. A performance knob only: the same blocks are accepted and a rejection names the same rule at every value. Matters above the last checkpoint and with `--no-checkpoints` |
| `--batch-blocks N` | 1000 | the most blocks the chain state holds back before it must commit them as one write batch; `1` is one batch per block, the old behaviour |
| `--batch-bytes MB` | 64 | commit early once this many megabytes of writes are pending |
| `--wal` | off | keep the write-ahead log on during initial sync. By default it is off until a peer first confirms we hold its top block, then on for good ([Sync throughput](internals.md#sync-throughput)) |
| `--log-level LEVEL` | `info` | `error`, `warn`, `info`, `debug`, `trace`, or the C++ numbering `0`-`4` |
| `--log-file PATH` | — | append every line to this file as well. Rotated at 32 MiB, keeping one previous generation as `PATH.1`, so a busy node at `debug` cannot fill the disk. Created 0640 where the platform has modes: it holds peer addresses and this node's peer id |
| `--log-format FORMAT` | `text` | `json` writes each line as one object, `{"time":"2026-09-10T14:03:22.123Z","level":"INFO","message":"…"}`, on the terminal, in `--log-file` and in `log_tail` alike |
| `--no-console` | off | do not read commands on stdin. The periodic status line is printed either way |
| `attach SOCKET`, `--attach SOCKET` | — | instead of starting a node, attach a console to a daemon already running on this machine, over its `--rpc-ipc-path` socket ([Attaching to a running daemon](console-and-ipc.md#attaching-to-a-running-daemon)). Not on Windows |
| `--sync-to HEIGHT` | — | stop once the chain reaches this block index |
| `--exit-when-synced` | off | stop once a peer says we hold its top block |
| `--version` | — | print the version (and the commit, if the build set one) |
| `-h`, `--help` | — | the usage text |

Node fees are gone: `--fee-address` and `--fee-amount` are still accepted, so an
existing command line starts, but they do nothing and each draws one warning
at start-up. A configuration file's `fee-address` and `fee-amount` are read
and dropped, as the C++ drops them.

## Configuration file

`--config-file` (or `-c`) reads a `Wrkzd` configuration file as it stands:
the JSON that `Wrkzd --dump-config` writes, or the older one-`key=value`-a-line
form. The keys are the C++'s, and each is turned into the flag of the same name
**in front of** the real command line, so every value is checked exactly as the
flag would be and a flag given on the command line wins — the C++'s order of
command line, file, command line again. A list (`add-peer`, `seed-node`,
`add-exclusive-node`, `add-priority-node`) adds to what the command line gives.

- `"load-checkpoints": "default"` is the compiled-in table and `""` is none,
  as in the C++; a path is a CSV file.
- `"prune-depth"` counts only with `"prune": true`, since a dump writes it
  either way.
- The RocksDB keys (`db-*`), `skip-boot-compaction` and the two
  `auto-compaction-*` keys are read like their flags. `"db-enable-compression":
  false` becomes `--db-enable-compression=false`, and `db-max-open-files` and
  `db-compression-level` may be negative. `Wrkzd` reads its command line a
  second time after the file and, in doing so, puts `db-threads`,
  `db-max-open-files`, `db-read-buffer-size` and `db-write-buffer-size` back to
  their defaults unless the command line names them again; here the file's
  values stand.
- Every key `Wrkzd --dump-config` writes is read. The four `stratum-*` keys
  are read like their flags ([Mining](mining.md)), and so are `zmq-pub` and
  `no-zmq` ([ZMQ](zmq-and-hooks.md#zmq)), where `"zmq-pub": ""` is off, as in
  the C++, and the four notify keys ([Notify hooks](zmq-and-hooks.md#notify-hooks)).
  A key neither daemon knows gets a warning of its own.
- This port's own options that `--dump-config` writes — `lite`, `lite-height`,
  `no-listen`, `no-upnp`, `no-default-seeds`, `no-rpc`, `rpc-workers`,
  `rpc-max-connections-per-ip`, `enable-metrics`, `enable-health`,
  `decoy-selection`, `threads`, `batch-blocks`, `batch-bytes` and `wal` — are
  read back the same way.
- The one-off actions — `resync`, `rewind-to-height`, `import-blockchain`,
  `export-blockchain`, `dump-file`, `max-export-blocks`, `import-validate` —
  are command-line options in both daemons; one set in a file draws a warning
  and is ignored, and `--dump-config` writes none of them.

### Writing one: `--dump-config` {#dump-config}

`--dump-config` prints the effective configuration in the same JSON, under the
C++ keys plus this port's own (`threads`, `batch-blocks`, `wal`, …), and
`--save-config PATH` writes it to a file. What it writes reads back to the same
configuration, which a test pins:

```sh
wrkz-node --data-dir /var/lib/wrkz-rust --prune --save-config /etc/wrkz-node.json
wrkz-node -c /etc/wrkz-node.json
```

## Operating notes

- **Status line.** Every 30 seconds, at `info`: height, target height, peers
  in/out, pool size, and blocks per second over that interval (not since start,
  so a stall shows as `0.0 blocks/s`), an ETA while the node is behind,
  estimated from that same interval, and what each block cost over the interval
  (`decode`, `validate`, `commit`). It is printed with or without
  `--no-console`: under systemd the log is the only place anyone can see the
  node sync.
- **Metrics.** With `--enable-metrics`, `GET /metrics` on the RPC port gives
  a Prometheus scraper the height, network height, blocks behind, sync state,
  difficulty, pool size, connections and peer lists (`wrkz_*`), and the RPC's
  own requests by status class, gzip responses and wallet-sync cache size and
  hit rate (`wrkz_rpc_*`). With a token, configure the scrape job's
  `authorization: { credentials: TOKEN }`. Alert on `wrkz_blocks_behind` and
  `wrkz_synchronized`.
- **Health.** With `--enable-health`, `GET /health` answers 200 once the node
  is synchronized and 503 until then, so a load balancer only sends wallets to
  a node that can answer them, and a supervisor can tell "starting" from
  "stuck": `curl -fsS http://127.0.0.1:17856/health`. The slim Docker image
  carries no `curl`, so check it from outside the container (or add one to a
  derived image and a `HEALTHCHECK`); see [Docker](../building/docker.md).
- **Structured logs.** `--log-format json` makes every line one JSON object
  for journald's JSON export, Loki or Elasticsearch; the text format stays
  the default because it is the C++'s, grep for grep.
- **Shutdown.** `SIGINT`, `SIGTERM` and `SIGHUP` set a flag the main loop polls;
  the engine then closes every connection and writes `p2pstate.wrkz.bin`, the
  RPC joins its workers, and the pid lock is removed. Give it a few seconds.
- **One daemon per data directory.** `DIR/wrkz-node.pid` holds the pid. A second
  daemon on the same directory exits non-zero naming the pid and the file to
  remove; so does a daemon whose P2P or RPC port is already in use.
- **Exit codes.** `0` clean, `1` "nothing synced" (no peers reached) or a
  maintenance action that failed ([Maintenance](maintenance.md)), `2` a bad
  command line or a directory/port problem.
- **No block bodies.** A state imported by `wrkz-replay` *without* `--store-raw`
  holds every index and no block or transaction bytes. It validates and follows
  the tip perfectly, and below the import height it can serve neither peers nor
  wallets. The daemon probes for this at start-up (about 22 lookups, not a scan)
  and warns once, naming the height; `/info` reports the same height in
  `lite_start_height`, which is the field the C++ uses for a node whose bodies
  begin above genesis, so wallets floor their scan height at it. `status` and
  `prune_status` print it too. `--lite` is the deliberate version of the same
  state; see [Lite, pruned and explorer](reduced-modes.md).
