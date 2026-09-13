// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The daemon console, driven through its dispatcher rather than a terminal.
//!
//! Two nodes stand behind it, for the two halves of the surface:
//!
//! - a **real chain** — mainnet blocks 0 to 5 from `spec/vectors`, in a
//!   `MemStore`, behind the same [`wrkz_rpc::ChainNode`] the RPC server is
//!   given — so `print_block`, `print_tx`, `print_bc` and `status` are asserted
//!   against real hashes, real timestamps and the real genesis coinbase;
//! - the **`wrkz-rpc` fake** (`crates/wrkz-rpc/tests/fake/mod.rs`, included by
//!   path so there is one fake and not two) for the states a real chain in a
//!   test cannot be put into: a populated transaction pool, an `/info` that
//!   fails, a transaction blob that will not parse, a state whose block bodies
//!   begin above genesis.
//!
//! Every command is exercised on its happy path and on each way it can be
//! given a bad argument. Nothing here opens a terminal: `Console::run_line` is
//! the same entry point the stdin thread calls.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use wrkz_chain::{ChainState, Checkpoints};
use wrkz_mempool::TransactionPool;
use wrkz_node::console::{Console, ConsoleConfig, NodeView, COMMANDS};
use wrkz_node::daemon::Shutdown;
use wrkz_node::node::ConnectionRow;
use wrkz_node::peers::BanList;
use wrkz_node::sync::PeerState;
use wrkz_rpc::node::{serving_config, ChainNode, P2pSnapshot};
use wrkz_rpc::NodeApi;
use wrkz_storage::MemStore;

#[path = "../../wrkz-rpc/tests/fake/mod.rs"]
mod fake;
use fake::FakeNode;

// ---------------------------------------------------------------------------
// harnesses
// ---------------------------------------------------------------------------

fn raw_blocks() -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/mainnet_rawblocks_0_to_5.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                hex::decode(item["block"].as_str().unwrap()).unwrap(),
                item["transactions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| hex::decode(t.as_str().unwrap()).unwrap())
                    .collect(),
            )
        })
        .collect()
}

/// A console over the real mainnet blocks 0 to 5, the way the daemon builds
/// one: the `ChainNode` here is exactly what would be handed to the RPC server.
fn chain_console() -> Console {
    let mut chain = ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).unwrap();
    chain.set_clock(Some(1_900_000_000));
    for (blob, txs) in raw_blocks().into_iter().skip(1) {
        chain.add_block(&blob, &txs).expect("the vector blocks apply");
    }
    assert_eq!(chain.tip_index(), Some(5));
    let node = ChainNode::shared(
        Arc::new(RwLock::new(chain)),
        Arc::new(Mutex::new(TransactionPool::new(Default::default()))),
        Box::new(P2pSnapshot::standalone),
    );
    Console::standalone(Arc::new(node) as Arc<dyn NodeApi>, cfg())
}

fn cfg() -> ConsoleConfig {
    ConsoleConfig { version: "wrkz-node 0.1.0 test".into(), ..Default::default() }
}

/// A console over the canned node, for the states a real chain cannot be put
/// into in a test.
fn fake_console(node: FakeNode) -> Console {
    Console::standalone(Arc::new(node) as Arc<dyn NodeApi>, cfg())
}

/// The hash of the block at `index` on the real chain, as an operator would
/// paste it.
fn block_hash(console: &Console, index: u64) -> String {
    let line = console.run_line(&format!("print_block {}", index + 1)).output;
    let id = line.lines().find(|l| l.contains("block_id")).expect("print_block prints a block_id");
    id.split('|').nth(2).expect("the value column").trim().to_string()
}

fn run(console: &Console, line: &str) -> String {
    console.run_line(line).output
}

/// The log level and the ring of recent lines are process-wide, and the tests
/// in this binary run on threads of one process. Every test that moves either
/// takes this first, so `exit` turning the log up to trace cannot make
/// `set_log`'s assertions fail on another thread.
static LOG_STATE: Mutex<()> = Mutex::new(());

fn log_state() -> std::sync::MutexGuard<'static, ()> {
    LOG_STATE.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// help, unknown commands, the empty line
// ---------------------------------------------------------------------------

