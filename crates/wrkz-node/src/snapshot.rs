// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The daemon's side of lite node snapshots: the `snapshot_export` console
//! command (`DaemonCommandsHandler.cpp:1061-1318`).
//!
//! The walk and the file are `wrkz_chain::snapshot`; this is the command around
//! them — which height, which path, the refusals, and a worker thread the
//! console can ask about and cancel while the node carries on.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;

use wrkz_chain::snapshot::container::{self, Header, SnapshotResult};
use wrkz_chain::snapshot::export::{export_snapshot, ExportControl};
use wrkz_chain::{keys, ChainState};
use wrkz_primitives::constants::MIN_LITE_FULL_BLOCK_DEPTH;
use wrkz_storage::KvStore;

use crate::console::{dir_bytes, pretty_bytes};
use crate::daemon::available_bytes;

/// The usage line, the C++'s.
pub const USAGE: &str = "Usage: snapshot_export [start [height] [path] | status | cancel]";

/// What the console needs of an exporter, without the chain's store type.
pub trait SnapshotExport: Send + Sync {
    /// `snapshot_export …`, the words after the command.
    fn command(&self, args: &[&str]) -> String;

    /// Stop a running export and wait for it, so the daemon does not exit
    /// with a partial file behind it — `~DaemonCommandsHandler`, which sets
    /// the cancel flag before the future's destructor waits.
    fn shutdown(&self);
}

/// The `snapshot_export` command over the daemon's shared chain.
pub struct Exporter<S: KvStore + Send + Sync + 'static> {
    chain: Arc<RwLock<ChainState<S>>>,
    data_dir: PathBuf,
    min_depth: u64,
    task: Mutex<Task>,
}

#[derive(Default)]
struct Task {
    running: Option<Running>,
    last: Option<Result<(PathBuf, Header), (PathBuf, String)>>,
}

struct Running {
    path: PathBuf,
    started: Instant,
    control: Arc<ExportControl>,
    handle: JoinHandle<SnapshotResult<Header>>,
}

