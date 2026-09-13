// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The job queue and the hashing threads (`PowService.cpp`).
//!
//! One job is solved at a time with every thread on it, so a single wallet
//! gets its nonce as fast as this machine can produce one, and the queue keeps
//! everyone else in order. A prefix is checked before it is queued: it must be
//! a canonical transaction prefix that pays a fee and ends in the nonce field,
//! and its difficulty — derived here from its shape, never taken from the
//! client — must be within `--max-difficulty`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use wrkz_primitives::constants::{transaction_pow_difficulty, NORMAL_TX_MAX_OUTPUT_COUNT_V1, TX_POW_NONCE_SIZE};
use wrkz_primitives::tx::{Input, TransactionPrefix, TX_EXTRA_TRANSACTION_POW_NONCE};

use crate::log::{Level, Logger};

/// Anything above this is not a ring any wallet builds; it is a way to make
/// the server hash a huge prefix.
pub const MAX_RING_SIZE: usize = 128;

/// Where a job is (`PowJobState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    /// `powJobStateName`.
    pub fn name(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn finished(self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Cancelled)
    }
}

/// The shape of an accepted prefix, and the difficulty it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub inputs: usize,
    pub outputs: usize,
    pub difficulty: u64,
}

/// Why a prefix is refused (always HTTP 400).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
    /// Refused only for being over `--max-difficulty`, which `/stats` counts
    /// apart from malformed prefixes.
    pub too_difficult: bool,
}

fn refuse<T>(reason: &str) -> Result<T, Refusal> {
    Err(Refusal { reason: reason.to_string(), too_difficult: false })
}

/// `validatePrefix` (`PowService.cpp:42`), check for check and message for
/// message. `height` selects historical difficulty rules; without it the
/// current rule applies.
pub fn validate_prefix(bytes: &[u8], height: Option<u64>, max_difficulty: u64) -> Result<Shape, Refusal> {
    if bytes.len() < 1 + TX_POW_NONCE_SIZE {
        return refuse("prefix is too short");
    }
    let Ok(prefix) = TransactionPrefix::from_bytes(bytes) else {
        return refuse("prefix does not deserialize as a transaction prefix");
    };
    // The daemon hashes the canonical serialization; a prefix that round-trips
    // to other bytes would be a different transaction.
    if prefix.to_bytes() != bytes {
        return refuse("prefix is not canonically serialized");
    }
    if prefix.inputs.is_empty() {
        return refuse("prefix has no inputs");
    }
    if prefix.outputs.is_empty() {
        return refuse("prefix has no outputs");
    }
    if prefix.outputs.len() > NORMAL_TX_MAX_OUTPUT_COUNT_V1 {
        return refuse("prefix has more outputs than the network allows");
    }

    let mut input_sum: u64 = 0;
    for input in &prefix.inputs {
        let Input::Key { amount, key_offsets, .. } = input else {
            return refuse("prefix contains an input that is not a key input");
        };
        if key_offsets.is_empty() || key_offsets.len() > MAX_RING_SIZE {
            return refuse("prefix contains an input with an unreasonable ring size");
        }
        let Some(sum) = input_sum.checked_add(*amount) else { return refuse("input amounts overflow") };
        input_sum = sum;
    }
    let mut output_sum: u64 = 0;
    for output in &prefix.outputs {
        let Some(sum) = output_sum.checked_add(output.amount) else { return refuse("output amounts overflow") };
        output_sum = sum;
    }
    if output_sum > input_sum {
        return refuse("outputs exceed inputs");
    }
    // Zero fee is fusion, which the daemon holds to a different difficulty;
    // refused rather than answered with a nonce the daemon would reject.
    if output_sum == input_sum {
        return refuse("zero-fee transactions are not served");
    }

    let extra = &prefix.extra;
    if extra.len() < 1 + TX_POW_NONCE_SIZE
        || extra[extra.len() - 1 - TX_POW_NONCE_SIZE] != TX_EXTRA_TRANSACTION_POW_NONCE
    {
        return refuse("extra must end with the PoW nonce tag followed by 8 nonce bytes");
    }

    let (inputs, outputs) = (prefix.inputs.len(), prefix.outputs.len());
    let difficulty =
        transaction_pow_difficulty(height.unwrap_or(u64::MAX), false, inputs as u64, outputs as u64).unwrap_or(0);
    if difficulty == 0 {
        return refuse("no proof of work is required at that height");
    }
    if difficulty > max_difficulty {
        return Err(Refusal {
            reason: format!("difficulty {difficulty} is above this server's limit of {max_difficulty}"),
            too_difficult: true,
        });
    }
    Ok(Shape { inputs, outputs, difficulty })
}