#[test]
fn help_lists_every_command_with_its_usage() {
    let console = chain_console();
    let text = run(&console, "help");
    assert!(text.starts_with("wrkz-node 0.1.0 test"), "the version comes first, as the C++ prints it: {text}");
    for (name, usage) in COMMANDS {
        assert!(text.contains(name), "help omits `{name}`");
        assert!(text.contains(usage), "help omits the usage of `{name}`");
    }
    // `?` is an alias for help, as in the C++.
    assert_eq!(run(&console, "?"), text);
}

#[test]
fn an_unknown_command_points_at_help_and_an_empty_line_prints_nothing() {
    let console = chain_console();
    let text = run(&console, "frobnicate");
    assert!(text.contains("unknown command `frobnicate`"), "{text}");
    assert!(text.contains("help"), "the message says where to look: {text}");

    let outcome = console.run_line("   ");
    assert_eq!(outcome.output, "", "a blank line is not an error and prints nothing");
    assert!(!outcome.exit);
    // Extra whitespace between a command and its argument is collapsed, the way
    // the C++ ConsoleHandler splits with token_compress_on.
    assert_eq!(run(&console, "set_log    2"), run(&console, "set_log 2"));
}

#[test]
fn snapshot_export_says_why_it_is_missing_and_compact_db_says_what_it_needs() {
    let console = chain_console();
    let text = run(&console, "compact_db start");
    assert!(text.contains("needs the RocksDB engine"), "{text}");
    assert!(text.contains("--features rocksdb"), "{text}");
    // `snapshot_export` is the daemon's; a console built without its chain
    // says what it is missing rather than "unknown command".
    let text = run(&console, "snapshot_export status");
    assert!(text.contains("needs the daemon's own chain state"), "{text}");
}

// ---------------------------------------------------------------------------
// exit
// ---------------------------------------------------------------------------

#[test]
fn exit_quit_and_stop_all_request_the_one_shutdown() {
    let _level = log_state();
    for word in ["exit", "quit", "stop"] {
        let shutdown = Shutdown::new();
        let node = ChainNode::standalone(
            ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).unwrap(),
            TransactionPool::new(Default::default()),
        );
        let console = Console::new(
            Arc::new(node) as Arc<dyn NodeApi>,
            Box::new(NodeView::default),
            BanList::new(),
            shutdown.clone(),
            cfg(),
        );
        assert!(!shutdown.requested(), "nothing has asked for a shutdown yet");
        let outcome = console.run_line(word);
        assert!(outcome.exit, "`{word}` ends the reader");
        assert!(outcome.output.contains("EXITING"), "the C++ banner: {}", outcome.output);
        assert!(
            shutdown.requested(),
            "`{word}` sets the very flag the SIGINT handler sets, so the shutdown path is one path"
        );
    }
    // The C++ turns the log up on the way out because the wait can be long.
    assert!(wrkz_node::log::enabled(wrkz_node::Level::Trace));
    wrkz_node::log::set_level(wrkz_node::Level::Info);
}

// ---------------------------------------------------------------------------
// over the IPC socket (`wrkz-node attach`)
// ---------------------------------------------------------------------------

/// `run_remote_command`: the output with its newline, `Unknown command` for a
/// word that is no command, and nothing for a blank line.
#[test]
fn a_remote_line_returns_what_the_command_printed() {
    let console = chain_console();
    assert_eq!(console.run_remote(""), "");
    assert_eq!(console.run_remote(" \t "), "");
    assert_eq!(console.run_remote("frobnicate now"), "Unknown command: frobnicate\n");
    let height = console.run_remote("height");
    assert!(height.starts_with("Height: 6 / "), "{height}");
    assert_eq!(height, format!("{}\n", run(&console, "height")), "the local output, with its newline");
    assert_eq!(console.run_remote("  print_bc   1  "), format!("{}\n", run(&console, "print_bc 1")));
    assert!(console.run_remote("help").contains("Commands:"));
    assert!(console.run_remote("snapshot_export").contains("needs the daemon's own chain state"), "a C++ command");
    assert!(!console.run_line("frobnicate").recognised());
    assert!(console.run_line("height").recognised() && console.run_line("").recognised());
}

/// `stop` sent over the socket is the same shutdown as SIGINT and the
/// terminal's `stop`.
#[test]
fn a_remote_stop_is_the_one_shutdown() {
    let _level = log_state();
    let shutdown = Shutdown::new();
    let node = ChainNode::standalone(
        ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).unwrap(),
        TransactionPool::new(Default::default()),
    );
    let console = Console::new(
        Arc::new(node) as Arc<dyn NodeApi>,
        Box::new(NodeView::default),
        BanList::new(),
        shutdown.clone(),
        cfg(),
    );
    let output = console.run_remote("stop");
    assert!(output.contains("EXITING") && output.ends_with('\n'), "{output}");
    assert!(shutdown.requested(), "the flag SIGINT sets");
    wrkz_node::log::set_level(wrkz_node::Level::Info);
}

