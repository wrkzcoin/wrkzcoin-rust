// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The interactive console: `src/daemon/DaemonCommandsHandler.cpp`.
//!
//! An operator types `status` at the running daemon and gets the same table the
//! C++ `Wrkzd` prints. This module is the command set, the dispatcher and the
//! stdin reader; `src/bin/node.rs` wires it up and `docs/DAEMON.md` documents
//! it.
//!
//! # Where the numbers come from
//!
//! Every chain, pool and peer-list value a command prints is read through
//! [`wrkz_rpc::NodeApi`] — the *same* trait `wrkz-rpc`'s handlers read, over
//! the same `Arc`. `status` is `/info`, `height` is `/height`, `print_pl` is
//! `/peers`, `print_block` is the header accessor `getblockheaderbyheight`
//! uses. The console and the RPC therefore cannot disagree: there is one
//! accessor and one lock order behind both (chain first, then pool, exactly as
//! [`wrkz_rpc::ChainNode`] takes them).
//!
//! Two things `NodeApi` does not carry, because no RPC route exposes them, come
//! from the engine instead: the per-connection rows `print_cn` prints
//! ([`crate::node::ConnectionRow`], published once per tick by the daemon loop
//! from [`crate::Node::connection_rows`]) and the ban table
//! ([`crate::peers::BanList`], shared so `ban add` takes effect on the next
//! accept without a round trip through the engine).
//!
//! # Locks and the terminal
//!
//! A command builds its whole output into a `String`, taking and releasing the
//! `NodeApi` accessors' guards as it goes, and only when it has finished is a
//! single [`crate::log::console_print`] call made. Nothing is ever written to
//! the terminal with a chain or pool lock held, so an operator on a slow
//! terminal — a laggy ssh session, a scrolled-back tmux pane — cannot delay a
//! block by one microsecond.
//!
//! The corollary is that a command is a *series* of consistent reads, not one
//! consistent read. A wide `print_bc` takes the chain's read lock once per row,
//! and `status` reads `/info`'s snapshot and the engine's [`NodeView`]
//! separately. A reorganisation between two of those reads can therefore show
//! a table whose rows came from either side of it. That is the deliberate
//! trade: `NodeApi` exposes no multi-value transaction, and holding one across
//! a thousand rows would put the engine behind a terminal.
//!
//! [`crate::log::console_print`] and the logger share one mutex, so a log line
//! arriving mid-table waits for the table rather than landing inside it. See
//! `log.rs`, which also explains why the prompt redraw looks at stderr and
//! stdout separately.
//!
//! # Over the IPC socket
//!
//! [`Console::run_remote`] runs a line for `wrkz-node attach` ([`crate::attach`]),
//! behind the RPC server's `POST /console` ([`wrkz_rpc::console`]): the same
//! commands, with their output handed back instead of printed. Because a
//! command builds its output as a `String` anyway, nothing has to be diverted
//! from the terminal to capture it, and log lines written meanwhile are not
//! caught up in it. A command typed here and one sent over the socket take
//! turns, as the C++'s `m_commandMutex` makes them.
//!
//! # Differences from the C++, and why
//!
//! | C++ | here |
//! | --- | --- |
//! | `print_block` prints `storeToJson(block)` | a labelled field list: the same values, readable without a JSON pretty-printer |
//! | `print_tx` prints `storeToJson(tx)` | the same, plus the raw hex, which is what an operator pastes elsewhere |
//! | `print_pl` prints peer id and `last_seen` per row | `ip:port`, which is all `/peers` exposes and therefore all the console may read |
//! | `print_pool_sh` prints a per-transaction PoW difficulty | omitted: the pool summary the RPC exposes carries no input/output counts, and the ladder needs them |
//! | `status` prints `DB Engine: RocksDB` | whichever engine this build opened |
//! | `snapshot_export` | the same, over [`crate::snapshot::Exporter`]; a console built without one says so |
//! | `compact_db wait` waits holding the compaction lock | it does not; see [`crate::compaction`] |
//! | linenoise: history and line editing | none, and no `log_tail` there; see below |
//!
//! # No line editing, and what stands in for it
//!
//! The C++ console reads through linenoise, so it has arrow-key history and
//! in-line editing. This one reads whole lines from the terminal in the
//! terminal's own line mode: backspace and the shell's own kill-line work,
//! arrow keys do not, and there is no history. Adding history would mean
//! putting the terminal into raw mode and writing a line editor — cursor
//! movement, wrapping, a resize handler, Ctrl-C — which is a great deal of
//! `unsafe` termios and Win32 for a daemon console. It is a real gap; it is
//! listed as such rather than half-built.
//!
//! What the C++ does *not* have and this does: `log_tail`, which prints the
//! last lines the logger emitted. On a node whose stderr went somewhere the
//! operator cannot reach, that is the only way to see them.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wrkz_primitives::constants::{DIFFICULTY_TARGET, FORK_HEIGHTS, SOFTWARE_SUPPORTED_FORK_INDEX};
use wrkz_primitives::Hash;
use wrkz_rpc::api::{ApiError, BlockHeaderInfo};
use wrkz_rpc::NodeApi;

use crate::compaction::{Compaction, RunResult, Started, StatusReport, Trigger};
use crate::daemon::Shutdown;
use crate::log::{self, Level};
use crate::node::ConnectionRow;
use crate::peers::BanList;
use crate::sync::SyncTuning;

/// The most blocks `print_bc` will list in one go. The C++ has no such command
/// in this fork; the cap is here because a console must never be a way to make
/// the daemon allocate a hundred megabytes of table.
pub const MAX_PRINT_BC_RANGE: u64 = 1000;

/// The default `ban add` duration, `P2P_IP_BLOCKTIME` as the C++ `ban` command
/// uses it (`DaemonCommandsHandler.cpp:1725`).
pub const DEFAULT_BAN_SECONDS: u64 = 900;

/// What the engine knows and no RPC route reports.
///
/// The daemon loop publishes one of these per tick, from the engine's own
/// accessors, exactly as it publishes [`wrkz_rpc::P2pSnapshot`] for `/info`.
#[derive(Clone, Debug, Default)]
pub struct NodeView {
    /// One row per live connection, for `print_cn`.
    pub connections: Vec<ConnectionRow>,
    /// The address the P2P listener actually bound, if it listens.
    pub listen_addr: Option<SocketAddr>,
    /// Our own peer id, as the handshake advertises it.
    pub peer_id: u64,
    /// How many seed addresses this node was configured with.
    pub seed_count: usize,
}

/// The configuration a command prints back, which is not state and never
/// changes while the daemon runs.
#[derive(Clone, Debug)]
pub struct ConsoleConfig {
    /// The daemon's own version line.
    pub version: String,
    /// `--data-dir`.
    pub data_dir: PathBuf,
    /// Which storage engine this build opened.
    pub engine: String,
    /// `--out-peers` / `--in-peers`.
    pub out_peers: usize,
    pub in_peers: usize,
    /// The sync tuning the engine runs with.
    pub tuning: SyncTuning,
    /// The RocksDB options the state was opened with, for `db_status`; `None`
    /// for a state kept in memory.
    pub db: Option<wrkz_storage::dbconfig::DbConfig>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            version: String::new(),
            data_dir: PathBuf::new(),
            engine: "MemStore (in memory)".to_string(),
            out_peers: 0,
            in_peers: 0,
            tuning: SyncTuning::default(),
            db: None,
        }
    }
}

/// What one command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Everything the command printed. Never empty for a command that ran; an
    /// empty line produces `Outcome::silent`.
    pub output: String,
    /// `exit`, `quit` and `stop`: the daemon must shut down.
    pub exit: bool,
    /// False for a word this console has no command for.
    recognised: bool,
}

impl Outcome {
    fn text(output: impl Into<String>) -> Self {
        Self { output: output.into(), exit: false, recognised: true }
    }

    /// A blank input line: nothing happened and nothing is printed.
    fn silent() -> Self {
        Self { output: String::new(), exit: false, recognised: true }
    }

    /// A word that is no command.
    fn unrecognised(output: impl Into<String>) -> Self {
        Self { output: output.into(), exit: false, recognised: false }
    }

    /// Whether the line named a command this console has. A blank line
    /// counts: there was nothing to fail to recognise.
    pub fn recognised(&self) -> bool {
        self.recognised
    }
}

/// Every command, with the one-line usage `help` prints. The order is the order
/// `help` lists them in, grouped as the C++ registers them.
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "Show this help"),
    ("?", "Show this help"),
    ("exit", "Shutdown the daemon"),
    ("quit", "Shutdown the daemon"),
    ("stop", "Shutdown the daemon"),
    ("status", "Show daemon status"),
    ("height", "Print the local and network height"),
    ("print_pl", "Print peer list (white and gray, ip:port)"),
    ("print_cn", "Print connections"),
    ("print_bc", "print_bc <begin> [end] - Print block headers over a height range (max 1000)"),
    ("print_block", "print_block <block_hash> | <block_height> - Print one block, as fields rather than JSON"),
    ("print_tx", "print_tx <transaction_hash> - Print one transaction, as fields and hex rather than JSON"),
    ("print_pool", "Print transaction pool (long format)"),
    ("print_pool_sh", "Print transaction pool (short format)"),
    ("set_log", "set_log <level> - Change current log level, <level> is 0-4 or a name (error..trace)"),
    ("log_tail", "log_tail [count] - Print the last log lines this daemon emitted (default 20, max 200)"),
    ("sync_info", "Show compact synchronization information"),
    ("sync_peers", "Show current sync peer diagnostics"),
    ("sync_tune", "Show current sync tuning and adaptive sync stats"),
    ("prune_status", "Show prune mode and capability status"),
    ("db_status", "Show on-disk DB status for the active DB engine"),
    ("compact_db", "Manage DB compaction: compact_db [start|status|wait|force]"),
    ("save", "Force-save blockchain state to disk"),
    ("snapshot_export", "Export a lite node snapshot: snapshot_export [start [height] [path] | status | cancel]"),
    ("ban", "Manage in-memory host bans: ban list | ban add <ip> [seconds] | ban delete <ip>"),
];

