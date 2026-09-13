// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! DB compaction: the boot pass, the periodic scheduler and `compact_db`
//! (`DaemonCommandsHandler.cpp:356-439`, `:1329-1685`).
//!
//! # What runs, and when
//!
//! - **At boot** a full compaction starts in the background, every boot,
//!   unless `--skip-boot-compaction` is given.
//! - **Periodically** a scheduler thread looks every 60 seconds — every 30
//!   minutes once the node has stayed within two blocks of the network for
//!   three looks in a row — and starts one when all of these hold: none is
//!   running, the state directory's filesystem has at least
//!   `--auto-compaction-min-free-bytes` free, `--auto-compaction-min-gap-blocks`
//!   blocks have arrived since the last one started or finished, and 30
//!   minutes of wall-clock time have passed since then too. The wall-clock
//!   rule is what keeps a syncing node, which passes 720 blocks in seconds,
//!   from rewriting its database every minute.
//! - **On request** `compact_db` starts one, reports on it, or waits for it.
//!
//! All three go through one [`Compaction`]: one compaction at a time, one
//! marker file, one last result. Writes carry on while it runs; the node keeps
//! syncing and serving.
//!
//! # The marker
//!
//! `<data-dir>/state/.compact_db_in_progress` holds the Unix time a compaction
//! started and is removed only when one *completes*. A failed pass, a pass cut
//! short by a shutdown, and a process that died mid-pass all leave it, so the
//! next boot says it is resuming unfinished work rather than starting afresh.
//!
//! # Shutdown
//!
//! The C++ asks a running compaction to stop through
//! `CompactRangeOptions::canceled` and resumes it on the next start. rocksdb
//! 0.22 has no such flag: the only way to stop a manual compaction is to stop
//! *all* of the engine's background work, which cannot be undone short of
//! reopening the database. So [`Compaction::shutdown`] must be the last thing
//! to touch the engine: the daemon calls it after the chain state's final
//! commit, stops the scheduler before that, and the marker stays so the next
//! boot picks the work up.
//!
//! # Differences from the C++, and why
//!
//! - **A skipped boot pass stays skipped.** With `--skip-boot-compaction` the
//!   C++ scheduler has seen no compaction at all, so its first check, about 60
//!   seconds in, starts one (`:1637`, `:1663`). Here skipping counts the
//!   periodic rules from startup: the first automatic pass is at least a block
//!   gap and 30 minutes away.
//! - **`compact_db wait` does not hold the lock.** The C++ waits holding the
//!   compaction mutex (`:1397`), which blocks the scheduler, `compact_db
//!   status` and shutdown for the whole compaction.
//! - **No new blocks is fewer than the gap.** The C++ applies the block gap only
//!   once the height has risen past the last compaction (`currentHeight >
//!   lastActivityHeight`), so a node whose chain has stalled compacts every 30
//!   minutes over no new data.
//! - **Low space is logged once**, when it starts and when it ends, not on every
//!   check; and with `--auto-compaction-min-gap-blocks 0` no scheduler thread
//!   runs at all.
//! - **A failure is RocksDB's background-error counter** rising during the pass,
//!   because `compact_range_opt` returns no `Status`.
//! - **Completion is recorded when RocksDB returns**, by the compaction thread,
//!   not when something next looks (`refresh_compaction_state_locked`).
//! - The marker is in `<data-dir>/state/`, where this port's database lives,
//!   not `<data-dir>/DB/`; and the scheduler reads the heights through
//!   [`wrkz_rpc::NodeApi`] rather than its own `/info` over HTTP.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wrkz_storage::dbconfig::EngineCompactionStats;

/// The marker's file name, the C++'s `COMPACTION_MARKER_FILE`.
pub const MARKER_FILE: &str = ".compact_db_in_progress";

/// `autoCompactionMinGapBlocks` (`DaemonConfiguration.h:112`): about twelve
/// hours of blocks.
pub const DEFAULT_MIN_GAP_BLOCKS: u64 = 720;

/// `autoCompactionMinFreeBytes` (`DaemonConfiguration.h:114`).
pub const DEFAULT_MIN_FREE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// `AUTO_COMPACTION_CHECK_INTERVAL_FAST_SECONDS`.
pub const CHECK_INTERVAL_FAST: Duration = Duration::from_secs(60);
/// `AUTO_COMPACTION_CHECK_INTERVAL_SLOW_SECONDS`.
pub const CHECK_INTERVAL_SLOW: Duration = Duration::from_secs(30 * 60);
/// `AUTO_COMPACTION_NEAR_SYNC_LAG_BLOCKS`: this far behind counts as synced.
pub const NEAR_SYNC_LAG_BLOCKS: u64 = 2;
/// `AUTO_COMPACTION_RESYNC_LAG_BLOCKS`: this far behind counts as syncing again.
pub const RESYNC_LAG_BLOCKS: u64 = 20;
/// `AUTO_COMPACTION_NEAR_SYNC_STREAK_REQUIRED`.
pub const NEAR_SYNC_STREAK_REQUIRED: u32 = 3;
/// `AUTO_COMPACTION_MIN_WALL_SECONDS`.
pub const MIN_WALL_SECONDS: u64 = 30 * 60;