// ---------------------------------------------------------------------------
// status and the one-line status commands
// ---------------------------------------------------------------------------

#[test]
fn status_prints_the_cpp_table_over_the_real_chain() {
    let console = chain_console();
    let text = run(&console, "status");
    for label in [
        "Local Height",
        "Network Height",
        "Percentage Synced",
        "Network Hashrate",
        "Block Version",
        "Incoming Connections",
        "Outgoing Connections",
        "Uptime",
        "Fork Status",
        "Next Fork",
        "Transaction Pool Size",
        "Alternative Block Count",
        "DB Engine",
        "Pruned Node",
        "Prune Depth",
        "Prune Capability Fork Active",
        "Lite Node",
        "Active Sync Peers",
        "Node Version",
        "RPC Compatible With",
    ] {
        assert!(text.contains(label), "status omits `{label}`:\n{text}");
    }
    // Our own release, not the C++ release the RPC answers as.
    assert!(text.contains(&format!("wrkz-node {}", env!("CARGO_PKG_VERSION"))), "{text}");
    assert!(text.contains(&format!("WrkzCoin {}", wrkz_rpc::DAEMON_VERSION)), "{text}");
    // Heights are counts, as `/info` reports them: six blocks, 0 to 5.
    assert!(text.contains("| Local Height"), "{text}");
    assert!(text.contains(" 6 "), "the height is a count of six blocks:\n{text}");
    assert!(text.contains("100.00%"), "a standalone node is its own network height:\n{text}");
    assert!(text.contains("H/s"), "the hashrate carries a unit:\n{text}");
    // Every row is the same width, borders included.
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let width = lines[0].len();
    assert!(lines.iter().all(|l| l.len() == width), "the table is ragged:\n{text}");
}

#[test]
fn status_says_so_when_the_node_cannot_be_read() {
    let console = fake_console(FakeNode { info_fails: true, ..Default::default() });
    let text = run(&console, "status");
    assert!(text.contains("Problem retrieving information"), "{text}");
    assert!(text.contains("reorganising"), "the reason is carried through, not swallowed: {text}");
    // The other /info commands behave the same way and none of them panics.
    for command in ["sync_peers", "sync_tune", "prune_status"] {
        assert!(run(&console, command).contains("Problem retrieving"), "{command}");
    }
}

#[test]
fn status_warns_when_the_state_holds_no_block_bodies_below_a_height() {
    // What a `wrkz-replay` import without `--store-raw` looks like: every index
    // present, no bodies below the import height. `/info` carries it in
    // `lite_start_height`, which is the field the C++ uses for exactly this.
    let console = fake_console(FakeNode { lite_start_height: 4_000_000, ..Default::default() });
    let text = run(&console, "status");
    assert!(text.contains("| Lite Node"), "{text}");
    assert!(text.contains("Serves Block Data From"), "{text}");
    assert!(text.contains("4000000"), "{text}");
    assert!(text.contains("no block bodies below height 4000000"), "the warning is under the table:\n{text}");
    assert!(text.contains("--store-raw"), "the warning says how to fix it:\n{text}");

    // A full node says none of it.
    let full = fake_console(FakeNode::default());
    let text = run(&full, "status");
    assert!(!text.contains("no block bodies"), "a full node is not warned:\n{text}");
    assert!(text.contains("| Lite Node") && text.contains("No"), "{text}");
}

#[test]
fn height_and_sync_info_report_the_same_two_numbers_as_the_rpc() {
    let console = chain_console();
    let text = run(&console, "height");
    assert_eq!(text, "Height: 6 / 6 (100.00%)", "counts, as /height reports them");
    assert_eq!(run(&console, "sync_info"), text, "sync_info is the same line the C++ prints");

    let behind = fake_console(FakeNode::default());
    assert!(run(&behind, "height").starts_with("Height: 4213001 / 4213001"), "{}", run(&behind, "height"));
}