/// The daemon console.
///
/// Built once by the daemon and shared with the stdin reader thread. Every
/// method is `&self`: the console owns nothing mutable of its own.
pub struct Console {
    api: Arc<dyn NodeApi>,
    view: Box<dyn Fn() -> NodeView + Send + Sync>,
    bans: BanList,
    shutdown: Shutdown,
    cfg: ConsoleConfig,
    /// What `compact_db` drives; `None` when there is no database on disk.
    compaction: Option<Arc<Compaction>>,
    /// Held while a command runs, so one typed at the terminal and one sent
    /// over the IPC socket take turns (`m_commandMutex`,
    /// `DaemonCommandsHandler.cpp:265`).
    commands: Mutex<()>,
    /// `snapshot_export`, when the daemon built this console over its chain.
    snapshot: Option<Arc<dyn crate::snapshot::SnapshotExport>>,
}

impl Console {
    pub fn new(
        api: Arc<dyn NodeApi>,
        view: Box<dyn Fn() -> NodeView + Send + Sync>,
        bans: BanList,
        shutdown: Shutdown,
        cfg: ConsoleConfig,
    ) -> Self {
        Self { api, view, bans, shutdown, cfg, compaction: None, commands: Mutex::new(()), snapshot: None }
    }

    /// Give `compact_db` and `db_status` the daemon's compaction state. The
    /// same one the boot pass and the scheduler use, so the console reports
    /// and waits for those too.
    pub fn with_compaction(mut self, compaction: Arc<Compaction>) -> Self {
        self.compaction = Some(compaction);
        self
    }

    /// Give the console the `snapshot_export` command. The daemon does, over
    /// its shared chain; a console without it answers that command with why.
    pub fn with_snapshot_export(mut self, exporter: Arc<dyn crate::snapshot::SnapshotExport>) -> Self {
        self.snapshot = Some(exporter);
        self
    }

    /// A console with no P2P engine behind it: `print_cn` reports no
    /// connections and `ban` has a table of its own. Used by the tests that
    /// drive the dispatcher directly.
    pub fn standalone(api: Arc<dyn NodeApi>, cfg: ConsoleConfig) -> Self {
        Self::new(api, Box::new(NodeView::default), BanList::new(), Shutdown::new(), cfg)
    }

    /// Run one line the operator typed.
    ///
    /// Never panics and never returns an error: an unknown command, a bad
    /// argument and a chain that could not be read all come back as text the
    /// operator reads. Every command the C++ registers is here;
    /// `snapshot_export` and `compact_db` answer with what they are missing on
    /// a console built without [`Console::with_snapshot_export`] or
    /// [`Console::with_compaction`].
    ///
    /// `compact_db wait` is the one command that blocks: it returns when the
    /// running compaction does, as the C++'s does.
    pub fn run_line(&self, line: &str) -> Outcome {
        let mut tokens = line.split_whitespace();
        let Some(command) = tokens.next() else {
            return Outcome::silent();
        };
        let args: Vec<&str> = tokens.collect();
        // A command that panicked has left nothing half-done behind this lock.
        let _turn = self.commands.lock().unwrap_or_else(|p| p.into_inner());
        self.dispatch(command, &args)
    }