/// What a compaction needs from the storage engine.
///
/// Implemented by `wrkz_storage::rocks::CompactionHandle` when the daemon is
/// built with RocksDB, and by fakes in the tests, so everything in this module
/// is exercised without an engine.
pub trait CompactionEngine: Send + Sync {
    /// Compact the whole database, blocking until it is done. `Err` says why
    /// it failed.
    fn compact_full(&self, rewrite_bottommost: bool) -> Result<(), String>;

    /// Stop every flush and compaction for good, so a running
    /// [`CompactionEngine::compact_full`] returns. See the module
    /// documentation for why this is terminal.
    fn stop_all_background_work(&self);

    /// The engine's own counters, for `db_status`.
    fn stats(&self) -> EngineCompactionStats;
}

#[cfg(feature = "rocksdb")]
impl CompactionEngine for wrkz_storage::rocks::CompactionHandle {
    fn compact_full(&self, rewrite_bottommost: bool) -> Result<(), String> {
        wrkz_storage::rocks::CompactionHandle::compact_full(self, rewrite_bottommost)
    }

    fn stop_all_background_work(&self) {
        wrkz_storage::rocks::CompactionHandle::stop_all_background_work(self);
    }

    fn stats(&self) -> EngineCompactionStats {
        wrkz_storage::rocks::CompactionHandle::stats(self)
    }
}

/// Who asked for a compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The boot pass.
    Boot,
    /// The scheduler.
    Periodic,
    /// `compact_db start`, or `compact_db force` with `rewrite_bottommost`.
    Manual { rewrite_bottommost: bool },
}

impl Trigger {
    /// Whether this pass rewrites the bottommost level too. Only `force` does.
    pub fn rewrite_bottommost(self) -> bool {
        matches!(self, Trigger::Manual { rewrite_bottommost: true })
    }

    /// The C++'s words for it, as its "Starting DB compaction (...)" line has
    /// them.
    pub fn describe(self) -> &'static str {
        match self {
            Trigger::Boot => "boot background task",
            Trigger::Periodic => "automatic periodic background task",
            Trigger::Manual { .. } => "manual console request",
        }
    }
}

/// How a compaction ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// RocksDB finished it without a background error. The marker is gone.
    Completed,
    /// The daemon shut down under it. The marker stays and the next boot
    /// resumes the work.
    Stopped,
    /// RocksDB reported a background error while it ran. The marker stays.
    Failed(String),
}

/// What [`Compaction::start`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Started {
    /// A compaction is running now. `resumed` when a marker from an earlier
    /// run was already there.
    Started { resumed: bool },
    /// One was already running; nothing changed.
    AlreadyRunning,
    /// The daemon is shutting down and starts no more.
    ShuttingDown,
    /// The compaction thread could not be started.
    Failed(String),
}

/// A height and a Unix time, which is how the scheduler measures "since the
/// last compaction".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub at: u64,
    pub height: u64,
}

/// The running compaction, as `compact_db status` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunningReport {
    pub trigger: Trigger,
    pub elapsed_secs: u64,
}

/// Everything `compact_db status` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub running: Option<RunningReport>,
    /// Whether the marker file is on disk.
    pub marker_present: bool,
    /// How the last compaction since the daemon started ended, if one has and
    /// none is running now.
    pub last_result: Option<RunResult>,
}

/// What the scheduler rules look at: whether a compaction is running, and
/// when the last activity was — the latest of the last start, the last finish
/// and, after a skipped boot pass, startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    pub running: bool,
    pub last_height: Option<u64>,
    pub last_at: Option<u64>,
}

/// The periodic rules (`DaemonCommandsHandler.cpp:1627-1666`), as a pure
/// function of what they read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoCompaction {
    /// `--auto-compaction-min-gap-blocks`; 0 turns periodic compaction off.
    pub min_gap_blocks: u64,
    /// `--auto-compaction-min-free-bytes`.
    pub min_free_bytes: u64,
}

impl Default for AutoCompaction {
    fn default() -> Self {
        Self { min_gap_blocks: DEFAULT_MIN_GAP_BLOCKS, min_free_bytes: DEFAULT_MIN_FREE_BYTES }
    }
}