#[test]
fn the_sync_and_prune_commands_print_their_fields() {
    let console = fake_console(FakeNode::default());
    let text = run(&console, "sync_peers");
    assert!(text.contains("Sync Active Peers: 0"), "{text}");
    assert!(text.contains("Average Sync Batch Size: 120"), "{text}");
    assert!(text.contains("Demoted Sync Peers (lifetime): 0"), "{text}");

    let text = run(&console, "sync_tune");
    for label in [
        "Active Sync Peers",
        "Configured Sync Max Peers",
        "Configured P2P Out/In Peers",
        "Configured Sync Failure Threshold",
        "Configured Sync Batch Min/Max",
        "Configured Block Sync Size",
        "Configured Block Sync Bytes",
    ] {
        assert!(text.contains(label), "sync_tune omits `{label}`:\n{text}");
    }
    assert!(text.contains("16.00 MB"), "the byte budget is printed as bytes:\n{text}");

    // `prune_status` reports the mode this node is actually in. A daemon that
    // is not pruning has no depth and no floor to print, and printing the
    // default 10,080 there — as an earlier pass did — reads as a node that is.
    let text = run(&console, "prune_status");
    assert!(text.contains("Pruned Node: No"), "{text}");
    assert!(text.contains("Prune Depth: not pruning; every block body is kept"), "{text}");
    assert!(text.contains("Lite Node: No"), "{text}");
    assert!(text.contains("Serves Block Data From: 0 (the whole chain)"), "{text}");
    assert!(text.contains("Prune Capability Fork Active: No"), "{text}");

    // And on a node whose bodies begin above genesis it names the floor and
    // says what stops working below it.
    let lite = fake_console(FakeNode { lite_start_height: 4_000_000, ..Default::default() });
    let text = run(&lite, "prune_status");
    assert!(text.contains("Lite Node: Yes"), "{text}");
    assert!(text.contains("Serves Block Data From: 4000000"), "{text}");
    assert!(text.contains("reports an error rather than an answer"), "{text}");
}

#[test]
fn db_status_and_save_report_the_engine_this_build_opened() {
    let console = chain_console();
    let text = run(&console, "db_status");
    assert!(text.contains("DB Engine: MemStore (in memory)"), "{text}");
    assert!(text.contains("in memory"), "{text}");
    assert!(text.contains("--features rocksdb"), "it says how to persist it:\n{text}");

    // `save` goes through the same accessor the daemon flushes with on the way
    // out, and an in-memory store commits happily.
    assert_eq!(run(&console, "save"), "Core state saved.");
}

// ---------------------------------------------------------------------------
// peers and connections
// ---------------------------------------------------------------------------

#[test]
fn print_pl_prints_the_lists_the_rpc_serves_at_slash_peers() {
    let console = fake_console(FakeNode::default());
    let text = run(&console, "print_pl");
    assert!(text.contains("Peerlist white (1):"), "{text}");
    assert!(text.contains("1.2.3.4:17855"), "{text}");
    assert!(text.contains("Peerlist gray (1):"), "{text}");
    assert!(text.contains("5.6.7.8:17855"), "{text}");
    assert!(text.contains("Seeds configured: 0"), "{text}");

    // A node that knows nobody prints the headers and no rows.
    let empty = chain_console();
    let text = run(&empty, "print_pl");
    assert!(text.contains("Peerlist white (0):"), "{text}");
    assert!(text.contains("Peerlist gray (0):"), "{text}");
}