    /// `DaemonCommandsHandler::run_remote_command`
    /// (`DaemonCommandsHandler.cpp:290-322`): one line from a console attached
    /// over the IPC socket, with what the command printed returned instead of
    /// written to the terminal.
    ///
    /// As in the C++: a blank line is `""`, a word that is no command is
    /// `Unknown command: <word>\n`, and every command is allowed — `stop`, `exit`
    /// and `quit` too, which shut the daemon down through the flag SIGINT sets.
    /// Where the C++ catches a command's exception, a command that panics here
    /// is `Command failed: <why>\n`. The output ends with a newline, as the
    /// C++'s `std::endl`-terminated output does.
    pub fn run_remote(&self, line: &str) -> String {
        let Some(command) = line.split_whitespace().next() else {
            return String::new();
        };
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_line(line)));
        match ran {
            Ok(outcome) if !outcome.recognised() => format!("Unknown command: {command}\n"),
            Ok(outcome) if outcome.output.is_empty() || outcome.output.ends_with('\n') => outcome.output,
            Ok(outcome) => outcome.output + "\n",
            Err(panic) => {
                let why = match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
                    (Some(text), _) => text.to_string(),
                    (None, Some(text)) => text.clone(),
                    (None, None) => "the command panicked".to_string(),
                };
                format!("Command failed: {why}\n")
            }
        }
    }

    fn dispatch(&self, command: &str, args: &[&str]) -> Outcome {
        match command {
            "help" | "?" => Outcome::text(self.help()),
            "exit" | "quit" | "stop" => self.exit(),
            "status" => Outcome::text(self.status()),
            "height" => Outcome::text(self.height()),
            "print_pl" => Outcome::text(self.print_pl()),
            "print_cn" => Outcome::text(self.print_cn()),
            "print_bc" => Outcome::text(self.print_bc(args)),
            "print_block" => Outcome::text(self.print_block(args)),
            "print_tx" => Outcome::text(self.print_tx(args)),
            "print_pool" => Outcome::text(self.print_pool(true)),
            "print_pool_sh" => Outcome::text(self.print_pool(false)),
            "set_log" => Outcome::text(set_log(args)),
            "log_tail" => Outcome::text(log_tail(args)),
            "sync_info" => Outcome::text(self.sync_info()),
            "sync_peers" => Outcome::text(self.sync_peers()),
            "sync_tune" => Outcome::text(self.sync_tune()),
            "prune_status" => Outcome::text(self.prune_status()),
            "db_status" => Outcome::text(self.db_status()),
            "save" => Outcome::text(self.save()),
            "ban" => Outcome::text(self.ban(args)),
            "compact_db" => Outcome::text(self.compact_db(args)),
            "snapshot_export" => Outcome::text(match &self.snapshot {
                Some(exporter) => exporter.command(args),
                None => "snapshot_export needs the daemon's own chain state to walk, and this console was \
                         started without it."
                    .to_string(),
            }),
            other => Outcome::unrecognised(format!("unknown command `{other}`. Type `help` for the command list.")),
        }
    }

    // -- help and exit -------------------------------------------------------

    /// `DaemonCommandsHandler::get_commands_str`: the version, then the
    /// commands in a column.
    fn help(&self) -> String {
        let width = COMMANDS.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 3;
        let mut out = String::new();
        if !self.cfg.version.is_empty() {
            out.push_str(&self.cfg.version);
            out.push('\n');
        }
        out.push_str("Commands:\n");
        for (name, usage) in COMMANDS {
            out.push_str(&format!("  {name:width$}{usage}\n"));
        }
        out
    }

    /// `DaemonCommandsHandler::exit`: say so loudly, turn the log all the way
    /// up because the wait can be long, and signal the shutdown.
    ///
    /// The flag is the one the SIGINT handler sets, so `exit` and Ctrl-C leave
    /// through exactly the same code: the engine closes every connection and
    /// writes `p2pstate.wrkz.bin`, the chain state is flushed, the RPC joins
    /// its workers.
    fn exit(&self) -> Outcome {
        log::set_level(Level::Trace);
        self.shutdown.request();
        Outcome {
            output: "================= EXITING ==================\n\
                     == PLEASE WAIT, THIS MAY TAKE A LONG TIME ==\n\
                     ============================================"
                .to_string(),
            exit: true,
            recognised: true,
        }
    }

    // -- status --------------------------------------------------------------

    /// `DaemonCommandsHandler::status`, from `/info`'s own snapshot.
    fn status(&self) -> String {
        let info = match self.api.info() {
            Ok(i) => i,
            Err(e) => return busy("Problem retrieving information from the node", &e),
        };
        let view = (self.view)();

        let upgrades: Vec<u64> = FORK_HEIGHTS.to_vec();
        let supported = FORK_HEIGHTS.get(SOFTWARE_SUPPORTED_FORK_INDEX).copied().unwrap_or(0);
        let fork = fork_status(info.network_height, &upgrades, supported);

        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push(("Local Height".into(), info.height.to_string()));
        rows.push(("Network Height".into(), info.network_height.to_string()));
        rows.push(("Percentage Synced".into(), format!("{}%", sync_percentage(info.height, info.network_height))));
        rows.push(("Network Hashrate".into(), mining_speed(info.difficulty / DIFFICULTY_TARGET)));
        rows.push(("Block Version".into(), format!("v{}", info.major_version)));
        rows.push(("Incoming Connections".into(), info.incoming_connections_count.to_string()));
        rows.push(("Outgoing Connections".into(), info.outgoing_connections_count.to_string()));
        rows.push(("Uptime".into(), uptime(info.start_time)));
        rows.push(("Fork Status".into(), update_status(fork).into()));
        rows.push(("Next Fork".into(), fork_time(info.network_height, &upgrades)));
        rows.push(("Transaction Pool Size".into(), info.tx_pool_size.to_string()));
        rows.push(("Alternative Block Count".into(), info.alt_blocks_count.to_string()));
        rows.push(("DB Engine".into(), self.cfg.engine.clone()));
        let policy = self.effective_policy(&info);
        rows.push(("Pruned Node".into(), yes_no(policy.is_pruned())));
        rows.push((
            "Prune Depth".into(),
            match policy.prune_depth {
                Some(depth) => depth.to_string(),
                None => "not pruning".into(),
            },
        ));
        rows.push(("Prune Capability Fork Active".into(), yes_no(info.prune_capability_active)));
        rows.push(("Lite Node".into(), yes_no(policy.is_lite())));
        if policy.is_lite() {
            rows.push(("Lite Height (permanent)".into(), policy.lite_start_height.to_string()));
            if info.height < policy.lite_start_height {
                rows.push(("Lite Sync Stage".into(), "Index only (below lite height)".into()));
            }
        }
        if policy.floor != 0 {
            rows.push(("Serves Block Data From".into(), policy.floor.to_string()));
        }
        rows.push(("Active Sync Peers".into(), info.sync_active_peers.to_string()));
        rows.push(("Avg Sync Batch Size".into(), info.sync_avg_batch_size.to_string()));
        rows.push(("Demoted Sync Peers".into(), info.sync_demoted_peers.to_string()));
        rows.push((
            "P2P Listening On".into(),
            view.listen_addr.map(|a| a.to_string()).unwrap_or_else(|| "not listening".into()),
        ));
        rows.push(("Peer ID".into(), format!("{:016x}", view.peer_id)));
        rows.push(("White / Gray Peers".into(), format!("{} / {}", info.white_peerlist_size, info.grey_peerlist_size)));
        // The C++ prints one version here; this port has two. `/info`'s
        // `version` is the C++ release the RPC answers as (`DAEMON_VERSION`),
        // so our own release gets a row of its own rather than being hidden.
        // The console only ever runs inside the node, so the version it was
        // compiled with is the node's.
        rows.push(("Node Version".into(), node_version()));
        rows.push(("RPC Compatible With".into(), format!("WrkzCoin {}", info.version)));

        let mut out = two_column_table(&rows);
        if fork == ForkStatus::OutOfDate {
            out.push_str(&format!(
                "\nThis node is likely forked: it supports up to height {supported}, and the \
                 network is at {}. Update it.\n",
                info.network_height
            ));
        }
        if info.lite_start_height != 0 {
            out.push_str(&format!("\n{}\n", missing_bodies_warning(info.lite_start_height)));
        }
        out
    }

    /// Ours, not the C++'s: `/height` in one line, for an operator who wants
    /// the two numbers and nothing else.
    fn height(&self) -> String {
        let h = self.api.height();
        format!("Height: {} / {} ({}%)", h.height, h.network_height, sync_percentage(h.height, h.network_height))
    }

    /// `DaemonCommandsHandler::sync_info`.
    fn sync_info(&self) -> String {
        let h = self.api.height();
        format!("Height: {} / {} ({}%)", h.height, h.network_height, sync_percentage(h.height, h.network_height))
    }

    /// `DaemonCommandsHandler::sync_peers`.
    fn sync_peers(&self) -> String {
        let info = match self.api.info() {
            Ok(i) => i,
            Err(e) => return busy("Problem retrieving sync peer diagnostics", &e),
        };
        let mut out = String::new();
        out.push_str(&format!("Sync Active Peers: {}\n", info.sync_active_peers));
        out.push_str(&format!("Average Sync Batch Size: {}\n", info.sync_avg_batch_size));
        out.push_str(&format!("Demoted Sync Peers (lifetime): {}", info.sync_demoted_peers));
        out
    }

    /// `DaemonCommandsHandler::sync_tune`: the live numbers, then what the
    /// daemon was configured with.
    fn sync_tune(&self) -> String {
        let info = match self.api.info() {
            Ok(i) => i,
            Err(e) => return busy("Problem retrieving sync tuning", &e),
        };
        let t = &self.cfg.tuning;
        let mut out = String::new();
        out.push_str(&format!("Active Sync Peers: {}\n", info.sync_active_peers));
        out.push_str(&format!("Average Sync Batch Size: {}\n", info.sync_avg_batch_size));
        out.push_str(&format!("Demoted Sync Peers: {}\n", info.sync_demoted_peers));
        out.push_str(&format!("Configured Sync Max Peers: {}\n", t.max_peers));
        out.push_str(&format!("Configured P2P Out/In Peers: {}/{}\n", self.cfg.out_peers, self.cfg.in_peers));
        out.push_str(&format!("Configured Sync Failure Threshold: {}\n", t.peer_failure_threshold));
        out.push_str(&format!("Configured Sync Batch Min/Max: {}/{}\n", t.batch_min, t.batch_max));
        out.push_str(&format!("Configured Block Sync Size: {}\n", t.block_sync_size));
        out.push_str(&format!("Configured Block Sync Bytes: {}", pretty_bytes(t.block_sync_bytes)));
        out
    }

    /// What this node keeps, from both places that know something about it.
    ///
    /// [`wrkz_rpc::NodeApi::body_policy`] is the configuration — `--lite`,
    /// `--prune` — and `/info`'s `lite_start_height` is the floor the daemon
    /// *probed for* at start-up, which is the only thing that knows about a
    /// state imported without block bodies. Neither alone is the whole truth,
    /// and reporting the configuration alone would tell an operator with a
    /// body-less import that their node serves the whole chain.
    fn effective_policy(&self, info: &wrkz_rpc::api::InfoSnapshot) -> wrkz_rpc::api::BodyPolicy {
        let mut p = self.api.body_policy();
        p.lite_start_height = p.lite_start_height.max(info.lite_start_height);
        p.floor = p.floor.max(p.lite_start_height);
        if p.prune_depth.is_none() && info.pruned {
            p.prune_depth = Some(info.prune_depth);
        }
        p
    }

    /// `DaemonCommandsHandler::prune_status`, extended to cover all three
    /// reduced modes rather than prune alone: an operator asking what this node
    /// keeps needs one place that answers.
    ///
    /// Every number is the node's own configuration and tip
    /// ([`wrkz_rpc::api::BodyPolicy`]), not a default: "Serves Block Data From"
    /// is the height below which every body request is refused, which is the
    /// higher of the lite line and the prune window's floor.
    fn prune_status(&self) -> String {
        let info = match self.api.info() {
            Ok(i) => i,
            Err(e) => return busy("Problem retrieving prune status", &e),
        };
        let policy = self.effective_policy(&info);
        let mut out = String::new();
        out.push_str(&format!("Pruned Node: {}\n", yes_no(policy.is_pruned())));
        if let Some(depth) = policy.prune_depth {
            out.push_str(&format!("Prune Depth: {depth}\n"));
            out.push_str(&format!(
                "Prune Floor Height: {} (of a tip at {})\n",
                info.height.saturating_sub(1).saturating_sub(depth).max(policy.floor),
                info.height.saturating_sub(1)
            ));
        } else {
            out.push_str("Prune Depth: not pruning; every block body is kept\n");
        }
        out.push_str(&format!("Lite Node: {}\n", yes_no(policy.is_lite())));
        if policy.is_lite() {
            out.push_str(&format!("Lite Height: {} (permanent for this database)\n", policy.lite_start_height));
        }
        out.push_str(&format!(
            "Serves Block Data From: {}\n",
            if policy.floor == 0 { "0 (the whole chain)".to_string() } else { policy.floor.to_string() }
        ));
        if policy.transactions_from != 0 {
            out.push_str(&format!(
                "Transaction Records From: {} (imported from a lite snapshot; none below)\n",
                policy.transactions_from
            ));
        }
        if policy.floor != 0 {
            out.push_str(
                "Below that height this node holds every consensus record and no block bodies.\n\
                 It validates and follows the chain exactly as a full node does, and \
                 reports an error rather than an answer for a block, a wallet scan or an \
                 explorer lookup there.\n",
            );
        }
        out.push_str(&format!("Prune Capability Fork Active: {}", yes_no(info.prune_capability_active)));
        out
    }

    /// `DaemonCommandsHandler::db_status`, for whichever engine this build
    /// opened. There is no on-disk directory to walk when the state is in
    /// memory, and the command says so instead of printing a zero-byte total.
    ///
    /// On RocksDB it also prints the C++'s compression lines, how the read
    /// cache is split, and the compaction state: this daemon's own, as
    /// `compact_db status` reports it, and RocksDB's counters.
    fn db_status(&self) -> String {
        let mut lines: Vec<String> = vec![format!("DB Engine: {}", self.cfg.engine)];
        if let Some(db) = &self.cfg.db {
            lines.push(format!("Compression Enabled: {}", yes_no(db.compression)));
            lines.push(format!(
                "Compression Mode: {}",
                if db.compression { "RocksDB ZSTD (L2+; L0/L1 uncompressed)" } else { "Disabled" }
            ));
        }
        lines.push(format!("Data Directory: {}", self.cfg.data_dir.display()));
        let state = self.cfg.data_dir.join("state");
        match dir_stats(&state) {
            Some(stats) => {
                lines.push(format!("DB Path: {}", state.display()));
                lines.push(format!("DB Size: {}", pretty_bytes(stats.bytes)));
                lines.push(format!("Files: {}", stats.files));
                lines.push(format!("Directories: {}", stats.directories));
                if stats.extensions.is_empty() {
                    lines.push("No DB files found in the selected path.".to_string());
                } else {
                    lines.push("File type counts:".to_string());
                    lines.extend(stats.extensions.iter().map(|(ext, count)| format!("  {ext}: {count}")));
                }
            }
            None => lines.push(
                "DB Path: none — this build keeps the chain state in memory. \
                 Build with --features rocksdb to persist it."
                    .to_string(),
            ),
        }
        if let Some(db) = &self.cfg.db {
            lines.push(format!(
                "Read Cache: {} ({} row cache, {} block cache), {} KiB blocks, bottommost filters {}",
                pretty_bytes(db.read_cache_bytes()),
                pretty_bytes(db.row_cache_bytes()),
                pretty_bytes(db.block_cache_bytes()),
                db.block_size_bytes() / 1024,
                if db.bottommost_filters { "on" } else { "off" }
            ));
        }
        if let Some(compaction) = &self.compaction {
            lines.extend(compaction_status(&compaction.status()).lines().map(str::to_string));
            let engine = compaction.engine_stats();
            let known = |value: Option<String>| value.unwrap_or_else(|| "unknown".to_string());
            let rows = [
                ("RocksDB Compaction Pending", known(engine.compaction_pending.map(yes_no))),
                ("RocksDB Running Compactions", known(engine.running_compactions.map(|n| n.to_string()))),
                ("RocksDB Pending Compaction Bytes", known(engine.pending_compaction_bytes.map(pretty_bytes))),
                ("RocksDB Live SST Size", known(engine.live_sst_bytes.map(pretty_bytes))),
                ("RocksDB Background Errors", known(engine.background_errors.map(|n| n.to_string()))),
            ];
            lines.extend(rows.into_iter().map(|(label, value)| format!("{label}: {value}")));
        }
        // What the database *contains*, which is the half of "db status" that
        // decides whether it can answer anything: a lite or pruned database is
        // a third the size of a full one and the size alone will not say why.
        let policy = match self.api.info() {
            Ok(info) => self.effective_policy(&info),
            Err(_) => self.api.body_policy(),
        };
        lines.push(format!(
            "Block Bodies: {}",
            match (policy.is_lite(), policy.prune_depth) {
                (true, _) => format!("from height {} up (lite, permanent)", policy.lite_start_height),
                (false, Some(depth)) => format!("the most recent {depth} blocks (pruned)"),
                (false, None) => "every block (full)".to_string(),
            }
        ));
        if policy.transactions_from != 0 {
            lines.push(format!(
                "Transaction Records: from height {} up (imported from a lite snapshot)",
                policy.transactions_from
            ));
        }
        lines.join("\n")
    }

    /// `DaemonCommandsHandler::compact_db`: `start` (the default), `force`,
    /// `status` and `wait`.
    ///
    /// `force` also rewrites the bottommost level. After the first full
    /// compaction that level holds nearly the whole database, so an ordinary
    /// pass leaves it alone; a forced one is slow and wants free space about the
    /// size of the database, and it is how changed compression and block
    /// settings reach data already written.
    fn compact_db(&self, args: &[&str]) -> String {
        const USAGE: &str = "Usage: compact_db [start|status|wait|force]";
        const FOLLOW_UP: &str = "Use `compact_db status` or `compact_db wait`.";
        let sub = args.first().copied().unwrap_or("start");
        if !matches!(sub, "start" | "status" | "wait" | "force") {
            return USAGE.to_string();
        }
        let Some(compaction) = &self.compaction else {
            return "compact_db needs the RocksDB engine: this build keeps the chain state in memory, so there \
                    is no database on disk to compact. Build with --features rocksdb."
                .to_string();
        };
        match sub {
            "status" => compaction_status(&compaction.status()),
            "wait" => match compaction.wait() {
                None => "No DB compaction is running.".to_string(),
                Some(result) => {
                    let end = match result {
                        RunResult::Completed => "DB compaction completed.".to_string(),
                        RunResult::Stopped => {
                            "DB compaction stopped: the daemon is shutting down. It resumes on the next start."
                                .to_string()
                        }
                        RunResult::Failed(e) => format!("DB compaction failed: {e}"),
                    };
                    format!("Waiting for DB compaction to complete...\n{end}")
                }
            },
            _ => match compaction.start(Trigger::Manual { rewrite_bottommost: sub == "force" }) {
                Started::Started { .. } => format!("DB compaction started in background. {FOLLOW_UP}"),
                Started::AlreadyRunning => format!("DB compaction is already running. {FOLLOW_UP}"),
                Started::ShuttingDown => "The daemon is shutting down; no DB compaction will be started.".to_string(),
                Started::Failed(e) => format!("DB compaction could not be started: {e}"),
            },
        }
    }

    /// `DaemonCommandsHandler::save`.
    fn save(&self) -> String {
        match self.api.save() {
            Ok(()) => "Core state saved.".to_string(),
            Err(e) => format!("Could not save the chain state: {e}"),
        }
    }

    // -- peers ---------------------------------------------------------------

    /// `DaemonCommandsHandler::print_pl`, from `/peers`.
    ///
    /// The C++ prints a peer id and a `last_seen` age per row; `/peers` carries
    /// neither, and reading them would mean the console looking at state the
    /// RPC cannot see. Addresses it is.
    fn print_pl(&self) -> String {
        let lists = self.api.peers();
        let mut out = String::new();
        out.push_str(&format!("Peerlist white ({}):\n", lists.white.len()));
        for addr in &lists.white {
            out.push_str(&format!("  {addr}\n"));
        }
        out.push_str(&format!("Peerlist gray ({}):\n", lists.gray.len()));
        for addr in &lists.gray {
            out.push_str(&format!("  {addr}\n"));
        }
        out.push_str(&format!("Seeds configured: {}", (self.view)().seed_count));
        out
    }

    /// `CryptoNoteProtocolHandler::connections_to_string`, the same nine
    /// columns in the same order.
    fn print_cn(&self) -> String {
        let view = (self.view)();
        let mut out = String::from("Connections:\n");
        if view.connections.is_empty() {
            out.push_str("  none");
            return out;
        }
        let header =
            ["Dir", "Remote", "Peer ID", "State", "Uptime", "Height", "Pruned", "Batch", "Fail"].map(String::from);
        let mut rows: Vec<[String; 9]> = vec![header];
        for c in &view.connections {
            rows.push([
                if c.incoming { "IN".into() } else { "OUT".into() },
                c.addr.to_string(),
                format!("{:016x}", c.peer_id),
                c.state.name().to_string(),
                time_interval(c.uptime),
                c.remote_height.to_string(),
                yes_no_lower(c.remote_is_pruned),
                c.sync_batch_size.to_string(),
                c.sync_failures.to_string(),
            ]);
        }
        out.push_str(&grid(&rows));
        out
    }

    /// `DaemonCommandsHandler::ban`.
    fn ban(&self, args: &[&str]) -> String {
        const USAGE: &str = "Usage: ban list | ban add <ip> [seconds] | ban delete <ip>";
        match args {
            ["list"] => {
                let entries = self.bans.entries();
                if entries.is_empty() {
                    return "Ban list is empty.".to_string();
                }
                let mut out = String::from("Banned hosts:\n");
                for (ip, remaining) in entries {
                    out.push_str(&format!("  {ip} ({remaining}s remaining)\n"));
                }
                out.pop();
                out
            }
            ["add", ip] | ["add", ip, _] => {
                let seconds = match args.get(2) {
                    None => DEFAULT_BAN_SECONDS,
                    Some(s) => match s.parse::<u64>() {
                        Ok(0) => return "Ban seconds must be greater than zero.".to_string(),
                        Ok(n) => n,
                        Err(_) => return format!("Invalid ban seconds value `{s}`."),
                    },
                };
                match parse_ip(ip) {
                    Some(ip) => {
                        self.bans.ban(ip, seconds);
                        format!("Ban added for {ip} ({seconds}s)")
                    }
                    None => format!("Invalid IP address: {ip}"),
                }
            }
            ["delete", ip] => match parse_ip(ip) {
                Some(ip) => {
                    if self.bans.unban(ip) {
                        format!("Ban removed for {ip}")
                    } else {
                        "IP not found in ban list.".to_string()
                    }
                }
                None => format!("Invalid IP address: {ip}"),
            },
            _ => USAGE.to_string(),
        }
    }

    // -- blocks and transactions ---------------------------------------------

    /// Ours: a header table over `[begin, end]`, capped at
    /// [`MAX_PRINT_BC_RANGE`] rows.
    fn print_bc(&self, args: &[&str]) -> String {
        let (begin, end) = match args {
            [] => return "expected: print_bc <begin_height> [end_height]".to_string(),
            [begin] => match parse_index(begin) {
                Ok(b) => (b, b),
                Err(e) => return e,
            },
            [begin, end] => match (parse_index(begin), parse_index(end)) {
                (Ok(b), Ok(e)) => (b, e),
                (Err(e), _) | (_, Err(e)) => return e,
            },
            _ => return "expected: print_bc <begin_height> [end_height]".to_string(),
        };
        if end < begin {
            return format!("the range is inverted: {begin} is above {end}. Use print_bc <begin> <end>.");
        }
        // The span is checked before a single row is allocated, so an operator
        // typing `print_bc 0 4000000` gets a message rather than a daemon that
        // allocates for a minute.
        //
        // The *span* and not the count: `end - begin` cannot overflow because
        // the inversion was refused above, but `end - begin + 1` can — with
        // `print_bc 0 18446744073709551615` it is `u64::MAX + 1`, which panics
        // a debug build. The count is only ever formed as a `u128`, for the
        // message.
        let span = end - begin;
        if span >= MAX_PRINT_BC_RANGE {
            let count = u128::from(span) + 1;
            return format!(
                "that range is {count} blocks; print_bc prints at most {MAX_PRINT_BC_RANGE} at a \
                 time. Ask for a smaller range."
            );
        }
        let top = self.api.top_index();
        if begin > top {
            return format!("block wasn't found. Current block chain height: {}, requested: {begin}", top + 1);
        }

        let header = ["Height", "Hash", "Time", "Version", "Difficulty", "Size", "Txs"].map(String::from);
        let mut rows: Vec<[String; 7]> = vec![header];
        for index in begin..=end.min(top) {
            match self.api.block_header_by_index(index) {
                Ok(Some(h)) => rows.push([
                    h.height.to_string(),
                    hex::encode(h.hash),
                    log::format_time_utc(h.timestamp),
                    format!("v{}.{}", h.major_version, h.minor_version),
                    h.difficulty.to_string(),
                    h.block_size.to_string(),
                    h.num_txes.to_string(),
                ]),
                Ok(None) => break,
                Err(e) => return busy(&format!("Could not read the header at {index}"), &e),
            }
        }
        if rows.len() == 1 {
            return format!("no blocks in {begin}..={end}");
        }
        grid(&rows)
    }

    /// `DaemonCommandsHandler::print_block`: a bare number is a **height**
    /// (a count, as the C++ takes it), anything else is a hash.
    fn print_block(&self, args: &[&str]) -> String {
        let Some(arg) = args.first() else {
            return "expected: print_block (<block_hash> | <block_height>)".to_string();
        };
        // The C++ falls through to the hash lookup for anything that is not a
        // complete number, which is what an unparsable argument should do here
        // too: a 64-hex hash is not a number, and junk is neither.
        if arg.chars().all(|c| c.is_ascii_digit()) {
            let Ok(height) = arg.parse::<u64>() else {
                return format!("`{arg}` is too large to be a block height.");
            };
            let top = self.api.top_index();
            // The C++ takes a height (a count) here and looks up height - 1.
            if height == 0 || height - 1 > top {
                return format!("block wasn't found. Current block chain height: {}, requested: {height}", top + 1);
            }
            match self.api.block_header_by_index(height - 1) {
                Ok(Some(h)) => self.render_block(&h),
                Ok(None) => format!("block wasn't found at height {height}"),
                Err(e) => busy("Could not read that block", &e),
            }
        } else {
            let Some(hash) = parse_hash(arg) else {
                return format!("`{arg}` is neither a block height nor a 64-character block hash.");
            };
            match self.api.block_header_by_hash(&hash) {
                Ok(Some(h)) => self.render_block(&h),
                Ok(None) => format!("block wasn't found: {arg}"),
                Err(e) => busy("Could not read that block", &e),
            }
        }
    }

    /// One block as a field list. The C++ prints `storeToJson(block)`; this is
    /// the same values without a JSON reader in the way.
    fn render_block(&self, h: &BlockHeaderInfo) -> String {
        let top = self.api.top_index();
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push(("block_id".into(), hex::encode(h.hash)));
        rows.push(("height (index)".into(), h.height.to_string()));
        rows.push(("depth".into(), top.saturating_sub(h.height).to_string()));
        rows.push(("major.minor version".into(), format!("{}.{}", h.major_version, h.minor_version)));
        rows.push(("timestamp".into(), format!("{} ({})", h.timestamp, log::format_time_utc(h.timestamp))));
        rows.push(("prev_hash".into(), hex::encode(h.prev_hash)));
        rows.push(("nonce".into(), h.nonce.to_string()));
        rows.push(("orphan".into(), yes_no(h.orphan_status)));
        rows.push(("difficulty".into(), h.difficulty.to_string()));
        rows.push(("reward".into(), format_amount(h.reward)));
        rows.push(("transactions (incl. coinbase)".into(), h.num_txes.to_string()));
        rows.push(("block_size".into(), pretty_bytes(h.block_size)));
        // The transaction list is only in the details record, which the
        // explorer's `f_block_json` reads; ask for it and add what it has.
        if let Ok(Some(details)) = self.api.block_details(&h.hash) {
            rows.push(("total_fee_amount".into(), format_amount(details.total_fee_amount)));
            rows.push(("base_reward".into(), format_amount(details.base_reward)));
            rows.push(("already_generated_coins".into(), details.already_generated_coins.to_string()));
            let mut out = two_column_table(&rows);
            out.push_str("transactions:\n");
            for tx in &details.transactions {
                out.push_str(&format!(
                    "  {} fee {} out {} size {}\n",
                    hex::encode(tx.hash),
                    format_amount(tx.fee),
                    format_amount(tx.amount_out),
                    pretty_bytes(tx.size)
                ));
            }
            return out;
        }
        two_column_table(&rows)
    }

    /// `DaemonCommandsHandler::print_tx`.
    fn print_tx(&self, args: &[&str]) -> String {
        let Some(arg) = args.first() else {
            return "expected: print_tx <transaction_hash>".to_string();
        };
        let Some(hash) = parse_hash(arg) else {
            return format!("`{arg}` is not a 64-character transaction hash.");
        };
        let blob = match self.api.transaction_blob(&hash) {
            Ok(Some(blob)) => blob,
            Ok(None) => return format!("transaction wasn't found: <{arg}>"),
            Err(e) => return busy("Could not read that transaction", &e),
        };
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push(("id".into(), hex::encode(hash)));
        rows.push(("blobSize".into(), pretty_bytes(blob.len() as u64)));
        match wrkz_primitives::tx::Transaction::from_bytes(&blob) {
            Ok(tx) => {
                rows.push(("version".into(), tx.prefix.version.to_string()));
                rows.push(("unlock_time".into(), tx.prefix.unlock_time.to_string()));
                rows.push(("inputs".into(), tx.prefix.inputs.len().to_string()));
                rows.push(("outputs".into(), tx.prefix.outputs.len().to_string()));
                let out_total: u64 = tx.prefix.outputs.iter().map(|o| o.amount).sum();
                rows.push(("amount_out".into(), format_amount(out_total)));
                rows.push(("extra".into(), format!("{} bytes", tx.prefix.extra.len())));
                rows.push(("signature rings".into(), tx.signatures.len().to_string()));
            }
            // The blob came out of the chain, so this is a corrupt record or a
            // record this port cannot parse yet. Say which, and still print the
            // hex: that is what makes the report actionable.
            Err(e) => rows.push(("parse error".into(), e.to_string())),
        }
        let mut out = two_column_table(&rows);
        if let Some(index) = self.transaction_height(&hash) {
            out.push_str(&format!("mined in block index {index}\n"));
        }
        out.push_str("blob:\n");
        out.push_str(&hex::encode(&blob));
        out
    }

    /// The block a transaction was mined in, when this node keeps that index.
    fn transaction_height(&self, hash: &Hash) -> Option<u64> {
        self.api.transaction_block_index(hash).ok().flatten()
    }

    // -- the pool ------------------------------------------------------------

    /// `DaemonCommandsHandler::print_pool` (`long`) and `print_pool_sh`
    /// (`!long`), over `f_on_transactions_pool_json`'s own summary.
    fn print_pool(&self, long: bool) -> String {
        let pool = match self.api.pool_transactions() {
            Ok(p) => p,
            Err(e) => return busy("Problem retrieving the transaction pool", &e),
        };
        if pool.is_empty() {
            return "Pool state: Empty.".to_string();
        }
        let mut out = String::from("Pool state:\n");
        let mut total = 0u64;
        for tx in &pool {
            let fusion = tx.fee == 0;
            if long {
                out.push_str(&format!("id: {}\n", hex::encode(tx.hash)));
                out.push_str(&format!("fee: {}\n", tx.fee));
                out.push_str(&format!("blobSize: {}\n", tx.size));
                out.push_str(&format!("amountOut: {}\n", tx.amount_out));
                out.push_str(&format!("fusion: {}\n\n", yes_no(fusion)));
            } else {
                out.push_str(&format!(
                    "Hash: {}, Size: {}, Fee: {}, Fusion: {}\n",
                    hex::encode(tx.hash),
                    pretty_bytes(tx.size),
                    format_amount(tx.fee),
                    if fusion { "Yes" } else { "No" }
                ));
            }
            total += tx.size;
        }
        // `Utilities::getMaxTxSize` is the median-based block size limit; this
        // port exposes the same number through the block details of the tip,
        // and falls back to the fixed reward zone when it cannot read one.
        let max_tx_size = self.max_tx_size();
        let blocks = total.div_ceil(max_tx_size.max(1));
        out.push_str(&format!("\nTotal transactions: {}\n", pool.len()));
        out.push_str(&format!("Total size of transactions: {}\n", pretty_bytes(total)));
        out.push_str(&format!("Estimated full blocks to clear: {blocks}"));
        out
    }

    /// The size one block can carry, as `print_pool_sh`'s "blocks to clear"
    /// divides by.
    fn max_tx_size(&self) -> u64 {
        let floor = wrkz_primitives::constants::CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2 as u64;
        match self.api.block_header_by_index(self.api.top_index()) {
            Ok(Some(h)) if h.block_size > 0 => h.block_size.max(floor),
            _ => floor,
        }
    }
}