impl<S: KvStore + Send + Sync + 'static> Exporter<S> {
    /// An exporter over `chain`, whose state lives under `data_dir`.
    pub fn new(chain: Arc<RwLock<ChainState<S>>>, data_dir: PathBuf) -> Self {
        Self { chain, data_dir, min_depth: MIN_LITE_FULL_BLOCK_DEPTH, task: Mutex::new(Task::default()) }
    }

    /// Replace `MIN_LITE_FULL_BLOCK_DEPTH`, the settled margin a snapshot's
    /// region must be below the tip. For tests, whose chains are short.
    pub fn with_min_depth(mut self, depth: u64) -> Self {
        self.min_depth = depth;
        self
    }

    /// Collect a finished worker, so `status` reports its result.
    fn refresh(task: &mut Task) {
        if task.running.as_ref().is_some_and(|r| r.handle.is_finished()) {
            let running = task.running.take().expect("checked above");
            let result = running.handle.join().unwrap_or_else(|_| Err("the export thread panicked".into()));
            task.last = Some(match result {
                Ok(header) => Ok((running.path, header)),
                Err(e) => Err((running.path, e)),
            });
        }
    }

    fn status(task: &Task) -> String {
        if let Some(r) = &task.running {
            let p = r.control.progress();
            return format!(
                "Snapshot export: running ({}s elapsed)\n  Writing: {}\n  Table:   {}\n  \
                 Records: {} kept of {} scanned",
                r.started.elapsed().as_secs(),
                r.path.display(),
                if p.table.is_empty() { "starting" } else { p.table.as_str() },
                p.kept,
                p.scanned
            );
        }
        let mut out = "Snapshot export: idle".to_string();
        match &task.last {
            None => {}
            Some(Err((_, e))) => out.push_str(&format!("\nLast result: failed - {e}")),
            Some(Ok((path, header))) => out.push_str(&format!(
                "\nLast result: wrote {}\n  Lite height: {}\n  Records:     {}\n  Digest:      {}",
                path.display(),
                header.lite_height,
                header.total_records(),
                hex::encode(header.payload_digest)
            )),
        }
        out
    }

    /// `snapshot_export start [height] [path]`.
    fn start(&self, task: &mut Task, args: &[&str]) -> String {
        if task.running.is_some() {
            return "A snapshot export is already running. Use `snapshot_export status`.".into();
        }
        let (lite_height, top) = {
            let chain = self.chain.read().unwrap_or_else(PoisonError::into_inner);
            (chain.lite_start_height(), chain.tip_index().unwrap_or(0))
        };
        // The height is optional on a lite node, where the only sensible value
        // is the one its database was built at; a full node has to say. The
        // argument rule is the C++'s: a number while the height is still the
        // default and no path has been given, anything else the path.
        let mut height = lite_height;
        let mut path: Option<&str> = None;
        for arg in args {
            let numeric = !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit());
            if numeric && height == lite_height && path.is_none() {
                match arg.parse::<u32>() {
                    Ok(h) => height = h,
                    Err(_) => return format!("Could not read {arg} as a height."),
                }
            } else {
                path = Some(arg);
            }
        }
        if height == 0 {
            return "This node has no lite height, so a snapshot height has to be given:\n  \
                    snapshot_export start <height> [path]"
                .into();
        }
        if lite_height != 0 && height != lite_height {
            return format!(
                "This is a lite node built at height {lite_height}, and it does not hold the block data a snapshot \
                 at {height} would need."
            );
        }
        // The exported region can never be reorganised away: the same margin
        // lite mode demands of its own line.
        let settled_from = u64::from(height) + self.min_depth;
        if u64::from(top) + 1 < settled_from {
            return format!(
                "This node is at height {} and a snapshot at {height} needs it to be at least {settled_from}, so \
                 the exported region is beyond any reorg.",
                u64::from(top) + 1
            );
        }
        let output = container::default_output_path(&self.data_dir, path.map(Path::new), height);
        if output.exists() {
            return format!("{} already exists. Move it aside or name another path.", output.display());
        }
        // A snapshot is smaller than the database it comes out of, so the
        // database's size is a conservative floor that costs nothing.
        let database = dir_bytes(&self.data_dir.join("state")).unwrap_or(0);
        let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let available = available_bytes(parent);
        if available < database {
            return format!(
                "Only {} free where the snapshot would go, and the database it comes from is {}.",
                pretty_bytes(available),
                pretty_bytes(database)
            );
        }

        let control = Arc::new(ExportControl::new());
        let handle = {
            let (chain, control, output) = (Arc::clone(&self.chain), Arc::clone(&control), output.clone());
            let spawned = std::thread::Builder::new().name("snapshot-export".into()).spawn(move || {
                let result = export_snapshot(&chain, &output, height, &control);
                match &result {
                    Ok(header) => crate::log_info!(
                        "snapshot export finished: wrote {}, {} records, digest {}",
                        output.display(),
                        header.total_records(),
                        hex::encode(header.payload_digest)
                    ),
                    Err(e) => crate::log_warn!("snapshot export of {} stopped: {e}", output.display()),
                }
                result
            });
            match spawned {
                Ok(handle) => handle,
                Err(e) => return format!("Could not start the export thread: {e}"),
            }
        };
        task.last = None;
        task.running = Some(Running { path: output.clone(), started: Instant::now(), control, handle });
        format!(
            "Exporting a lite node snapshot at height {height} to {}\nThis walks the whole database and takes tens of \
             minutes. Follow it with `snapshot_export status`.",
            output.display()
        )
    }
}