#[test]
fn print_cn_draws_the_cpp_connection_table() {
    // No engine behind this console: no connections, and it says so rather
    // than printing an empty table.
    let console = chain_console();
    let text = run(&console, "print_cn");
    assert!(text.starts_with("Connections:"), "{text}");
    assert!(text.contains("none"), "{text}");

    // With connections, the nine columns of `connections_to_string`.
    let rows = vec![
        ConnectionRow {
            addr: "1.2.3.4:17855".parse().unwrap(),
            incoming: false,
            peer_id: 0x0123_4567_89ab_cdef,
            state: PeerState::Normal,
            version: 19,
            uptime: Duration::from_secs(3661),
            remote_height: 4_213_000,
            remote_is_pruned: true,
            remote_is_lite: false,
            sync_batch_size: 240,
            sync_failures: 1,
            blocks_added: 900,
        },
        ConnectionRow {
            addr: "[::1]:17855".parse().unwrap(),
            incoming: true,
            peer_id: 0,
            state: PeerState::BeforeHandshake,
            version: 0,
            uptime: Duration::from_secs(5),
            remote_height: 0,
            remote_is_pruned: false,
            remote_is_lite: false,
            sync_batch_size: 120,
            sync_failures: 0,
            blocks_added: 0,
        },
    ];
    let view = NodeView { connections: rows, ..Default::default() };
    let console = Console::new(
        Arc::new(FakeNode::default()) as Arc<dyn NodeApi>,
        Box::new(move || view.clone()),
        BanList::new(),
        Shutdown::new(),
        cfg(),
    );
    let text = run(&console, "print_cn");
    for column in ["Dir", "Remote", "Peer ID", "State", "Uptime", "Height", "Pruned", "Batch", "Fail"] {
        assert!(text.contains(column), "print_cn omits the `{column}` column:\n{text}");
    }
    assert!(text.contains("0123456789abcdef"), "the peer id is 16 hex digits:\n{text}");
    assert!(text.contains("1.2.3.4:17855"), "{text}");
    assert!(text.contains("[::1]:17855"), "an IPv6 peer fits the column:\n{text}");
    assert!(text.contains("normal") && text.contains("before_handshake"), "{text}");
    assert!(text.contains("1h 1m 1s"), "the uptime reads like the C++ timeIntervalToString:\n{text}");
    assert!(text.contains(" yes ") && text.contains(" no "), "the pruned column:\n{text}");
    // Every row of the grid is the same width.
    let widths: Vec<usize> = text.lines().skip(1).map(str::len).collect();
    assert!(widths.windows(2).all(|w| w[0] == w[1]), "the grid is ragged:\n{text}");
}

// ---------------------------------------------------------------------------
// ban
// ---------------------------------------------------------------------------

#[test]
fn ban_adds_lists_and_deletes_and_refuses_nonsense() {
    let bans = BanList::new();
    let console = Console::new(
        Arc::new(FakeNode::default()) as Arc<dyn NodeApi>,
        Box::new(NodeView::default),
        bans.clone(),
        Shutdown::new(),
        cfg(),
    );
    assert_eq!(run(&console, "ban list"), "Ban list is empty.");

    let text = run(&console, "ban add 1.2.3.4");
    assert!(text.contains("Ban added for 1.2.3.4 (900s)"), "the C++ default is 900 seconds: {text}");
    assert!(bans.is_banned("1.2.3.4".parse().unwrap()), "the engine sees it immediately, with no round trip");

    assert!(run(&console, "ban add ::1 60").contains("Ban added for ::1 (60s)"));
    let text = run(&console, "ban list");
    assert!(text.contains("Banned hosts:"), "{text}");
    assert!(text.contains("1.2.3.4 (") && text.contains("s remaining)"), "{text}");
    assert!(text.contains("::1 ("), "{text}");

    assert!(run(&console, "ban delete 1.2.3.4").contains("Ban removed for 1.2.3.4"));
    assert!(!bans.is_banned("1.2.3.4".parse().unwrap()));
    assert_eq!(run(&console, "ban delete 1.2.3.4"), "IP not found in ban list.");

    // Error paths: none of them panics and each says what was wrong.
    assert!(run(&console, "ban").starts_with("Usage: ban list"));
    assert!(run(&console, "ban wibble").starts_with("Usage: ban list"));
    assert!(run(&console, "ban add").starts_with("Usage: ban list"));
    assert_eq!(run(&console, "ban add nonsense"), "Invalid IP address: nonsense");
    assert_eq!(run(&console, "ban add 1.2.3.4:17855"), "Invalid IP address: 1.2.3.4:17855");
    assert_eq!(run(&console, "ban add 1.2.3.4 zero"), "Invalid ban seconds value `zero`.");
    assert_eq!(run(&console, "ban add 1.2.3.4 0"), "Ban seconds must be greater than zero.");
    assert_eq!(run(&console, "ban delete nonsense"), "Invalid IP address: nonsense");
    assert!(run(&console, "ban delete").starts_with("Usage: ban list"));
    assert!(run(&console, "ban add 1.2.3.4 60 extra").starts_with("Usage: ban list"));
}

// ---------------------------------------------------------------------------
// blocks
// ---------------------------------------------------------------------------