// ---------------------------------------------------------------------------
// set_log
// ---------------------------------------------------------------------------

/// `DaemonCommandsHandler::set_log`. The C++ takes 0-4 and adds one, so 0 is
/// `FATAL+1` = `ERROR` and 4 is `TRACE`; this port's [`Level`] is the same five
/// levels in the same order, and the names are accepted as well because
/// `--log-level` takes them.
fn set_log(args: &[&str]) -> String {
    const USAGE: &str = "use: set_log <log_level_number_0-4>, or a name (error, warn, info, debug, trace)";
    let [arg] = args else {
        return USAGE.to_string();
    };
    let level = match Level::from_number(arg) {
        Some(level) => level,
        // A number that is not 0-4 is a range complaint; anything else might
        // still be a name. Keeps the two C++ messages apart.
        None if arg.parse::<u16>().is_ok() => return format!("wrong number range, {USAGE}"),
        None => match Level::parse(arg) {
            Some(level) => level,
            None => return format!("wrong number format, {USAGE}"),
        },
    };
    log::set_level(level);
    format!("log level set to {}", level_name(level))
}

/// Ours, not the C++'s: the last lines the logger emitted.
///
/// The C++ console has nothing like it, and an operator there is expected to
/// `tail` the log file. That does not work here for the two cases this command
/// exists for: a node started with no `--log-file` keeps no file to tail, and a
/// node whose stderr was redirected somewhere the operator cannot reach (a
/// service manager, a pipe) has a console on stdout and a log they cannot see.
/// [`crate::log`] keeps the last [`log::RECENT_CAPACITY`] lines in memory for
/// exactly this; it costs a bounded ring of strings and nothing else.
fn log_tail(args: &[&str]) -> String {
    const USAGE: &str = "use: log_tail [count], 1 to 200";
    let count = match args {
        [] => 20,
        [arg] => match arg.parse::<usize>() {
            Ok(0) => return format!("a count of zero prints nothing; {USAGE}"),
            Ok(n) => n.min(log::RECENT_CAPACITY),
            Err(_) => return format!("`{arg}` is not a line count; {USAGE}"),
        },
        _ => return USAGE.to_string(),
    };
    let lines = log::recent(count);
    if lines.is_empty() {
        return "No log lines have been kept yet.".to_string();
    }
    lines.join("\n")
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "error (0)",
        Level::Warn => "warn (1)",
        Level::Info => "info (2)",
        Level::Debug => "debug (3)",
        Level::Trace => "trace (4)",
    }
}

