// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The pieces the daemon binary needs that are worth testing on their own: the
//! shutdown flag and its signal handlers, the data-directory lock, the state
//! tag check, and the periodic status line.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The tag `wrkz-replay` writes for a **linear** replay: a chain validated from
/// genesis, block by block, which is a complete chain safe to serve.
pub const TAG_LINEAR: &str = "linear";
/// The tag a **windowed** replay writes. Such a state is seeded with block
/// infos that were never validated and is full of holes by design; a daemon
/// must never serve it or sync on top of it.
pub const TAG_WINDOWS: &str = "windows";
/// The tag `--import-lite-snapshot` leaves on a finished import: a lite node
/// whose region below its lite height came from a C++ snapshot and so holds no
/// transaction records ([`wrkz_chain::keys::TAG_LITE_SNAPSHOT`]).
pub const TAG_LITE_SNAPSHOT: &str = wrkz_chain::keys::TAG_LITE_SNAPSHOT;
/// The tag an import carries while it writes. A state still carrying it was
/// interrupted and holds part of a snapshot.
pub const TAG_LITE_SNAPSHOT_IMPORTING: &str = wrkz_chain::keys::TAG_LITE_SNAPSHOT_IMPORTING;

/// Whether a state directory with this tag may be served.
///
/// - `None` — either a fresh state this daemon created, or one it built itself
///   by syncing. `wrkz-replay` refuses to adopt an untagged directory that
///   already holds blocks, so an untagged non-empty state came from a daemon.
/// - `"linear"` — `wrkz-replay --state DIR` from a C++ database, validated from
///   genesis. This is the recommended way to bring a node up (`docs/DAEMON.md`).
/// - `"lite-snapshot"` — `--import-lite-snapshot`, finished. Served as the lite
///   node it is: the state records its lite height, so it opens only with the
///   matching `--lite --lite-height`, and every transaction question below that
///   height is refused rather than answered ([`wrkz_rpc::api::BodyPolicy`]).
/// - `"windows"` and `"lite-snapshot-importing"` — refused.
pub fn check_state_tag(tag: Option<&str>) -> Result<(), String> {
    match tag {
        None | Some(TAG_LINEAR) | Some(TAG_LITE_SNAPSHOT) => Ok(()),
        Some(TAG_LITE_SNAPSHOT_IMPORTING) => Err(format!(
            "this state directory holds a lite snapshot import that did not finish (`{TAG_LITE_SNAPSHOT_IMPORTING}`): \
             it has part of the snapshot's records and no chain tip to go with them, so it must never be served. \
             Delete it, and run --import-lite-snapshot again into an empty --data-dir."
        )),
        Some(TAG_WINDOWS) => Err(format!(
            "this state directory was written by a windowed replay (`{TAG_WINDOWS}`). A windowed \
             replay seeds block infos it never validated, so the chain in it has holes and must \
             never be served or synced on top of. Point --data-dir at a different directory, or \
             rebuild it with `wrkz-replay --state DIR` (a linear replay)."
        )),
        Some(other) => Err(format!(
            "this state directory carries the tag `{other}`, which this daemon does not know how \
             to serve. Point --data-dir at a different directory."
        )),
    }
}