/// What one scheduler check decided, in the order the checks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A compaction is already running.
    Running,
    /// Free space is below the minimum. Unlike a prune pass, low space
    /// *prevents* a compaction: a full pass needs room for the files it
    /// writes before it can delete the ones it replaces.
    LowSpace { free: u64, required: u64 },
    /// `--auto-compaction-min-gap-blocks 0`.
    Disabled,
    /// Fewer blocks than the gap since the last activity.
    GapNotReached { blocks: u64 },
    /// Less than [`MIN_WALL_SECONDS`] since the last activity.
    TooSoon { seconds: u64 },
    /// Start one.
    Start,
}

impl AutoCompaction {
    /// One scheduler check. `height` is a count, as `/height` reports it;
    /// `now` is Unix seconds; `free` is the state directory's free bytes.
    pub fn verdict(&self, activity: &Activity, now: u64, height: u64, free: u64) -> Verdict {
        if activity.running {
            return Verdict::Running;
        }
        if free < self.min_free_bytes {
            return Verdict::LowSpace { free, required: self.min_free_bytes };
        }
        if self.min_gap_blocks == 0 {
            return Verdict::Disabled;
        }
        if let Some(last) = activity.last_height {
            // A height at or below the last one is no blocks at all, not a
            // reason to skip the rule (see the module documentation).
            let blocks = height.saturating_sub(last);
            if blocks < self.min_gap_blocks {
                return Verdict::GapNotReached { blocks };
            }
        }
        if let Some(at) = activity.last_at {
            // `>=`, as the C++ has it; a clock that went backwards skips the rule.
            if now >= at && now - at < MIN_WALL_SECONDS {
                return Verdict::TooSoon { seconds: now - at };
            }
        }
        Verdict::Start
    }
}

/// The scheduler's adaptive check interval (`DaemonCommandsHandler.cpp:1538-1571`):
/// once a minute while the node is catching up, every half hour once it has
/// stayed near the network's height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    streak: u32,
    interval: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self { streak: 0, interval: CHECK_INTERVAL_FAST }
    }
}

impl Cadence {
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Weigh one look at the heights. `Some` is the new interval when it
    /// changed. A lag between the two thresholds leaves the streak alone.
    pub fn observe(&mut self, local: u64, network: u64) -> Option<Duration> {
        let lag = network.saturating_sub(local);
        if lag <= NEAR_SYNC_LAG_BLOCKS {
            self.streak = self.streak.saturating_add(1);
        } else if lag >= RESYNC_LAG_BLOCKS {
            self.streak = 0;
        }
        let desired = if self.streak >= NEAR_SYNC_STREAK_REQUIRED { CHECK_INTERVAL_SLOW } else { CHECK_INTERVAL_FAST };
        if desired == self.interval {
            return None;
        }
        self.interval = desired;
        Some(desired)
    }
}

/// Where the local and network heights come from: a count each, as `/height`
/// reports them.
pub type Heights = Box<dyn Fn() -> (u64, u64) + Send>;
/// Free bytes on the filesystem holding the database.
pub type FreeBytes = Box<dyn Fn() -> u64 + Send>;

struct Inner {
    running: Option<(Trigger, Mark)>,
    last_result: Option<RunResult>,
    last_start: Option<Mark>,
    last_finish: Option<Mark>,
    /// Startup, when the boot pass was skipped.
    baseline: Option<Mark>,
    /// Compactions started and finished, so a `wait` waits for the one that
    /// was running when it began and no later one.
    started: u64,
    finished: u64,
    /// Callers blocked in [`Compaction::wait`].
    waiters: usize,
    worker: Option<JoinHandle<()>>,
}

impl Inner {
    fn activity(&self) -> Activity {
        let marks = [self.last_start, self.last_finish, self.baseline];
        Activity {
            running: self.running.is_some(),
            last_height: marks.iter().flatten().map(|m| m.height).max(),
            last_at: marks.iter().flatten().map(|m| m.at).max(),
        }
    }
}

struct SchedulerThread {
    stop: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

/// The one compaction state of a daemon. See the module documentation.
pub struct Compaction {
    engine: Arc<dyn CompactionEngine>,
    marker: PathBuf,
    /// The local height, a count.
    height: Box<dyn Fn() -> u64 + Send + Sync>,
    inner: Mutex<Inner>,
    finished: Condvar,
    stopping: AtomicBool,
    scheduler: Mutex<Option<SchedulerThread>>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Compaction {
    /// A compaction state for the database in `state_dir`. `height` reads the
    /// local chain height (a count); it is never called with the compaction
    /// lock held, so it may take the chain lock.
    pub fn new(
        engine: Arc<dyn CompactionEngine>,
        state_dir: &Path,
        height: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            marker: state_dir.join(MARKER_FILE),
            height,
            inner: Mutex::new(Inner {
                running: None,
                last_result: None,
                last_start: None,
                last_finish: None,
                baseline: None,
                started: 0,
                finished: 0,
                waiters: 0,
                worker: None,
            }),
            finished: Condvar::new(),
            stopping: AtomicBool::new(false),
            scheduler: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The marker file's path.
    pub fn marker_path(&self) -> &Path {
        &self.marker
    }

    /// Whether the marker is on disk. An unreadable path reads as absent, as
    /// the C++'s `fs::exists` with an error code does.
    pub fn marker_present(&self) -> bool {
        self.marker.try_exists().unwrap_or(false)
    }

    fn write_marker(&self) {
        let written = self
            .marker
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&self.marker, format!("{}\n", unix_now())));
        if let Err(e) = written {
            crate::log_warn!("Failed to create DB compaction marker file: {} ({e})", self.marker.display());
        }
    }