struct JobInner {
    state: JobState,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
    nonce: [u8; TX_POW_NONCE_SIZE],
    error: String,
}

/// One queued prefix (`PowJob`).
pub struct Job {
    pub id: String,
    /// The serialized prefix, nonce field included (the trailing 8 bytes).
    pub prefix: Vec<u8>,
    pub shape: Shape,
    submitted_at: Instant,
    inner: Mutex<JobInner>,
    changed: Condvar,
    cancel_requested: AtomicBool,
    hashes: AtomicU64,
}

/// A job as a reply reports it, read under one lock.
#[derive(Clone, Debug)]
pub struct JobView {
    pub id: String,
    pub state: JobState,
    pub shape: Shape,
    pub hashes: u64,
    pub elapsed_ms: u64,
    pub nonce: [u8; TX_POW_NONCE_SIZE],
    pub error: String,
}

impl Job {
    fn new(id: String, prefix: Vec<u8>, shape: Shape) -> Self {
        Job {
            id,
            prefix,
            shape,
            submitted_at: Instant::now(),
            inner: Mutex::new(JobInner {
                state: JobState::Queued,
                started_at: None,
                finished_at: None,
                nonce: [0; TX_POW_NONCE_SIZE],
                error: String::new(),
            }),
            changed: Condvar::new(),
            cancel_requested: AtomicBool::new(false),
            hashes: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, JobInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn finished(&self) -> bool {
        self.lock().state.finished()
    }

    /// Block until the job finishes or `timeout` passes; true when finished.
    pub fn wait_for(&self, timeout: Duration) -> bool {
        let guard = self.lock();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |inner| !inner.state.finished())
            .unwrap_or_else(PoisonError::into_inner);
        guard.state.finished()
    }

    pub fn view(&self) -> JobView {
        let inner = self.lock();
        let until = inner.finished_at.unwrap_or_else(Instant::now);
        JobView {
            id: self.id.clone(),
            state: inner.state,
            shape: self.shape,
            hashes: self.hashes.load(Ordering::Relaxed),
            elapsed_ms: until.saturating_duration_since(self.submitted_at).as_millis() as u64,
            nonce: inner.nonce,
            error: inner.error.clone(),
        }
    }

    fn start(&self) {
        let mut inner = self.lock();
        inner.state = JobState::Running;
        inner.started_at = Some(Instant::now());
    }

    fn finish(&self, state: JobState, error: &str, nonce: Option<[u8; TX_POW_NONCE_SIZE]>) {
        {
            let mut inner = self.lock();
            inner.state = state;
            inner.error = error.to_string();
            inner.finished_at = Some(Instant::now());
            if let Some(n) = nonce {
                inner.nonce = n;
            }
        }
        self.changed.notify_all();
    }
}

/// What the service is allowed to do (`PowServiceLimits`).
#[derive(Clone, Debug)]
pub struct Limits {
    pub threads: usize,
    pub max_queue: usize,
    pub max_difficulty: u64,
    pub job_timeout: Duration,
    pub result_ttl: Duration,
    /// Hold every job to this difficulty instead of the network's rule. For
    /// tests, which need a nonce in milliseconds; no command-line option sets it.
    pub fixed_difficulty: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            threads: 1,
            max_queue: 64,
            max_difficulty: 1_000_000,
            job_timeout: Duration::from_secs(600),
            result_ttl: Duration::from_secs(300),
            fixed_difficulty: None,
        }
    }
}

