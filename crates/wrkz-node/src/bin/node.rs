// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-node`: the daemon. Syncs the chain over P2P and serves the daemon
//! RPC on the same state.
//!
//!     wrkz-node --data-dir DIR [--rpc-bind-port 17856] [--seed-node host:port]...
//!
//! One process, three parts:
//!
//! - the **P2P engine** (`wrkz_node::Node`), single-threaded, the only writer
//!   of the chain state;
//! - the **RPC server** (`wrkz_rpc`), a bounded pool of worker threads that
//!   only ever read the chain;
//! - one **chain state** behind an `RwLock` and one **transaction pool** behind
//!   a `Mutex`, shared by both. See `docs/DAEMON.md` for the reasoning.
//!
//! The chain state lives in memory unless the crate is built with
//! `--features rocksdb`, in which case `--data-dir DIR` also holds `DIR/state`,
//! a RocksDB database in this port's own key namespace (never the C++ one).

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv6Addr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use wrkz_chain::{ChainState, Checkpoints, Config};
use wrkz_mempool::TransactionPool;
use wrkz_node::chain_notifier::{AtTip, ChainNotifier};
use wrkz_node::compaction::{AutoCompaction, Compaction, CompactionEngine};
use wrkz_node::console::{self, Console, ConsoleConfig, NodeView};
use wrkz_node::daemon::{self, DirLock, StatusLine, StatusSnapshot};
use wrkz_node::log::Level;
use wrkz_node::mempool::SharedMempool;
use wrkz_node::node::PinnedNode;
use wrkz_node::stratum::{ChainReady, StratumConfig, StratumServer};
use wrkz_node::upnp::{PortMapper, UpnpConfig};
use wrkz_node::zmq::ZmqPublisher;
use wrkz_node::{log_error, log_info, log_warn, Node, NodeConfig};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::P2P_DEFAULT_PORT;
use wrkz_rpc::events::{EventListener, Events};
use wrkz_rpc::node::{ChainNode, P2pSnapshot};
use wrkz_rpc::server::{self, RpcMode, ServerConfig};
use wrkz_storage::batch::BatchStore;
use wrkz_storage::dbconfig::DbConfig;
use wrkz_storage::KvStore;

/// The daemon's own version, and the git commit when the build found one.
///
/// `WRKZ_GIT_COMMIT` is set at compile time by `build.rs`, from the
/// environment or the checkout's `.git`, and is absent outside a checkout;
/// nothing shells out during the build.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: Option<&str> = option_env!("WRKZ_GIT_COMMIT");

/// How often the status line is printed.
const STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// How long shutdown waits for a UPnP mapping attempt still under way before
/// it goes on without it. One that finishes later removes its own mapping.
const UPNP_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