impl<S: KvStore + Send + Sync + 'static> SnapshotExport for Exporter<S> {
    fn command(&self, args: &[&str]) -> String {
        let sub = args.first().copied().unwrap_or("start");
        if !matches!(sub, "start" | "status" | "cancel") {
            return USAGE.into();
        }
        let mut task = self.task.lock().unwrap_or_else(PoisonError::into_inner);
        Self::refresh(&mut task);
        match sub {
            "status" => Self::status(&task),
            "cancel" => match &task.running {
                Some(r) => {
                    r.control.cancel();
                    "Cancelling the snapshot export. The partial file will be removed.".into()
                }
                None => "No snapshot export is running.".into(),
            },
            _ => self.start(&mut task, args.get(1..).unwrap_or(&[])),
        }
    }

    fn shutdown(&self) {
        let running = self.task.lock().unwrap_or_else(PoisonError::into_inner).running.take();
        if let Some(r) = running {
            r.control.cancel();
            let _ = r.handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// --snapshot-stats
// ---------------------------------------------------------------------------

/// One table of the state, measured by `--snapshot-stats`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableStats {
    pub name: &'static str,
    pub records: u64,
    pub key_bytes: u64,
    pub value_bytes: u64,
}

impl TableStats {
    pub fn total_bytes(&self) -> u64 {
        self.key_bytes + self.value_bytes
    }
}

/// This port's tables, named for what they hold and marked the way
/// `DatabaseBlockchainCache::measureStorage` marks the C++'s
/// (`DatabaseBlockchainCache.cpp:3294-3312`): what a snapshot carries, what an
/// import derives, what a lite node drops — and, this port's own mark, what a
/// node imported from a snapshot does not have below its height.
pub const STORAGE_TABLES: &[(u8, &str)] = &[
    (keys::TAG_OUTPUT, "key output info (snapshot)"),
    (keys::TAG_KEY_IMAGE, "spent key images (snapshot)"),
    (keys::TAG_BLOCK_INFO, "block info (snapshot)"),
    (keys::TAG_HASH_TO_INDEX, "block hash to index (derived)"),
    (keys::TAG_OUTPUT_COUNT, "key output amounts (derived)"),
    (keys::TAG_RAW_BLOCK, "raw blocks (lite drops)"),
    (keys::TAG_TRANSACTION_INDEX, "transaction index (import drops)"),
    (keys::TAG_BLOCK_TX_HASHES, "block tx hashes (import drops)"),
    (keys::TAG_BLOCK_KEY_IMAGES, "block key images (import drops)"),
    (keys::TAG_BLOCK_OUTPUTS, "block output refs (import drops)"),
    (keys::TAG_PAYMENT_ID, "payment id lists (import drops)"),
    (keys::TAG_PAYMENT_ID_ENTRY, "payment id entries (import drops)"),
    (keys::TAG_PAYMENT_ID_COUNT, "payment id counts (import drops)"),
    (keys::TAG_BLOCK_PAYMENT_IDS, "block payment ids (import drops)"),
    (keys::TAG_META, "state metadata"),
];

/// Walk every table of [`STORAGE_TABLES`] a page at a time, reporting each as it
/// lands (`measureStorage`'s log lines), and return what each holds.
pub fn measure_storage<S: KvStore>(store: &S, log: &mut dyn FnMut(String)) -> Result<Vec<TableStats>, String> {
    let mut out = Vec::with_capacity(STORAGE_TABLES.len());
    for (n, (tag, name)) in STORAGE_TABLES.iter().enumerate() {
        log(format!("measureStorage: walking {name} ({}/{})...", n + 1, STORAGE_TABLES.len()));
        let started = Instant::now();
        let mut stats = TableStats { name, records: 0, key_bytes: 0, value_bytes: 0 };
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = store
                .scan(&[keys::NS, *tag], after.as_deref(), 65_536)
                .map_err(|e| format!("measureStorage: failed walking {name}: {e}"))?;
            for (key, value) in &page.entries {
                stats.records += 1;
                stats.key_bytes += key.len() as u64;
                stats.value_bytes += value.len() as u64;
            }
            match page.resume_after {
                Some(key) => after = Some(key),
                None => break,
            }
        }
        log(format!(
            "measureStorage: {name}: {} records, {} MB logical, {}s",
            stats.records,
            stats.total_bytes() / (1024 * 1024),
            started.elapsed().as_secs()
        ));
        out.push(stats);
    }
    Ok(out)
}