    fn remove_marker(&self) {
        let _ = std::fs::remove_file(&self.marker);
    }

    /// Start a compaction in the background, unless one is running.
    pub fn start(self: &Arc<Self>, trigger: Trigger) -> Started {
        // Read before the lock: the height takes the chain lock.
        let height = (self.height)();
        let mut inner = self.lock();
        if self.stopping.load(Ordering::SeqCst) {
            return Started::ShuttingDown;
        }
        if inner.running.is_some() {
            return Started::AlreadyRunning;
        }
        // The previous compaction thread has recorded its result, which is
        // the last thing it does; collect it.
        if let Some(previous) = inner.worker.take() {
            let _ = previous.join();
        }
        let resumed = self.marker_present();
        let mark = Mark { at: unix_now(), height };
        self.write_marker();
        let rewrite = if trigger.rewrite_bottommost() { ", rewriting the bottommost level." } else { "." };
        crate::log_info!("Starting DB compaction ({}){rewrite}", trigger.describe());

        let me = Arc::clone(self);
        // The thread records its result under the lock this function holds,
        // so it cannot finish before `running` below is set.
        match std::thread::Builder::new().name("wrkz-compaction".into()).spawn(move || me.run(trigger)) {
            Ok(handle) => {
                inner.running = Some((trigger, mark));
                inner.last_start = Some(mark);
                inner.started += 1;
                inner.worker = Some(handle);
                Started::Started { resumed }
            }
            Err(e) => {
                if !resumed {
                    self.remove_marker();
                }
                let message = format!("could not start the compaction thread: {e}");
                crate::log_warn!("DB compaction not started: {message}");
                Started::Failed(message)
            }
        }
    }

    /// The compaction thread.
    fn run(&self, trigger: Trigger) {
        let rewrite = trigger.rewrite_bottommost();
        crate::log_info!(
            "Starting RocksDB full compaction{}",
            if rewrite { " (rewriting the bottommost level)..." } else { "..." }
        );
        let began = Instant::now();
        let outcome = self.engine.compact_full(rewrite);
        let elapsed = crate::daemon::format_eta(began.elapsed().as_secs_f64());
        let result = match outcome {
            _ if self.stopping.load(Ordering::SeqCst) => {
                crate::log_info!("RocksDB compaction stopped early on request; it will resume on the next start.");
                RunResult::Stopped
            }
            Ok(()) => {
                crate::log_info!("RocksDB full compaction completed in {elapsed}.");
                RunResult::Completed
            }
            Err(e) => {
                crate::log_error!("RocksDB compaction failed after {elapsed}: {e}");
                RunResult::Failed(e)
            }
        };
        // At shutdown the chain is still there: its owner waits for this thread.
        let height = (self.height)();
        let mut inner = self.lock();
        // The marker means "started and not finished", so only a pass that
        // finished may clear it.
        if result == RunResult::Completed {
            self.remove_marker();
        }
        inner.running = None;
        inner.last_finish = Some(Mark { at: unix_now(), height });
        inner.last_result = Some(result);
        inner.finished += 1;
        drop(inner);
        self.finished.notify_all();
    }

    /// `compact_db status`.
    pub fn status(&self) -> StatusReport {
        let marker_present = self.marker_present();
        let inner = self.lock();
        StatusReport {
            running: inner
                .running
                .map(|(trigger, mark)| RunningReport { trigger, elapsed_secs: unix_now().saturating_sub(mark.at) }),
            marker_present,
            // The C++ clears its result when the next compaction starts; a
            // result is only ever about a compaction that is not running.
            last_result: if inner.running.is_some() { None } else { inner.last_result.clone() },
        }
    }

    /// `compact_db wait`: `None` when nothing is running, otherwise how the
    /// running compaction ended. The lock is released while waiting.
    pub fn wait(&self) -> Option<RunResult> {
        let mut inner = self.lock();
        // A compaction is running exactly when more have started than finished.
        let target = inner.started;
        if inner.finished >= target {
            return None;
        }
        inner.waiters += 1;
        while inner.finished < target {
            inner = self.finished.wait(inner).unwrap_or_else(|p| p.into_inner());
        }
        inner.waiters -= 1;
        inner.last_result.clone()
    }