const USAGE: &str = "\
wrkz-node --data-dir DIR [options]
wrkz-node attach SOCKET      a console for a daemon already running here, over
                             its --rpc-ipc-path socket

  Configuration
  -c, --config-file PATH     read settings from a Wrkzd configuration file (its
                             JSON, or the older key=value form). Flags given on
                             the command line win; lists add up
  --dump-config              print the effective configuration as JSON, exit
  --save-config PATH         write it to PATH instead, exit

  Data and chain state
  --data-dir DIR             where p2pstate.wrkz.bin, the pid lock and (with
                             --features rocksdb) DIR/state live. Required.
  --no-checkpoints           disable the compiled-in checkpoints (much slower)
  --load-checkpoints VALUE   'default' (the compiled-in table), a CSV file of
                             index,hash lines that replaces it, or '' for none

  Maintenance (once, before the node starts)
  --resync                   delete the chain state and the peer state (the
                             ban list is kept), then sync from the network
  --rewind-to-height N       remove every block from height N up, at most
                             4320 below the top, then start
  --export-blockchain        write the chain to --dump-file, then exit
  --import-blockchain        apply the blocks in --dump-file, each validated
                             as a peer's block, then exit. Blocks the state
                             already holds are skipped, so a rerun resumes
  --dump-file PATH           the dump to write or read (default
                             blockchain.dump, in the current directory)
  --max-export-blocks N      export a chain of at most N blocks
  --import-validate          accepted for C++ compatibility: every imported
                             block is validated already. Add --no-checkpoints
                             to check work and signatures below the last
                             checkpoint too

  Node modes
  --prune                    enable pruned-node mode for daemon sync behavior
  --prune-depth N            when prune mode is enabled, retain at least this
                             many recent blocks locally (default 10080; a
                             smaller value is raised to it)
  --lite                     enable lite-node mode: store full blocks only from
                             --lite-height upward. Permanent for this database
  --lite-height H            height at and above which a lite node stores full
                             block data (required with --lite)
  --import-lite-snapshot FILE
                             load a lite node snapshot into an empty database,
                             then exit. Needs --lite and the --lite-height the
                             snapshot was made at
  --snapshot-info FILE       print what a lite node snapshot file contains, as
                             JSON, and exit (needs no --data-dir)
  --snapshot-stats           report per-table record counts and byte totals,
                             then exit. Takes minutes on a synced chain
  --auto-prune-min-gap-blocks N
                             minimum block gap between automatic prune passes
                             (0 disables periodic auto-prune; default 120)
  --auto-prune-min-free-bytes N
                             minimum free bytes required before the regular
                             auto-prune schedule; below it a prune is forced
                             (default 4 GiB)

  P2P
  --p2p-bind-ip ADDR         listening address (default 0.0.0.0)
  --p2p-bind-port PORT       listening port (default 17855; 0 picks a free one)
  --p2p-external-port PORT   tell peers to reach this node on PORT, for a NAT
                             that forwards it here (default 0: the listening
                             port)
  --p2p-bind-ipv6-address ADDR
                             also listen on this IPv6 address (e.g. :: for all
                             interfaces). Unset means no IPv6 listener at all.
  --p2p-bind-port-ipv6 PORT  port for the IPv6 listener (0 = --p2p-bind-port)
  --no-listen                do not listen; outbound connections only
  --add-peer HOST:PORT       put this peer on the white list and dial it at
                             start, repeatable
  --seed-node HOST:PORT      an extra seed, repeatable
  --add-priority-node HOST:PORT
                             keep a connection to this peer: dialled until
                             connected and again whenever it drops, repeatable
  --add-exclusive-node HOST:PORT
                             connect only to these peers: no seeds, no peer
                             list, no --add-peer; repeatable
  --no-default-seeds         do not use the compiled-in seed and DNS seeds
  --out-peers N              outbound connection target (default 15)
  --in-peers N               inbound connection limit (default 15)
  --sync-max-peers N         connections that may pull the chain at once (3)
  --sync-peer-failure-threshold N
                             failures before a sync peer is demoted (2)
  --sync-batch-min N         smallest adaptive block request (120)
  --sync-batch-max N         largest adaptive block request (600)
  --block-sync-size N        hard ceiling on blocks per request (600)
  --block-sync-bytes N       bytes a request is sized against (16 MiB, at
                             least 2 MiB)
  --allow-local-ip           accept private and loopback addresses as peers
  --hide-my-port             advertise my_port = 0 (no back pings, never white)
  --no-upnp                  do not ask the router to forward the P2P port
  --p2p-reset-peerstate      new peer id, empty peer lists

  RPC
  --rpc-bind-ip ADDR         (default 127.0.0.1; use 0.0.0.0 to expose it)
  --rpc-bind-port PORT       (default 17856)
  --rpc-bind-ipv6-address ADDR
                             also serve the RPC on this IPv6 address, on
                             --rpc-bind-port. Unset means no IPv6 RPC listener.
  --rpc-use-ipv6             accepted for C++ compatibility; giving
                             --rpc-bind-ipv6-address already enables IPv6
  --rpc-ipc-path PATH        also serve the RPC on a local socket at PATH, e.g.
                             /run/wrkz/wrkzd.sock; @name for Linux's abstract
                             namespace. Not on Windows
  --rpc-ipc-mode MODE        octal permissions of the socket file (default
                             0600, owner only; 0660 with --rpc-ipc-group)
  --rpc-ipc-group GROUP      group to own the socket file
  --rpc-ipc-require-token    also demand --rpc-access-token on the socket
  --no-rpc                   do not start the RPC server at all
  --enable-cors VALUE        send Access-Control-Allow-Origin: VALUE
  --rpc-access-token TOKEN   require X-API-Key or Authorization: Bearer
  --rpc-max-rpm N            per-IP rate limit (default 240; 0 disables); also
                             spelled --rpc-max-requests-per-minute
  --rpc-max-connections-per-ip N
                             connections one remote address may hold open at
                             once; past it the RPC answers 429 (default 8;
                             0 disables; loopback and --rpc-trust-proxy exempt).
                             Raise it for an explorer back end on another host
  --rpc-read-timeout SECS    (default 10, at least 1)
  --rpc-write-timeout SECS   (default 10, at least 1)
  --rpc-max-body-bytes N     largest request body (default 2 MiB, at least 1024)
  --daemon-mode MODE         standard (default) or explorer: also serve the
                             f_* explorer methods and /queryblocksdetailed
  --rpc-max-block-count N    cap on blockCount (default 1000)
  --rpc-max-global-index-range N   cap on the index range (default 5000)
  --rpc-workers N            worker threads (default 16)
  --rpc-trust-proxy          read the client IP from X-Forwarded-For
  --rpc-sync-cache-size MB   finished wallet sync responses kept for the next
                             wallet asking for the same range (default 64;
                             0 disables)
  --rpc-stream-threshold KB  compress a body this large or larger straight into
                             the socket, framed Transfer-Encoding: chunked,
                             instead of building the compressed copy first.
                             Saves a worker that copy on large answers; the
                             framing then differs from the C++ daemon's, which
                             always sends Content-Length. 0 (default) is off
  --enable-metrics           serve GET /metrics, Prometheus text, on the RPC
                             port (behind --rpc-access-token when one is set)
  --enable-health            serve GET /health on the RPC port: 200 once
                             synced, 503 before (same token rule)
  --decoy-selection MODE     uniform (default; what every C++ node does) or
                             recent: /getrandom_outs favours recent outputs,
                             as real spends do

  Mining
  --stratum-bind-port PORT   serve a stratum server on PORT, so a stock miner
                             (xmrig, ...) mines straight to this node. 0, the
                             default, leaves it off
  --stratum-bind-ip ADDR     its listening address (default 127.0.0.1)
  --stratum-share-difficulty N
                             difficulty miners are given. 0 (default) is the
                             network difficulty: a miner reports only blocks;
                             lower makes it report progress too
  --stratum-max-connections N
                             miners allowed at once (default 32)

  Integration
  --zmq-pub ADDRESS          publish blocks, reorganisations and pool changes
                             on a ZMQ PUB socket, tcp://host:port or ipc://path
                             (default tcp://127.0.0.1:17857; empty is off)
  --no-zmq                   do not publish on ZMQ
  --block-notify CMD|URL     run CMD, or POST JSON to URL, when a block joins
                             the main chain: %s hash, %h index
  --reorg-notify CMD|URL     the same on a reorganisation: %s split height,
                             %h new top index, %n new blocks, %d discarded
  --tx-notify CMD|URL        the same when a transaction enters the pool: %s
                             hash
  --notify-during-sync       announce during the initial sync too; by default
                             nothing is announced until the node is synced

  Storage (only meaningful with --features rocksdb)
  --db-threads N             background flush and compaction threads (8)
  --db-max-open-files N      RocksDB open file limit (4096; -1 is no limit)
  --db-read-buffer-size MB   read cache, row cache and block cache together
                             (256)
  --db-row-cache-percent N   share of the read cache kept as a row cache, at
                             most 90; 0 (default) is an eighth
  --db-write-buffer-size MB  memtable size (64)
  --db-block-size KB         SST data block size (4)
  --db-enable-compression[=false]
                             ZSTD from level 2 down (on)
  --db-compression-level N   ZSTD level of the bottommost level; 0 (default)
                             is RocksDB's own
  --db-compression-dict-bytes N
                             per-SST ZSTD dictionary size; 0 (default) is off
  --db-bottom-filters        keep bloom filters on the bottommost level, for
                             lookups that miss. Block size, compression and
                             filters apply to files written from then on;
                             `compact_db force` rewrites the rest
  --skip-boot-compaction     do not compact the database at start-up
  --auto-compaction-min-gap-blocks N
                             blocks between automatic compactions (default
                             720; 0 turns periodic compaction off)
  --auto-compaction-min-free-bytes N
                             free bytes a periodic compaction needs to start
                             (default 8 GiB)
  --batch-blocks N           blocks the chain state may hold back before it
                             must commit them as one write batch (default
                             1000; 1 is one batch per block). A downloaded
                             sync batch is committed as soon as it is applied
  --batch-bytes MB           commit early once this many megabytes are
                             pending (default 64)
  --wal                      keep the write-ahead log on during initial sync;
                             by default it is off until the node first
                             synchronizes, as wrkz-replay runs an import

  Validation
  --threads N                verify ring signatures on up to N threads
                             (default half the logical cores; 1 is sequential)
  --transaction-validation-threads N
                             the C++ name for --threads; 0 keeps the default

  Running
  --log-level LEVEL          error, warn, info (default), debug, trace
  --log-file PATH            also append every line to this file
  --log-format FORMAT        text (default) or json: one JSON object a line,
                             for a log shipper
  --no-console              do not read commands on stdin, and do not print
                             the periodic status line
  --attach SOCKET            attach a console to a running daemon instead of
                             starting one (the same as `attach SOCKET`)
  --sync-to HEIGHT           stop once the chain reaches this block index
  --exit-when-synced         stop once a peer reports we hold its top block
  --version                  print the version and exit
  -h, --help                 this text
";

fn main() -> ExitCode {
    // `wrkz-node attach <socket>`, picked off before the option parser can
    // see a bare word it does not know (`Daemon.cpp:379-391`).
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|word| word == "attach") {
        if argv.len() != 3 {
            println!("Usage: {} attach <rpc ipc socket path>", program_name());
            return ExitCode::from(1);
        }
        return ExitCode::from(wrkz_node::attach::run(&argv[2].to_string_lossy()));
    }
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wrkz-node: {e}");
            eprintln!();
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

struct Args {
    cfg: NodeConfig,
    rpc: ServerConfig,
    serve_rpc: bool,
    /// `--stratum-*`; port 0 is off.
    stratum: StratumConfig,
    /// `--zmq-pub`; empty is off, as in the C++.
    zmq_pub: String,
    /// `--no-zmq`.
    no_zmq: bool,
    /// `--block-notify`, `--reorg-notify`, `--tx-notify`, `--notify-during-sync`.
    hooks: wrkz_node::chain_notifier::HookSpecs,
    add_peers: Vec<String>,
    /// `--add-exclusive-node` and `--add-priority-node` as given; resolved
    /// into [`NodeConfig`] at start, so `--dump-config` writes what was typed.
    exclusive_nodes: Vec<String>,
    priority_nodes: Vec<String>,
    /// The `--db-*` options, starting from the C++ daemon's defaults.
    db: DbConfig,
    /// `--skip-boot-compaction`.
    skip_boot_compaction: bool,
    /// `--auto-compaction-min-gap-blocks` and `--auto-compaction-min-free-bytes`.
    auto_compaction: AutoCompaction,
    /// `--threads`: ring signature verification threads.
    validate_threads: usize,
    /// `--batch-blocks`: the most blocks the store holds back.
    batch_blocks: u32,
    /// `--batch-bytes`, in megabytes.
    batch_bytes_mb: u64,
    level: Level,
    log_file: Option<PathBuf>,
    /// `--log-format json`.
    log_json: bool,
    /// `--decoy-selection`.
    decoys: wrkz_rpc::node::DecoySelection,
    console: bool,
    checkpoints: bool,
    /// `--load-checkpoints`, as given.
    load_checkpoints: Option<String>,
    help: bool,
    version: bool,
    /// `--prune-depth`, already clamped, when `--prune` was given.
    prune_depth: Option<u32>,
    /// `--lite-height`, when `--lite` was given.
    lite_height: Option<u32>,
    /// `--import-lite-snapshot`: load this file into the empty state, then exit.
    import_lite_snapshot: Option<PathBuf>,
    /// `--snapshot-info`: describe this snapshot file as JSON, then exit.
    snapshot_info: Option<PathBuf>,
    /// `--snapshot-stats`: measure the state's tables, then exit.
    snapshot_stats: bool,
    auto_prune_min_gap_blocks: u32,
    auto_prune_min_free_bytes: u64,
    /// `--dump-config`.
    dump_config: bool,
    /// `--save-config`.
    save_config: Option<PathBuf>,
    /// `--config-file`, when one was read.
    config_file: Option<PathBuf>,
    /// What the file set that this daemon will not honour, for the log.
    config_notes: Vec<String>,
    /// Command-line options accepted only so an old command line still
    /// starts, and ignored — one warning each at start-up.
    ignored: Vec<String>,
    /// `--resync`: delete the chain and peer state before opening them.
    resync: bool,
    /// `--rewind-to-height`, a block count; never 0.
    rewind_to_height: Option<u32>,
    /// `--import-blockchain`.
    import_chain: bool,
    /// `--export-blockchain`.
    export_chain: bool,
    /// `--dump-file`.
    dump_file: PathBuf,
    /// `--max-export-blocks`; never 0.
    max_export_blocks: Option<u64>,
    /// `--import-validate`.
    import_validate: bool,
    /// `--no-upnp`: this port's; the C++ always tries a mapping.
    no_upnp: bool,
    /// `--attach`: be a console for a running daemon instead of a node.
    attach: Option<String>,
}

/// `DaemonConfiguration::MIN_PRUNE_DEPTH`
/// (`DaemonConfiguration.h:22-26`): `EXPECTED_NUMBER_OF_BLOCKS_PER_DAY * 7`,
/// with 1440 blocks a day at a 60-second target.
///
/// This is a *network-health* minimum, far above the
/// [`wrkz_chain::MIN_PRUNE_DEPTH`] a reorganisation needs (181). Both exist:
/// the chain crate refuses a depth that could break a reorganisation, and this
/// keeps a pruned node useful to its peers.
const MIN_PRUNE_DEPTH: u32 = 1440 * 7;
/// `DaemonConfiguration::DEFAULT_PRUNE_DEPTH` — the same value (`:25-26`).
const DEFAULT_PRUNE_DEPTH: u32 = MIN_PRUNE_DEPTH;
/// `autoPruneMinGapBlocks` (`DaemonConfiguration.h:111`).
const DEFAULT_AUTO_PRUNE_MIN_GAP_BLOCKS: u32 = 120;
/// `autoPruneMinFreeBytes` (`:113`).
const DEFAULT_AUTO_PRUNE_MIN_FREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// `clampPruneDepth` (`DaemonConfiguration.cpp:31-44`): a depth below the
/// minimum is **raised to it with a message**, not refused, and the daemon
/// carries on. The message is the C++'s, word for word, because an operator
/// who has seen it once should not have to wonder whether it means the same
/// thing here.
fn clamp_prune_depth(depth: u32, source: &str) -> u32 {
    if depth >= MIN_PRUNE_DEPTH {
        return depth;
    }
    println!(
        "The configured prune depth ({depth}) from {source} is below the enforced minimum \
         ({MIN_PRUNE_DEPTH}, about 7 days). Using the minimum for network health."
    );
    MIN_PRUNE_DEPTH
}

/// The command line, with a `--config-file`'s settings in front of it: the
/// C++'s order of command line, file, command line again
/// (`Daemon.cpp:394-461`), in one pass.
fn parse_args() -> Result<Args, String> {
    let cli: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = config_file_of(&cli)? else { return parse_args_from(cli) };
    let file = wrkz_node::config_file::load(&path)?;
    let mut a = parse_args_from(file.args.into_iter().chain(cli))?;
    a.config_file = Some(path);
    a.config_notes = file.notes;
    Ok(a)
}

/// `--config-file` / `-c`, found before anything is parsed, because what it
/// holds goes in front of every other argument.
fn config_file_of(cli: &[String]) -> Result<Option<PathBuf>, String> {
    let mut found = None;
    let mut i = 0;
    while i < cli.len() {
        if cli[i] == "--config-file" || cli[i] == "-c" {
            let path = cli.get(i + 1).ok_or_else(|| "--config-file needs a value".to_string())?;
            found = Some(PathBuf::from(path));
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(found)
}

fn parse_args_from(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        cfg: NodeConfig {
            data_dir: PathBuf::new(),
            p2p_port: P2P_DEFAULT_PORT,
            unlogged_initial_sync: true,
            ..Default::default()
        },
        rpc: ServerConfig { bind: "127.0.0.1:17856".into(), ..Default::default() },
        serve_rpc: true,
        stratum: StratumConfig::default(),
        zmq_pub: wrkz_node::zmq::DEFAULT_ENDPOINT.to_string(),
        no_zmq: false,
        hooks: Default::default(),
        add_peers: Vec::new(),
        exclusive_nodes: Vec::new(),
        priority_nodes: Vec::new(),
        db: DbConfig::default(),
        skip_boot_compaction: false,
        auto_compaction: AutoCompaction::default(),
        validate_threads: default_validate_threads(),
        batch_blocks: wrkz_storage::batch::DEFAULT_BATCH_POINTS,
        batch_bytes_mb: (wrkz_storage::batch::DEFAULT_BATCH_BYTES >> 20) as u64,
        level: Level::Info,
        log_file: None,
        log_json: false,
        decoys: wrkz_rpc::node::DecoySelection::Uniform,
        console: true,
        checkpoints: true,
        load_checkpoints: None,
        help: false,
        version: false,
        prune_depth: None,
        lite_height: None,
        import_lite_snapshot: None,
        snapshot_info: None,
        snapshot_stats: false,
        auto_prune_min_gap_blocks: DEFAULT_AUTO_PRUNE_MIN_GAP_BLOCKS,
        auto_prune_min_free_bytes: DEFAULT_AUTO_PRUNE_MIN_FREE_BYTES,
        dump_config: false,
        save_config: None,
        config_file: None,
        config_notes: Vec::new(),
        ignored: Vec::new(),
        resync: false,
        rewind_to_height: None,
        import_chain: false,
        export_chain: false,
        dump_file: PathBuf::from(wrkz_chain::dump::DEFAULT_DUMP_FILE),
        max_export_blocks: None,
        import_validate: false,
        no_upnp: false,
        attach: None,
    };
    let mut prune = false;
    let mut prune_depth = DEFAULT_PRUNE_DEPTH;
    let mut lite = false;
    let mut lite_height: Option<u32> = None;
    let mut explorer = false;
    let mut rpc_ip = "127.0.0.1".to_string();
    let mut rpc_ipv6: Option<Ipv6Addr> = None;
    let mut rpc_port: u16 = 17856;
    let mut data_dir_seen = false;
    let mut args = argv.into_iter();

    while let Some(arg) = args.next() {
        if let Some((name, on)) = switch(&arg)? {
            match name {
                "--db-enable-compression" => a.db.compression = on,
                "--db-bottom-filters" => a.db.bottommost_filters = on,
                "--skip-boot-compaction" => a.skip_boot_compaction = on,
                _ => unreachable!("every name in SWITCHES is matched"),
            }
            continue;
        }
        let mut value =
            |name: &str| -> Result<String, String> { args.next().ok_or_else(|| format!("{name} needs a value")) };
        let number =
            |name: &str, v: String| -> Result<u64, String> { v.parse::<u64>().map_err(|e| format!("{name}: {e}")) };
        let int =
            |name: &str, v: String| -> Result<i32, String> { v.parse::<i32>().map_err(|e| format!("{name}: {e}")) };
        match arg.as_str() {
            "-h" | "--help" => a.help = true,
            "--version" => a.version = true,

            "--data-dir" => {
                a.cfg.data_dir = PathBuf::from(value("--data-dir")?);
                data_dir_seen = true;
            }
            "--no-checkpoints" => a.checkpoints = false,
            "--load-checkpoints" => a.load_checkpoints = Some(value("--load-checkpoints")?),

            // `DaemonConfiguration.cpp:95-140,502-537`. Both numbers are the
            // C++'s `uint32_t`, and 0 is refused for each with its message.
            "--resync" => a.resync = true,
            "--rewind-to-height" => {
                let n = number("--rewind-to-height", value("--rewind-to-height")?)?;
                if n == 0 {
                    return Err("Please use the `--resync` option instead of `--rewind-to-height 0` to completely \
                         reset the synchronization state."
                        .to_string());
                }
                a.rewind_to_height =
                    Some(u32::try_from(n).map_err(|_| format!("--rewind-to-height: {n} is not a block height"))?);
            }
            "--import-blockchain" => a.import_chain = true,
            "--export-blockchain" => a.export_chain = true,
            "--dump-file" => a.dump_file = PathBuf::from(value("--dump-file")?),
            "--max-export-blocks" => {
                let n = number("--max-export-blocks", value("--max-export-blocks")?)?;
                if n == 0 {
                    return Err("`--max-export-blocks` can not be 0.".to_string());
                }
                u32::try_from(n).map_err(|_| format!("--max-export-blocks: {n} is more than a block count"))?;
                a.max_export_blocks = Some(n);
            }
            "--import-validate" => a.import_validate = true,
            "--wal" => a.cfg.unlogged_initial_sync = false,
            "--rpc-sync-cache-size" => {
                let mb = number("--rpc-sync-cache-size", value("--rpc-sync-cache-size")?)?;
                a.rpc.sync_cache_bytes = usize::try_from(mb).unwrap_or(usize::MAX).saturating_mul(1 << 20);
            }
            "--rpc-stream-threshold" => {
                let kb = number("--rpc-stream-threshold", value("--rpc-stream-threshold")?)?;
                a.rpc.stream_threshold_bytes = usize::try_from(kb).unwrap_or(usize::MAX).saturating_mul(1 << 10);
            }

            "--prune" => prune = true,
            "--prune-depth" => prune_depth = number("--prune-depth", value("--prune-depth")?)? as u32,
            "--lite" => lite = true,
            "--lite-height" => lite_height = Some(number("--lite-height", value("--lite-height")?)? as u32),
            "--import-lite-snapshot" => {
                a.import_lite_snapshot = Some(PathBuf::from(value("--import-lite-snapshot")?));
            }
            "--snapshot-info" => a.snapshot_info = Some(PathBuf::from(value("--snapshot-info")?)),
            "--snapshot-stats" => a.snapshot_stats = true,
            "--auto-prune-min-gap-blocks" => {
                a.auto_prune_min_gap_blocks =
                    number("--auto-prune-min-gap-blocks", value("--auto-prune-min-gap-blocks")?)? as u32
            }
            "--auto-prune-min-free-bytes" => {
                a.auto_prune_min_free_bytes =
                    number("--auto-prune-min-free-bytes", value("--auto-prune-min-free-bytes")?)?
            }
            "--auto-compaction-min-gap-blocks" => {
                a.auto_compaction.min_gap_blocks =
                    number("--auto-compaction-min-gap-blocks", value("--auto-compaction-min-gap-blocks")?)?
            }
            "--auto-compaction-min-free-bytes" => {
                a.auto_compaction.min_free_bytes =
                    number("--auto-compaction-min-free-bytes", value("--auto-compaction-min-free-bytes")?)?
            }

            "--p2p-bind-ip" | "--bind" => {
                a.cfg.bind = value("--p2p-bind-ip")?.parse::<IpAddr>().map_err(|e| format!("--p2p-bind-ip: {e}"))?
            }
            "--p2p-bind-port" | "--p2p-port" => {
                a.cfg.p2p_port = number("--p2p-bind-port", value("--p2p-bind-port")?)? as u16
            }
            "--p2p-external-port" => {
                // An `int` in the C++, checked with its message (`Daemon.cpp:522-526`),
                // so a negative number is that refusal and not a parse error.
                let v = value("--p2p-external-port")?;
                let n: i64 = v.trim().parse().map_err(|e| format!("--p2p-external-port: {e}"))?;
                a.cfg.external_port =
                    u16::try_from(n).map_err(|_| "P2P External Port must be between 0 and 65,535".to_string())?;
            }
            "--p2p-bind-ipv6-address" => {
                let v = value("--p2p-bind-ipv6-address")?;
                a.cfg.bind_ipv6 = Some(
                    v.trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<Ipv6Addr>()
                        .map_err(|e| format!("--p2p-bind-ipv6-address: {e}"))?,
                );
            }
            "--p2p-bind-port-ipv6" => {
                a.cfg.p2p_port_ipv6 = number("--p2p-bind-port-ipv6", value("--p2p-bind-port-ipv6")?)? as u16
            }
            "--no-listen" => a.cfg.listen = false,
            "--add-peer" => a.add_peers.push(value("--add-peer")?),
            "--seed-node" | "--seed" => a.cfg.seeds.push(value("--seed-node")?),
            "--add-exclusive-node" => a.exclusive_nodes.push(value("--add-exclusive-node")?),
            "--add-priority-node" => a.priority_nodes.push(value("--add-priority-node")?),
            "--no-default-seeds" => a.cfg.use_default_seeds = false,
            "--out-peers" => a.cfg.max_outgoing = number("--out-peers", value("--out-peers")?)? as usize,
            "--in-peers" => a.cfg.max_incoming = number("--in-peers", value("--in-peers")?)? as usize,
            "--sync-max-peers" => {
                a.cfg.tuning.max_peers = number("--sync-max-peers", value("--sync-max-peers")?)? as usize
            }
            "--allow-local-ip" => a.cfg.allow_local_ip = true,
            "--hide-my-port" => a.cfg.hide_my_port = true,
            "--no-upnp" => a.no_upnp = true,
            "--p2p-reset-peerstate" => a.cfg.reset_peer_state = true,
            // The C++ floors, `DaemonConfiguration.cpp:906-930`.
            "--sync-peer-failure-threshold" => {
                let n = number("--sync-peer-failure-threshold", value("--sync-peer-failure-threshold")?)?;
                a.cfg.tuning.peer_failure_threshold = n.clamp(1, u64::from(u32::MAX)) as u32;
            }
            "--sync-batch-min" => {
                let n = number("--sync-batch-min", value("--sync-batch-min")?)?;
                a.cfg.tuning.batch_min = n.clamp(1, u64::from(u32::MAX)) as u32;
            }
            "--sync-batch-max" => {
                let n = number("--sync-batch-max", value("--sync-batch-max")?)?;
                a.cfg.tuning.batch_max = n.clamp(1, u64::from(u32::MAX)) as u32;
            }
            "--block-sync-size" => {
                let n = number("--block-sync-size", value("--block-sync-size")?)?;
                a.cfg.tuning.block_sync_size = n.clamp(1, u64::from(u32::MAX)) as u32;
            }
            "--block-sync-bytes" => {
                a.cfg.tuning.block_sync_bytes =
                    number("--block-sync-bytes", value("--block-sync-bytes")?)?.max(2 * 1024 * 1024)
            }

            "--rpc-bind-ip" => rpc_ip = value("--rpc-bind-ip")?,
            "--rpc-bind-port" => rpc_port = number("--rpc-bind-port", value("--rpc-bind-port")?)? as u16,
            "--rpc-bind-ipv6-address" => {
                let v = value("--rpc-bind-ipv6-address")?;
                rpc_ipv6 = Some(
                    v.trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<Ipv6Addr>()
                        .map_err(|e| format!("--rpc-bind-ipv6-address: {e}"))?,
                );
            }
            // The C++ needs `rpc-use-ipv6` *and* an address (`RpcServer.cpp:97`).
            // Here the address alone enables it, as it does for the P2P
            // listener; the flag is still accepted so a C++ command line runs.
            "--rpc-use-ipv6" => {}
            "--no-rpc" => a.serve_rpc = false,
            "--enable-cors" => a.rpc.cors_header = value("--enable-cors")?,
            // Refused, not ignored: ignoring it would quietly start a node
            // without the explorer its operator asked for.
            "--enable-blockexplorer" => {
                return Err("--enable-blockexplorer was removed: use --daemon-mode explorer".to_string())
            }
            "--rpc-access-token" => a.rpc.access_token = value("--rpc-access-token")?,
            "--rpc-ipc-path" => a.rpc.ipc_path = value("--rpc-ipc-path")?,
            "--rpc-ipc-mode" => {
                let v = value("--rpc-ipc-mode")?;
                a.rpc.ipc_mode = wrkz_rpc::ipc::parse_mode(&v)
                    .ok_or_else(|| format!("--rpc-ipc-mode must be octal permissions such as 0600, got {v}"))?;
            }
            "--rpc-ipc-group" => a.rpc.ipc_group = value("--rpc-ipc-group")?,
            "--rpc-ipc-require-token" => a.rpc.ipc_require_token = true,
            "--rpc-max-rpm" | "--rpc-max-requests-per-minute" => {
                a.rpc.max_requests_per_minute = number("--rpc-max-rpm", value("--rpc-max-rpm")?)? as u32
            }
            "--rpc-max-connections-per-ip" => {
                a.rpc.max_connections_per_ip =
                    number("--rpc-max-connections-per-ip", value("--rpc-max-connections-per-ip")?)? as usize
            }
            // The C++ floors, `DaemonConfiguration.cpp:780-793`.
            "--rpc-read-timeout" => {
                a.rpc.read_timeout =
                    Duration::from_secs(number("--rpc-read-timeout", value("--rpc-read-timeout")?)?.max(1))
            }
            "--rpc-write-timeout" => {
                a.rpc.write_timeout =
                    Duration::from_secs(number("--rpc-write-timeout", value("--rpc-write-timeout")?)?.max(1))
            }
            "--rpc-max-body-bytes" => {
                let n = number("--rpc-max-body-bytes", value("--rpc-max-body-bytes")?)?.max(1024);
                a.rpc.limits.max_body = usize::try_from(n).unwrap_or(usize::MAX);
            }
            "--daemon-mode" => match value("--daemon-mode")?.to_ascii_lowercase().as_str() {
                "standard" => {
                    a.rpc.mode = RpcMode::Standard;
                    explorer = false;
                }
                "explorer" => {
                    a.rpc.mode = RpcMode::Explorer;
                    explorer = true;
                }
                other => return Err(format!("--daemon-mode: expected standard or explorer, got {other}")),
            },
            "--rpc-max-block-count" => {
                a.rpc.max_block_count = number("--rpc-max-block-count", value("--rpc-max-block-count")?)?.max(1)
            }
            "--rpc-max-global-index-range" => {
                a.rpc.max_global_index_range =
                    number("--rpc-max-global-index-range", value("--rpc-max-global-index-range")?)?.max(100)
            }
            "--rpc-workers" => a.rpc.workers = number("--rpc-workers", value("--rpc-workers")?)?.max(1) as usize,
            "--rpc-trust-proxy" => a.rpc.trust_proxy = true,

            "--stratum-bind-ip" => {
                let v = value("--stratum-bind-ip")?;
                a.stratum.bind_ip = v
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .map_err(|e| format!("--stratum-bind-ip: {e}"))?;
            }
            "--stratum-bind-port" => {
                let n = number("--stratum-bind-port", value("--stratum-bind-port")?)?;
                a.stratum.port = u16::try_from(n).map_err(|_| format!("--stratum-bind-port: {n} is not a port"))?;
            }
            "--stratum-share-difficulty" => {
                a.stratum.share_difficulty = number("--stratum-share-difficulty", value("--stratum-share-difficulty")?)?
            }
            "--stratum-max-connections" => {
                a.stratum.max_connections =
                    number("--stratum-max-connections", value("--stratum-max-connections")?)? as usize
            }
            // Node fees are gone: nothing reports one to a wallet. The two
            // options are still accepted, so an existing service file starts.
            "--fee-address" => {
                let _ = value("--fee-address")?;
                a.ignored.push("--fee-address is ignored: node fees were removed".to_string());
            }
            "--fee-amount" => {
                let _ = value("--fee-amount")?;
                a.ignored.push("--fee-amount is ignored: node fees were removed".to_string());
            }

            // `DaemonConfiguration.cpp:384-425`. Sizes are megabytes, except the
            // block size (kilobytes) and the dictionary (bytes).
            "--db-threads" => {
                a.db.background_threads = int("--db-threads", value("--db-threads")?)?;
                if a.db.background_threads < 1 {
                    return Err("--db-threads must be at least 1".to_string());
                }
            }
            // RocksDB reads -1 as "no limit", and the C++'s `int` passes it on.
            "--db-max-open-files" => a.db.max_open_files = int("--db-max-open-files", value("--db-max-open-files")?)?,
            "--db-read-buffer-size" => {
                a.db.read_cache_mb = number("--db-read-buffer-size", value("--db-read-buffer-size")?)?
            }
            "--db-write-buffer-size" => {
                a.db.write_buffer_mb = number("--db-write-buffer-size", value("--db-write-buffer-size")?)?;
                if a.db.write_buffer_mb == 0 {
                    return Err("--db-write-buffer-size must be at least 1 (megabytes)".to_string());
                }
            }
            "--db-row-cache-percent" => {
                a.db.row_cache_percent = Some(number("--db-row-cache-percent", value("--db-row-cache-percent")?)?)
            }
            "--db-block-size" => a.db.block_size_kb = number("--db-block-size", value("--db-block-size")?)?,
            "--db-compression-level" => {
                a.db.compression_level = int("--db-compression-level", value("--db-compression-level")?)?
            }
            "--db-compression-dict-bytes" => {
                a.db.compression_dict_bytes =
                    number("--db-compression-dict-bytes", value("--db-compression-dict-bytes")?)?
            }
            "--batch-blocks" => {
                a.batch_blocks = number("--batch-blocks", value("--batch-blocks")?)?.min(u64::from(u32::MAX)) as u32
            }
            "--batch-bytes" => a.batch_bytes_mb = number("--batch-bytes", value("--batch-bytes")?)?,
            "--threads" => a.validate_threads = number("--threads", value("--threads")?)? as usize,
            "--transaction-validation-threads" => {
                let n = number("--transaction-validation-threads", value("--transaction-validation-threads")?)?;
                if n > 0 {
                    a.validate_threads = n as usize;
                }
            }
            // Read before parsing began (`parse_args`); its value is skipped here.
            "--config-file" | "-c" => {
                let _ = value("--config-file")?;
            }
            "--dump-config" => a.dump_config = true,
            "--enable-metrics" => a.rpc.metrics = true,
            "--enable-health" => a.rpc.health = true,
            "--zmq-pub" => a.zmq_pub = value("--zmq-pub")?,
            "--no-zmq" => a.no_zmq = true,
            "--block-notify" => a.hooks.block = value("--block-notify")?,
            "--reorg-notify" => a.hooks.reorg = value("--reorg-notify")?,
            "--tx-notify" => a.hooks.tx = value("--tx-notify")?,
            "--notify-during-sync" => a.hooks.notify_during_sync = true,
            "--decoy-selection" => {
                let v = value("--decoy-selection")?;
                a.decoys = wrkz_rpc::node::DecoySelection::parse(&v)
                    .ok_or_else(|| format!("--decoy-selection: expected uniform or recent, got {v}"))?;
            }
            "--log-format" => {
                a.log_json = match value("--log-format")?.to_ascii_lowercase().as_str() {
                    "text" => false,
                    "json" => true,
                    other => return Err(format!("--log-format: expected text or json, got {other}")),
                };
            }
            "--save-config" => a.save_config = Some(PathBuf::from(value("--save-config")?)),

            "--log-level" => {
                let v = value("--log-level")?;
                // Names or the C++ CLI's 0-4, which `set_log` also takes.
                a.level = Level::parse(&v)
                    .or_else(|| Level::from_number(&v))
                    .ok_or_else(|| format!("--log-level: expected a name or 0-4, got {v}"))?;
            }
            "--log-file" => a.log_file = Some(PathBuf::from(value("--log-file")?)),
            "--no-console" => a.console = false,
            // Command line only, as in the C++: no configuration file key.
            "--attach" => a.attach = Some(value("--attach")?),
            "--sync-to" => a.cfg.sync_to = Some(number("--sync-to", value("--sync-to")?)? as u32),
            "--exit-when-synced" => a.cfg.exit_when_synced = true,

            other => return Err(format!("unknown argument {other}")),
        }
    }
    a.rpc.bind = format!("{rpc_ip}:{rpc_port}");
    // One port for both families, as the C++ does: `listenIpv6` uses `m_port`
    // (`RpcServer.cpp:394`).
    a.rpc.bind_ipv6 = rpc_ipv6.map(|ip| format!("[{ip}]:{rpc_port}")).unwrap_or_default();
    a.rpc.version = wrkz_rpc::DAEMON_VERSION.to_string();
    // `syncBatchMax = max(syncBatchMin, …)` (`DaemonConfiguration.cpp:919`).
    a.cfg.tuning.batch_max = a.cfg.tuning.batch_max.max(a.cfg.tuning.batch_min);
    let exits_early = a.help
        || a.version
        || a.dump_config
        || a.save_config.is_some()
        || a.attach.is_some()
        || a.snapshot_info.is_some();
    if !exits_early && !data_dir_seen {
        return Err("--data-dir is required".to_string());
    }

    // `resolveLiteProfile` (`Daemon.cpp:221-325`), which refuses each of these
    // outright rather than picking one meaning and going with it. The wording
    // is the C++'s.
    if lite && lite_height.is_none() {
        return Err("--lite requires --lite-height, the height from which full block data is kept. There \
             is no sensible default: it decides what this node can never serve or rescan again."
            .to_string());
    }
    if lite && prune {
        return Err("--lite and --prune cannot be combined. Pruning below the lite height would remove \
             nothing, and above it would break the promise a lite node makes to serve every block \
             from its lite height up."
            .to_string());
    }
    if lite && explorer {
        return Err("--lite and --daemon-mode explorer cannot be combined. Block and transaction lookups \
             below the lite height need the block data a lite node never stores, so the explorer \
             endpoints would return an error for those heights rather than an answer."
            .to_string());
    }
    if !lite && lite_height.is_some() {
        return Err("--lite-height needs --lite".to_string());
    }
    // `Daemon.cpp:838-846`, the C++'s words.
    if !lite && a.import_lite_snapshot.is_some() {
        return Err("--import-lite-snapshot needs --lite and the --lite-height the snapshot was made at. A snapshot \
             only describes the region below a lite height, so there is nothing to import it into without one."
            .to_string());
    }
    if lite {
        a.lite_height = lite_height;
        a.cfg.lite_start_height = lite_height;
        a.cfg.lite_height_check = lite_height;
    }
    if prune {
        let depth = clamp_prune_depth(prune_depth, "CLI");
        a.prune_depth = Some(depth);
        a.cfg.pruned_depth = Some(depth);
    } else if prune_depth != DEFAULT_PRUNE_DEPTH {
        return Err("--prune-depth needs --prune".to_string());
    }
    if a.validate_threads == 0 {
        return Err("--threads must be at least 1 (1 is the sequential path)".to_string());
    }
    if a.batch_blocks == 0 {
        return Err("--batch-blocks must be at least 1 (1 is one write batch per block)".to_string());
    }
    if a.batch_bytes_mb == 0 {
        return Err("--batch-bytes must be at least 1".to_string());
    }
    Ok(a)
}

/// The boolean options the C++ declares with an implicit value: alone they
/// mean true, and `=false` attached is how its command line turns one off.
const SWITCHES: [&str; 3] = ["--db-enable-compression", "--db-bottom-filters", "--skip-boot-compaction"];

/// `Some((name, value))` when `arg` is one of [`SWITCHES`], bare or with
/// `=true`/`=false` (or `1`/`0`, `yes`/`no`, `on`/`off`) attached. A longer
/// option that merely starts with one of the names is not one.
fn switch(arg: &str) -> Result<Option<(&'static str, bool)>, String> {
    for name in SWITCHES {
        let Some(rest) = arg.strip_prefix(name) else { continue };
        let on = match rest.to_ascii_lowercase().as_str() {
            "" | "=true" | "=1" | "=yes" | "=on" => true,
            "=false" | "=0" | "=no" | "=off" => false,
            _ if rest.starts_with('=') => return Err(format!("{name}: expected true or false, got {}", &rest[1..])),
            _ => continue,
        };
        return Ok(Some((name, on)));
    }
    Ok(None)
}

/// `asJSON` (`DaemonConfiguration.cpp:2010`): the effective configuration,
/// compact, under the C++'s keys for every setting this daemon has, plus this
/// port's own. `--config-file` reads it back to the same configuration, and a
/// `Wrkzd` reads the C++ keys of it and ignores the rest.
fn dump_config(a: &Args) -> String {
    use wrkz_rpc::json::{Json, Obj};
    let strings = |v: &[String]| Json::Array(v.iter().map(|s| Json::from(s.as_str())).collect());
    let (rpc_ip, rpc_port) = split_host_port(&a.rpc.bind);
    let rpc_port: u16 = rpc_port.parse().unwrap_or(17856);
    let rpc_ipv6 = split_host_port(&a.rpc.bind_ipv6).0.trim_start_matches('[').trim_end_matches(']').to_string();
    let checkpoints =
        if a.checkpoints { a.load_checkpoints.clone().unwrap_or_else(|| "default".into()) } else { String::new() };
    let mut o = Obj::new();
    o.set("data-dir", a.cfg.data_dir.display().to_string())
        .set("load-checkpoints", checkpoints)
        .set("log-file", a.log_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default())
        .set("log-level", a.level as u8)
        .set("log-format", if a.log_json { "json" } else { "text" })
        .set("no-console", !a.console)
        .set("allow-local-ip", a.cfg.allow_local_ip)
        .set("hide-my-port", a.cfg.hide_my_port)
        .set("no-upnp", a.no_upnp)
        .set("p2p-bind-ip", a.cfg.bind.to_string())
        .set("p2p-bind-port", a.cfg.p2p_port)
        .set("p2p-external-port", a.cfg.external_port)
        .set("out-peers", a.cfg.max_outgoing)
        .set("in-peers", a.cfg.max_incoming)
        .set("p2p-reset-peerstate", a.cfg.reset_peer_state)
        .set("p2p-bind-ipv6-address", a.cfg.bind_ipv6.map(|ip| ip.to_string()).unwrap_or_default())
        .set("p2p-bind-port-ipv6", a.cfg.p2p_port_ipv6)
        .set("rpc-bind-ipv6-address", rpc_ipv6)
        .set("rpc-bind-ip", rpc_ip)
        .set("rpc-bind-port", rpc_port)
        .set("add-exclusive-node", strings(&a.exclusive_nodes))
        .set("add-priority-node", strings(&a.priority_nodes))
        .set("seed-node", strings(&a.cfg.seeds))
        .set("no-default-seeds", !a.cfg.use_default_seeds)
        .set("add-peer", strings(&a.add_peers))
        .set("daemon-mode", if a.rpc.mode == RpcMode::Explorer { "explorer" } else { "standard" })
        .set("rpc-ipc-path", a.rpc.ipc_path.clone())
        .set("rpc-ipc-mode", wrkz_rpc::ipc::format_mode(a.rpc.ipc_mode))
        .set("rpc-ipc-group", a.rpc.ipc_group.clone())
        .set("rpc-ipc-require-token", a.rpc.ipc_require_token)
        .set("enable-cors", a.rpc.cors_header.as_str())
        .set("rpc-access-token", a.rpc.access_token.as_str())
        .set("rpc-read-timeout", a.rpc.read_timeout.as_secs())
        .set("rpc-write-timeout", a.rpc.write_timeout.as_secs())
        .set("rpc-max-body-bytes", a.rpc.limits.max_body)
        .set("rpc-max-rpm", a.rpc.max_requests_per_minute)
        .set("rpc-max-connections-per-ip", a.rpc.max_connections_per_ip)
        .set("rpc-max-global-index-range", a.rpc.max_global_index_range)
        .set("rpc-max-block-count", a.rpc.max_block_count)
        .set("rpc-sync-cache-size", a.rpc.sync_cache_bytes >> 20)
        .set("rpc-stream-threshold", a.rpc.stream_threshold_bytes >> 10)
        .set("rpc-trust-proxy", a.rpc.trust_proxy)
        .set("rpc-workers", a.rpc.workers)
        .set("enable-metrics", a.rpc.metrics)
        .set("enable-health", a.rpc.health)
        .set("decoy-selection", a.decoys.name())
        .set("no-rpc", !a.serve_rpc)
        .set("stratum-bind-ip", a.stratum.bind_ip.to_string())
        .set("stratum-bind-port", a.stratum.port)
        .set("stratum-share-difficulty", a.stratum.share_difficulty)
        .set("stratum-max-connections", a.stratum.max_connections)
        .set("zmq-pub", a.zmq_pub.as_str())
        .set("no-zmq", a.no_zmq)
        .set("block-notify", a.hooks.block.as_str())
        .set("reorg-notify", a.hooks.reorg.as_str())
        .set("tx-notify", a.hooks.tx.as_str())
        .set("notify-during-sync", a.hooks.notify_during_sync)
        .set("transaction-validation-threads", a.validate_threads)
        .set("sync-max-peers", a.cfg.tuning.max_peers)
        .set("sync-peer-failure-threshold", a.cfg.tuning.peer_failure_threshold)
        .set("sync-batch-min", a.cfg.tuning.batch_min)
        .set("sync-batch-max", a.cfg.tuning.batch_max)
        .set("block-sync-size", a.cfg.tuning.block_sync_size)
        .set("block-sync-bytes", a.cfg.tuning.block_sync_bytes)
        .set("auto-prune-min-gap-blocks", a.auto_prune_min_gap_blocks)
        .set("auto-prune-min-free-bytes", a.auto_prune_min_free_bytes)
        .set("prune", a.prune_depth.is_some())
        .set("prune-depth", a.prune_depth.unwrap_or(DEFAULT_PRUNE_DEPTH))
        .set("lite", a.lite_height.is_some())
        .set("no-listen", !a.cfg.listen)
        .set("batch-blocks", a.batch_blocks)
        .set("batch-bytes", a.batch_bytes_mb)
        .set("wal", !a.cfg.unlogged_initial_sync);
    if let Some(height) = a.lite_height {
        o.set("lite-height", height);
    }
    // Always written, as `asJSON` writes them (`DaemonConfiguration.cpp:2019-2029`,
    // `:2083-2085`), so a dump carries the engine options it will run with.
    o.set("skip-boot-compaction", a.skip_boot_compaction)
        .set("db-enable-compression", a.db.compression)
        .set("db-compression-dict-bytes", a.db.compression_dict_bytes)
        .set("db-compression-level", i64::from(a.db.compression_level))
        .set("db-row-cache-percent", a.db.row_cache_percent.unwrap_or(0))
        .set("db-bottom-filters", a.db.bottommost_filters)
        .set("db-block-size", a.db.block_size_kb)
        .set("db-max-open-files", i64::from(a.db.max_open_files))
        .set("db-read-buffer-size", a.db.read_cache_mb)
        .set("db-threads", i64::from(a.db.background_threads))
        .set("db-write-buffer-size", a.db.write_buffer_mb)
        .set("auto-compaction-min-gap-blocks", a.auto_compaction.min_gap_blocks)
        .set("auto-compaction-min-free-bytes", a.auto_compaction.min_free_bytes);
    o.build().to_string()
}

/// `host:port` split at the last colon; an empty string gives two.
fn split_host_port(bind: &str) -> (String, String) {
    match bind.rfind(':') {
        Some(i) => (bind[..i].to_string(), bind[i + 1..].to_string()),
        None => (bind.to_string(), String::new()),
    }
}

/// `--threads` when it is not given: half the logical cores. Every thread
/// verifying a ring is a core not answering an RPC call, which is why the
/// daemon does not take `wrkz_pow::parallel::default_threads`, the offline
/// replay's all-cores default.
fn default_validate_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).div_ceil(2).clamp(1, 16)
}