/// The lowest height whose **block body** this state actually holds, or `None`
/// when it holds none at all.
///
/// `wrkz-replay` defaults to `store_raw = false`: the state it writes then has
/// every index, output, key image and transaction-index entry, and no block or
/// transaction bytes. Such a node validates and follows the tip perfectly and
/// is a third the size — but below the import height it cannot serve a block to
/// a syncing peer, cannot answer `/getrawblocks`, `/getwalletsyncdata` or the
/// explorer methods, and could not put an unwound block back as an alternative.
/// An operator has to be told that at start-up, not by a wallet failing a week
/// later.
///
/// The probe is a binary search over the body predicate, not a scan: bodies are
/// present from some height upward and absent below it (an import writes none,
/// and everything the daemon adds afterwards has one), so about
/// `log2(tip)` — 22 lookups on mainnet — settles it. `Err` from the store is
/// treated as "no body here", which can only make the answer more pessimistic.
pub fn lowest_stored_block<S: wrkz_storage::KvStore>(chain: &wrkz_chain::ChainState<S>) -> Option<u32> {
    let has = |index: u32| chain.raw_block(index).ok().flatten().is_some();
    let tip = chain.tip_index()?;
    if !has(tip) {
        return None;
    }
    if has(0) {
        return Some(0);
    }
    // Invariant: `lo` has no body, `hi` has one. Both hold going in, and the
    // gap halves every round, so this terminates in `log2(tip)` iterations.
    let (mut lo, mut hi) = (0u32, tip);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if has(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

/// The chain state's directory inside `--data-dir`.
pub const STATE_DIR: &str = "state";

/// `--resync` (`Daemon.cpp:495-513`): delete the chain state and the peer
/// state, so the daemon starts at genesis and syncs from the network. Returns
/// what was deleted; what is not there is not an error.
///
/// The caller must already hold the data directory's [`DirLock`]. The C++
/// deletes without checking whether a daemon is running on the directory;
/// taking the lock first means a second daemon's `--resync` can never delete
/// the state a running one is writing.
///
/// The C++ deletes `DB` and `p2pstate.wrkz.bin`. Here the state is
/// [`STATE_DIR`], and two more peer files sit beside the C++ one:
///
/// - `p2panchors.wrkz.txt`, the outbound peers of the last run, **goes**. It is
///   peer state like the file the C++ deletes, and a resync is how an operator
///   starts over from a chain they no longer trust — the peers that served it
///   are the last ones to dial first.
/// - `p2pbans.wrkz.txt` **stays**. A ban is a decision, not a cache: the
///   operator's `ban add`, or a peer that misbehaved badly enough to be shut
///   out for a day. A node resyncing from genesis is the node that most needs
///   to keep those peers out.
pub fn resync(data_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    for name in [STATE_DIR, wrkz_primitives::constants::P2P_NET_DATA_FILENAME, crate::peers::ANCHORS_FILENAME] {
        let path = data_dir.join(name);
        let removal = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => Err(e),
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
        };
        // The C++'s message (`Daemon.cpp:510`), with the reason it lacks.
        removal.map_err(|e| format!("Could not delete data path: \"{}\" ({e})", path.display()))?;
        removed.push(path);
    }
    Ok(removed)
}

/// What [`rewind_to_height`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewind {
    /// The chain held no more blocks than asked for; nothing changed.
    NotNeeded { height: u64 },
    /// `removed` blocks went, and the chain is `height` blocks tall.
    Rewound { removed: u32, height: u64 },
}

/// `--rewind-to-height N` (`Daemon.cpp:871-887`, `Core::rewind`,
/// `Core.cpp:3991-4013`): leave a chain of `N` blocks, the block at `N - 1`
/// on top, and commit it.
///
/// The C++'s limits, in its order: a lite node refuses a height below its lite
/// height, and no rewind may remove blocks more than
/// `MAX_BLOCK_ALLOWED_TO_REWIND` (4,320) below the top. Where this differs:
///
/// - **A rewind that cannot be done is an error**, which the daemon exits on.
///   The C++ logs the depth limit at `INFO` and starts anyway, on a chain the
///   operator explicitly asked not to be on.
/// - **`N` is always the height afterwards.** The C++'s early return
///   `height >= currentTop` (`DatabaseBlockchainCache.cpp:984`) compares a
///   count with an index, so `N` equal to the top index does nothing there
///   while `N - 1` removes two blocks. Here it removes the top block.
/// - **A short chain can be rewound.** The C++ also refuses any rewind on a
///   chain of fewer than 4,320 blocks, which protects nothing it records.
/// - **Missing data is refused before anything is written.** This state undoes
///   a block from its own records, or rebuilds them from the body; where it
///   has neither, the rewind is refused with the state untouched
///   ([`wrkz_chain::ChainState::rewind_to`]).
pub fn rewind_to_height<S: wrkz_storage::KvStore>(
    chain: &mut wrkz_chain::ChainState<S>,
    height: u32,
) -> Result<Rewind, String> {
    use wrkz_chain::Rule;
    use wrkz_primitives::constants::MAX_BLOCK_ALLOWED_TO_REWIND;

    let tip = chain.tip_index().ok_or("the chain state has no genesis block")?;
    if height == 0 {
        return Err("Please use the `--resync` option instead of `--rewind-to-height 0` to completely reset the \
                    synchronization state."
            .to_string());
    }
    let lite = chain.lite_start_height();
    if lite != 0 && height < lite {
        return Err(format!(
            "Cannot rewind to {height} on a lite node whose full block data starts at {lite}. The blocks below that \
             height were never stored."
        ));
    }
    let count = u64::from(tip) + 1;
    if u64::from(height) >= count {
        return Ok(Rewind::NotNeeded { height: count });
    }
    if u64::from(tip - height) > MAX_BLOCK_ALLOWED_TO_REWIND {
        return Err(format!(
            "Cannot rewind to {height}: the chain is {count} blocks tall and a rewind may reach at most \
             {MAX_BLOCK_ALLOWED_TO_REWIND} blocks below its top, so you can only rewind to {}. Nothing was changed. \
             Use --resync to rebuild the chain from the network instead.",
            u64::from(tip) - MAX_BLOCK_ALLOWED_TO_REWIND
        ));
    }
    let removed = chain.rewind_to(height - 1).map_err(|e| match e.rule() {
        Some(Rule::ReorganisationUnavailable { at_index }) => format!(
            "Cannot rewind to {height}: this state holds neither the record of the outputs block {at_index} created \
             nor that block's body, so they cannot be taken back out. Nothing was changed. Use --resync to rebuild \
             the chain from the network instead."
        ),
        _ => format!("Cannot rewind to {height}: {e}. Nothing was changed."),
    })?;
    chain.flush().map_err(|e| format!("rewinding to {height}: committing the chain state: {e}"))?;
    Ok(Rewind::Rewound { removed, height: u64::from(height) })
}