    /// The scheduler rules' view of this state.
    pub fn activity(&self) -> Activity {
        self.lock().activity()
    }

    /// The engine's counters.
    pub fn engine_stats(&self) -> EngineCompactionStats {
        self.engine.stats()
    }

    /// The boot pass (`start_boot_compaction_if_needed`): start one, or record
    /// that it was skipped so the periodic rules count from now.
    pub fn boot(self: &Arc<Self>, skip: bool) {
        if skip {
            let mark = Mark { at: unix_now(), height: (self.height)() };
            self.lock().baseline = Some(mark);
            crate::log_info!("Boot DB compaction: skipped by configuration (--skip-boot-compaction).");
            if self.marker_present() {
                crate::log_warn!(
                    "{} is left from a compaction that did not finish; it stays until one does \
                     (`compact_db` starts one)",
                    self.marker.display()
                );
            }
            return;
        }
        match self.start(Trigger::Boot) {
            Started::Started { resumed: true } => crate::log_warn!(
                "Detected unfinished DB compaction marker from previous run. Restarting DB compaction in background."
            ),
            Started::Started { resumed: false } => crate::log_info!("Boot DB compaction started in background."),
            Started::AlreadyRunning | Started::ShuttingDown => {}
            Started::Failed(e) => crate::log_warn!("Boot DB compaction could not start: {e}"),
        }
    }

    /// Start the scheduler thread. Does nothing when periodic compaction is
    /// off or the thread is already running.
    pub fn start_scheduler(self: &Arc<Self>, policy: AutoCompaction, heights: Heights, free_bytes: FreeBytes) {
        if policy.min_gap_blocks == 0 {
            crate::log_info!("Automatic periodic DB compaction is off (--auto-compaction-min-gap-blocks 0).");
            return;
        }
        let mut slot = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return;
        }
        let (stop, stopped) = mpsc::channel();
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("wrkz-compaction-scheduler".into())
            .spawn(move || me.schedule(policy, &stopped, &heights, &free_bytes));
        match spawned {
            Ok(handle) => {
                *slot = Some(SchedulerThread { stop, handle });
                crate::log_info!("Automatic periodic DB compaction monitoring is enabled.");
            }
            Err(e) => crate::log_warn!("Automatic periodic DB compaction is off: could not start its thread: {e}"),
        }
    }

    /// The scheduler loop (`compaction_scheduler_loop`).
    fn schedule(
        self: &Arc<Self>,
        policy: AutoCompaction,
        stop: &mpsc::Receiver<()>,
        heights: &Heights,
        free: &FreeBytes,
    ) {
        let mut cadence = Cadence::default();
        let mut low_space = false;
        loop {
            match stop.recv_timeout(cadence.interval()) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.stopping.load(Ordering::SeqCst) {
                return;
            }
            let (local, network) = heights();
            if let Some(interval) = cadence.observe(local, network) {
                crate::log_info!(
                    "Adaptive maintenance scheduler interval switched to {}s (local height: {local}, network \
                     height: {network}, lag: {}).",
                    interval.as_secs(),
                    network.saturating_sub(local)
                );
            }
            let free_bytes = free();
            let verdict = policy.verdict(&self.activity(), unix_now(), local, free_bytes);
            match verdict {
                Verdict::Running => continue,
                Verdict::LowSpace { free, required } => {
                    if !low_space {
                        crate::log_warn!(
                            "Skipping automatic DB compaction due to low free disk space ({free} bytes, required \
                             at least {required} bytes)."
                        );
                    }
                    low_space = true;
                    continue;
                }
                _ if low_space => {
                    low_space = false;
                    crate::log_info!(
                        "Free disk space is back to {free_bytes} bytes; automatic DB compaction may run again."
                    );
                }
                _ => {}
            }
            if verdict != Verdict::Start {
                continue;
            }
            match self.start(Trigger::Periodic) {
                Started::Started { .. } => {
                    crate::log_info!("Automatic periodic DB compaction started in background.")
                }
                Started::AlreadyRunning => {}
                Started::ShuttingDown => return,
                Started::Failed(e) => crate::log_warn!("Automatic periodic DB compaction could not start: {e}"),
            }
        }
    }

    /// Stop the scheduler thread and wait for it. It starts nothing more.
    pub fn stop_scheduler(&self) {
        let taken = self.scheduler.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(scheduler) = taken {
            let _ = scheduler.stop.send(());
            let _ = scheduler.handle.join();
        }
    }