/// The `(snapshot)` tables' records at the C++ encoding's sizes, as
/// `(records, bytes)`: what the C++'s own "Snapshot payload (logical)" row
/// reports for the same chain, over whole tables as it measures them, and so
/// roughly a `.litesnap` payload before compression (which adds a LEB128 length
/// pair per record).
pub fn cpp_snapshot_payload(stats: &[TableStats]) -> (u64, u64) {
    use wrkz_storage::fixed;
    let each = |name: &str| match name {
        "key output info (snapshot)" => Some(fixed::KEY_OUTPUT_KEY_LEN + fixed::KEY_OUTPUT_VALUE_LEN),
        "spent key images (snapshot)" => Some(fixed::KEY_IMAGE_KEY_LEN + fixed::KEY_IMAGE_VALUE_LEN),
        "block info (snapshot)" => Some(fixed::BLOCK_INFO_KEY_LEN + fixed::BLOCK_INFO_VALUE_LEN),
        _ => None,
    };
    stats
        .iter()
        .filter_map(|t| each(t.name).map(|size| (t.records, t.records * size as u64)))
        .fold((0, 0), |(records, bytes), (r, b)| (records + r, bytes + b))
}

/// The bytes of every file under `path`, `None` when it does not exist.
pub fn directory_bytes(path: &Path) -> Option<u64> {
    dir_bytes(path)
}