// ---------------------------------------------------------------------------
// the stdin reader
// ---------------------------------------------------------------------------

/// The prompt, as the C++ console draws it.
pub const PROMPT: &str = "wrkz-node> ";

/// The longest line the reader will accept. A terminal in its own line mode
/// caps a typed line long before this; a paste of a megabyte is neither a
/// command nor something to allocate for.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// True when both stdin and stdout are terminals, which is the only case in
/// which a prompt may be drawn: under systemd, in a pipeline or with stdout
/// redirected, escape sequences would end up in a log file.
///
/// stderr is a separate question, and [`crate::log`] asks it separately: the
/// log goes there, and `wrkz-node 2>daemon.log` must not put an escape
/// sequence or a redrawn prompt in that file.
pub fn on_a_terminal() -> bool {
    let term = log::terminal();
    term.stdin && term.stdout
}

/// Start the stdin reader on its own thread.
///
/// Returns `false`, having started nothing, when stdin is not a terminal —
/// piped input, `< /dev/null`, or systemd with no tty. That is deliberate: a
/// daemon under a service manager has no operator at a keyboard, and a reader
/// that sat on a closed descriptor would either spin or hold a thread for
/// nothing. Everything else about the daemon is unchanged in that case.
///
/// The thread is detached. It is blocked in `read_line` most of the time and
/// there is no portable way to interrupt that, so shutdown never waits for it;
/// the process exiting takes it with it. Nothing it holds needs a destructor to
/// run.
pub fn spawn_reader(console: Arc<Console>) -> bool {
    if !on_a_terminal() {
        crate::log_debug!("console: stdin is not a terminal, not starting the command reader");
        return false;
    }
    std::thread::Builder::new()
        .name("wrkz-console".into())
        .spawn(move || read_loop(&console))
        .map(|_| true)
        .unwrap_or_else(|e| {
            crate::log_warn!("console: could not start the command reader: {e}");
            false
        })
}