    /// Stop for good: the scheduler, then a running compaction, which keeps
    /// its marker and resumes on the next start.
    ///
    /// **Call this only after the chain state's last commit.** Stopping a
    /// running compaction stops all of the engine's background work, flushes
    /// included, until the database is reopened. With nothing running, the
    /// engine is not touched.
    pub fn shutdown(&self) {
        self.stop_scheduler();
        // Set before the lock is taken, so a `start` that has the lock now
        // either began before this and is seen below, or sees the flag.
        self.stopping.store(true, Ordering::SeqCst);
        let (running, worker) = {
            let mut inner = self.lock();
            (inner.running.is_some(), inner.worker.take())
        };
        if running {
            crate::log_info!("Stopping background DB compaction; it will resume on the next start...");
            self.engine.stop_all_background_work();
        }
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// An engine whose compaction blocks until the test releases it or the
    /// engine is stopped, and records what it was asked.
    #[derive(Default)]
    struct Gate {
        state: Mutex<GateState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct GateState {
        entered: usize,
        released: usize,
        stopped: bool,
        rewrites: Vec<bool>,
        fail_with: Option<String>,
    }

    impl Gate {
        fn release(&self) {
            self.state.lock().unwrap().released += 1;
            self.changed.notify_all();
        }

        fn wait_entered(&self, n: usize) {
            let mut s = self.state.lock().unwrap();
            while s.entered < n {
                s = self.changed.wait(s).unwrap();
            }
        }
    }

    impl CompactionEngine for Gate {
        fn compact_full(&self, rewrite_bottommost: bool) -> Result<(), String> {
            let mut s = self.state.lock().unwrap();
            s.entered += 1;
            s.rewrites.push(rewrite_bottommost);
            let me = s.entered;
            self.changed.notify_all();
            while s.released < me && !s.stopped {
                s = self.changed.wait(s).unwrap();
            }
            match &s.fail_with {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn stop_all_background_work(&self) {
            self.state.lock().unwrap().stopped = true;
            self.changed.notify_all();
        }

        fn stats(&self) -> EngineCompactionStats {
            EngineCompactionStats::default()
        }
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("wrkz-compaction-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Block until `n` callers are inside [`Compaction::wait`], so a test that
    /// means to release a waiter has one to release.
    fn wait_for_waiters(c: &Compaction, n: usize) {
        let began = Instant::now();
        while c.lock().waiters < n {
            assert!(began.elapsed() < Duration::from_secs(10), "the waiter never started waiting");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn compaction(dir: &Dir, gate: &Arc<Gate>, height: &Arc<AtomicU64>) -> Arc<Compaction> {
        let height = Arc::clone(height);
        Compaction::new(
            Arc::clone(gate) as Arc<dyn CompactionEngine>,
            &dir.0,
            Box::new(move || height.load(Ordering::SeqCst)),
        )
    }

    #[test]
    fn one_compaction_runs_at_a_time_and_a_completed_one_clears_its_marker() {
        let (dir, gate, height) = (Dir::new("once"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(500)));
        let c = compaction(&dir, &gate, &height);
        assert_eq!(c.wait(), None, "nothing to wait for");

        assert_eq!(c.start(Trigger::Manual { rewrite_bottommost: false }), Started::Started { resumed: false });
        assert!(c.marker_present(), "the marker is written before the compaction runs");
        let stamp = std::fs::read_to_string(c.marker_path()).unwrap();
        assert!(stamp.trim().parse::<u64>().is_ok_and(|t| t > 1_600_000_000), "a Unix time: {stamp:?}");
        gate.wait_entered(1);
        assert_eq!(c.start(Trigger::Periodic), Started::AlreadyRunning);
        let status = c.status();
        assert_eq!(status.running.map(|r| r.trigger), Some(Trigger::Manual { rewrite_bottommost: false }));
        assert!(status.last_result.is_none());
        assert!(c.activity().running);

        gate.release();
        assert_eq!(c.wait(), Some(RunResult::Completed));
        assert!(!c.marker_present(), "only a completed pass clears the marker");
        let status = c.status();
        assert_eq!((status.running, status.last_result), (None, Some(RunResult::Completed)));
        let activity = c.activity();
        assert_eq!((activity.running, activity.last_height), (false, Some(500)));
        assert_eq!(gate.state.lock().unwrap().rewrites, [false]);
        c.shutdown();
    }

    #[test]
    fn force_rewrites_the_bottommost_level_and_nothing_else_does() {
        let (dir, gate, height) = (Dir::new("force"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(1)));
        let c = compaction(&dir, &gate, &height);
        gate.release();
        gate.release();
        gate.release();
        for trigger in [Trigger::Boot, Trigger::Periodic, Trigger::Manual { rewrite_bottommost: true }] {
            assert!(matches!(c.start(trigger), Started::Started { .. }));
            assert_eq!(c.wait(), Some(RunResult::Completed));
        }
        assert_eq!(gate.state.lock().unwrap().rewrites, [false, false, true]);
        c.shutdown();
    }

    #[test]
    fn a_failed_compaction_keeps_the_marker_and_says_why() {
        let (dir, gate, height) = (Dir::new("fail"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(9)));
        gate.state.lock().unwrap().fail_with = Some("RocksDB counted 1 background error(s)".into());
        let c = compaction(&dir, &gate, &height);
        gate.release();
        assert!(matches!(c.start(Trigger::Boot), Started::Started { .. }));
        assert_eq!(c.wait(), Some(RunResult::Failed("RocksDB counted 1 background error(s)".into())));
        assert!(c.marker_present(), "a failed pass is unfinished work");
        assert!(matches!(c.status().last_result, Some(RunResult::Failed(_))));

        // The next boot says it is resuming.
        gate.state.lock().unwrap().fail_with = None;
        gate.release();
        assert_eq!(c.start(Trigger::Boot), Started::Started { resumed: true });
        assert_eq!(c.status().last_result, None, "a new pass clears the last result, as the C++ does");
        assert_eq!(c.wait(), Some(RunResult::Completed));
        assert!(!c.marker_present());
        c.shutdown();
    }

    #[test]
    fn shutdown_stops_a_running_compaction_keeps_its_marker_and_starts_no_more() {
        let (dir, gate, height) = (Dir::new("shutdown"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(3)));
        let c = compaction(&dir, &gate, &height);
        assert!(matches!(c.start(Trigger::Boot), Started::Started { .. }));
        gate.wait_entered(1);

        // A `wait` in progress is released by the shutdown, not left hanging.
        let waiter = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || c.wait())
        };
        wait_for_waiters(&c, 1);
        c.shutdown();
        assert!(gate.state.lock().unwrap().stopped, "the engine was told to stop");
        assert_eq!(waiter.join().unwrap(), Some(RunResult::Stopped));
        assert!(c.marker_present(), "the next boot resumes the work");
        assert_eq!(c.start(Trigger::Manual { rewrite_bottommost: false }), Started::ShuttingDown);
    }

    #[test]
    fn shutdown_with_nothing_running_leaves_the_engine_alone() {
        let (dir, gate, height) = (Dir::new("idle"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(3)));
        let c = compaction(&dir, &gate, &height);
        c.shutdown();
        assert!(!gate.state.lock().unwrap().stopped, "stopping the engine is terminal; an idle shutdown must not");
    }

    /// The C++ `compact_db wait` held the compaction mutex for the whole
    /// compaction. Here another thread can still ask for the status meanwhile.
    #[test]
    fn a_wait_does_not_block_the_status() {
        let (dir, gate, height) = (Dir::new("wait"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(3)));
        let c = compaction(&dir, &gate, &height);
        assert!(matches!(c.start(Trigger::Periodic), Started::Started { .. }));
        gate.wait_entered(1);
        let waiter = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || c.wait())
        };
        wait_for_waiters(&c, 1);
        for _ in 0..50 {
            assert!(c.status().running.is_some());
            std::thread::sleep(Duration::from_millis(1));
        }
        gate.release();
        assert_eq!(waiter.join().unwrap(), Some(RunResult::Completed));
        c.shutdown();
    }

    #[test]
    fn a_marker_left_by_a_previous_run_is_resumed_and_a_skipped_boot_leaves_it() {
        let (dir, gate, height) = (Dir::new("resume"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(3)));
        std::fs::write(dir.0.join(MARKER_FILE), "1700000000\n").unwrap();
        let skipped = compaction(&dir, &gate, &height);
        skipped.boot(true);
        assert!(skipped.status().running.is_none(), "skipped");
        assert!(skipped.marker_present(), "and the unfinished work is still recorded");

        let c = compaction(&dir, &gate, &height);
        gate.release();
        assert_eq!(c.start(Trigger::Boot), Started::Started { resumed: true });
        assert_eq!(c.wait(), Some(RunResult::Completed));
        c.shutdown();
    }

    /// The C++ bug this fixes: with the boot pass skipped the scheduler had
    /// seen no compaction at all, so its first check started one.
    #[test]
    fn a_skipped_boot_counts_the_periodic_rules_from_startup() {
        let (dir, gate, height) = (Dir::new("skip"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(4_000_000)));
        let policy = AutoCompaction::default();
        let now = unix_now();

        let fresh = compaction(&dir, &gate, &height);
        assert_eq!(
            policy.verdict(&fresh.activity(), now + 60, 4_000_010, u64::MAX),
            Verdict::Start,
            "no activity at all: the C++'s behaviour, and what a boot that did compact replaces"
        );

        fresh.boot(true);
        let activity = fresh.activity();
        assert_eq!(activity.last_height, Some(4_000_000));
        assert!(!activity.running);
        assert_eq!(
            policy.verdict(&activity, now + 60, 4_000_010, u64::MAX),
            Verdict::GapNotReached { blocks: 10 },
            "a minute later, ten blocks in"
        );
        assert_eq!(policy.verdict(&activity, now + 60, 4_000_720, u64::MAX), Verdict::TooSoon { seconds: 60 });
        assert_eq!(policy.verdict(&activity, now + 1800, 4_000_720, u64::MAX), Verdict::Start);
        fresh.shutdown();
    }

    #[test]
    fn the_periodic_rules_run_in_the_cpp_order() {
        let policy = AutoCompaction { min_gap_blocks: 720, min_free_bytes: 1000 };
        let idle = Activity { running: false, last_height: Some(10_000), last_at: Some(1_000_000) };
        let later = 1_000_000 + MIN_WALL_SECONDS;

        assert_eq!(policy.verdict(&Activity { running: true, ..idle }, later, 20_000, 0), Verdict::Running);
        assert_eq!(policy.verdict(&idle, later, 20_000, 999), Verdict::LowSpace { free: 999, required: 1000 });
        let off = AutoCompaction { min_gap_blocks: 0, ..policy };
        assert_eq!(off.verdict(&idle, later, 20_000, 5000), Verdict::Disabled);
        assert_eq!(policy.verdict(&idle, later, 10_719, 5000), Verdict::GapNotReached { blocks: 719 });
        assert_eq!(policy.verdict(&idle, later, 10_720, 5000), Verdict::Start);
        assert_eq!(policy.verdict(&idle, later - 1, 10_720, 5000), Verdict::TooSoon { seconds: MIN_WALL_SECONDS - 1 });
        // A clock behind the last activity skips the wall rule, as `now >= at` does.
        assert_eq!(policy.verdict(&idle, 999_999, 10_720, 5000), Verdict::Start);
        // Never compacted: only space and the switch decide.
        assert_eq!(policy.verdict(&Activity::default(), 0, 0, 5000), Verdict::Start);
    }

    /// No new block since the last compaction is fewer blocks than the gap.
    /// The C++ skipped the rule when the height had not risen, and so
    /// compacted a stalled node every half hour.
    #[test]
    fn a_stalled_chain_is_not_compacted_again() {
        let policy = AutoCompaction { min_gap_blocks: 720, min_free_bytes: 0 };
        let last = Activity { running: false, last_height: Some(10_000), last_at: Some(0) };
        let much_later = 10 * MIN_WALL_SECONDS;
        assert_eq!(policy.verdict(&last, much_later, 10_000, 1), Verdict::GapNotReached { blocks: 0 });
        assert_eq!(policy.verdict(&last, much_later, 9_000, 1), Verdict::GapNotReached { blocks: 0 }, "after a rewind");
    }

    #[test]
    fn the_check_interval_slows_after_three_near_synced_looks_and_speeds_up_on_a_real_lag() {
        let mut cadence = Cadence::default();
        assert_eq!(cadence.interval(), CHECK_INTERVAL_FAST);
        assert_eq!(cadence.observe(100, 102), None, "one look");
        assert_eq!(cadence.observe(100, 100), None, "two");
        // A lag between the thresholds neither counts nor resets.
        assert_eq!(cadence.observe(100, 110), None);
        assert_eq!(cadence.observe(100, 101), Some(CHECK_INTERVAL_SLOW), "the third near-synced look");
        assert_eq!(cadence.observe(100, 119), None, "19 behind keeps the streak");
        assert_eq!(cadence.observe(100, 120), Some(CHECK_INTERVAL_FAST), "20 behind resets it");
        // A local height above the network's is no lag.
        let mut ahead = Cadence::default();
        for _ in 0..2 {
            assert_eq!(ahead.observe(500, 1), None);
        }
        assert_eq!(ahead.observe(500, 1), Some(CHECK_INTERVAL_SLOW));
    }

    #[test]
    fn the_scheduler_thread_stops_promptly_and_is_not_started_when_disabled() {
        let (dir, gate, height) = (Dir::new("scheduler"), Arc::new(Gate::default()), Arc::new(AtomicU64::new(3)));
        let c = compaction(&dir, &gate, &height);
        c.start_scheduler(AutoCompaction { min_gap_blocks: 0, min_free_bytes: 0 }, Box::new(|| (0, 0)), Box::new(|| 0));
        assert!(c.scheduler.lock().unwrap().is_none(), "a zero gap starts no thread");

        c.start_scheduler(AutoCompaction::default(), Box::new(|| (0, 0)), Box::new(|| u64::MAX));
        assert!(c.scheduler.lock().unwrap().is_some());
        let began = Instant::now();
        c.shutdown();
        assert!(began.elapsed() < Duration::from_secs(5), "the stop wakes it, it does not sleep out its minute");
        assert!(c.scheduler.lock().unwrap().is_none());
        assert_eq!(gate.state.lock().unwrap().entered, 0, "nothing ran");
    }
}