/// Free bytes on the filesystem holding `path`, or `u64::MAX` when it cannot be
/// determined.
///
/// `u64::MAX` on failure is the C++'s own choice (`getAvailableBytes`,
/// `DaemonCommandsHandler.cpp:142-151`) and it is worth being explicit about
/// what it means: an unreadable path reads as *infinite* free space, so the
/// low-space paths never fire. That is the safe direction for a *forced* prune
/// — a node that cannot see its disk does not start deleting because of it —
/// and it is why [`AutoPrune`] treats free space as a trigger and never as a
/// permission.
pub fn available_bytes(path: &Path) -> u64 {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                free_bytes_available_to_caller: *mut u64,
                total_bytes: *mut u64,
                total_free_bytes: *mut u64,
            ) -> i32;
        }
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available = 0u64;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
        // call, and the three out-parameters are valid writable `u64`s.
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) };
        if ok == 0 {
            return u64::MAX;
        }
        available
    }
    #[cfg(unix)]
    {
        // The `libc` crate's `struct statvfs`, not one declared here: the
        // layout differs between glibc, musl, bionic and macOS (where the block
        // counts are 32-bit), and reading it as an array of `u64`s once took
        // field 5 — `f_files`, the inode count — for `f_bavail`, which is
        // field 4 on glibc and at a different offset again on macOS.
        use std::os::unix::ffi::OsStrExt;
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return u64::MAX };
        // SAFETY: `c_path` is NUL-terminated and outlives the call, and `stat`
        // is a writable `struct statvfs` of this platform's layout.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return u64::MAX;
        }
        #[allow(clippy::unnecessary_cast, reason = "32-bit on macOS and 32-bit Linux, 64-bit elsewhere")]
        let (available, fragment, block) = (stat.f_bavail as u64, stat.f_frsize as u64, stat.f_bsize as u64);
        // POSIX counts `f_bavail` in units of `f_frsize`; a filesystem that
        // leaves it zero means `f_bsize`.
        available.saturating_mul(if fragment != 0 { fragment } else { block })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        u64::MAX
    }
}

/// When a pruned node runs a catch-up prune pass.
///
/// [`wrkz_chain::ChainState::push_block`] drops one body per block applied, so
/// a node that has always been pruned needs nothing here. This is for the case
/// that is not steady: a full database given `--prune`, or one whose depth was
/// lowered. The C++ scheduler is the model
/// (`DaemonCommandsHandler.cpp:1580-1615`), including the part that reads
/// oddly at first: **low free space forces a prune rather than preventing
/// one**, which is what `--auto-prune-min-free-bytes`'s help text means by
/// "low-space mode can still force prune".
///
/// `min_gap_blocks == 0` disables the periodic schedule but not the forced
/// path, exactly as the C++ `||` does.
#[derive(Clone, Debug)]
pub struct AutoPrune {
    /// `--auto-prune-min-gap-blocks`.
    pub min_gap_blocks: u32,
    /// `--auto-prune-min-free-bytes`.
    pub min_free_bytes: u64,
    /// The height the last pass ran at; 0 means none has.
    last_height: u32,
}

impl AutoPrune {
    pub fn new(min_gap_blocks: u32, min_free_bytes: u64) -> Self {
        Self { min_gap_blocks, min_free_bytes, last_height: 0 }
    }