/// `--load-checkpoints` (`Daemon.cpp:635-657`): `default` is the compiled-in
/// table, a path is a CSV of `index,hash` lines that **replaces** it, and an
/// empty value means none. `--no-checkpoints` is this port's spelling of the
/// empty value, so the two together are refused rather than resolved.
///
/// Stricter than the C++ in one place: a file naming a different hash at an
/// index the compiled-in table also covers is refused. Such a file can only be
/// wrong — the compiled-in table is the chain every node agrees on — and a
/// node running it would reject the real chain and ban every peer serving it.
fn select_checkpoints(enabled: bool, load: Option<&str>) -> Result<Checkpoints, String> {
    match (enabled, load) {
        (false, Some(_)) => Err("--no-checkpoints and --load-checkpoints cannot be combined".to_string()),
        (false, None) | (true, Some("")) => Ok(Checkpoints::none()),
        (true, None) | (true, Some("default")) => Ok(Checkpoints::mainnet()),
        (true, Some(path)) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("--load-checkpoints {path}: {e}"))?;
            let loaded = Checkpoints::from_csv(&text).map_err(|e| format!("--load-checkpoints {path}: {e}"))?;
            let compiled = Checkpoints::mainnet();
            if let Some((index, hash)) = loaded.iter().find(|(i, h)| compiled.get(*i).is_some_and(|c| c != *h)) {
                return Err(format!(
                    "--load-checkpoints {path}: the checkpoint for block {index} ({}) contradicts the \
                     compiled-in one ({}). This file does not describe the WrkzCoin chain.",
                    hex::encode(hash),
                    hex::encode(compiled.get(index).expect("found above"))
                ));
            }
            log_info!(
                "loaded {} checkpoints from {path}, up to block {}",
                loaded.len(),
                loaded.top_index().map_or_else(|| "none".to_string(), |t| t.to_string())
            );
            Ok(loaded)
        }
    }
}