/// Read and run lines until end of input.
///
/// Reads **bytes**, not a `String`: `Stdin::read_line` fails the whole read
/// with `InvalidData` on a byte that is not UTF-8, and one stray byte — a
/// Latin-1 paste, a key that sent a raw escape — would have ended the reader
/// for the rest of the run. A lossy conversion turns it into a replacement
/// character, the command comes back unknown, and the console carries on.
fn read_loop(console: &Console) {
    use std::io::BufRead;

    log::set_prompt(Some(PROMPT.to_string()));
    log::console_print(&format!("{}\nType `help` for the command list.", console.cfg.version));
    let stdin = std::io::stdin();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        match stdin.lock().read_until(b'\n', &mut buffer) {
            // End of input: Ctrl-D, or the terminal went away. The daemon keeps
            // running — the console is an accessory to it, not its owner.
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                crate::log_warn!("console: stdin error, stopping the reader: {e}");
                break;
            }
        }
        if buffer.len() > MAX_LINE_BYTES {
            log::console_print(&format!(
                "that line is {} bytes; the console reads at most {MAX_LINE_BYTES}. Ignored.",
                buffer.len()
            ));
            // Nothing is parsed, and the capacity a pasted megabyte grew the
            // buffer to is given back rather than held for the run.
            buffer = Vec::new();
            continue;
        }
        let line = String::from_utf8_lossy(&buffer);
        let outcome = console.run_line(line.trim());
        if outcome.output.is_empty() {
            // A blank line prints nothing, but the terminal has already echoed
            // the newline: without this the operator is left on a bare row with
            // no prompt until the next log line happens to redraw one.
            log::redraw_prompt();
        } else {
            log::console_print(&outcome.output);
        }
        if outcome.exit {
            break;
        }
    }
    log::set_prompt(None);
    crate::log_info!("console: reader stopped; the daemon is unaffected");
}

// ---------------------------------------------------------------------------
// formatting, ported from src/utilities/FormatTools.cpp
// ---------------------------------------------------------------------------

/// This node's own release, in the shape `wrkz-node --version` prints it:
/// `WRKZ_GIT_COMMIT` is set at compile time by `build.rs` and is absent
/// outside a git checkout.
fn node_version() -> String {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    match option_env!("WRKZ_GIT_COMMIT") {
        Some(commit) if !commit.is_empty() => format!("wrkz-node {VERSION} ({commit})"),
        _ => format!("wrkz-node {VERSION}"),
    }
}

fn yes_no(b: bool) -> String {
    if b {
        "Yes".into()
    } else {
        "No".into()
    }
}

fn yes_no_lower(b: bool) -> String {
    if b {
        "yes".into()
    } else {
        "no".into()
    }
}

/// The message a failed accessor turns into. The console never reports an
/// error code an operator would have to look up.
fn busy(what: &str, e: &ApiError) -> String {
    format!("{what}: {e}")
}

/// The one wording for "this state has no block bodies below `height`", used by
/// `status` and by the daemon's start-up warning so an operator who sees one
/// recognises the other.
///
/// `/info`'s `lite_start_height` is what carries the fact: the C++ uses that
/// field for exactly this — a node whose block bodies begin above genesis — and
/// wallets already floor their scan height at it (`RpcServer::info`). A state
/// imported without `--store-raw` is that node, whatever produced it, so no new
/// field was invented.
/// What a **deliberately** reduced node prints at start-up.
///
/// [`missing_bodies_warning`] tells an operator how to get the bodies back,
/// which is the right advice for an import that lost them by accident and
/// exactly the wrong advice for a node that was asked to drop them: a lite
/// database cannot be re-imported into, and a pruned one is doing what it was
/// told. This says what the node will and will not answer instead.
pub fn reduced_mode_notice(floor: u64, pruned: bool) -> String {
    let mode = if pruned { "pruned" } else { "lite" };
    format!(
        "{mode} node: no block bodies below height {floor}. Every block is validated exactly as a \
         full node validates it, and below {floor} this node cannot serve a block to a peer, a \
         wallet rescan, or an explorer lookup — those report an error rather than an answer."
    )
}

pub fn missing_bodies_warning(height: u64) -> String {
    format!(
        "no block bodies below height {height}: this node cannot serve blocks, wallet sync or \
         transactions below that height to peers or wallets. Re-import with `wrkz-replay \
         --store-raw` into a new state directory to fix it."
    )
}

/// `Utilities::get_sync_percentage`, to two decimals, never 100.00 below the
/// target.
pub fn sync_percentage(height: u64, target: u64) -> String {
    if height == 0 || target == 0 {
        return "0.00".to_string();
    }
    let capped = height.min(target);
    let mut percent = 100.0 * capped as f64 / target as f64;
    if capped < target && percent > 99.99 {
        percent = 99.99;
    }
    format!("{percent:.2}")
}

/// `Utilities::get_mining_speed`.
pub fn mining_speed(hashrate: u64) -> String {
    let h = hashrate as f64;
    if h > 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h > 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h > 1e3 {
        format!("{:.2} KH/s", h / 1e3)
    } else {
        format!("{h:.2} H/s")
    }
}

/// `Utilities::prettyPrintBytes`.
pub fn pretty_bytes(bytes: u64) -> String {
    let mut n = bytes as f64;
    let suffixes = ["B", "KB", "MB", "GB", "TB"];
    let mut i = 0;
    while n >= 1024.0 && i < suffixes.len() - 1 {
        i += 1;
        n /= 1024.0;
    }
    format!("{n:.2} {}", suffixes[i])
}

/// `Utilities::formatAmount`: two decimal places and the ticker
/// (`CRYPTONOTE_DISPLAY_DECIMAL_POINT` is 2).
pub fn format_amount(atomic: u64) -> String {
    let divisor = 10u64.pow(wrkz_primitives::constants::CRYPTONOTE_DISPLAY_DECIMAL_POINT);
    let whole = atomic / divisor;
    let cents = atomic % divisor;
    let places = wrkz_primitives::constants::CRYPTONOTE_DISPLAY_DECIMAL_POINT as usize;
    // The C++ groups the whole part with commas; so does this, for the same
    // reason: a nine-figure reward is unreadable without them.
    let mut grouped = String::new();
    let digits = whole.to_string();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{grouped}.{cents:0places$} WRKZ")
}

/// The uptime line of `status`, from `/info`'s `start_time`.
fn uptime(start_time: u64) -> String {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(start_time);
    let seconds = now.saturating_sub(start_time);
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    format!("{days}d {}h {}m {}s", hours % 24, minutes % 60, seconds % 60)
}

/// `Common::timeIntervalToString`, for `print_cn`'s uptime column.
pub fn time_interval(d: Duration) -> String {
    let seconds = d.as_secs();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d {}h {}m", hours % 24, minutes % 60)
    } else if hours > 0 {
        format!("{hours}h {}m {}s", minutes % 60, seconds % 60)
    } else if minutes > 0 {
        format!("{minutes}m {}s", seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// `Utilities::ForkStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkStatus {
    UpToDate,
    ForkLater,
    ForkSoonReady,
    ForkSoonNotReady,
    OutOfDate,
}

/// `EXPECTED_NUMBER_OF_BLOCKS_PER_DAY` at a 60-second target.
const BLOCKS_PER_DAY: f64 = (24 * 60 * 60) as f64 / DIFFICULTY_TARGET as f64;

/// `Utilities::get_fork_status`.
pub fn fork_status(height: u64, upgrades: &[u64], supported: u64) -> ForkStatus {
    if upgrades.is_empty() {
        return ForkStatus::UpToDate;
    }
    let mut next_fork = 0;
    for &upgrade in upgrades {
        if height >= upgrade && supported < upgrade {
            return ForkStatus::OutOfDate;
        }
        if upgrade > height {
            next_fork = upgrade;
            break;
        }
    }
    let days = (next_fork.saturating_sub(height)) as f64 / BLOCKS_PER_DAY;
    if next_fork != 0 && days < 30.0 {
        return if supported < next_fork { ForkStatus::ForkSoonNotReady } else { ForkStatus::ForkSoonReady };
    }
    if height > next_fork {
        return ForkStatus::UpToDate;
    }
    ForkStatus::ForkLater
}

/// `Utilities::get_update_status`.
pub fn update_status(status: ForkStatus) -> &'static str {
    match status {
        ForkStatus::UpToDate | ForkStatus::ForkLater => "Up To Date",
        ForkStatus::ForkSoonReady => "Forking Soon",
        ForkStatus::ForkSoonNotReady => "Update Needed",
        ForkStatus::OutOfDate => "Likely Forked",
    }
}