/// The answer to a submission.
pub struct Submitted {
    pub job: Option<Arc<Job>>,
    /// 200 with a job; otherwise the status to answer with.
    pub status: u16,
    pub error: String,
}

impl Submitted {
    fn refused(status: u16, error: impl Into<String>) -> Self {
        Submitted { job: None, status, error: error.into() }
    }
}

/// Counters since start-up, for `/stats`. The HTTP layer keeps the last four.
#[derive(Default)]
pub struct Counters {
    received: AtomicU64,
    accepted: AtomicU64,
    rejected_invalid: AtomicU64,
    rejected_difficulty: AtomicU64,
    rejected_queue_full: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    expired: AtomicU64,
    hashes: AtomicU64,
    busy_ms: AtomicU64,
    solve_ms_total: AtomicU64,
    solve_ms_max: AtomicU64,
    queue_wait_ms_total: AtomicU64,
    pub requests: AtomicU64,
    pub rate_limited: AtomicU64,
    pub global_limited: AtomicU64,
    pub unauthorized: AtomicU64,
}

/// Count one.
pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

struct Queue {
    waiting: VecDeque<Arc<Job>>,
    jobs: HashMap<String, Arc<Job>>,
    active: Option<Arc<Job>>,
    stopping: bool,
}

/// The queue and the workers (`PowService`).
pub struct PowService {
    limits: Limits,
    logger: Arc<Logger>,
    queue: Mutex<Queue>,
    work: Condvar,
    stop: AtomicBool,
    pub counters: Counters,
    started_at: Instant,
    started_epoch: u64,
    coordinator: Mutex<Option<JoinHandle<()>>>,
}

impl PowService {
    /// Start the coordinator thread. [`PowService::stop`] ends it.
    pub fn start(mut limits: Limits, logger: Arc<Logger>) -> Arc<Self> {
        limits.threads = limits.threads.max(1);
        limits.max_queue = limits.max_queue.max(1);
        let service = Arc::new(PowService {
            limits,
            logger,
            queue: Mutex::new(Queue { waiting: VecDeque::new(), jobs: HashMap::new(), active: None, stopping: false }),
            work: Condvar::new(),
            stop: AtomicBool::new(false),
            counters: Counters::default(),
            started_at: Instant::now(),
            started_epoch: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            coordinator: Mutex::new(None),
        });
        let worker = Arc::clone(&service);
        let handle = std::thread::Builder::new()
            .name("pow-coordinator".into())
            .spawn(move || worker.coordinator_loop())
            .expect("spawn the coordinator thread");
        *service.coordinator.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        service
    }