/// Put the import's write-batching overlay in front of the engine.
///
/// The engine applies a downloaded batch of blocks and then commits it as one
/// write batch rather than one per block (`Node::step`). Every read — the
/// RPC's and the console's included — consults the pending writes first, so
/// nothing sees a stale chain. The resume height is part of the same atomic
/// batch as the blocks it covers, so a crash leaves a whole number of blocks
/// and the next run re-syncs the rest.
fn batched<S: KvStore>(store: S, args: &Args) -> BatchStore<S> {
    let bytes = (args.batch_bytes_mb as usize).saturating_mul(1 << 20);
    BatchStore::with_limits(store, args.batch_blocks, bytes)
}

/// The one-off maintenance of `Daemon.cpp:772-887`, on the opened state and
/// before the engine, the RPC or the console exist, in the C++'s order: an
/// import or an export runs and the daemon exits — the import, when both are
/// asked for — and then a rewind, after which the node starts on the rewound
/// chain. `Some` is the exit code when the daemon stops here.
fn maintenance<S: KvStore>(args: &Args, chain: &mut ChainState<S>) -> Option<ExitCode> {
    if args.import_chain {
        if args.export_chain {
            log_warn!("--import-blockchain and --export-blockchain were both given: importing, as Wrkzd does");
        }
        return Some(import_blockchain(args, chain));
    }
    if args.export_chain {
        return Some(export_blockchain(args, chain));
    }
    let height = args.rewind_to_height?;
    log_info!("Rewinding blockchain to: {height}");
    match daemon::rewind_to_height(chain, height) {
        Ok(daemon::Rewind::NotNeeded { height: tall }) => {
            log_info!("the chain is {tall} blocks tall, no taller than --rewind-to-height {height}: nothing to rewind");
            None
        }
        Ok(daemon::Rewind::Rewound { removed, height }) => {
            log_info!("Blockchain rewound to: {height} ({removed} blocks removed)");
            None
        }
        Err(e) => {
            log_error!("{e}");
            Some(ExitCode::FAILURE)
        }
    }
}

/// `--import-blockchain` (`Daemon.cpp:772-791`): 0 with the time it took, or
/// 1 with the reason. See `wrkz_chain::dump` for what is validated and how the
/// state is committed.
fn import_blockchain<S: KvStore>(args: &Args, chain: &mut ChainState<S>) -> ExitCode {
    let path = &args.dump_file;
    let started = Instant::now();
    let fail = |message: &str| {
        log_error!("Failed to import blockchain: {message}");
        ExitCode::FAILURE
    };
    log_info!("Importing blockchain from {}...", path.display());
    if args.import_validate {
        log_info!(
            "--import-validate: every imported block is validated as a peer's block whether or not it is given. \
             Below the last checkpoint the checkpoint vouches for proof of work and ring signatures, as in a peer \
             sync; add --no-checkpoints to check those too."
        );
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => return fail(&format!("Failed to open filepath specified: {e}")),
    };
    match file.metadata() {
        Ok(meta) if meta.len() == 0 => return fail(&format!("Blockchain import file {} is empty.", path.display())),
        Ok(_) => {}
        Err(e) => return fail(&format!("Failed to open filepath specified: {e}")),
    }
    // As an initial sync runs, unless `--wal`: every committed batch carries its
    // own resume height, so a crash loses whole blocks and a rerun resumes.
    if args.cfg.unlogged_initial_sync {
        if let Err(e) = chain.set_write_ahead_log(false) {
            return fail(&format!("could not switch the write-ahead log off: {e}"));
        }
    }
    let shutdown = daemon::install_signal_handlers();
    let input = std::io::BufReader::with_capacity(8 << 20, file);
    match wrkz_chain::dump::import(chain, input, &|| shutdown.triggered(), &mut |line| log_info!("{line}")) {
        Ok(report) if report.stopped => {
            log_warn!(
                "Import interrupted at block {} with {} blocks imported, all committed. Run the same command again \
                 to resume.",
                report.top,
                report.imported
            );
            ExitCode::FAILURE
        }
        Ok(_) => {
            println!("Time to import {} seconds.", started.elapsed().as_secs());
            println!();
            ExitCode::SUCCESS
        }
        Err(failure) => fail(&failure.message),
    }
}