    /// Whether to run a pass now, and whether low space is what forced it.
    /// Records the height when it says yes.
    pub fn due(&mut self, height: u32, free_bytes: u64) -> Option<PruneReason> {
        let low_space = free_bytes < self.min_free_bytes;
        let gap_reached = self.min_gap_blocks != 0
            && (self.last_height == 0 || height.saturating_sub(self.last_height) >= self.min_gap_blocks);
        if !gap_reached && !low_space {
            return None;
        }
        self.last_height = height;
        Some(if !gap_reached { PruneReason::LowSpace(free_bytes) } else { PruneReason::Scheduled })
    }
}

/// Why [`AutoPrune::due`] said yes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneReason {
    /// The block gap since the last pass was reached.
    Scheduled,
    /// Free space fell below `--auto-prune-min-free-bytes`, which forces a pass
    /// whatever the gap says.
    LowSpace(u64),
}

/// A lock file in the data directory, so two daemons cannot share one state.
///
/// Advisory and deliberately simple: the file holds our process id, and the
/// lock is released by deleting it on a clean shutdown. A stale file after a
/// crash is reported with the pid that wrote it and the path to remove, which
/// is more useful than silently taking a directory a live process is writing.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
}

impl DirLock {
    /// `Err` names the pid in the existing file, if it could be read.
    pub fn acquire(dir: &Path) -> Result<Self, String> {
        let path = dir.join("wrkz-node.pid");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let holder = holder.trim();
                Err(format!(
                    "the data directory {} is locked by process {}. If no daemon is running, \
                     delete {} and start again.",
                    dir.display(),
                    if holder.is_empty() { "?" } else { holder },
                    path.display()
                ))
            }
            Err(e) => Err(format!("cannot create {}: {e}", path.display())),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A flag a signal handler sets and the main loop polls.
///
/// No extra crate: `signal(2)` is declared here and the handler does the one
/// thing a signal handler is allowed to do, which is store to an atomic. On
/// Windows the same flag is set by a console control handler.
#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The one flag the signal handlers can reach. A handler may not touch
/// anything but an atomic, so this is a static rather than a captured value.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Install handlers for SIGINT and SIGTERM (or the console control handler on
/// Windows) and return the flag they set.
///
/// Safe to call more than once; the last call wins and every returned handle
/// watches the same flag.
pub fn install_signal_handlers() -> Shutdown {
    #[cfg(unix)]
    {
        // `signal(2)`: the two the daemon must answer, plus SIGHUP, which
        // systemd's `ExecReload` and a closing terminal both send.
        const SIGHUP: i32 = 1;
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        unsafe extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        extern "C" fn handler(_signum: i32) {
            SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        }
        // SAFETY: `signal` is the C library's, and `handler` does nothing but
        // an atomic store, which is async-signal-safe.
        // Through a function pointer: casting the function item straight to an
        // integer is linted (`function_casts_as_integer`), and CI denies it.
        let handler = handler as extern "C" fn(i32) as usize;
        unsafe {
            signal(SIGINT, handler);
            signal(SIGTERM, handler);
            signal(SIGHUP, handler);
        }
    }
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
        }
        unsafe extern "system" fn handler(_event: u32) -> i32 {
            SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
            1
        }
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
    Shutdown::new()
}

impl Shutdown {
    /// True when a signal arrived, or [`Shutdown::request`] was called on this
    /// handle or one it was cloned from.
    pub fn triggered(&self) -> bool {
        self.requested() || SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
    }
}

/// The periodic line an operator watches while a node syncs.
///
/// Rate is measured over the interval, not since start, so a node that stalls
/// reports 0 blocks/s rather than a comfortable average.
pub struct StatusLine {
    interval: Duration,
    last: Instant,
    last_height: u32,
}

impl StatusLine {
    pub fn new(interval: Duration, height: u32) -> Self {
        Self { interval, last: Instant::now(), last_height: height }
    }

    /// The line to print, or `None` when the interval has not elapsed.
    pub fn tick(&mut self, s: &StatusSnapshot) -> Option<String> {
        let elapsed = self.last.elapsed();
        if elapsed < self.interval {
            return None;
        }
        let gained = s.height.saturating_sub(self.last_height);
        let rate = gained as f64 / elapsed.as_secs_f64().max(0.001);
        self.last = Instant::now();
        self.last_height = s.height;
        let target = if s.network_height > s.height { format!("/{}", s.network_height) } else { String::new() };
        // Only a node that is behind and moving has an estimate worth printing.
        let behind = s.network_height.saturating_sub(s.height);
        let eta = if behind > 0 && rate >= 0.05 {
            format!("  ETA {}", format_eta(behind as f64 / rate))
        } else {
            String::new()
        };
        Some(format!(
            "height {}{target}  peers {}in/{}out  pool {}  {:.1} blocks/s{eta}{}",
            s.height,
            s.incoming,
            s.outgoing,
            s.pool,
            rate,
            if s.synced { "  (synced)" } else { "" }
        ))
    }
}