    /// Stop solving. A running job ends as failed, as does everything still
    /// queued, so every held request is answered at once.
    pub fn stop(&self) {
        {
            let mut q = self.lock_queue();
            if q.stopping {
                return;
            }
            q.stopping = true;
        }
        self.stop.store(true, Ordering::SeqCst);
        self.work.notify_all();
        if let Some(handle) = self.coordinator.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = handle.join();
        }
        let leftovers: Vec<_> = self.lock_queue().waiting.drain(..).collect();
        for job in leftovers {
            job.finish(JobState::Failed, "server shutting down", None);
            bump(&self.counters.failed);
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn lock_queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Check `prefix`, work out its difficulty and queue it. `reserve_slot`
    /// runs after the checks and before queueing, so only work that would run
    /// is charged to the jobs-per-minute budget; `false` refuses with 429.
    pub fn submit(&self, prefix: &[u8], height: Option<u64>, reserve_slot: impl FnOnce() -> bool) -> Submitted {
        bump(&self.counters.received);

        let mut shape = match validate_prefix(prefix, height, self.limits.max_difficulty) {
            Ok(shape) => shape,
            Err(refusal) => {
                bump(if refusal.too_difficult {
                    &self.counters.rejected_difficulty
                } else {
                    &self.counters.rejected_invalid
                });
                return Submitted::refused(400, refusal.reason);
            }
        };
        if let Some(difficulty) = self.limits.fixed_difficulty {
            shape.difficulty = difficulty;
        }

        if !reserve_slot() {
            return Submitted::refused(429, "the server has reached its jobs-per-minute limit, retry later");
        }

        let job = {
            let mut q = self.lock_queue();
            if q.stopping {
                return Submitted::refused(503, "server is shutting down");
            }
            if q.waiting.len() >= self.limits.max_queue {
                bump(&self.counters.rejected_queue_full);
                return Submitted::refused(503, "queue is full, try again later");
            }
            let job = Arc::new(Job::new(new_job_id(), prefix.to_vec(), shape));
            q.waiting.push_back(Arc::clone(&job));
            q.jobs.insert(job.id.clone(), Arc::clone(&job));
            job
        };
        bump(&self.counters.accepted);
        self.work.notify_one();

        self.logger.log(
            Level::Debug,
            format!(
                "Job {} queued: {} inputs, {} outputs, difficulty {}",
                job.id, shape.inputs, shape.outputs, shape.difficulty
            ),
        );
        Submitted { job: Some(job), status: 200, error: String::new() }
    }

    pub fn find(&self, id: &str) -> Option<Arc<Job>> {
        self.lock_queue().jobs.get(id).cloned()
    }

    /// True when the job existed and was still cancellable. A queued job ends
    /// here and frees its slot; a running one ends when its workers notice.
    pub fn cancel(&self, id: &str) -> bool {
        let (job, was_queued) = {
            let mut q = self.lock_queue();
            let Some(job) = q.jobs.get(id).cloned() else { return false };
            if job.finished() {
                return false;
            }
            job.cancel_requested.store(true, Ordering::SeqCst);
            let before = q.waiting.len();
            q.waiting.retain(|j| !Arc::ptr_eq(j, &job));
            (job, q.waiting.len() != before)
        };
        if was_queued {
            job.finish(JobState::Cancelled, "cancelled before it started", None);
            bump(&self.counters.cancelled);
        }
        true
    }

    /// Jobs waiting for the workers.
    pub fn queued(&self) -> usize {
        self.lock_queue().waiting.len()
    }

    fn coordinator_loop(&self) {
        loop {
            let (job, expired) = {
                let mut q = self.lock_queue();
                while !q.stopping && q.waiting.is_empty() {
                    q = self.work.wait(q).unwrap_or_else(PoisonError::into_inner);
                }
                if q.stopping {
                    return;
                }
                let job = q.waiting.pop_front().expect("the queue is not empty");
                let expired = job.submitted_at.elapsed() > self.limits.job_timeout;
                if !expired {
                    job.start();
                    q.active = Some(Arc::clone(&job));
                }
                (job, expired)
            };

            if expired {
                job.finish(JobState::Failed, "expired before a worker was free", None);
                bump(&self.counters.expired);
            } else {
                self.solve(&job);
                self.lock_queue().active = None;
            }
            self.housekeeping();
        }
    }

    fn solve(&self, job: &Job) {
        let threads = self.limits.threads;
        let found = AtomicBool::new(false);
        let winner = Mutex::new(None);

        std::thread::scope(|scope| {
            for start in 0..threads {
                let (found, winner) = (&found, &winner);
                scope.spawn(move || self.pow_worker(job, start as u64, threads as u64, found, winner));
            }
        });

        let started = job.lock().started_at.unwrap_or(job.submitted_at);
        let solve_ms = started.elapsed().as_millis() as u64;
        let hashes = job.hashes.load(Ordering::Relaxed);
        self.counters.hashes.fetch_add(hashes, Ordering::Relaxed);
        self.counters.busy_ms.fetch_add(solve_ms, Ordering::Relaxed);

        if let Some(nonce) = winner.into_inner().unwrap_or_else(PoisonError::into_inner) {
            job.finish(JobState::Done, "", Some(nonce));
            bump(&self.counters.completed);
            self.counters.solve_ms_total.fetch_add(solve_ms, Ordering::Relaxed);
            let waited = started.saturating_duration_since(job.submitted_at).as_millis() as u64;
            self.counters.queue_wait_ms_total.fetch_add(waited, Ordering::Relaxed);
            self.counters.solve_ms_max.fetch_max(solve_ms, Ordering::Relaxed);
            self.logger.log(Level::Info, format!("Job {} solved in {solve_ms} ms after {hashes} hashes", job.id));
        } else if job.cancel_requested.load(Ordering::SeqCst) {
            job.finish(JobState::Cancelled, "cancelled by the client", None);
            bump(&self.counters.cancelled);
            self.logger.log(Level::Info, format!("Job {} cancelled", job.id));
        } else {
            job.finish(JobState::Failed, "server shutting down", None);
            bump(&self.counters.failed);
        }
    }

    /// Thread `start` of `step` tries nonces `start`, `start + step`, …, as the
    /// C++ workers do; the nonce is written little-endian over the last 8 bytes.
    fn pow_worker(
        &self,
        job: &Job,
        start: u64,
        step: u64,
        found: &AtomicBool,
        winner: &Mutex<Option<[u8; TX_POW_NONCE_SIZE]>>,
    ) {
        let mut prefix = job.prefix.clone();
        let at = prefix.len() - TX_POW_NONCE_SIZE;
        let mut nonce = start;
        let mut since_report = 0u64;

        while !found.load(Ordering::Relaxed)
            && !job.cancel_requested.load(Ordering::Relaxed)
            && !self.stop.load(Ordering::Relaxed)
        {
            prefix[at..].copy_from_slice(&nonce.to_le_bytes());
            since_report += 1;
            if wrkz_pow::check_hash(&wrkz_pow::cn_upx(&prefix), job.shape.difficulty) {
                if !found.swap(true, Ordering::SeqCst) {
                    *winner.lock().unwrap_or_else(PoisonError::into_inner) = Some(nonce.to_le_bytes());
                }
                break;
            }
            nonce = nonce.wrapping_add(step);
            if since_report == 256 {
                job.hashes.fetch_add(256, Ordering::Relaxed);
                since_report = 0;
            }
        }
        job.hashes.fetch_add(since_report, Ordering::Relaxed);
    }

    /// Forget finished jobs older than `--result-ttl`.
    fn housekeeping(&self) {
        let ttl = self.limits.result_ttl;
        self.lock_queue().jobs.retain(|_, job| {
            let inner = job.lock();
            !(inner.state.finished() && inner.finished_at.is_some_and(|at| at.elapsed() > ttl))
        });
    }

    /// `statsJson`: counters only. The running job appears by shape, never by
    /// id, because `/stats` is public and the id is what `DELETE` takes.
    pub fn stats_json(&self) -> Value {
        let (queued, tracked, active) = {
            let q = self.lock_queue();
            let active = q.active.as_ref().map(|job| {
                let v = job.view();
                let running_ms = job.lock().started_at.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
                json!({
                    "difficulty": v.shape.difficulty,
                    "inputs": v.shape.inputs,
                    "outputs": v.shape.outputs,
                    "hashes": v.hashes,
                    "running_ms": running_ms,
                })
            });
            (q.waiting.len(), q.jobs.len(), active)
        };
        let c = &self.counters;
        let (completed, busy_ms, hashes) = (get(&c.completed), get(&c.busy_ms), get(&c.hashes));
        json!({
            "started_at": self.started_epoch,
            "uptime_seconds": self.started_at.elapsed().as_secs(),
            "threads": self.limits.threads,
            "queue": { "waiting": queued, "capacity": self.limits.max_queue, "tracked_jobs": tracked },
            "active": active,
            "jobs": {
                "received": get(&c.received),
                "accepted": get(&c.accepted),
                "completed": completed,
                "failed": get(&c.failed),
                "cancelled": get(&c.cancelled),
                "expired": get(&c.expired),
                "rejected_invalid": get(&c.rejected_invalid),
                "rejected_difficulty": get(&c.rejected_difficulty),
                "rejected_queue_full": get(&c.rejected_queue_full),
                "rejected_rate_limited": get(&c.rate_limited),
                "rejected_global_limit": get(&c.global_limited),
            },
            "work": {
                "hashes": hashes,
                "busy_ms": busy_ms,
                "hashrate": if busy_ms == 0 { 0.0 } else { hashes as f64 * 1000.0 / busy_ms as f64 },
                "solve_ms_avg": get(&c.solve_ms_total).checked_div(completed).unwrap_or(0),
                "solve_ms_max": get(&c.solve_ms_max),
                "queue_wait_ms_avg": get(&c.queue_wait_ms_total).checked_div(completed).unwrap_or(0),
            },
            "http": { "requests": get(&c.requests), "unauthorized": get(&c.unauthorized) },
        })
    }
}

/// 32 hex characters from the system CSPRNG. The id is the capability to
/// cancel a job, so it must not be guessable (the C++ draws it from a seeded
/// `mt19937_64`).
fn new_job_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    hex::encode(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wrkz_primitives::tx::Output;

    /// A prefix of `inputs` (amount, ring size) and `outputs`, ending in a
    /// zeroed nonce field unless `nonce_field` is false.
    pub(crate) fn prefix(inputs: &[(u64, usize)], outputs: &[u64], nonce_field: bool) -> Vec<u8> {
        let mut extra = vec![0x01];
        extra.extend_from_slice(&[0x11; 32]);
        if nonce_field {
            extra.push(TX_EXTRA_TRANSACTION_POW_NONCE);
            extra.extend_from_slice(&[0; TX_POW_NONCE_SIZE]);
        }
        TransactionPrefix {
            version: 1,
            unlock_time: 0,
            inputs: inputs
                .iter()
                .map(|&(amount, ring)| Input::Key {
                    amount,
                    key_offsets: (1..=ring as u64).collect(),
                    key_image: [7; 32],
                })
                .collect(),
            outputs: outputs.iter().map(|&amount| Output { amount, key: [9; 32] }).collect(),
            extra,
        }
        .to_bytes()
    }

    const NOW: Option<u64> = Some(4_400_000);

    #[test]
    fn a_wallet_prefix_is_accepted_with_the_network_difficulty() {
        let shape =
            validate_prefix(&prefix(&[(1000, 8), (500, 8)], &[900, 400, 100, 50, 30, 10], true), NOW, 1_000_000);
        // 40,000 + (2 + 4 × 6) × 1,000: the guide's own example.
        assert_eq!(shape, Ok(Shape { inputs: 2, outputs: 6, difficulty: 66_000 }));
    }

    #[test]
    fn every_refusal_has_the_cpp_message() {
        let good = prefix(&[(1000, 8)], &[900], true);
        let cases: Vec<(Vec<u8>, Option<u64>, u64, &str)> = vec![
            (vec![0; 5], NOW, 1_000_000, "prefix is too short"),
            (vec![0xff; 64], NOW, 1_000_000, "does not deserialize"),
            ([good.clone(), vec![0]].concat(), NOW, 1_000_000, "does not deserialize"),
            (prefix(&[], &[900], true), NOW, 1_000_000, "no inputs"),
            (prefix(&[(1000, 8)], &[], true), NOW, 1_000_000, "no outputs"),
            (prefix(&[(1_000_000, 8)], &vec![1; 91], true), NOW, 1_000_000, "more outputs than the network allows"),
            (prefix(&[(1000, 0)], &[900], true), NOW, 1_000_000, "unreasonable ring size"),
            (prefix(&[(1000, 129)], &[900], true), NOW, 1_000_000, "unreasonable ring size"),
            (prefix(&[(u64::MAX, 2), (1, 2)], &[900], true), NOW, 1_000_000, "input amounts overflow"),
            (prefix(&[(1000, 8)], &[u64::MAX, 1], true), NOW, 1_000_000, "output amounts overflow"),
            (prefix(&[(1000, 8)], &[1001], true), NOW, 1_000_000, "outputs exceed inputs"),
            (prefix(&[(1000, 8)], &[1000], true), NOW, 1_000_000, "zero-fee transactions are not served"),
            (prefix(&[(1000, 8)], &[900], false), NOW, 1_000_000, "must end with the PoW nonce tag"),
            (good.clone(), Some(1_000_000), 1_000_000, "no proof of work is required at that height"),
            (good, NOW, 44_999, "difficulty 45000 is above this server's limit of 44999"),
        ];
        for (bytes, height, max, needle) in cases {
            let refusal = validate_prefix(&bytes, height, max).unwrap_err();
            assert!(refusal.reason.contains(needle), "{needle}: got {:?}", refusal.reason);
            assert_eq!(refusal.too_difficult, needle.starts_with("difficulty"), "{needle}");
        }
    }

    #[test]
    fn job_ids_are_32_hex_characters_and_differ() {
        let (a, b) = (new_job_id(), new_job_id());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    fn service(limits: Limits) -> Arc<PowService> {
        PowService::start(limits, Arc::new(Logger::new(Level::Disabled)))
    }

    #[test]
    fn a_job_is_solved_and_the_nonce_meets_its_difficulty() {
        let s = service(Limits { threads: 2, fixed_difficulty: Some(200), ..Limits::default() });
        let bytes = prefix(&[(1000, 8)], &[900], true);
        let job = s.submit(&bytes, NOW, || true).job.expect("queued");
        assert!(job.wait_for(Duration::from_secs(60)));
        let v = job.view();
        assert_eq!(v.state, JobState::Done);
        let mut solved = bytes;
        let at = solved.len() - TX_POW_NONCE_SIZE;
        solved[at..].copy_from_slice(&v.nonce);
        assert!(wrkz_pow::check_hash(&wrkz_pow::cn_upx(&solved), 200));
        assert!(v.hashes >= 1);
        assert_eq!(s.stats_json()["jobs"]["completed"], 1);
        s.stop();
    }

    #[test]
    fn a_full_queue_a_spent_budget_and_a_stopped_server_refuse() {
        // A difficulty nobody reaches keeps the first job running.
        let s = service(Limits { max_queue: 1, fixed_difficulty: Some(u64::MAX), ..Limits::default() });
        let bytes = prefix(&[(1000, 8)], &[900], true);
        let running = s.submit(&bytes, NOW, || true).job.unwrap();
        while running.view().state != JobState::Running {
            std::thread::sleep(Duration::from_millis(5));
        }
        let queued = s.submit(&bytes, NOW, || true).job.unwrap();
        let full = s.submit(&bytes, NOW, || true);
        assert_eq!((full.status, full.error.as_str()), (503, "queue is full, try again later"));
        let limited = s.submit(&bytes, NOW, || false);
        assert_eq!(limited.status, 429);

        // Cancelling the queued one frees its slot at once.
        assert!(s.cancel(&queued.id));
        assert_eq!(queued.view().state, JobState::Cancelled);
        assert!(!s.cancel(&queued.id), "already finished");
        assert!(!s.cancel("00000000000000000000000000000000"));

        s.stop();
        assert_eq!(running.view().state, JobState::Failed, "a stop answers the held request");
        assert_eq!(s.submit(&bytes, NOW, || true).status, 503);
        let stats = s.stats_json();
        assert_eq!(stats["jobs"]["rejected_queue_full"], 1);
        assert!(stats["active"].is_null() || stats["active"].get("job_id").is_none());
    }

    #[test]
    fn a_running_job_is_cancelled_by_its_workers() {
        let s = service(Limits { threads: 2, fixed_difficulty: Some(u64::MAX), ..Limits::default() });
        let job = s.submit(&prefix(&[(1000, 8)], &[900], true), NOW, || true).job.unwrap();
        while job.view().state != JobState::Running {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(s.stats_json()["active"]["difficulty"] == u64::MAX);
        assert!(s.cancel(&job.id));
        assert!(job.wait_for(Duration::from_secs(30)));
        assert_eq!(job.view().state, JobState::Cancelled);
        s.stop();
    }
}