/// `--export-blockchain` (`Daemon.cpp:792-811`): 0 with the time it took, or
/// 1 with the reason, and no partial file left behind.
fn export_blockchain<S: KvStore>(args: &Args, chain: &ChainState<S>) -> ExitCode {
    let path = &args.dump_file;
    let started = Instant::now();
    let fail = |message: &str| {
        log_error!("Failed to export blockchain: {message}");
        ExitCode::FAILURE
    };
    log_info!("Exporting blockchain to {}...", path.display());
    if path.exists() {
        return fail(&format!("{} already exists.", path.display()));
    }
    let plan = match wrkz_chain::dump::plan_export(chain, args.max_export_blocks) {
        Ok(plan) => plan,
        Err(e) => return fail(&e),
    };
    if let Some(note) = &plan.note {
        log_info!("{note}");
    }
    // `create_new`: a file that appeared since the check above is refused too,
    // never overwritten.
    let file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(e) => return fail(&format!("Failed to open filepath specified: {e}")),
    };
    let shutdown = daemon::install_signal_handlers();
    let mut out = std::io::BufWriter::with_capacity(8 << 20, file);
    let stop = || shutdown.triggered();
    let written =
        wrkz_chain::dump::export(chain, &mut out, plan.start, plan.end, &stop, &mut |line| log_info!("{line}"))
            .and_then(|_| out.flush().map_err(|e| format!("Failed writing the dump file: {e}")))
            .and_then(|_| out.get_ref().sync_all().map_err(|e| format!("Failed writing the dump file: {e}")));
    match written {
        Ok(()) => {
            println!("Time to export {} seconds.", started.elapsed().as_secs());
            println!();
            ExitCode::SUCCESS
        }
        Err(e) => {
            // A dump that stops mid-chain imports cleanly up to where it stops,
            // which is worse than no dump (`Core.cpp:3236`).
            drop(out);
            let _ = std::fs::remove_file(path);
            fail(&e)
        }
    }
}

/// Whether a `host:port` bind string names a loopback address of either
/// family. `0.0.0.0` and `::` are not loopback: they are every interface.
fn bind_is_loopback(bind: &str) -> bool {
    let host = match bind.rfind(':') {
        Some(i) => &bind[..i],
        None => bind,
    };
    wrkz_rpc::server::is_loopback_ip(host)
}

/// `--add-exclusive-node` or `--add-priority-node`, resolved. An entry that
/// resolves to nothing stops the start, as a malformed one stops the C++'s
/// (`NetNodeConfig::init` fails, and `Daemon.cpp:1048` exits on it).
fn pinned_nodes(flag: &str, entries: &[String]) -> Result<Vec<PinnedNode>, String> {
    entries.iter().map(|entry| PinnedNode::resolve(entry).map_err(|e| format!("{flag} {entry}: {e}"))).collect()
}

/// The UPnP port mapping of the P2P port (`NetNode.cpp:780`), which the C++
/// tries unconditionally. Not here when it is switched off, or when it could
/// not help: nothing listens, no peer is told the port, or the listener is on
/// loopback. `listen` is the address the IPv4 listener bound.
fn start_upnp(args: &Args, listen: Option<std::net::SocketAddr>) -> Option<PortMapper> {
    if args.no_upnp {
        log_info!("UPnP port mapping disabled (--no-upnp)");
        return None;
    }
    if !args.cfg.listen {
        log_info!("UPnP port mapping skipped: this node does not listen (--no-listen)");
        return None;
    }
    if args.cfg.hide_my_port {
        log_info!("UPnP port mapping skipped: --hide-my-port tells no peer the port");
        return None;
    }
    let addr = listen?;
    if addr.ip().is_loopback() {
        log_info!("UPnP port mapping skipped: the P2P listener is on loopback ({addr})");
        return None;
    }
    Some(PortMapper::spawn(addr.port(), addr.ip(), UpnpConfig::default()))
}

/// The program's file name, as the C++ prints `fs::path(argv[0]).filename()`.
fn program_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|argv0| std::path::Path::new(&argv0).file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "wrkz-node".to_string())
}

fn version_line() -> String {
    match GIT_COMMIT {
        Some(commit) if !commit.is_empty() => {
            format!("wrkz-node {VERSION} ({commit}), daemon RPC compatible with WrkzCoin {}", wrkz_rpc::DAEMON_VERSION)
        }
        _ => format!("wrkz-node {VERSION}, daemon RPC compatible with WrkzCoin {}", wrkz_rpc::DAEMON_VERSION),
    }
}