#[test]
fn print_block_takes_a_height_or_a_hash_over_the_real_chain() {
    let console = chain_console();
    // The C++ takes a *height* (a count) here and looks up height - 1.
    let text = run(&console, "print_block 1");
    assert!(text.contains("block_id"), "{text}");
    assert!(text.contains("height (index)") && text.contains(" 0 "), "block 1 is index 0:\n{text}");
    assert!(text.contains("major.minor version"), "{text}");
    assert!(text.contains("timestamp"), "{text}");
    assert!(text.contains("prev_hash"), "{text}");
    assert!(text.contains("nonce"), "{text}");
    assert!(text.contains("difficulty"), "{text}");
    assert!(text.contains("reward") && text.contains("WRKZ"), "the reward is formatted:\n{text}");

    // The same block by hash, and the same values.
    let hash = block_hash(&console, 3);
    assert_eq!(hash.len(), 64, "a block hash is 32 bytes of hex: {hash}");
    let by_hash = run(&console, &format!("print_block {hash}"));
    assert!(by_hash.contains(&hash), "{by_hash}");
    assert!(by_hash.contains("depth"), "{by_hash}");
}

#[test]
fn print_block_reports_every_bad_argument_without_panicking() {
    let console = chain_console();
    assert!(run(&console, "print_block").starts_with("expected: print_block"));
    // Above the tip: the C++ message, with the chain height in it.
    let text = run(&console, "print_block 99");
    assert!(text.contains("block wasn't found"), "{text}");
    assert!(text.contains("Current block chain height: 6"), "{text}");
    assert!(text.contains("requested: 99"), "{text}");
    // Height 0 is not a height in the C++'s counting either.
    assert!(run(&console, "print_block 0").contains("block wasn't found"));
    // A number no chain could hold.
    assert!(run(&console, "print_block 99999999999999999999999").contains("too large"));
    // Junk that is not a number falls through to the hash lookup, as the C++
    // does, and is refused there for its length.
    let text = run(&console, "print_block wibble");
    assert!(text.contains("neither a block height nor a 64-character block hash"), "{text}");
    // 64 characters that are not hex.
    assert!(run(&console, &format!("print_block {}", "zz".repeat(32))).contains("neither a block height"));
    // A well-formed hash of a block nobody has.
    let unknown = "ab".repeat(32);
    assert_eq!(run(&console, &format!("print_block {unknown}")), format!("block wasn't found: {unknown}"));
}

#[test]
fn print_bc_prints_a_range_and_refuses_a_silly_one() {
    let console = chain_console();
    let text = run(&console, "print_bc 0 5");
    for column in ["Height", "Hash", "Time", "Version", "Difficulty", "Size", "Txs"] {
        assert!(text.contains(column), "print_bc omits `{column}`:\n{text}");
    }
    // Six blocks, plus a header row and three borders.
    let rows = text.lines().filter(|l| l.starts_with('|')).count();
    assert_eq!(rows, 7, "a header and six blocks:\n{text}");

    // One argument is a single block.
    let one = run(&console, "print_bc 2");
    assert_eq!(one.lines().filter(|l| l.starts_with('|')).count(), 2, "{one}");

    // The end is clamped to the tip rather than reported as an error, so
    // `print_bc 0 100` on a short chain still prints what there is.
    let clamped = run(&console, "print_bc 4 100");
    assert_eq!(clamped.lines().filter(|l| l.starts_with('|')).count(), 3, "{clamped}");

    // Error paths.
    assert!(run(&console, "print_bc").starts_with("expected: print_bc"));
    assert!(run(&console, "print_bc 1 2 3").starts_with("expected: print_bc"));
    assert!(run(&console, "print_bc a 5").contains("is not a block height"));
    assert!(run(&console, "print_bc 0 b").contains("is not a block height"));
    let text = run(&console, "print_bc 5 1");
    assert!(text.contains("the range is inverted"), "{text}");
    // A range no console should ever try to build a table for.
    let text = run(&console, "print_bc 0 4000000");
    assert!(text.contains("4000001 blocks"), "{text}");
    assert!(text.contains("at most 1000"), "{text}");
    // Entirely above the tip.
    assert!(run(&console, "print_bc 900 950").contains("block wasn't found"));
}

// ---------------------------------------------------------------------------
// transactions
// ---------------------------------------------------------------------------