/// A coarse duration for a human: the two largest units, no more.
pub fn format_eta(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    let (days, hours, minutes, seconds) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// `LITE_DEPTH_CHECK_MIN_SAMPLES` (`CryptoNoteProtocolHandler.cpp:48`): the
/// peer reports the lite-height check weighs before it may stop the node.
pub const LITE_DEPTH_CHECK_MIN_SAMPLES: u32 = 4;

/// The lite-height depth check (`CryptoNoteProtocolHandler.cpp:415-448`).
///
/// A lite node keeps no block bodies below its lite height, so no
/// reorganisation may ever reach that far: the C++ requires at least
/// `MIN_LITE_FULL_BLOCK_DEPTH` (20,160) blocks of full data above it and exits
/// when the network is not that tall. What is weighed is the tallest chain any
/// peer has claimed, over several reports, so a short answer cannot lower the
/// result and one peer cannot produce the fatal verdict alone. Our own chain
/// settles it without asking anyone once it is tall enough, which covers every
/// restart of a synced node.
#[derive(Clone, Debug, Default)]
pub struct LiteDepthCheck {
    max_peer_height: u64,
    samples: u32,
    settled: bool,
}

impl LiteDepthCheck {
    /// Weigh one peer's claimed height. Heights are counts. `Err` carries the
    /// C++'s fatal message; after either verdict the check is settled and
    /// reports nothing more.
    pub fn observe(&mut self, lite_height: u32, our_height: u64, peer_height: u64) -> Result<(), String> {
        if self.settled || lite_height == 0 || peer_height == 0 {
            return Ok(());
        }
        let required = wrkz_primitives::constants::MIN_LITE_FULL_BLOCK_DEPTH;
        let needed = u64::from(lite_height) + required;
        self.max_peer_height = self.max_peer_height.max(peer_height);
        self.samples += 1;
        let network_height = our_height.max(self.max_peer_height);
        if network_height >= needed {
            // Settled for good: the margin only widens as the chain grows.
            self.settled = true;
            return Ok(());
        }
        if self.samples < LITE_DEPTH_CHECK_MIN_SAMPLES {
            return Ok(());
        }
        self.settled = true;
        let max_allowed = network_height.saturating_sub(required);
        Err(format!(
            "--lite-height {lite_height} is too close to the network top ({network_height}, the tallest chain \
             seen across {} peers). A lite node must keep at least {required} blocks of full data above its \
             lite height so a reorg can never reach the part it did not store. The highest value this network \
             currently allows is {max_allowed}. Delete the data directory and restart with a lower --lite-height.",
            self.samples
        ))
    }

    pub fn is_settled(&self) -> bool {
        self.settled
    }
}

/// What the status line prints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusSnapshot {
    /// A **count**, as `/height` reports it.
    pub height: u32,
    pub network_height: u32,
    pub incoming: usize,
    pub outgoing: usize,
    pub pool: usize,
    pub synced: bool,
}