/// `Utilities::get_fork_time`.
pub fn fork_time(height: u64, upgrades: &[u64]) -> String {
    let next_fork = upgrades.iter().copied().find(|&u| u > height).unwrap_or(0);
    if next_fork == 0 {
        return "No Fork Planned".to_string();
    }
    if height == next_fork {
        return "Now!".to_string();
    }
    let days = (next_fork - height) as f64 / BLOCKS_PER_DAY;
    if days < 1.0 {
        format!("{:.2} Hours", days * 24.0)
    } else {
        format!("{days:.2} Days")
    }
}

// ---------------------------------------------------------------------------
// tables
// ---------------------------------------------------------------------------

/// `status`'s table: two columns, padded to the widest, with a rule above and
/// below (`DaemonCommandsHandler::status`).
fn two_column_table(rows: &[(String, String)]) -> String {
    let left = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let right = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    let width = left + right + 7;
    let rule = "-".repeat(width);
    let mut out = format!("{rule}\n");
    for (k, v) in rows {
        out.push_str(&format!("| {k:left$} | {v:right$} |\n"));
    }
    out.push_str(&rule);
    out.push('\n');
    out
}

/// A bordered grid whose first row is the header, as
/// `connections_to_string` draws it: every column padded to its widest cell.
fn grid<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0usize; N];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let border = {
        let mut s = String::from("+");
        for w in widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s
    };
    let mut out = format!("{border}\n");
    for (n, row) in rows.iter().enumerate() {
        out.push('|');
        for (i, cell) in row.iter().enumerate() {
            let w = widths[i];
            out.push_str(&format!(" {cell:w$} |"));
        }
        out.push('\n');
        if n == 0 {
            out.push_str(&border);
            out.push('\n');
        }
    }
    out.push_str(&border);
    out
}

// ---------------------------------------------------------------------------
// argument parsing
// ---------------------------------------------------------------------------

/// A block index, with the error an operator can act on.
fn parse_index(arg: &str) -> Result<u64, String> {
    arg.parse::<u64>().map_err(|_| format!("`{arg}` is not a block height."))
}

/// A 64-character hex hash. Deliberately strict about the length: a truncated
/// hash silently matching nothing is worse than being told it is truncated.
fn parse_hash(arg: &str) -> Option<Hash> {
    if arg.len() != 64 {
        return None;
    }
    let bytes = hex::decode(arg).ok()?;
    Hash::try_from(bytes.as_slice()).ok()
}

/// An IPv4 or IPv6 address, as `ban add` and `ban delete` take one.
fn parse_ip(arg: &str) -> Option<IpAddr> {
    arg.parse::<IpAddr>().ok()
}

// ---------------------------------------------------------------------------

/// `compact_db status`, in the C++'s words, plus who started a running pass.
fn compaction_status(status: &StatusReport) -> String {
    if let Some(running) = status.running {
        let by = match running.trigger {
            Trigger::Manual { rewrite_bottommost: true } => "manual console request, rewriting the bottommost level",
            other => other.describe(),
        };
        return format!("DB compaction status: running ({}s elapsed)\nStarted by: {by}", running.elapsed_secs);
    }
    let mut out = String::from("DB compaction status: idle");
    if status.marker_present {
        out.push_str("\nPersistent compaction marker present (previous run may have terminated mid-compaction).");
    }
    match &status.last_result {
        Some(RunResult::Completed) => out.push_str("\nLast result: completed successfully"),
        Some(RunResult::Stopped) => {
            out.push_str("\nLast result: failed - stopped early on request (it will resume on the next start)")
        }
        Some(RunResult::Failed(e)) => out.push_str(&format!("\nLast result: failed - {e}")),
        None => {}
    }
    out
}

/// Directory bytes, files and subdirectories, and files by extension, for
/// `db_status`. `None` when the path does not exist, which is the in-memory
/// case.
struct DirStats {
    bytes: u64,
    files: u64,
    directories: u64,
    /// `.sst`, `.log`, … with the dot, and `<none>`, as the C++ labels them.
    extensions: std::collections::BTreeMap<String, u64>,
}

/// The bytes of every file under `path`, or `None` when it does not exist:
/// what `snapshot_export` weighs its free-space check against.
pub(crate) fn dir_bytes(path: &std::path::Path) -> Option<u64> {
    dir_stats(path).map(|s| s.bytes)
}