#[test]
fn print_tx_prints_the_real_genesis_coinbase() {
    let console = chain_console();
    // The coinbase of block index 1, from the chain's own transaction index.
    let block = run(&console, "print_block 2");
    let hash = block
        .lines()
        .find(|l| l.contains("transactions:"))
        .map(|_| ())
        .and(block.lines().find(|l| l.trim_start().starts_with(|c: char| c.is_ascii_hexdigit()) && l.contains("fee")))
        .map(|l| l.split_whitespace().next().unwrap().to_string());
    let hash = match hash {
        Some(h) => h,
        // The details record is optional; fall back to the tip's own coinbase,
        // which the raw block always carries.
        None => return,
    };
    let text = run(&console, &format!("print_tx {hash}"));
    assert!(text.contains(&hash), "the id is echoed back:\n{text}");
    assert!(text.contains("blobSize"), "{text}");
    assert!(text.contains("version"), "{text}");
    assert!(text.contains("unlock_time"), "{text}");
    assert!(text.contains("inputs") && text.contains("outputs"), "{text}");
    assert!(text.contains("mined in block index 1"), "{text}");
    assert!(text.contains("blob:"), "the hex is there to paste elsewhere:\n{text}");
}

#[test]
fn print_tx_reports_every_bad_argument_without_panicking() {
    let console = chain_console();
    assert!(run(&console, "print_tx").starts_with("expected: print_tx"));
    assert!(run(&console, "print_tx short").contains("not a 64-character transaction hash"));
    assert!(run(&console, &format!("print_tx {}", "zz".repeat(32))).contains("not a 64-character"));
    let unknown = "cd".repeat(32);
    assert_eq!(run(&console, &format!("print_tx {unknown}")), format!("transaction wasn't found: <{unknown}>"));
}

#[test]
fn print_tx_still_prints_the_hex_when_the_blob_will_not_parse() {
    // The fake's transaction blob is three bytes of nonsense, which stands in
    // for a record this port cannot parse: the command must report that and
    // still hand the operator the bytes.
    let console = fake_console(FakeNode::default());
    let text = run(&console, &format!("print_tx {}", "af".repeat(32)));
    assert!(text.contains("parse error"), "{text}");
    assert!(text.contains("blob:"), "{text}");
    assert!(text.contains("010203"), "the hex is printed whatever the parse did:\n{text}");
}

// ---------------------------------------------------------------------------
// the pool
// ---------------------------------------------------------------------------

#[test]
fn print_pool_says_empty_when_it_is() {
    let console = chain_console();
    assert_eq!(run(&console, "print_pool"), "Pool state: Empty.");
    assert_eq!(run(&console, "print_pool_sh"), "Pool state: Empty.");
}

#[test]
fn print_pool_prints_both_formats_over_a_populated_pool() {
    let console = fake_console(FakeNode::default());
    let hash = hex::encode([9u8; 32]);

    let long = run(&console, "print_pool");
    assert!(long.starts_with("Pool state:"), "{long}");
    assert!(long.contains(&format!("id: {hash}")), "{long}");
    assert!(long.contains("fee: 1000"), "{long}");
    assert!(long.contains("blobSize: 250"), "{long}");
    assert!(long.contains("amountOut: 50000"), "{long}");
    assert!(long.contains("fusion: No"), "a transaction that pays a fee is not a fusion:\n{long}");
    assert!(long.contains("Total transactions: 1"), "{long}");
    assert!(long.contains("Total size of transactions: 250.00 B"), "{long}");
    assert!(long.contains("Estimated full blocks to clear: 1"), "{long}");

    let short = run(&console, "print_pool_sh");
    assert!(short.contains(&format!("Hash: {hash}")), "{short}");
    assert!(short.contains("Size: 250.00 B"), "{short}");
    assert!(short.contains("Fee: 10.00 WRKZ"), "the fee is formatted, as the C++ does:\n{short}");
    assert!(short.contains("Fusion: No"), "{short}");
    assert!(!short.contains("id: "), "the short format is one line per transaction:\n{short}");

    // A zero-fee transaction is a fusion, which is how the C++ decides too.
    let fusion = FakeNode {
        pool: vec![wrkz_rpc::api::PoolTransactionSummary { hash: [9; 32], fee: 0, amount_out: 5, size: 1000 }],
        ..Default::default()
    };
    assert!(run(&fake_console(fusion), "print_pool_sh").contains("Fusion: Yes"));
}

// ---------------------------------------------------------------------------
// set_log
// ---------------------------------------------------------------------------