/// The table `--snapshot-stats` prints (`Daemon.cpp:903-953`), over this port's
/// tables.
pub fn render_storage_stats(stats: &[TableStats], top_index: u32, on_disk_bytes: Option<u64>) -> String {
    let mb = |bytes: u64| format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0));
    let row = |name: &str, records: String, size: String| format!("{name:<36}{records:>14}{size:>16}\n");
    let rule = "-".repeat(78);
    let mut out = format!("\nStorage by table, logical bytes (top block {top_index})\n{rule}\n");
    let (mut total, mut snapshot_records, mut snapshot_bytes) = (0u64, 0u64, 0u64);
    for t in stats {
        total += t.total_bytes();
        if t.name.contains("(snapshot)") {
            snapshot_records += t.records;
            snapshot_bytes += t.total_bytes();
        }
        out.push_str(&row(t.name, t.records.to_string(), mb(t.total_bytes())));
    }
    let (cpp_records, cpp_bytes) = cpp_snapshot_payload(stats);
    out.push_str(&format!("{rule}\n"));
    out.push_str(&row("TOTAL measured (logical)", String::new(), mb(total)));
    out.push_str(&row("Snapshot tables here (logical)", snapshot_records.to_string(), mb(snapshot_bytes)));
    out.push_str(&row("C++ snapshot payload (logical)", cpp_records.to_string(), mb(cpp_bytes)));
    if let Some(disk) = on_disk_bytes.filter(|b| *b > 0) {
        out.push_str(&row("On disk (compressed)", String::new(), mb(disk)));
        out.push_str(&row("Compression ratio", String::new(), format!("{:.2}x", total as f64 / disk as f64)));
    }
    out.push_str(
        "\nThese are logical key and value bytes of this port's own tables, not what the\n\
         database occupies and not the C++ node's tables. The (snapshot) rows are the\n\
         three tables a lite node snapshot carries, in this port's encoding; the C++\n\
         payload row is the same records at the size the .litesnap file holds them,\n\
         over whole tables as the C++ measures them. (derived) rows are rebuilt by an\n\
         import, (lite drops) are never kept below a lite height, and (import drops) are\n\
         what a node imported from a snapshot does not have below its height. See\n\
         docs/DAEMON.md, \"Lite node snapshots\".\n\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wrkz_chain::{Checkpoints, Config};
    use wrkz_storage::MemStore;

    fn chain(lite: u32) -> Arc<RwLock<ChainState<MemStore>>> {
        let cfg = Config { lite_start_height: lite, ..Config::default() };
        Arc::new(RwLock::new(ChainState::open_or_genesis(MemStore::default(), cfg, Checkpoints::none()).unwrap()))
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wrkz-node-export-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_storage_table_measures_this_ports_tables() {
        let chain = chain(0);
        let state = chain.read().unwrap();
        let mut lines = Vec::new();
        let stats = measure_storage(state.store(), &mut |line| lines.push(line)).unwrap();
        assert_eq!(stats.len(), STORAGE_TABLES.len());
        let table = |name: &str| stats.iter().find(|t| t.name == name).unwrap().clone();
        // Genesis: one block info (a 6-byte key, a 68-byte record) and three
        // outputs of one amount.
        assert_eq!((table("block info (snapshot)").records, table("block info (snapshot)").total_bytes()), (1, 74));
        assert_eq!(table("key output info (snapshot)").records, 3);
        assert_eq!(table("spent key images (snapshot)").records, 0);
        assert_eq!(cpp_snapshot_payload(&stats), (4, 238 + 3 * 223), "35 + 203 and 59 + 164 bytes a record");
        assert_eq!(lines[0], "measureStorage: walking key output info (snapshot) (1/15)...");

        let text = render_storage_stats(&stats, 0, Some(1 << 20));
        assert!(text.contains("Storage by table, logical bytes (top block 0)"), "{text}");
        assert!(text.contains(&format!("{:<36}{:>14}{:>16}", "block info (snapshot)", 1, "0.0 MB")), "{text}");
        assert!(text.contains("C++ snapshot payload (logical)                   4          0.0 MB"), "{text}");
        assert!(text.contains("Compression ratio"), "{text}");
        assert!(!render_storage_stats(&stats, 0, None).contains("Compression ratio"));
    }

    #[test]
    fn the_command_refuses_what_the_cpp_refuses() {
        let dir = scratch("refusals");
        let full = Exporter::new(chain(0), dir.join("data"));
        assert_eq!(full.command(&["frobnicate"]), USAGE);
        assert!(full.command(&[]).starts_with("This node has no lite height, so a snapshot height has to be given"));
        assert_eq!(full.command(&["status"]), "Snapshot export: idle");
        assert_eq!(full.command(&["cancel"]), "No snapshot export is running.");
        let e = full.command(&["start", "1"]);
        assert_eq!(
            e,
            "This node is at height 1 and a snapshot at 1 needs it to be at least 20161, so the exported region is \
             beyond any reorg."
        );

        let lite = Exporter::new(chain(5), dir.join("data")).with_min_depth(0);
        let e = lite.command(&["start", "7"]);
        assert!(e.starts_with("This is a lite node built at height 5, and it does not hold"), "{e}");

        let existing = dir.join("taken.litesnap");
        std::fs::write(&existing, b"x").unwrap();
        let full = Exporter::new(chain(0), dir.join("data")).with_min_depth(0);
        let e = full.command(&["start", "1", existing.to_str().unwrap()]);
        assert!(e.ends_with("already exists. Move it aside or name another path."), "{e}");
        let _ = std::fs::remove_file(&existing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_export_runs_in_the_background_and_reports_its_digest() {
        let dir = scratch("runs");
        let exporter = Exporter::new(chain(0), dir.join("data")).with_min_depth(0);
        let out = exporter.command(&["start", "1", dir.to_str().unwrap()]);
        let path = dir.join("wrkz-lite-base-h1-v1.litesnap");
        assert!(out.starts_with(&format!("Exporting a lite node snapshot at height 1 to {}", path.display())), "{out}");
        let mut status = exporter.command(&["status"]);
        for _ in 0..600 {
            if status.starts_with("Snapshot export: idle") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            status = exporter.command(&["status"]);
        }
        assert!(status.contains(&format!("Last result: wrote {}", path.display())), "{status}");
        assert!(status.contains("Lite height: 1") && status.contains("Records:     4"), "{status}");
        let header = container::read_header(&path).unwrap();
        assert!(status.ends_with(&hex::encode(header.payload_digest)), "{status}");
        exporter.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