/// Open a log file for appending, creating it and its directory.
pub fn open_log_file(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windowed_state_is_refused_and_a_linear_one_is_served() {
        assert!(check_state_tag(None).is_ok(), "a fresh or daemon-built state");
        assert!(check_state_tag(Some(TAG_LINEAR)).is_ok(), "a linear replay is a complete chain");
        let e = check_state_tag(Some(TAG_WINDOWS)).unwrap_err();
        assert!(e.contains("windowed replay"), "{e}");
        assert!(e.contains("wrkz-replay --state"), "the message says how to fix it: {e}");
        assert!(check_state_tag(Some("something else")).is_err());
        assert!(check_state_tag(Some(TAG_LITE_SNAPSHOT)).is_ok(), "a finished snapshot import is a lite node");
        let e = check_state_tag(Some(TAG_LITE_SNAPSHOT_IMPORTING)).unwrap_err();
        assert!(e.contains("did not finish") && e.contains("--import-lite-snapshot"), "{e}");
    }

    #[test]
    fn the_directory_lock_refuses_a_second_holder_and_releases_on_drop() {
        let dir = std::env::temp_dir().join(format!("wrkz-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("wrkz-node.pid"));
        let lock = DirLock::acquire(&dir).expect("first holder");
        let second = DirLock::acquire(&dir).unwrap_err();
        assert!(second.contains("is locked by process"), "{second}");
        assert!(second.contains("wrkz-node.pid"), "the message names the file to delete: {second}");
        drop(lock);
        assert!(DirLock::acquire(&dir).is_ok(), "the lock is released on drop");
        let _ = std::fs::remove_file(dir.join("wrkz-node.pid"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn resync_deletes_the_chain_and_peer_state_and_keeps_the_bans() {
        use crate::peers::{ANCHORS_FILENAME, BANS_FILENAME};
        use wrkz_primitives::constants::P2P_NET_DATA_FILENAME;

        let dir = std::env::temp_dir().join(format!("wrkz-resync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(STATE_DIR).join("nested")).unwrap();
        std::fs::write(dir.join(STATE_DIR).join("nested").join("000001.sst"), b"sst").unwrap();
        std::fs::write(dir.join(STATE_DIR).join("CURRENT"), b"MANIFEST").unwrap();
        for name in [P2P_NET_DATA_FILENAME, ANCHORS_FILENAME, BANS_FILENAME, "wrkz-node.pid", "wrkz.log"] {
            std::fs::write(dir.join(name), name).unwrap();
        }

        let removed = resync(&dir).unwrap();
        assert_eq!(removed, [dir.join(STATE_DIR), dir.join(P2P_NET_DATA_FILENAME), dir.join(ANCHORS_FILENAME)]);
        for gone in &removed {
            assert!(!gone.exists(), "{} is deleted", gone.display());
        }
        for kept in [BANS_FILENAME, "wrkz-node.pid", "wrkz.log"] {
            assert!(dir.join(kept).exists(), "{kept} is kept");
        }
        assert!(resync(&dir).unwrap().is_empty(), "nothing left to delete is not an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mainnet_chain_to_5() -> wrkz_chain::ChainState<wrkz_storage::MemStore> {
        use wrkz_chain::{Checkpoints, Config};
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/mainnet_rawblocks_0_to_5.json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut chain = wrkz_chain::ChainState::open_or_genesis(
            wrkz_storage::MemStore::default(),
            Config::default(),
            Checkpoints::mainnet(),
        )
        .unwrap();
        chain.set_clock(Some(1_900_000_000));
        for item in &v["items"].as_array().unwrap()[1..] {
            let block = hex::decode(item["block"].as_str().unwrap()).unwrap();
            let txs: Vec<Vec<u8>> = item["transactions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| hex::decode(t.as_str().unwrap()).unwrap())
                .collect();
            chain.add_block(&block, &txs).unwrap();
        }
        chain
    }

    #[test]
    fn a_rewind_leaves_the_height_asked_for_even_one_below_the_top_index() {
        let mut chain = mainnet_chain_to_5();
        assert_eq!(rewind_to_height(&mut chain, 3).unwrap(), Rewind::Rewound { removed: 3, height: 3 });
        assert_eq!(chain.tip_index(), Some(2));
        // `N` equal to the top index: the C++ compares it with a count and does
        // nothing. The height afterwards is still `N`.
        assert_eq!(rewind_to_height(&mut chain, 2).unwrap(), Rewind::Rewound { removed: 1, height: 2 });
        assert_eq!(chain.tip_index(), Some(1));
        assert_eq!(rewind_to_height(&mut chain, 2).unwrap(), Rewind::NotNeeded { height: 2 });
        assert_eq!(rewind_to_height(&mut chain, 900).unwrap(), Rewind::NotNeeded { height: 2 });
    }

    #[test]
    fn a_rewind_that_cannot_or_may_not_be_done_is_refused_and_changes_nothing() {
        let mut chain = seeded_state(5000, 0);
        assert!(rewind_to_height(&mut chain, 0).unwrap_err().contains("use the `--resync` option"));

        // At most 4,320 below the top: 5000 - 680 is exactly that.
        let e = rewind_to_height(&mut chain, 679).unwrap_err();
        assert!(e.contains("you can only rewind to 680. Nothing was changed."), "{e}");
        let e = rewind_to_height(&mut chain, 680).unwrap_err();
        assert!(!e.contains("you can only rewind to"), "680 is within the limit, and fails for another reason: {e}");
        assert_eq!(chain.tip_index(), Some(5000));

        // No output records and no bodies: nothing to undo a block from.
        let mut bare = seeded_state(100, 101);
        let e = rewind_to_height(&mut bare, 90).unwrap_err();
        assert!(e.contains("neither the record of the outputs block 100 created nor that block's body"), "{e}");
        assert_eq!(bare.tip_index(), Some(100));

        // Below a lite height, whatever the state could do.
        let mut lite = mainnet_chain_to_5();
        lite.declare_lite_height(4).unwrap();
        let e = rewind_to_height(&mut lite, 3).unwrap_err();
        assert!(e.contains("Cannot rewind to 3 on a lite node whose full block data starts at 4"), "{e}");
        assert_eq!(lite.tip_index(), Some(5));
        assert_eq!(rewind_to_height(&mut lite, 4).unwrap(), Rewind::Rewound { removed: 2, height: 4 });
    }

    #[test]
    fn the_status_line_reports_the_rate_over_the_interval() {
        let mut line = StatusLine::new(Duration::ZERO, 100);
        let s = StatusSnapshot { height: 160, network_height: 1000, incoming: 2, outgoing: 3, pool: 4, synced: false };
        let text = line.tick(&s).expect("the interval has elapsed");
        assert!(text.starts_with("height 160/1000  peers 2in/3out  pool 4  "), "{text}");
        assert!(text.contains("blocks/s"));
        assert!(!text.contains("(synced)"));
        // A node at the network height reports no target and says so.
        let s = StatusSnapshot { height: 1000, network_height: 1000, synced: true, ..s };
        let text = line.tick(&s).unwrap();
        assert!(text.starts_with("height 1000  "), "{text}");
        assert!(text.ends_with("(synced)"));
        // Nothing before the interval elapses.
        let mut slow = StatusLine::new(Duration::from_secs(3600), 0);
        assert!(slow.tick(&s).is_none());
    }

    #[test]
    fn the_eta_prints_the_two_largest_units() {
        assert_eq!(format_eta(0.4), "0s");
        assert_eq!(format_eta(59.0), "59s");
        assert_eq!(format_eta(61.0), "1m 01s");
        assert_eq!(format_eta(3.0 * 3600.0 + 5.0 * 60.0 + 7.0), "3h 05m");
        assert_eq!(format_eta(2.0 * 86_400.0 + 7.0 * 3600.0 + 1.0), "2d 7h");
        assert_eq!(format_eta(-5.0), "0s");
    }

    #[test]
    fn the_lite_depth_check_waits_for_four_reports_and_takes_the_tallest() {
        // A lite height of 100,000 needs a network of 120,160.
        let mut c = LiteDepthCheck::default();
        for _ in 0..3 {
            assert!(c.observe(100_000, 50_000, 110_000).is_ok(), "fewer than four reports decide nothing");
        }
        let e = c.observe(100_000, 50_000, 110_000).unwrap_err();
        assert!(e.contains("--lite-height 100000 is too close to the network top (110000"), "{e}");
        assert!(e.contains("across 4 peers"), "{e}");
        assert!(e.contains("currently allows is 89840"), "{e}");
        assert!(c.observe(100_000, 50_000, 110_000).is_ok(), "one verdict, then silence");

        // One tall report settles it for good.
        let mut c = LiteDepthCheck::default();
        assert!(c.observe(100_000, 50_000, 120_160).is_ok());
        assert!(c.is_settled());

        // So does our own chain, whatever the peer says.
        let mut c = LiteDepthCheck::default();
        assert!(c.observe(100_000, 130_000, 1).is_ok());
        assert!(c.is_settled());

        // A short report after a tall one cannot lower the maximum.
        let mut c = LiteDepthCheck::default();
        c.observe(100_000, 0, 119_000).unwrap();
        c.observe(100_000, 0, 5).unwrap();
        c.observe(100_000, 0, 5).unwrap();
        let e = c.observe(100_000, 0, 5).unwrap_err();
        assert!(e.contains("network top (119000"), "{e}");

        // Not a lite node, or a peer reporting no height: no check at all.
        let mut c = LiteDepthCheck::default();
        for _ in 0..10 {
            assert!(c.observe(0, 0, 5).is_ok());
            assert!(c.observe(100_000, 0, 0).is_ok());
        }
        assert!(!c.is_settled());
    }

    #[test]
    fn the_body_probe_finds_where_the_block_bodies_start() {
        use wrkz_chain::{Checkpoints, Config};
        use wrkz_storage::MemStore;

        // A state that stores raw blocks has a body from genesis up.
        let full = wrkz_chain::ChainState::open_or_genesis(
            MemStore::default(),
            Config { store_raw_blocks: true, ..Config::default() },
            Checkpoints::mainnet(),
        )
        .unwrap();
        assert_eq!(lowest_stored_block(&full), Some(0), "bodies from genesis: nothing to warn about");

        // A state built the way `wrkz-replay` builds one without `--store-raw`:
        // the index is there, the bytes are not. This is the import an operator
        // has to be warned about.
        let bodyless = wrkz_chain::ChainState::open_or_genesis(
            MemStore::default(),
            Config { store_raw_blocks: false, ..Config::default() },
            Checkpoints::mainnet(),
        )
        .unwrap();
        assert_eq!(lowest_stored_block(&bodyless), None, "not one body anywhere");

        // And the case the probe exists for: an import that wrote every index
        // and the bodies only from some height up. The binary search has to
        // land on the first one, not on a body it happened to look at.
        for first_body in [1u32, 40, 100] {
            let chain = seeded_state(100, first_body);
            assert_eq!(
                lowest_stored_block(&chain),
                Some(first_body),
                "bodies start at {first_body} and the probe must say so"
            );
        }
    }

    /// A state with block infos for `0..=tip` and raw blocks only from
    /// `first_body` up — the shape a `wrkz-replay` import leaves behind.
    #[cfg(test)]
    fn seeded_state(tip: u32, first_body: u32) -> wrkz_chain::ChainState<wrkz_storage::MemStore> {
        use wrkz_chain::records::BlockInfo;
        use wrkz_chain::{keys, records, Checkpoints, Config};
        use wrkz_storage::{KvStore, MemStore};

        let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for index in 0..=tip {
            let info = BlockInfo {
                block_hash: [index as u8; 32],
                timestamp: 1_500_000_000 + index as u64 * 60,
                block_size: 100,
                cumulative_difficulty: index as u64 + 1,
                already_generated_coins: 1,
                already_generated_transactions: index as u64 + 1,
            };
            ops.push((keys::block_info(index), Some(info.encode())));
            if index >= first_body {
                ops.push((keys::raw_block(index), Some(records::encode_raw_block(&[1, 2, 3], &[]))));
            }
        }
        ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
        ops.push((keys::meta(keys::META_TIP), Some(tip.to_le_bytes().to_vec())));
        let mut store = MemStore::default();
        store.write_batch(ops).expect("the seed writes");
        wrkz_chain::ChainState::open(store, Config { store_raw_blocks: true, ..Config::default() }, Checkpoints::none())
            .expect("the seeded state opens")
    }

    #[test]
    fn low_space_forces_a_prune_pass_that_the_block_gap_would_not() {
        // The gap alone: the first call always fires, then not until 120 blocks.
        let mut a = AutoPrune::new(120, 1000);
        assert_eq!(a.due(500, 5000), Some(PruneReason::Scheduled), "the first pass is always due");
        assert_eq!(a.due(560, 5000), None, "60 blocks is inside the gap");
        assert_eq!(a.due(620, 5000), Some(PruneReason::Scheduled));

        // Low space fires inside the gap, and says that is why.
        assert_eq!(a.due(640, 10), Some(PruneReason::LowSpace(10)));

        // A zero gap disables the schedule but not the forced path — the C++
        // `pruneGapReached || lowSpaceForPrune`.
        let mut off = AutoPrune::new(0, 1000);
        assert_eq!(off.due(500, 5000), None, "0 disables the periodic schedule");
        assert_eq!(off.due(500, 10), Some(PruneReason::LowSpace(10)), "and low space still forces one");

        // An unreadable path reads as infinite free space, so it never forces.
        let mut blind = AutoPrune::new(0, u64::MAX - 1);
        assert_eq!(blind.due(500, u64::MAX), None);
    }

    #[test]
    fn free_space_is_reported_or_reported_as_unknown() {
        // Whatever the platform, the answer must be usable: either a real
        // number for a directory that exists, or the C++'s `u64::MAX` for one
        // that does not — never a panic and never a misleading zero.
        let here = available_bytes(&std::env::temp_dir());
        assert!(here > 0, "an existing directory reports something");
        let missing = available_bytes(Path::new("Z:/no/such/directory/at/all"));
        assert!(missing > 0, "an unreadable path reads as infinite, never as zero: {missing}");
    }

    #[test]
    fn the_shutdown_flag_is_set_by_request_and_read_by_every_handle() {
        let a = Shutdown::new();
        let b = a.clone();
        assert!(!a.requested() && !b.requested());
        a.request();
        assert!(b.requested(), "clones share the flag");
        assert!(b.triggered());
    }
}