fn run() -> Result<ExitCode, String> {
    let mut args = parse_args()?;
    if args.help {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if args.version {
        println!("{}", version_line());
        return Ok(ExitCode::SUCCESS);
    }
    // Attaching needs no data directory, no state and no engine: it is a
    // client of a daemon that has all of those (`Daemon.cpp:402-407`).
    if let Some(endpoint) = &args.attach {
        return Ok(ExitCode::from(wrkz_node::attach::run(endpoint)));
    }
    // `--snapshot-info` (`Daemon.cpp:468-471`) needs the file and nothing else —
    // no data directory, no state — so it answers before any of those is made.
    // One line on stdout, the header only, and the digest checked against the
    // compiled-in list; exit 1 when the file cannot be read at all.
    if let Some(path) = &args.snapshot_info {
        use wrkz_chain::snapshot::container;
        let (line, readable) = container::describe(path, &container::compiled_in_digests());
        println!("{line}");
        return Ok(if readable { ExitCode::SUCCESS } else { ExitCode::FAILURE });
    }
    // `Daemon.cpp:473-490`: the dump wins over the save, and both exit.
    if args.dump_config {
        println!("{}", dump_config(&args));
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(path) = &args.save_config {
        std::fs::write(path, format!("{}\n", dump_config(&args)))
            .map_err(|e| format!("--save-config {}: {e}", path.display()))?;
        println!("Configuration saved to: {}", path.display());
        return Ok(ExitCode::SUCCESS);
    }

    wrkz_node::log::set_level(args.level);
    wrkz_node::log::set_json(args.log_json);
    if let Some(path) = &args.log_file {
        // `set_file_at` owns the path, so it can rotate by renaming and keep one
        // generation. Handing it an already-open file instead limits it to
        // truncating in place, which a Windows append handle refuses.
        wrkz_node::log::set_file_at(path, wrkz_node::log::DEFAULT_MAX_FILE_BYTES)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        log_info!("logging to {}", path.display());
    }
    std::fs::create_dir_all(&args.cfg.data_dir).map_err(|e| format!("--data-dir: {e}"))?;

    // One daemon per data directory. Held for the whole run and released on
    // the way out, including a clean shutdown from a signal.
    let _lock = DirLock::acquire(&args.cfg.data_dir)?;

    log_info!("{}", version_line());
    if let Some(path) = &args.config_file {
        log_info!("configuration read from {}", path.display());
    }
    for note in &args.config_notes {
        log_warn!("configuration file: {note}");
    }
    for note in &args.ignored {
        log_warn!("{note}");
    }
    // `--resync` (`Daemon.cpp:495-513`), after the configuration exits and
    // before anything is opened — and with the lock held, so a second daemon's
    // resync cannot delete the state a running one is writing.
    if args.resync {
        match daemon::resync(&args.cfg.data_dir) {
            Ok(removed) if removed.is_empty() => {
                log_info!("--resync: {} holds no chain or peer state to delete", args.cfg.data_dir.display())
            }
            Ok(removed) => {
                for path in removed {
                    log_info!("--resync: deleted {}", path.display());
                }
            }
            Err(e) => {
                log_error!("{e}");
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    if !args.checkpoints {
        log_info!("checkpoints disabled: every block is fully validated, which is much slower");
    }
    // Both binds are checked, and both families of loopback count as safe: a
    // `starts_with("127.0.0.1")` test would have called `[::1]:17856` public
    // and `127.0.0.99:17856` public too.
    if args.serve_rpc {
        for bind in [args.rpc.bind.as_str(), args.rpc.bind_ipv6.as_str()] {
            if bind.is_empty() || bind_is_loopback(bind) {
                continue;
            }
            log_warn!(
                "the RPC is bound to {bind} — it is reachable from outside this machine. \
                 Use --rpc-access-token, or a firewall, or bind it to 127.0.0.1 / [::1]."
            );
        }
    }

    // Before the state is opened, so a name that does not resolve costs nothing.
    args.cfg.exclusive_nodes = pinned_nodes("--add-exclusive-node", &args.exclusive_nodes)?;
    args.cfg.priority_nodes = pinned_nodes("--add-priority-node", &args.priority_nodes)?;

    let checkpoints = select_checkpoints(args.checkpoints, args.load_checkpoints.as_deref())?;
    // `store_raw_blocks` must stay on: the node serves NOTIFY_REQUEST_GET_OBJECTS
    // and `/getrawblocks` from these bytes, and needs them to put an unwound
    // block back as an alternative chain. `unwind_history` is unbounded because
    // `/get_global_indexes_for_range` and `/get_o_indexes` read the per-block
    // output records for any height a wallet asks about.
    let chain_cfg = Config {
        store_raw_blocks: true,
        // Lite and prune narrow which heights keep a body; neither touches a
        // consensus record, and neither changes what is validated.
        lite_start_height: args.lite_height.unwrap_or(0),
        prune_depth: args.prune_depth,
        unwind_history: u32::MAX,
        recent_window: 256,
        // A performance knob only: the same blocks are accepted and a
        // rejection names the same rule at every value.
        validate_threads: args.validate_threads,
    };
    log_info!(
        "ring signatures verified on up to {} threads; chain state committed per downloaded batch \
         (at most {} blocks or {} MB held back)",
        args.validate_threads,
        args.batch_blocks,
        args.batch_bytes_mb
    );
    if let Some(h) = args.lite_height {
        log_info!("lite node: full block data from height {h}. This is permanent for this database.");
    }
    if let Some(d) = args.prune_depth {
        log_info!("pruned node: keeping the block bodies of the last {d} blocks");
    }

    open_and_run(args, chain_cfg, checkpoints)
}

/// Opens the state with whichever engine this build has, then runs the daemon.
#[cfg(not(feature = "rocksdb"))]
fn open_and_run(args: Args, chain_cfg: Config, checkpoints: Checkpoints) -> Result<ExitCode, String> {
    log_warn!("chain state: in memory — nothing is persisted. Build with --features rocksdb to keep it.");
    if args.db != DbConfig::default() {
        log_warn!("the --db-* options need --features rocksdb; ignored");
    }
    if args.skip_boot_compaction || args.auto_compaction != AutoCompaction::default() {
        log_warn!("--skip-boot-compaction and --auto-compaction-* need --features rocksdb; ignored");
    }
    let store = batched(wrkz_storage::MemStore::default(), &args);
    let chain = ChainState::open_or_genesis(store, chain_cfg, checkpoints).map_err(|e| format!("chain state: {e}"))?;
    serve(args, chain, "MemStore (in memory, nothing persisted)".to_string(), None)
}

/// The RocksDB path. It needs libclang to build, which is why it is kept to the
/// few lines that differ from the in-memory path.
#[cfg(feature = "rocksdb")]
fn open_and_run(args: Args, chain_cfg: Config, checkpoints: Checkpoints) -> Result<ExitCode, String> {
    let path = args.cfg.data_dir.join("state");
    log_info!("chain state: RocksDB at {}", path.display());
    let db = &args.db;
    log_info!(
        "RocksDB: {} read cache ({} row, {} block), {} KiB blocks, {} MB write buffer, compression {}, \
         bottommost filters {}, {} background threads, {} open files",
        console::pretty_bytes(db.read_cache_bytes()),
        console::pretty_bytes(db.row_cache_bytes()),
        console::pretty_bytes(db.block_cache_bytes()),
        db.block_size_bytes() / 1024,
        db.write_buffer_mb,
        if db.compression { "ZSTD from L2" } else { "off" },
        if db.bottommost_filters { "on" } else { "off" },
        db.background_threads,
        db.max_open_files
    );
    let store = wrkz_storage::rocks::RocksStore::open(&path, db).map_err(|e| {
        format!(
            "chain state at {}: {e}. Another process may be using it, or the directory may be unreadable.",
            path.display()
        )
    })?;
    // Taken before the store disappears into the chain state, so a compaction
    // reaches the engine without the chain lock.
    let compactor: Arc<dyn CompactionEngine> = Arc::new(store.compaction_handle());
    let store = batched(store, &args);
    let chain = ChainState::open_or_genesis(store, chain_cfg, checkpoints).map_err(|e| format!("chain state: {e}"))?;
    serve(args, chain, "RocksDB".to_string(), Some(compactor))
}

/// `--import-lite-snapshot` (`Daemon.cpp:827-866`): load the file into the
/// state this run opened, through the same batching store the node writes
/// through, then exit — 0 when it imported, 1 when it did not.
///
/// Checked against the compiled-in checkpoints and the compiled-in digests
/// only, as the C++ is: there is no flag that blesses another file. The
/// `WRKZ-IMPORT {…}` progress lines go to stdout, where a supervising process
/// reads them; everything else is logged.
fn import_lite_snapshot<S: KvStore>(
    mut chain: ChainState<S>,
    path: &std::path::Path,
    lite_height: u32,
) -> Result<ExitCode, String> {
    use wrkz_chain::snapshot::{container, import};
    let mut events = |event: import::ImportEvent| match event {
        import::ImportEvent::Info(line) => log_info!("{line}"),
        import::ImportEvent::Progress { phase, done, total } => {
            println!("{}", import::progress_line(phase, done, total))
        }
    };
    let blessed = container::compiled_in_digests();
    match import::import_snapshot(&mut chain, path, lite_height, &blessed, &Checkpoints::mainnet(), &mut events) {
        Ok(_) => {
            log_info!(
                "Import finished. Start the daemon again with the same flags but without --import-lite-snapshot, and \
                 it will sync the rest of the chain from the network."
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            log_error!("Could not import that snapshot: {e}");
            // Whatever an interrupted write pass left, the mark that says so
            // is committed with it.
            if let Err(e) = chain.sync() {
                log_error!("could not flush the chain state: {e}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// `--snapshot-stats` (`Daemon.cpp:889-957`): every table of this port's state,
/// measured and printed, and the C++ payload the snapshot tables come to.
fn snapshot_stats<S: KvStore>(chain: &ChainState<S>, data_dir: &std::path::Path) -> Result<ExitCode, String> {
    log_info!("Measuring database storage. This walks every key and takes minutes on a synced chain...");
    let stats = wrkz_node::snapshot::measure_storage(chain.store(), &mut |line| log_info!("{line}"))?;
    let on_disk = wrkz_node::snapshot::directory_bytes(&data_dir.join(daemon::STATE_DIR));
    print!("{}", wrkz_node::snapshot::render_storage_stats(&stats, chain.tip_index().unwrap_or(0), on_disk));
    Ok(ExitCode::SUCCESS)
}

/// Start the engine and the RPC over one shared state, then run the loop.
/// `compactor` is the database's compaction handle, when there is a database.
fn serve<S: KvStore + Send + Sync + 'static>(
    mut args: Args,
    mut chain: ChainState<S>,
    engine: String,
    compactor: Option<Arc<dyn CompactionEngine>>,
) -> Result<ExitCode, String> {
    // `--import-lite-snapshot` runs here, on the state this run has opened and
    // genesis written into, and exits either way (`Daemon.cpp:827-866`).
    if let Some(path) = args.import_lite_snapshot.clone() {
        return import_lite_snapshot(chain, &path, args.lite_height.unwrap_or(0));
    }
    // A state a windowed replay wrote is full of holes and must never be
    // served; one a linear replay wrote is a complete chain and is the
    // recommended way to bring a node up (docs/DAEMON.md).
    daemon::check_state_tag(chain.tag().map_err(|e| format!("chain state: {e}"))?.as_deref())?;
    if chain.tip_index().is_none() {
        return Err("chain state has no genesis block".to_string());
    }
    log_info!("chain state opened at height {}", chain.tip_index().map_or(0, |t| t as u64 + 1));
    if let Some(code) = maintenance(&args, &mut chain) {
        return Ok(code);
    }
    // After a rewind, as the C++ measures (`Daemon.cpp:889-957`), and exits.
    if args.snapshot_stats {
        return snapshot_stats(&chain, &args.cfg.data_dir);
    }
    let tip_count = chain.tip_index().map_or(0, |t| t as u64 + 1);

    // An import made without `wrkz-replay --store-raw` holds every index and no
    // block bodies. Such a node syncs and validates perfectly and then fails
    // every wallet and peer request below the import height — so say it now,
    // once, at start-up, rather than let it surface as a wallet that will not
    // sync a week later. About 22 lookups; see `daemon::lowest_stored_block`.
    let bodies_from = daemon::lowest_stored_block(&chain);
    let configured_floor = u64::from(chain.body_floor().unwrap_or(0));
    let discovered = match bodies_from {
        Some(0) => 0,
        Some(height) => u64::from(height),
        // Not one body anywhere: everything below the tip is unserveable.
        None => tip_count.max(1),
    };
    // The configured policy and what is actually on disk are two different
    // facts and the honest floor is the higher of them: a pruned node's floor
    // moves with the tip and is not on disk yet, and a body-less import's floor
    // is on disk and in no configuration.
    // `lite_start_height` is the *lite* floor only. A pruned node's floor moves
    // with its tip and is carried by `pruned` and `prune_depth`; putting it here
    // too would make `/info` report `lite: true` on a node that is not lite, and
    // the C++ fills the two fields from `m_liteHeight` and `m_prunedNodeDepth`
    // separately (`CryptoNoteProtocolHandler.cpp:618-625`).
    let lite_start_height = u64::from(args.lite_height.unwrap_or(0)).max(discovered);
    // A state whose bodies begin above genesis is a lite node whether or not it
    // was asked to be one, so it has to advertise `NODE_CAPABILITY_FLAG_LITE`
    // and its floor. Otherwise a syncing peer asks it for blocks it will answer
    // with `missed_ids` every time.
    if lite_start_height != 0 {
        args.cfg.lite_start_height = Some(lite_start_height as u32);
    }
    // Two different messages, because they call for two different actions: an
    // import that lost its bodies can be redone with `--store-raw`, and a node
    // that was asked to drop them must not be told to.
    // A snapshot import is a lite node that has not reached its line yet, not
    // an import that lost its bodies, so it gets the lite notice.
    let from_snapshot = chain.transactions_floor();
    if discovered != 0 && discovered >= configured_floor && from_snapshot == 0 {
        log_warn!("{}", console::missing_bodies_warning(discovered));
    } else if lite_start_height != 0 {
        log_info!("{}", console::reduced_mode_notice(lite_start_height, args.prune_depth.is_some()));
    }
    if from_snapshot != 0 {
        log_info!(
            "imported from a lite snapshot: below height {from_snapshot} this node holds no transaction records, so \
             a transaction or payment-id lookup there is refused rather than answered"
        );
    }

    // A node that cannot answer for most of the chain must not advertise the
    // explorer methods over it. `--lite --daemon-mode explorer` was already
    // refused at parse time, as the C++ refuses it (`Daemon.cpp:285-292`);
    // this is the case the C++ has no flag
    // for — a database whose bodies simply start above genesis.
    //
    // A *pruned* node is not refused, here or in the C++: pruning keeps every
    // index and every transaction record, so the explorer answers correctly for
    // the whole retained window and returns an error, not a wrong answer, below
    // it.
    if args.rpc.mode == RpcMode::Explorer && args.prune_depth.is_none() && discovered != 0 {
        return Err(format!(
            "--daemon-mode explorer cannot be served by this state: it holds no block bodies below \
             height {discovered}, so f_block_json and f_transaction_json would return an error \
             rather than an answer for every height below that. Re-import with `wrkz-replay \
             --store-raw`, or start the daemon in --daemon-mode standard."
        ));
    }
    if args.rpc.mode == RpcMode::Explorer && args.prune_depth.is_some() {
        log_warn!(
            "block explorer on a pruned node: f_block_json and f_transaction_json answer only \
             within the {} most recent blocks and report an error below that",
            args.prune_depth.unwrap_or(0)
        );
    }

    let chain = Arc::new(RwLock::new(chain));
    let pool = Arc::new(Mutex::new(TransactionPool::new(Default::default())));

    // What `/info` and `/peers` report about the network. The engine refreshes
    // it once per loop; the RPC threads read the last published copy, so an RPC
    // call never waits on the engine. The notify hooks read it too, to tell
    // whether the node has caught up.
    //
    // `lite_start_height` is set once here and never touched again: it is a
    // property of the state on disk, and it is the field the C++ `/info`
    // already uses for "block bodies begin at this height" — wallets floor
    // their scan height at it, which is exactly the behaviour a body-less
    // import needs.
    let status: Arc<Mutex<P2pSnapshot>> = Arc::new(Mutex::new(P2pSnapshot {
        lite_start_height,
        pruned: args.prune_depth.is_some(),
        prune_depth: u64::from(args.prune_depth.unwrap_or(0)),
        ..P2pSnapshot::standalone()
    }));

    // `Daemon.cpp:1085-1104`: on unless `--no-zmq` or an empty address, and a
    // failure to bind is logged and the node runs on without it. Started
    // before the engine and the RPC, so both publish from their first block.
    let mut zmq = None;
    if args.no_zmq {
        if !args.zmq_pub.is_empty() {
            log_info!("ZMQ publisher disabled by --no-zmq.");
        }
    } else if !args.zmq_pub.is_empty() {
        match ZmqPublisher::start(&args.zmq_pub) {
            Ok(publisher) => zmq = Some(publisher),
            Err(_) => log_warn!("Failed to start ZMQ publisher on {}. Continuing without ZMQ.", args.zmq_pub),
        }
    }
    // `Daemon.cpp:1129-1149`: the hooks start only when one is configured, and
    // specs none of which can be used leave the node without them.
    let mut hooks = None;
    if !args.hooks.block.is_empty() || !args.hooks.reorg.is_empty() || !args.hooks.tx.is_empty() {
        let top_index = chain.read().unwrap_or_else(|p| p.into_inner()).tip_index().unwrap_or(0);
        let at_tip: AtTip = {
            let status = Arc::clone(&status);
            // `isSynchronized() || blockIndex + 1 >= getObservedHeight()`, both
            // heights counts; a node that has observed nothing is the tip.
            Box::new(move |index| {
                let s = status.lock().unwrap_or_else(|p| p.into_inner());
                s.synchronized || u64::from(index) + 1 >= s.blockchain_height
            })
        };
        let notifier = Arc::new(ChainNotifier::new(&args.hooks, top_index, at_tip));
        if notifier.any_enabled() {
            notifier.log_started();
            hooks = Some(notifier);
        } else {
            log_warn!("No usable notification hook configured. Continuing without notifications.");
        }
    }
    let mut listeners: Vec<Arc<dyn EventListener>> = zmq.iter().map(ZmqPublisher::listener).collect();
    if let Some(notifier) = &hooks {
        listeners.push(Arc::clone(notifier) as Arc<dyn EventListener>);
    }
    let events = Events::new(listeners);
    let mempool = SharedMempool::new(Arc::clone(&pool), Arc::clone(&chain)).with_events(events.clone());

    let mut node = Node::with_shared_chain(Arc::clone(&chain), mempool, args.cfg.clone());
    node.set_events(events.clone());
    node.start()
        .map_err(|e| format!("p2p listener on port {}: {e}. Another process may be using it.", args.cfg.p2p_port))?;
    for peer in &args.add_peers {
        match wrkz_node::peers::resolve(peer, P2P_DEFAULT_PORT) {
            Ok(addrs) => {
                for addr in addrs {
                    node.add_peer(addr);
                }
                if node.exclusive_mode() {
                    log_info!("--add-peer {peer}: on the white list, not dialled while exclusive nodes are configured");
                }
            }
            Err(e) => log_warn!("--add-peer {peer}: {e}"),
        }
    }

    let rpc_node = {
        let status = Arc::clone(&status);
        // A block `submitblock` or stratum adds is announced by this loop; the
        // hook wakes it so that happens now, not at the end of the tick.
        let waker = node.waker();
        ChainNode::shared(
            Arc::clone(&chain),
            Arc::clone(&pool),
            Box::new(move || status.lock().unwrap_or_else(|p| p.into_inner()).clone()),
        )
        .with_decoy_selection(args.decoys)
        .with_mined_block_hook(Box::new(move || waker.wake()))
        .with_events(events)
    };
    let rpc_node = Arc::new(rpc_node);

    // The C++'s warnings for an IPC socket wider than owner only
    // (`Daemon.cpp:1000-1019`), before it is bound.
    if args.serve_rpc && !args.rpc.ipc_path.is_empty() && wrkz_rpc::ipc::supported() {
        let mode = wrkz_rpc::ipc::format_mode(args.rpc.ipc_mode);
        if wrkz_rpc::ipc::is_abstract(&args.rpc.ipc_path) {
            log_warn!(
                "abstract namespace socket {} carries no permissions; every process in this network namespace \
                 can reach the RPC",
                args.rpc.ipc_path
            );
        } else if args.rpc.ipc_mode & 0o007 != 0 {
            log_warn!("--rpc-ipc-mode {mode} leaves the RPC socket open to every user on this machine");
        } else if args.rpc.ipc_mode & 0o070 != 0 && args.rpc.ipc_group.is_empty() {
            log_warn!(
                "--rpc-ipc-mode {mode} grants group access, but no --rpc-ipc-group was given, so the socket keeps \
                 the daemon user's primary group"
            );
        }
    }
    let mut rpc = None;
    if args.serve_rpc {
        let server = server::start(Arc::clone(&rpc_node) as Arc<dyn wrkz_rpc::NodeApi>, args.rpc.clone())
            .map_err(|e| format!("rpc listener on {}: {e}. Another process may be using it.", args.rpc.bind))?;
        log_info!("rpc listening on http://{}", server.local_addr());
        if let Some(addr6) = server.local_addr6() {
            log_info!("rpc also listening on http://{addr6} (IPv6 only)");
        }
        if let Some(path) = server.ipc_path() {
            let group =
                if args.rpc.ipc_group.is_empty() { String::new() } else { format!(", group {}", args.rpc.ipc_group) };
            log_info!(
                "rpc also listening on {} (mode {}{group})",
                wrkz_rpc::ipc::describe(path),
                wrkz_rpc::ipc::format_mode(args.rpc.ipc_mode)
            );
        } else if let Some(problem) = server.ipc_error() {
            log_warn!("rpc IPC listener not started: {problem}");
        }
        rpc = Some(server);
    } else {
        log_info!("rpc disabled (--no-rpc)");
    }

    // `Daemon.cpp:1106-1127`: off unless a port is given, loopback by default,
    // and a failure to bind is logged and the node runs on without it.
    let mut stratum = None;
    if args.stratum.port != 0 {
        // `chainReady`: the latched "synchronized", or nothing observed above
        // us — which keeps an isolated node from being locked out forever.
        let ready: ChainReady = {
            let status = Arc::clone(&status);
            let api = Arc::clone(&rpc_node);
            Box::new(move || {
                let synchronized = status.lock().unwrap_or_else(|p| p.into_inner()).synchronized;
                synchronized || {
                    let h = wrkz_rpc::NodeApi::height(api.as_ref());
                    h.height >= h.network_height
                }
            })
        };
        match StratumServer::start(Arc::clone(&rpc_node) as Arc<dyn wrkz_rpc::NodeApi>, &args.stratum, ready) {
            Ok(server) => {
                if !server.local_addr().ip().is_loopback() {
                    log_warn!(
                        "the stratum server is bound to {} — anyone who can reach it can make this node build \
                         templates and check shares. Put it behind a firewall unless that is intended.",
                        server.local_addr()
                    );
                }
                stratum = Some(server);
            }
            Err(e) => log_warn!(
                "failed to bind the stratum server to {}:{} - {e}. Continuing without it.",
                args.stratum.bind_ip,
                args.stratum.port
            ),
        }
    }

    // On its own thread: the C++ holds up `NodeServer::init` for it.
    let upnp = start_upnp(&args, node.listen_addr());

    let shutdown = daemon::install_signal_handlers();

    // The interactive console. It reads the chain, the pool and the peer lists
    // through `rpc_node` — the same `NodeApi` the RPC server was just handed —
    // so a command and an HTTP call can never report different numbers. What
    // no RPC route exposes (the per-connection rows and the ban table) comes
    // from the engine: `view` is republished once per tick below, and the ban
    // table is shared outright so `ban add` takes effect on the next accept.
    let view: Arc<Mutex<NodeView>> = Arc::new(Mutex::new(NodeView::default()));
    // `snapshot_export` walks the same shared chain on a thread of its own.
    let exporter = Arc::new(wrkz_node::snapshot::Exporter::new(Arc::clone(&chain), args.cfg.data_dir.clone()));
    let state_dir = args.cfg.data_dir.join("state");
    // One compaction state for the boot pass, the scheduler and `compact_db`,
    // when there is a database on disk to compact. It reads the height
    // through `rpc_node`, as `/height` does.
    let compaction = compactor.map(|engine| {
        let api = Arc::clone(&rpc_node);
        Compaction::new(engine, &state_dir, Box::new(move || wrkz_rpc::NodeApi::height(api.as_ref()).height))
    });
    let mut console = Console::new(
        Arc::clone(&rpc_node) as Arc<dyn wrkz_rpc::NodeApi>,
        {
            let view = Arc::clone(&view);
            Box::new(move || view.lock().unwrap_or_else(|p| p.into_inner()).clone())
        },
        node.ban_list(),
        shutdown.clone(),
        ConsoleConfig {
            version: version_line(),
            data_dir: args.cfg.data_dir.clone(),
            engine,
            out_peers: args.cfg.max_outgoing,
            in_peers: args.cfg.max_incoming,
            tuning: args.cfg.tuning,
            // The engine options mean something only to an engine that has a
            // database, which is exactly when there is a compaction state.
            db: compaction.is_some().then(|| args.db.clone()),
        },
    )
    .with_snapshot_export(Arc::clone(&exporter) as Arc<dyn wrkz_node::snapshot::SnapshotExport>);
    if let Some(compaction) = &compaction {
        console = console.with_compaction(Arc::clone(compaction));
        // `Daemon.cpp:1177`: once the console can reach it and before the
        // console reads a command. Neither the boot pass nor the scheduler
        // waits for the node to sync, as neither does in the C++.
        compaction.boot(args.skip_boot_compaction);
        let heights = {
            let api = Arc::clone(&rpc_node);
            Box::new(move || {
                let h = wrkz_rpc::NodeApi::height(api.as_ref());
                (h.height, h.network_height)
            })
        };
        let free = {
            let dir = state_dir.clone();
            Box::new(move || daemon::available_bytes(&dir))
        };
        compaction.start_scheduler(args.auto_compaction, heights, free);
    }
    let console = Arc::new(console);
    // `Daemon.cpp:1166-1175`: a console attached over the IPC socket runs its
    // commands through this same console, whether or not `--no-console` keeps
    // the terminal's reader off.
    if let Some(server) = &rpc {
        let remote = Arc::clone(&console);
        server.console().install(Arc::new(move |line: &str| remote.run_remote(line)));
        if let Some(path) = server.ipc_path() {
            log_info!(
                "Console commands are available over {}: {} attach {path}",
                wrkz_rpc::ipc::describe(path),
                program_name()
            );
        }
    }
    if args.console {
        // Nothing is started when stdin is not a terminal: a daemon under
        // systemd has no operator at a keyboard and must not hold a thread on
        // a closed descriptor. Everything else runs exactly as before.
        if console::spawn_reader(Arc::clone(&console)) {
            log_info!("console: reading commands on stdin; type `help` at the prompt");
        }
    } else {
        log_info!("console disabled (--no-console)");
    }

    let mut line = StatusLine::new(STATUS_INTERVAL, node.height());
    let tick = args.cfg.tick_interval;
    let mut auto_prune = daemon::AutoPrune::new(args.auto_prune_min_gap_blocks, args.auto_prune_min_free_bytes);
    // One pass never blocks the loop for long: 10,000 keys is what the C++
    // write batch holds (`DatabaseBlockchainCache.cpp:3061`), and the resume
    // point means the next pass starts where this one stopped.
    const PRUNE_BATCH: u32 = 10_000;

    while !node.should_stop() && !shutdown.triggered() {
        node.step(tick);

        // Publish what the RPC reports, and hand the engine anything the RPC
        // accepted so it reaches the network.
        let (incoming, outgoing) = node.connection_counts();
        let height = node.height();
        let observed = node.observed_height();
        let network_height = std::cmp::max(height, observed);
        {
            let mut s = status.lock().unwrap_or_else(|p| p.into_inner());
            s.total_connections = (incoming + outgoing) as u64;
            s.outgoing_connections = outgoing as u64;
            s.white_peers = node.peer_manager().white_addresses().iter().map(|a| a.to_string()).collect();
            s.gray_peers = node.peer_manager().gray_addresses().iter().map(|a| a.to_string()).collect();
            s.seed_nodes_count = node.seed_count() as u64;
            s.observed_height = observed as u64;
            s.blockchain_height = network_height as u64;
            s.synchronized = node.is_synchronized();
            s.prune_capability_active =
                wrkz_node::sync::prune_capability_fork_active(u64::from(height), u64::from(network_height));
        }

        // The catch-up prune pass. A node that has always been pruned finds
        // nothing to do here — `push_block` already dropped each body as it
        // fell out of the window — so this costs one meta read per pass in the
        // steady state.
        if args.prune_depth.is_some() {
            if let Some(reason) = auto_prune.due(height, daemon::available_bytes(&state_dir)) {
                let removed = chain.write().unwrap_or_else(|p| p.into_inner()).prune_bodies(PRUNE_BATCH);
                match removed {
                    Ok(0) => {}
                    Ok(n) => match reason {
                        daemon::PruneReason::LowSpace(free) => {
                            log_info!("low free space ({free} bytes): pruned {n} block bodies")
                        }
                        daemon::PruneReason::Scheduled => log_info!("pruned {n} block bodies"),
                    },
                    Err(e) => log_warn!("prune pass failed: {e}"),
                }
            }
        }
        // What `print_cn` and `print_pl` need and no RPC route carries.
        {
            let mut v = view.lock().unwrap_or_else(|p| p.into_inner());
            v.connections = node.connection_rows();
            v.listen_addr = node.listen_addr();
            v.peer_id = node.peer_manager().peer_id();
            v.seed_count = node.seed_count();
        }
        // Blocks first: a mined block is the one thing here a delay can cost.
        for mined in rpc_node.take_block_relay_queue() {
            let peers = node.relay_new_block(&mined.block, &mined.transactions);
            let header = BlockTemplate::from_bytes(&mined.block)
                .and_then(|b| b.hash())
                .ok()
                .and_then(|hash| wrkz_rpc::NodeApi::block_header_by_hash(rpc_node.as_ref(), &hash).ok().flatten());
            match header {
                Some(h) => log_info!(
                    "block mined through this node at height {}, difficulty {}, hash {}: announced to {peers} peer(s)",
                    h.height,
                    h.difficulty,
                    hex::encode(h.hash)
                ),
                None => log_info!("block mined through this node: announced to {peers} peer(s)"),
            }
        }
        let relay = rpc_node.take_relay_queue();
        if !relay.is_empty() {
            node.relay_transactions(&relay);
        }

        if args.console {
            let pool_size = pool.lock().unwrap_or_else(|p| p.into_inner()).len();
            let snapshot = StatusSnapshot {
                height,
                network_height,
                incoming,
                outgoing,
                pool: pool_size,
                synced: node.is_synchronized(),
            };
            if let Some(text) = line.tick(&snapshot) {
                log_info!("{text}");
            }
        }
    }

    // `Daemon.cpp:1199`: the engine has stopped, so a command sent over the
    // socket from here on is told the console is not available.
    if let Some(server) = &rpc {
        server.console().clear();
    }
    // No more prompt: the shutdown lines below are the last thing on the
    // terminal and must not have one redrawn under them.
    wrkz_node::log::set_prompt(None);
    if shutdown.triggered() {
        log_info!("shutting down");
    }
    // No automatic compaction starts from here on. One that is running carries
    // on until the chain state has been committed below.
    if let Some(compaction) = &compaction {
        compaction.stop_scheduler();
    }
    // A running export is cancelled and waited for, so its partial file is
    // removed before the process goes.
    wrkz_node::snapshot::SnapshotExport::shutdown(exporter.as_ref());
    // The engine writes the peer state file and closes every connection; the
    // RPC server joins its workers. This is the one shutdown path: SIGINT,
    // SIGTERM and the console's `exit` all set the same flag and leave here.
    if let Some(mut server) = stratum {
        log_info!("stopping the stratum server");
        server.stop();
    }
    // The C++ leaves its mapping in place; this removes it, unless the router
    // says the port now belongs to another host.
    if let Some(mapper) = upnp {
        mapper.shutdown(UPNP_SHUTDOWN_WAIT);
    }
    node.shutdown();
    if let Some(mut server) = rpc {
        server.stop();
    }
    if let Some(notifier) = &hooks {
        log_info!("Stopping chain notifier...");
        notifier.stop();
    }
    if let Some(mut publisher) = zmq {
        publisher.stop();
    }
    // Commit whatever the store is holding back, through the same accessor the
    // console's `save` uses, so the state on disk is a resumable height.
    if let Err(e) = wrkz_rpc::NodeApi::save(rpc_node.as_ref()) {
        log_error!("could not flush the chain state: {e}");
    }
    // After the commit and not before: stopping a running compaction stops
    // every flush and compaction the engine would run until it is reopened
    // (`wrkz_node::compaction`). The marker stays and the next start resumes.
    if let Some(compaction) = &compaction {
        compaction.shutdown();
    }
    let height = node.height();
    log_info!("stopped at height {height} (top {})", hex::encode(node.top_hash()));

    // The C++ `exit(1)`s on the same conditions (the lite-height depth check).
    if node.fatal_error().is_some() {
        return Ok(ExitCode::FAILURE);
    }

    // A run that ends at genesis means nothing connected, which an acceptance
    // script must see as a failure rather than as "synced".
    if height <= 1 && !node.is_synchronized() && !shutdown.triggered() {
        log_error!("no blocks were synced; check outbound connectivity to port {P2P_DEFAULT_PORT}");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args_from(args.iter().map(|s| s.to_string())).expect("parses")
    }

    #[test]
    fn a_dumped_configuration_reads_back_to_itself() {
        let original = parse(&[
            "--data-dir",
            "/srv/wrkz",
            "--prune",
            "--prune-depth",
            "20160",
            "--p2p-bind-port",
            "27855",
            "--p2p-external-port",
            "37855",
            "--add-peer",
            "1.2.3.4:17855",
            "--add-exclusive-node",
            "5.6.7.8:17855",
            "--add-priority-node",
            "9.9.9.9:17855",
            "--seed-node",
            "seed.example:17855",
            "--rpc-bind-ip",
            "0.0.0.0",
            "--rpc-bind-port",
            "27856",
            "--rpc-bind-ipv6-address",
            "::1",
            "--daemon-mode",
            "explorer",
            "--rpc-ipc-path",
            "/run/wrkz/wrkzd.sock",
            "--rpc-ipc-mode",
            "0660",
            "--rpc-ipc-group",
            "wrkz",
            "--rpc-ipc-require-token",
            "--rpc-access-token",
            "t0k3n",
            "--rpc-max-rpm",
            "99",
            "--rpc-read-timeout",
            "7",
            "--rpc-max-body-bytes",
            "4096",
            "--sync-batch-min",
            "50",
            "--sync-batch-max",
            "900",
            "--block-sync-bytes",
            "33554432",
            "--threads",
            "3",
            "--batch-blocks",
            "10",
            "--wal",
            "--no-checkpoints",
            "--log-level",
            "debug",
            "--no-console",
            "--no-upnp",
            "--db-threads",
            "4",
            "--db-max-open-files",
            "-1",
            "--db-read-buffer-size",
            "1024",
            "--db-row-cache-percent",
            "30",
            "--db-write-buffer-size",
            "128",
            "--db-block-size",
            "16",
            "--db-enable-compression=false",
            "--db-compression-level",
            "-2",
            "--db-compression-dict-bytes",
            "65536",
            "--db-bottom-filters",
            "--skip-boot-compaction",
            "--auto-compaction-min-gap-blocks",
            "0",
            "--auto-compaction-min-free-bytes",
            "1073741824",
            "--stratum-bind-ip",
            "0.0.0.0",
            "--stratum-bind-port",
            "3333",
            "--stratum-share-difficulty",
            "5000",
            "--stratum-max-connections",
            "4",
            "--zmq-pub",
            "tcp://127.0.0.1:27857",
            "--no-zmq",
            "--block-notify",
            "notify-block %s %h",
            "--notify-during-sync",
        ]);
        assert_eq!(original.stratum.port, 3333);
        assert_eq!(original.stratum.share_difficulty, 5000);
        assert!(!original.db.compression && original.db.bottommost_filters && original.skip_boot_compaction);
        assert_eq!((original.db.max_open_files, original.db.compression_level), (-1, -2));
        assert_eq!(original.auto_compaction, AutoCompaction { min_gap_blocks: 0, min_free_bytes: 1 << 30 });
        let dumped = dump_config(&original);
        let file = wrkz_node::config_file::from_text(&dumped).expect("our own dump reads");
        assert!(file.notes.is_empty(), "every key of our dump is one we read: {:?}", file.notes);
        let reread = parse_args_from(file.args).expect("and parses");
        assert_eq!(dump_config(&reread), dumped);
    }

    #[test]
    fn the_command_line_wins_over_the_file_and_lists_add_up() {
        let file = wrkz_node::config_file::from_text(
            r#"{"data-dir": "/from/file", "rpc-bind-port": 1111, "add-peer": ["1.1.1.1:17855"]}"#,
        )
        .unwrap();
        let cli = ["--rpc-bind-port", "2222", "--add-peer", "2.2.2.2:17855"].map(String::from);
        let a = parse_args_from(file.args.into_iter().chain(cli)).unwrap();
        assert_eq!(a.cfg.data_dir, PathBuf::from("/from/file"));
        assert_eq!(a.rpc.bind, "127.0.0.1:2222");
        assert_eq!(a.add_peers, ["1.1.1.1:17855", "2.2.2.2:17855"]);
    }

    #[test]
    fn the_external_port_is_checked_as_the_cpp_checks_it() {
        assert_eq!(parse(&["--data-dir", "d", "--p2p-external-port", "443"]).cfg.external_port, 443);
        assert_eq!(parse(&["--data-dir", "d"]).cfg.external_port, 0, "the listening port by default");
        for bad in ["65536", "-1"] {
            let refused = parse_args_from(["--data-dir", "d", "--p2p-external-port", bad].map(String::from));
            assert_eq!(refused.err().as_deref(), Some("P2P External Port must be between 0 and 65,535"), "{bad}");
        }
        assert!(parse_args_from(["--data-dir", "d", "--p2p-external-port", "x"].map(String::from)).is_err());
    }

    #[test]
    fn attach_needs_no_data_directory() {
        let a = parse(&["--attach", "/run/wrkz/wrkzd.sock"]);
        assert_eq!(a.attach.as_deref(), Some("/run/wrkz/wrkzd.sock"));
        assert!(parse_args_from(["--attach"].map(String::from)).is_err(), "it needs its socket");
    }

    #[test]
    fn upnp_is_on_unless_switched_off_and_skipped_where_it_cannot_help() {
        let listen = Some("0.0.0.0:17855".parse().unwrap());
        assert!(!parse(&["--data-dir", "d"]).no_upnp, "on by default, as the C++ has it");
        assert!(start_upnp(&parse(&["--data-dir", "d", "--no-upnp"]), listen).is_none());
        assert!(start_upnp(&parse(&["--data-dir", "d", "--no-listen"]), None).is_none());
        assert!(start_upnp(&parse(&["--data-dir", "d", "--hide-my-port"]), listen).is_none());
        assert!(start_upnp(&parse(&["--data-dir", "d"]), Some("127.0.0.1:17855".parse().unwrap())).is_none());
    }

    #[test]
    fn exclusive_and_priority_nodes_are_lists_of_their_own() {
        let a = parse(&[
            "--data-dir",
            "d",
            "--add-exclusive-node",
            "1.2.3.4:17855",
            "--add-priority-node",
            "5.6.7.8",
            "--add-priority-node",
            "[2a01:4f8::1]:17855",
        ]);
        assert_eq!(a.exclusive_nodes, ["1.2.3.4:17855"]);
        assert_eq!(a.priority_nodes, ["5.6.7.8", "[2a01:4f8::1]:17855"]);
        assert!(a.cfg.use_default_seeds && a.cfg.seeds.is_empty(), "an exclusive node is not a seed");
        let nodes = pinned_nodes("--add-priority-node", &a.priority_nodes).expect("literals resolve");
        assert_eq!(nodes[0].addrs, ["5.6.7.8:17855".parse::<std::net::SocketAddr>().unwrap()]);
        let bad = pinned_nodes("--add-exclusive-node", &["1.2.3.4:99999".to_string()]).unwrap_err();
        assert!(bad.starts_with("--add-exclusive-node 1.2.3.4:99999: "), "{bad}");
    }

    #[test]
    fn the_config_file_is_found_before_anything_is_parsed() {
        let cli = ["--data-dir", "d", "-c", "x.json"].map(String::from);
        assert_eq!(config_file_of(&cli).unwrap(), Some(PathBuf::from("x.json")));
        assert_eq!(config_file_of(&["--data-dir".to_string(), "d".to_string()]).unwrap(), None);
        assert!(config_file_of(&["--config-file".to_string()]).is_err());
    }

    #[test]
    fn the_cpp_floors_apply_and_the_batch_ceiling_follows_the_floor() {
        let a = parse(&[
            "--data-dir",
            "d",
            "--rpc-read-timeout",
            "0",
            "--rpc-max-body-bytes",
            "5",
            "--sync-batch-min",
            "700",
            "--block-sync-bytes",
            "10",
            "--transaction-validation-threads",
            "0",
        ]);
        assert_eq!(a.rpc.read_timeout, Duration::from_secs(1));
        assert_eq!(a.rpc.limits.max_body, 1024);
        assert_eq!(a.cfg.tuning.batch_max, 700, "raised to the minimum, as the C++ does");
        assert_eq!(a.cfg.tuning.block_sync_bytes, 2 * 1024 * 1024);
        assert_eq!(a.validate_threads, default_validate_threads(), "0 keeps the default");
        assert!(parse_args_from(["--data-dir", "d", "--daemon-mode", "miner"].map(String::from)).is_err());
    }

    #[test]
    fn removed_options_say_what_happens_to_them() {
        let refused = parse_args_from(["--data-dir", "d", "--enable-blockexplorer"].map(String::from));
        let e = refused.err().expect("refused rather than ignored");
        assert!(e.contains("--daemon-mode explorer"), "and names the replacement: {e}");
        let a = parse(&["--data-dir", "d", "--fee-address", "WrkzX", "--fee-amount", "100"]);
        assert_eq!(a.ignored.len(), 2, "accepted, ignored, and said so at start-up");
        let explorer = parse(&["--data-dir", "d", "--daemon-mode", "explorer"]);
        assert_eq!(explorer.rpc.mode, RpcMode::Explorer);
    }

    #[test]
    fn maintenance_options_parse_as_the_cpp_parses_them() {
        let a = parse(&["--data-dir", "d"]);
        assert!(!a.resync && !a.import_chain && !a.export_chain && !a.import_validate);
        assert_eq!(a.dump_file, PathBuf::from("blockchain.dump"), "relative, so the current directory");
        assert_eq!((a.rewind_to_height, a.max_export_blocks), (None, None));

        let a = parse(&[
            "--data-dir",
            "d",
            "--resync",
            "--rewind-to-height",
            "4200000",
            "--export-blockchain",
            "--import-blockchain",
            "--dump-file",
            "chain.dump",
            "--max-export-blocks",
            "5000",
            "--import-validate",
        ]);
        assert!(a.resync && a.import_chain && a.export_chain && a.import_validate);
        assert_eq!((a.rewind_to_height, a.max_export_blocks), (Some(4_200_000), Some(5000)));
        assert_eq!(a.dump_file, PathBuf::from("chain.dump"));

        let refused = |args: &[&str]| parse_args_from(args.iter().map(|s| s.to_string())).err().expect("refused");
        assert!(refused(&["--data-dir", "d", "--rewind-to-height", "0"]).contains("use the `--resync` option"));
        assert_eq!(refused(&["--data-dir", "d", "--max-export-blocks", "0"]), "`--max-export-blocks` can not be 0.");
        assert!(refused(&["--data-dir", "d", "--rewind-to-height", "4294967296"]).contains("not a block height"));

        // One-off actions are not configuration: the C++ writes none of them
        // into a dump, and a file read at every start must not carry one.
        let dumped = dump_config(&a);
        for key in ["resync", "rewind", "import-blockchain", "export-blockchain", "dump-file", "max-export"] {
            assert!(!dumped.contains(key), "{key} in {dumped}");
        }
    }

    fn mainnet_blocks_1_to_5() -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/mainnet_rawblocks_0_to_5.json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        v["items"].as_array().unwrap()[1..]
            .iter()
            .map(|item| {
                let txs =
                    item["transactions"].as_array().unwrap().iter().map(|t| hex::decode(t.as_str().unwrap()).unwrap());
                (hex::decode(item["block"].as_str().unwrap()).unwrap(), txs.collect())
            })
            .collect()
    }

    fn genesis_chain() -> ChainState<wrkz_storage::MemStore> {
        let mut chain =
            ChainState::open_or_genesis(wrkz_storage::MemStore::default(), Config::default(), Checkpoints::mainnet())
                .unwrap();
        chain.set_clock(Some(1_900_000_000));
        chain
    }

    /// The wiring, on a chain in memory: an import of real blocks exits 0 at
    /// their height, a rewind carries on at the height asked for, and each
    /// failure exits 1 having changed nothing it should not.
    #[test]
    fn maintenance_imports_exports_and_rewinds_in_the_cpp_order() {
        let dir = std::env::temp_dir().join(format!("wrkz-maintenance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dump = dir.join("five.dump");
        let mut bytes = Vec::new();
        for (height, (block, txs)) in mainnet_blocks_1_to_5().iter().enumerate() {
            wrkz_chain::dump::write_record(&mut bytes, height as u64 + 1, block, txs).unwrap();
        }
        std::fs::write(&dump, &bytes).unwrap();
        let args = |extra: &[&str]| {
            let mut all = vec!["--data-dir", "d", "--dump-file", dump.to_str().unwrap()];
            all.extend_from_slice(extra);
            parse(&all)
        };

        // Nothing asked for: the node goes on.
        let mut chain = genesis_chain();
        assert_eq!(maintenance(&args(&[]), &mut chain), None);

        // An import wins over an export given with it, and exits.
        let import = args(&["--import-blockchain", "--export-blockchain", "--import-validate"]);
        assert_eq!(maintenance(&import, &mut chain), Some(ExitCode::SUCCESS));
        assert_eq!(chain.tip_index(), Some(5));

        // A rewind carries on, at the height asked for.
        assert_eq!(maintenance(&args(&["--rewind-to-height", "3"]), &mut chain), None);
        assert_eq!(chain.tip_index(), Some(2));
        assert_eq!(maintenance(&args(&["--rewind-to-height", "40"]), &mut chain), None, "already below");
        // The import resumes from there.
        assert_eq!(maintenance(&import, &mut chain), Some(ExitCode::SUCCESS));
        assert_eq!(chain.tip_index(), Some(5));

        // A rewind below a lite height stops the daemon.
        let mut lite = genesis_chain();
        for (block, txs) in mainnet_blocks_1_to_5() {
            lite.add_block(&block, &txs).unwrap();
        }
        lite.declare_lite_height(4).unwrap();
        assert_eq!(maintenance(&args(&["--rewind-to-height", "3"]), &mut lite), Some(ExitCode::FAILURE));
        assert_eq!(lite.tip_index(), Some(5));

        // An export refuses a chain this short, and leaves no file behind.
        let out = dir.join("out.dump");
        let export = parse(&["--data-dir", "d", "--export-blockchain", "--dump-file", out.to_str().unwrap()]);
        assert_eq!(maintenance(&export, &mut chain), Some(ExitCode::FAILURE));
        assert!(!out.exists());
        // And an existing file is never overwritten.
        let export_onto_dump = args(&["--export-blockchain"]);
        assert_eq!(maintenance(&export_onto_dump, &mut chain), Some(ExitCode::FAILURE));
        assert_eq!(std::fs::read(&dump).unwrap(), bytes);

        // An import of a file that is not there, or is empty, exits 1.
        let missing = parse(&["--data-dir", "d", "--import-blockchain", "--dump-file", out.to_str().unwrap()]);
        assert_eq!(maintenance(&missing, &mut genesis_chain()), Some(ExitCode::FAILURE));
        std::fs::write(&out, b"").unwrap();
        assert_eq!(maintenance(&missing, &mut genesis_chain()), Some(ExitCode::FAILURE));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_database_options_parse_as_the_cpp_parses_them() {
        let d = parse(&["--data-dir", "d"]);
        assert_eq!(d.db, DbConfig::default(), "the C++ daemon's defaults");
        assert!(!d.skip_boot_compaction);
        assert_eq!(d.auto_compaction, AutoCompaction { min_gap_blocks: 720, min_free_bytes: 8 * 1024 * 1024 * 1024 });

        // The attached boolean spelling, both ways, and the last one wins.
        let off = parse(&["--data-dir", "d", "--db-enable-compression=false", "--db-bottom-filters=TRUE"]);
        assert!(!off.db.compression);
        assert!(off.db.bottommost_filters);
        let on = parse(&[
            "--data-dir",
            "d",
            "--db-enable-compression=0",
            "--db-enable-compression",
            "--skip-boot-compaction=yes",
        ]);
        assert!(on.db.compression);
        assert!(on.skip_boot_compaction);
        let refused = parse_args_from(["--data-dir", "d", "--db-bottom-filters=maybe"].map(String::from));
        assert!(refused.err().is_some_and(|e| e.contains("expected true or false")));
        let longer = parse_args_from(["--data-dir", "d", "--db-enable-compressionx"].map(String::from));
        assert!(longer.err().is_some_and(|e| e.contains("unknown argument")), "not a prefix match");

        let a = parse(&[
            "--data-dir",
            "d",
            "--db-max-open-files",
            "-1",
            "--db-compression-level",
            "19",
            "--db-row-cache-percent",
            "0",
            "--db-block-size",
            "0",
        ]);
        assert_eq!(a.db.max_open_files, -1);
        assert_eq!(a.db.compression_level, 19);
        assert_eq!(a.db.row_cache_percent, Some(0));
        assert_eq!(a.db.block_size_bytes(), 1024, "a zero block size is one kilobyte, as Daemon.cpp:630 has it");
        for [flag, v] in [["--db-threads", "0"], ["--db-write-buffer-size", "0"], ["--db-block-size", "-4"]] {
            assert!(parse_args_from(["--data-dir", "d", flag, v].map(String::from)).is_err(), "{flag} {v}");
        }
    }

    #[test]
    fn a_snapshot_import_needs_the_lite_height_it_was_made_at() {
        let a =
            parse(&["--data-dir", "d", "--lite", "--lite-height", "4000000", "--import-lite-snapshot", "s.litesnap"]);
        assert_eq!(a.import_lite_snapshot, Some(PathBuf::from("s.litesnap")));
        assert_eq!(a.lite_height, Some(4_000_000));
        let e = parse_args_from(["--data-dir", "d", "--import-lite-snapshot", "s.litesnap"].map(String::from))
            .err()
            .expect("refused without --lite");
        assert!(e.starts_with("--import-lite-snapshot needs --lite and the --lite-height"), "{e}");

        // `--snapshot-info` reads a file and nothing else, so it needs no data
        // directory; `--snapshot-stats` measures one, so it does.
        let info = parse_args_from(["--snapshot-info", "s.litesnap"].map(String::from)).expect("no --data-dir needed");
        assert_eq!(info.snapshot_info, Some(PathBuf::from("s.litesnap")));
        assert!(parse(&["--data-dir", "d", "--snapshot-stats"]).snapshot_stats);
        assert!(parse_args_from(["--snapshot-stats"].map(String::from)).is_err());
    }

    #[test]
    fn ipc_options_parse_as_the_cpp_parses_them() {
        let a = parse(&["--data-dir", "d", "--rpc-ipc-path", "@wrkzd", "--rpc-ipc-mode", "660"]);
        assert_eq!(a.rpc.ipc_path, "@wrkzd");
        assert_eq!(a.rpc.ipc_mode, 0o660);
        assert!(!a.rpc.ipc_require_token);
        assert_eq!(parse(&["--data-dir", "d"]).rpc.ipc_mode, 0o600, "owner only unless widened");
        assert!(parse_args_from(["--data-dir", "d", "--rpc-ipc-mode", "0999"].map(String::from)).is_err());
    }
}