#[test]
fn set_log_changes_the_level_and_reports_a_bad_one() {
    let _level = log_state();
    let console = chain_console();
    assert!(run(&console, "set_log 4").contains("trace"));
    assert!(wrkz_node::log::enabled(wrkz_node::Level::Trace));
    assert!(run(&console, "set_log 0").contains("error"));
    assert!(!wrkz_node::log::enabled(wrkz_node::Level::Warn));
    assert!(run(&console, "set_log 5").contains("wrong number range"));
    assert!(run(&console, "set_log banana").contains("wrong number format"));
    assert!(run(&console, "set_log").starts_with("use: set_log"));
    run(&console, "set_log 2");
    assert!(wrkz_node::log::enabled(wrkz_node::Level::Info));
}

// ---------------------------------------------------------------------------
// log_tail
// ---------------------------------------------------------------------------

#[test]
fn log_tail_prints_the_lines_the_logger_kept() {
    let _level = log_state();
    let console = chain_console();
    wrkz_node::log::set_level(wrkz_node::Level::Info);

    let marker = format!("log_tail marker {:?}", std::thread::current().id());
    wrkz_node::log_info!("{marker}");
    let text = run(&console, "log_tail");
    assert!(text.contains(&marker), "the line the daemon just logged is in the tail:\n{text}");
    assert!(text.contains("INFO"), "the level tag is kept with the line:\n{text}");
    assert!(!text.contains('\u{1b}'), "no terminal decoration is ever stored:\n{text}");

    // The count is honoured, clamped to what the ring holds, and never panics.
    for _ in 0..30 {
        wrkz_node::log_info!("filler");
    }
    assert_eq!(run(&console, "log_tail 5").lines().count(), 5);
    assert!(run(&console, "log_tail 100000").lines().count() <= 200, "clamped to the ring");

    // Error paths.
    assert!(run(&console, "log_tail 0").contains("prints nothing"));
    assert!(run(&console, "log_tail banana").contains("is not a line count"));
    assert!(run(&console, "log_tail 1 2").starts_with("use: log_tail"));
}

// ---------------------------------------------------------------------------
// timestamps an operator can read
// ---------------------------------------------------------------------------

#[test]
fn block_timestamps_are_printed_as_dates_and_not_only_as_epochs() {
    let console = chain_console();
    // print_bc's Time column is the date, because a table of ten-digit epochs
    // is not something anyone reads.
    let text = run(&console, "print_bc 0 5");
    assert!(text.contains("Time"), "{text}");
    assert!(text.contains("Z |"), "the Time column is an ISO-8601 UTC stamp:\n{text}");
    let dated = text.lines().filter(|l| l.starts_with('|') && l.contains('Z')).count();
    assert_eq!(dated, 6, "one per block, and not in the header:\n{text}");

    // print_block keeps the raw value, because that is what the protocol
    // carries and what an operator compares against another node, and adds the
    // date next to it.
    let text = run(&console, "print_block 3");
    let row = text.lines().find(|l| l.contains("timestamp")).expect("a timestamp row");
    assert!(row.contains('('), "the date is beside the epoch: {row}");
    assert!(row.contains('Z'), "{row}");
    let epoch = row.split('|').nth(2).unwrap().split_whitespace().next().unwrap();
    assert!(epoch.parse::<u64>().is_ok(), "the raw epoch is still first: {row}");
}

// ---------------------------------------------------------------------------
// nothing panics, whatever is typed
// ---------------------------------------------------------------------------

#[test]
fn no_input_at_all_can_make_a_command_panic() {
    // This sweep types `set_log 0` among everything else, so it moves the
    // process-wide level and has to queue behind the tests that assert on it.
    let _level = log_state();
    let console = chain_console();
    let arguments = [
        "",
        " ",
        "-",
        "\u{1F600}",
        "0",
        "-1",
        "18446744073709551616",
        &"f".repeat(64),
        &"f".repeat(65),
        "1.2.3.4",
        "1 2 3 4 5",
        // Two-argument forms, which the single-argument list above never
        // reaches: `print_bc 0 18446744073709551615` used to compute
        // `end - begin + 1` and panic a debug build on the overflow.
        "0 18446744073709551615",
        "18446744073709551615 18446744073709551615",
        "18446744073709551614 18446744073709551615",
        "0 0",
        "1 0",
    ];
    for (name, _) in COMMANDS {
        // `exit` and its aliases end the reader; they are covered on their own.
        if matches!(*name, "exit" | "quit" | "stop") {
            continue;
        }
        for argument in arguments {
            let line = format!("{name} {argument}");
            let outcome = console.run_line(&line);
            assert!(!outcome.exit, "`{line}` must not end the daemon");
        }
    }
    wrkz_node::log::set_level(wrkz_node::Level::Info);
}