fn dir_stats(path: &std::path::Path) -> Option<DirStats> {
    if !path.exists() {
        return None;
    }
    let mut stats = DirStats { bytes: 0, files: 0, directories: 0, extensions: Default::default() };
    let mut stack = vec![path.to_path_buf()];
    // Iterative, and it never follows a symlink into a loop, because
    // `read_dir` on a symlinked directory is only entered when `file_type`
    // says directory and that is `lstat`, not `stat`.
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stats.directories += 1;
                stack.push(entry.path());
            } else if kind.is_file() {
                stats.files += 1;
                if let Ok(meta) = entry.metadata() {
                    stats.bytes += meta.len();
                }
                let ext = match entry.path().extension() {
                    Some(ext) => format!(".{}", ext.to_string_lossy()),
                    None => "<none>".to_string(),
                };
                *stats.extensions.entry(ext).or_insert(0) += 1;
            }
        }
    }
    Some(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_helpers_match_the_cpp() {
        // Utilities::get_sync_percentage
        assert_eq!(sync_percentage(0, 100), "0.00");
        assert_eq!(sync_percentage(100, 0), "0.00");
        assert_eq!(sync_percentage(50, 100), "50.00");
        assert_eq!(sync_percentage(200, 100), "100.00", "capped at the target");
        assert_eq!(sync_percentage(999_999, 1_000_000), "99.99", "never 100 below the target");
        // Utilities::get_mining_speed
        assert_eq!(mining_speed(500), "500.00 H/s");
        assert_eq!(mining_speed(2_000), "2.00 KH/s");
        assert_eq!(mining_speed(3_500_000), "3.50 MH/s");
        assert_eq!(mining_speed(2_000_000_000), "2.00 GH/s");
        // Utilities::prettyPrintBytes
        assert_eq!(pretty_bytes(0), "0.00 B");
        assert_eq!(pretty_bytes(1024), "1.00 KB");
        assert_eq!(pretty_bytes(1024 * 1024 * 3 / 2), "1.50 MB");
        // Utilities::formatAmount, two decimal places
        assert_eq!(format_amount(0), "0.00 WRKZ");
        assert_eq!(format_amount(1), "0.01 WRKZ");
        assert_eq!(format_amount(123_456_789), "1,234,567.89 WRKZ");
    }

    #[test]
    fn the_fork_helpers_match_the_cpp() {
        let upgrades = [100u64, 200, 1_000_000];
        // Supported past every fork we have reached, and the next one is far.
        assert_eq!(fork_status(300, &upgrades, 1_000_000), ForkStatus::ForkLater);
        assert_eq!(update_status(ForkStatus::ForkLater), "Up To Date");
        // A fork already passed that this build does not support.
        assert_eq!(fork_status(300, &upgrades, 100), ForkStatus::OutOfDate);
        assert_eq!(update_status(ForkStatus::OutOfDate), "Likely Forked");
        // Within 30 days of the next fork, ready and not ready.
        let soon = 1_000_000 - 100;
        assert_eq!(fork_status(soon, &upgrades, 1_000_000), ForkStatus::ForkSoonReady);
        assert_eq!(fork_status(soon, &upgrades, 200), ForkStatus::ForkSoonNotReady);
        assert_eq!(update_status(ForkStatus::ForkSoonNotReady), "Update Needed");
        // No fork ahead at all.
        assert_eq!(fork_time(2_000_000, &upgrades), "No Fork Planned");
        assert!(fork_time(soon, &upgrades).ends_with("Hours"), "{}", fork_time(soon, &upgrades));
        assert!(fork_time(0, &upgrades).ends_with("Hours") || fork_time(0, &upgrades).ends_with("Days"));
    }

    #[test]
    fn hashes_and_ips_are_parsed_strictly() {
        assert!(parse_hash("ab").is_none(), "a truncated hash is refused, not padded");
        assert!(parse_hash(&"zz".repeat(32)).is_none(), "non-hex is refused");
        assert_eq!(parse_hash(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(parse_ip("1.2.3.4"), Some("1.2.3.4".parse().unwrap()));
        assert_eq!(parse_ip("::1"), Some("::1".parse().unwrap()));
        assert!(parse_ip("1.2.3.4:17855").is_none(), "an address with a port is not an IP");
        assert!(parse_ip("nonsense").is_none());
        assert!(parse_index("12a").is_err());
        assert_eq!(parse_index("12"), Ok(12));
    }

    #[test]
    fn time_intervals_read_the_way_the_cpp_prints_them() {
        assert_eq!(time_interval(Duration::from_secs(9)), "9s");
        assert_eq!(time_interval(Duration::from_secs(65)), "1m 5s");
        assert_eq!(time_interval(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(time_interval(Duration::from_secs(90_061)), "1d 1h 1m");
    }

    #[test]
    fn set_log_takes_the_cpp_numbers_and_our_names() {
        assert!(set_log(&["4"]).contains("trace"));
        assert!(super::log::enabled(Level::Trace));
        assert!(set_log(&["warn"]).contains("warn"));
        assert!(!super::log::enabled(Level::Info));
        assert!(set_log(&["9"]).contains("wrong number range"));
        assert!(set_log(&["nonsense"]).contains("wrong number format"));
        assert!(set_log(&[]).starts_with("use: set_log"));
        assert!(set_log(&["2", "3"]).starts_with("use: set_log"));
        // Put it back so the rest of the suite logs as it did.
        set_log(&["2"]);
    }

    /// A console over a chain state with a given body policy, so the three
    /// mode commands can be asked what they report.
    fn console_over(cfg: wrkz_chain::Config) -> Console {
        console_with(cfg, ConsoleConfig::default())
    }

    fn console_with(cfg: wrkz_chain::Config, console_cfg: ConsoleConfig) -> Console {
        use wrkz_chain::{keys, Checkpoints};
        use wrkz_storage::{KvStore, MemStore};
        let mut store = MemStore::default();
        let mut ops = vec![(keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec()))];
        if cfg.lite_start_height != 0 {
            ops.push((keys::meta(keys::META_LITE_HEIGHT), Some(cfg.lite_start_height.to_le_bytes().to_vec())));
        }
        store.write_batch(ops).unwrap();
        let chain = wrkz_chain::ChainState::open_or_genesis(store, cfg, Checkpoints::none()).expect("opens");
        let node = wrkz_rpc::node::ChainNode::standalone(chain, wrkz_mempool::TransactionPool::new(Default::default()));
        Console::standalone(std::sync::Arc::new(node), console_cfg)
    }

    fn serving() -> wrkz_chain::Config {
        wrkz_rpc::node::serving_config()
    }

    /// `prune_status`, `db_status` and `status` have to report the state this
    /// daemon is actually in. A placeholder here is worse than no command: an
    /// operator reads "Pruned Node: No" and plans around a node that is pruned.
    #[test]
    fn the_mode_commands_report_the_real_state() {
        // A full node says so, and says nothing about depths it does not have.
        let full = console_over(serving());
        let out = full.run_line("prune_status").output;
        assert!(out.contains("Pruned Node: No"), "{out}");
        assert!(out.contains("Lite Node: No"), "{out}");
        assert!(out.contains("not pruning; every block body is kept"), "{out}");
        assert!(out.contains("Serves Block Data From: 0 (the whole chain)"), "{out}");
        assert!(full.run_line("db_status").output.contains("Block Bodies: every block (full)"));
        let status = full.run_line("status").output;
        assert!(status.contains("Pruned Node"), "{status}");
        assert!(status.contains("Lite Node"), "{status}");

        // A lite node names its line and says the choice is permanent.
        let lite = console_over(wrkz_chain::Config { lite_start_height: 1_000, ..serving() });
        let out = lite.run_line("prune_status").output;
        assert!(out.contains("Lite Node: Yes"), "{out}");
        assert!(out.contains("Lite Height: 1000 (permanent for this database)"), "{out}");
        assert!(out.contains("Serves Block Data From: 1000"), "{out}");
        assert!(out.contains("validates and follows the chain exactly as a full node"), "{out}");
        assert!(lite.run_line("db_status").output.contains("from height 1000 up (lite, permanent)"));
        let status = lite.run_line("status").output;
        assert!(status.contains("Lite Height (permanent)"), "{status}");
        assert!(status.contains("Serves Block Data From"), "{status}");

        // A pruned node names its depth and the floor its tip puts it at.
        let depth = wrkz_chain::MIN_PRUNE_DEPTH;
        let pruned = console_over(wrkz_chain::Config { prune_depth: Some(depth), ..serving() });
        let out = pruned.run_line("prune_status").output;
        assert!(out.contains("Pruned Node: Yes"), "{out}");
        assert!(out.contains(&format!("Prune Depth: {depth}")), "{out}");
        assert!(out.contains("Prune Floor Height:"), "{out}");
        assert!(pruned.run_line("db_status").output.contains(&format!("the most recent {depth} blocks (pruned)")));

        // A lite snapshot import says, besides, that its transaction records
        // start at the line too, and `print_tx` says why it cannot find one.
        let imported = {
            use wrkz_chain::{keys, Checkpoints};
            use wrkz_storage::{KvStore, MemStore};
            let mut store = MemStore::default();
            store
                .write_batch(vec![
                    (keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())),
                    (keys::meta(keys::META_LITE_HEIGHT), Some(1_000u32.to_le_bytes().to_vec())),
                    (keys::meta(keys::META_TAG), Some(keys::TAG_LITE_SNAPSHOT.as_bytes().to_vec())),
                ])
                .unwrap();
            let cfg = wrkz_chain::Config { lite_start_height: 1_000, ..serving() };
            let chain = wrkz_chain::ChainState::open_or_genesis(store, cfg, Checkpoints::none()).expect("opens");
            let node =
                wrkz_rpc::node::ChainNode::standalone(chain, wrkz_mempool::TransactionPool::new(Default::default()));
            Console::standalone(std::sync::Arc::new(node), ConsoleConfig::default())
        };
        let out = imported.run_line("prune_status").output;
        assert!(out.contains("Transaction Records From: 1000 (imported from a lite snapshot; none below)"), "{out}");
        assert!(imported.run_line("db_status").output.contains("Transaction Records: from height 1000 up"));
        let out = imported.run_line(&format!("print_tx {}", "ab".repeat(32))).output;
        assert!(out.contains("stores no transaction data below height 1000"), "{out}");
    }

    /// `compact_db` over a state kept in memory says there is nothing on disk
    /// to compact; `snapshot_export` says what it is missing, and reaches the
    /// exporter a console is given.
    #[test]
    fn the_unported_command_explains_itself_and_snapshot_export_is_wired() {
        let c = console_over(serving());
        let out = c.run_line("compact_db").output;
        assert!(out.contains("needs the RocksDB engine"), "{out}");
        assert!(out.contains("--features rocksdb"), "{out}");
        assert_eq!(c.run_line("compact_db sometimes").output, "Usage: compact_db [start|status|wait|force]");
        assert!(c.run_line("snapshot_export").output.contains("needs the daemon's own chain state"));

        struct Recording(std::sync::Mutex<Vec<String>>);
        impl crate::snapshot::SnapshotExport for Recording {
            fn command(&self, args: &[&str]) -> String {
                self.0.lock().unwrap().push(args.join(" "));
                "recorded".into()
            }
            fn shutdown(&self) {}
        }
        let recording = Arc::new(Recording(std::sync::Mutex::new(Vec::new())));
        let c = console_over(serving()).with_snapshot_export(Arc::clone(&recording) as _);
        assert_eq!(c.run_line("snapshot_export start 4000000 /tmp/x").output, "recorded");
        assert_eq!(*recording.0.lock().unwrap(), ["start 4000000 /tmp/x"]);
    }

    /// An engine that compacts at once and counts how often it was asked.
    struct InstantEngine(std::sync::atomic::AtomicUsize);

    impl crate::compaction::CompactionEngine for InstantEngine {
        fn compact_full(&self, _rewrite_bottommost: bool) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn stop_all_background_work(&self) {}

        fn stats(&self) -> wrkz_storage::dbconfig::EngineCompactionStats {
            wrkz_storage::dbconfig::EngineCompactionStats {
                compaction_pending: Some(false),
                running_compactions: Some(0),
                ..Default::default()
            }
        }
    }

    #[test]
    fn compact_db_and_db_status_report_the_compaction_state() {
        let dir = std::env::temp_dir().join(format!("wrkz-console-compact-{}", std::process::id()));
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("000001.sst"), b"sst").unwrap();
        std::fs::write(state.join("CURRENT"), b"MANIFEST-000001\n").unwrap();
        let engine = Arc::new(InstantEngine(Default::default()));
        let compaction = Compaction::new(engine.clone(), &state, Box::new(|| 42));
        let cfg = ConsoleConfig {
            data_dir: dir.clone(),
            engine: "RocksDB".into(),
            db: Some(Default::default()),
            ..Default::default()
        };
        let c = console_with(serving(), cfg).with_compaction(Arc::clone(&compaction));

        assert_eq!(c.run_line("compact_db status").output, "DB compaction status: idle");
        assert_eq!(c.run_line("compact_db wait").output, "No DB compaction is running.");
        assert_eq!(
            c.run_line("compact_db").output,
            "DB compaction started in background. Use `compact_db status` or `compact_db wait`."
        );
        // The engine here is instant, so `wait` may find it already done.
        let out = c.run_line("compact_db wait").output;
        assert!(out == "No DB compaction is running." || out.ends_with("\nDB compaction completed."), "{out}");
        compaction.wait();
        assert_eq!(
            c.run_line("compact_db status").output,
            "DB compaction status: idle\nLast result: completed successfully"
        );
        assert!(c.run_line("compact_db force").output.starts_with("DB compaction started in background."));
        compaction.wait();
        assert_eq!(engine.0.load(std::sync::atomic::Ordering::SeqCst), 2);

        let out = c.run_line("db_status").output;
        for line in [
            "DB Engine: RocksDB",
            "Compression Enabled: Yes",
            "Compression Mode: RocksDB ZSTD (L2+; L0/L1 uncompressed)",
            "File type counts:\n  .sst: 1\n  <none>: 1",
            "Read Cache: 256.00 MB (32.00 MB row cache, 224.00 MB block cache), 4 KiB blocks",
            "bottommost filters off",
            "DB compaction status: idle\nLast result: completed successfully",
            "RocksDB Compaction Pending: No",
            "RocksDB Running Compactions: 0",
            "RocksDB Live SST Size: unknown",
            "Block Bodies: every block (full)",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }

        compaction.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tables_line_up() {
        let text = two_column_table(&[("a".into(), "one".into()), ("bbb".into(), "2".into())]);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].chars().all(|c| c == '-'));
        assert_eq!(lines[1], "| a   | one |");
        assert_eq!(lines[2], "| bbb | 2   |");
        assert_eq!(lines[3].len(), lines[0].len());

        let text = grid(&[["Dir".into(), "Remote".into()], ["IN".into(), "1.2.3.4:17855".into()]]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "| Dir | Remote        |");
        assert_eq!(lines[3], "| IN  | 1.2.3.4:17855 |");
        assert_eq!(lines[0], lines[2], "the header rule and the top border match");
    }
}
