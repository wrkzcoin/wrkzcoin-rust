// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Per-connection sync state (`src/p2p/ConnectionContext.h`,
//! `src/cryptonoteprotocol/CryptoNoteProtocolHandler.cpp`; spec/08 "Sync state
//! machine").
//!
//! [`PeerCtx`] is `CryptoNoteConnectionContext` field for field, minus the
//! ones that only exist to drive the C++ console table. The transitions live
//! in [`crate::node`], where the chain and the other connections are in reach;
//! what is here is the state a single connection carries and the two purely
//! local rules that read it: the adaptive batch size and the sync-failure
//! counter.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use wrkz_p2p::msg::{CoreSyncData, LiteBlock, NODE_CAPABILITY_FLAG_LITE, NODE_CAPABILITY_FLAG_PRUNED};
use wrkz_primitives::Hash;

/// Identifies a connection for the lifetime of the process.
pub type ConnId = u64;

/// `CryptoNoteConnectionContext::state` (`ConnectionContext.h:38`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerState {
    /// `state_befor_handshake` (the C++ spelling).
    BeforeHandshake,
    /// `state_synchronizing`: a chain request or a get-objects batch is in flight.
    Synchronizing,
    /// `state_idle`: the peer sent us a block we already had; nothing to pull.
    Idle,
    /// `state_normal`: relay duty. Blocks and transactions are only accepted
    /// from a connection in this state.
    Normal,
    /// `state_sync_required`: the connection loop turns this into
    /// `Synchronizing` and sends `NOTIFY_REQUEST_CHAIN` (`NetNode.cpp:2843`).
    SyncRequired,
    /// `state_pool_sync_required`: the loop turns this into `Normal` and sends
    /// `NOTIFY_REQUEST_TX_POOL`.
    PoolSyncRequired,
    /// `state_shutdown`: the connection is being closed.
    Shutdown,
}

impl PeerState {
    /// The two states the C++ timed sync loop sends to (`timedSync`,
    /// `NetNode.cpp:934`).
    pub fn takes_timed_sync(self) -> bool {
        matches!(self, PeerState::Normal | PeerState::Idle)
    }

    /// What `process_payload_sync_data` counts against `--sync-max-peers`.
    pub fn is_syncing(self) -> bool {
        matches!(self, PeerState::Synchronizing | PeerState::SyncRequired)
    }

    pub fn name(self) -> &'static str {
        match self {
            PeerState::BeforeHandshake => "before_handshake",
            PeerState::Synchronizing => "synchronizing",
            PeerState::Idle => "idle",
            PeerState::Normal => "normal",
            PeerState::SyncRequired => "sync_required",
            PeerState::PoolSyncRequired => "pool_sync_required",
            PeerState::Shutdown => "shutdown",
        }
    }
}

/// A lite block whose transactions we asked the sender for
/// (`PendingLiteBlock.h`).
#[derive(Clone, Debug)]
pub struct PendingLiteBlock {
    pub request: LiteBlock,
    pub missed_transactions: HashSet<Hash>,
}

/// The daemon's sync knobs, with the C++ defaults
/// (`DaemonConfiguration.h:81-86`).
#[derive(Clone, Copy, Debug)]
pub struct SyncTuning {
    /// `--sync-max-peers`: how many connections may pull the chain at once.
    /// 0 disables the cap.
    pub max_peers: usize,
    /// `--sync-peer-failure-threshold`: failures before a sync peer is dropped.
    pub peer_failure_threshold: u32,
    /// `--sync-batch-min` / `--sync-batch-max`: blocks per
    /// `NOTIFY_REQUEST_GET_OBJECTS`.
    pub batch_min: u32,
    pub batch_max: u32,
    /// `--block-sync-size`: a hard ceiling on the count, independent of the
    /// adaptive value.
    pub block_sync_size: u32,
    /// `--block-sync-bytes`: the byte budget a batch is sized against, clamped
    /// to `SYNC_BLOCK_BUDGET_MIN/MAX_BYTES`.
    pub block_sync_bytes: u64,
}

/// `SYNC_BLOCK_BUDGET_MIN_BYTES` (`CryptoNoteProtocolHandler.cpp:40`).
pub const SYNC_BLOCK_BUDGET_MIN_BYTES: u64 = 2 * 1024 * 1024;
/// `SYNC_BLOCK_BUDGET_MAX_BYTES`.
pub const SYNC_BLOCK_BUDGET_MAX_BYTES: u64 = 48 * 1024 * 1024;
/// The floor the average block size is clamped to before it divides the byte
/// budget, so an empty-block chain cannot ask for an unbounded batch.
pub const SYNC_BLOCK_SIZE_ESTIMATE_FLOOR_BYTES: u64 = 256;
/// `SYNC_ORPHAN_RETRY_LIMIT`: chain re-requests after an orphan before the
/// peer is dropped.
pub const SYNC_ORPHAN_RETRY_LIMIT: u32 = 3;
/// `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT` (`CryptoNoteConfig.h:473`): the
/// most hashes one `NOTIFY_RESPONSE_CHAIN_ENTRY` may carry.
pub const BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT: usize = 10_000;

/// How long a peer has to answer `NOTIFY_REQUEST_CHAIN`. The C++ has no such
/// deadline: a peer that never answers holds one of the `--sync-max-peers`
/// slots until the idle timeout, which any other frame — a ping, a timed sync —
/// resets. A chain entry is at most 320 kB, so this is generous.
pub const CHAIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The floor and ceiling of a get-objects deadline ([`objects_timeout`]).
pub const OBJECTS_TIMEOUT_MIN: Duration = Duration::from_secs(60);
pub const OBJECTS_TIMEOUT_MAX: Duration = Duration::from_secs(180);
/// The transfer rate a get-objects deadline is scaled by: 128 KiB/s, a slow
/// link, so a batch sized to the 16 MiB default budget gets the full ceiling.
const OBJECTS_TIMEOUT_BYTES_PER_SEC: u64 = 128 * 1024;

/// How long a peer has to deliver a batch of `blocks` blocks averaging
/// `avg_block_bytes`: the floor plus the batch's expected size at a slow
/// link's rate, clamped to `[min, max]`.
pub fn objects_timeout(blocks: usize, avg_block_bytes: u64, min: Duration, max: Duration) -> Duration {
    let bytes = (blocks as u64).saturating_mul(avg_block_bytes.max(SYNC_BLOCK_SIZE_ESTIMATE_FLOOR_BYTES));
    let transfer = Duration::from_secs(bytes / OBJECTS_TIMEOUT_BYTES_PER_SEC);
    min.saturating_add(transfer).clamp(min, max.max(min))
}

impl Default for SyncTuning {
    fn default() -> Self {
        Self {
            max_peers: 3,
            peer_failure_threshold: 2,
            batch_min: 120,
            batch_max: 600,
            block_sync_size: 600,
            block_sync_bytes: 16 * 1024 * 1024,
        }
    }
}

impl SyncTuning {
    fn clamped_bytes(&self) -> u64 {
        self.block_sync_bytes.clamp(SYNC_BLOCK_BUDGET_MIN_BYTES, SYNC_BLOCK_BUDGET_MAX_BYTES)
    }
}

/// `CryptoNoteConnectionContext`.
pub struct PeerCtx {
    pub id: ConnId,
    pub addr: SocketAddr,
    /// `m_is_income`: only an inbound connection may send `COMMAND_HANDSHAKE`.
    pub incoming: bool,
    /// `peerId`, 0 until the handshake associates one.
    pub peer_id: u64,
    /// Whether `COMMAND_HANDSHAKE` has completed, in either direction. The
    /// C++ reads `peerId != 0` for this, but the id is the peer's claim: one
    /// that claimed 0 could handshake again and reset its sync-failure
    /// counters (`reset_sync_counters`).
    pub handshake_done: bool,
    /// `version`: `P2P_CURRENT_VERSION` of the peer. Decides lite-block relay
    /// (>= 4) and whether we send it an IPv6 peer list (>= 19).
    pub version: u8,
    /// The port the peer says it listens on; 0 means "do not back ping".
    pub my_port: u32,
    pub state: PeerState,

    /// `m_remote_blockchain_height`.
    pub remote_height: u32,
    /// `m_last_response_height`.
    pub last_response_height: u32,
    /// `m_needed_objects`: hashes from a chain entry we have not asked for yet.
    /// Each chain entry **replaces** it rather than appending (the C++
    /// appends), and a chain entry is only accepted in answer to our own
    /// request and carries at most [`BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT`]
    /// ids — so it holds at most that many, 320 kB.
    pub needed_objects: VecDeque<Hash>,
    /// `m_requested_objects`: hashes asked for and not yet delivered.
    pub requested_objects: HashSet<Hash>,

    /// `m_remote_is_pruned_node` / `m_remote_pruned_node_height`.
    pub remote_is_pruned: bool,
    pub remote_pruned_height: u32,
    /// `m_remote_is_lite_node` / `m_remote_lite_start_height`.
    pub remote_is_lite: bool,
    pub remote_lite_start_height: u32,

    /// `m_sync_batch_size`, adapted from the observed throughput.
    pub sync_batch_size: u32,
    /// `m_sync_failures`, `m_sync_orphan_retries`.
    pub sync_failures: u32,
    pub orphan_retries: u32,
    /// `m_sync_avg_block_bytes`, the rolling estimate the byte budget divides.
    pub avg_block_bytes: u64,
    /// `m_sync_blocks_per_second`, the rolling throughput.
    pub blocks_per_second: f32,
    /// `m_sync_chunk_start_time`.
    pub chunk_start: Instant,
    /// `m_pipelined_objects_outstanding` and `m_discard_next_objects_response`:
    /// a reply to a batch we abandoned is dropped silently, not counted
    /// against the peer.
    pub pipelined_objects_outstanding: bool,
    pub discard_next_objects_response: bool,

    /// When the `NOTIFY_REQUEST_CHAIN` in flight to this peer must be answered
    /// by; `None` when none is in flight, and a chain entry then is
    /// unsolicited.
    pub chain_request_deadline: Option<Instant>,
    /// The chain request timed out and the peer was taken off sync; its late
    /// answer is dropped once, silently, as `discard_next_objects_response`
    /// does for a batch.
    pub discard_next_chain_entry: bool,
    /// When the `NOTIFY_REQUEST_GET_OBJECTS` in flight must be answered by
    /// ([`objects_timeout`]). With a pipelined request it is the later one's.
    pub objects_deadline: Option<Instant>,
    /// We sent `COMMAND_TIMED_SYNC` and have not had its response. A response
    /// without one is unsolicited: each carries 250 peer-list entries.
    pub timed_sync_outstanding: bool,
    /// A `NOTIFY_REQUEST_GET_OBJECTS` that arrived while our answer to the
    /// previous one was still being written. Served once that one is out;
    /// another request while this is held is misbehaviour.
    pub deferred_objects_request: Option<Vec<Hash>>,

    /// `m_pending_lite_block`.
    pub pending_lite_block: Option<PendingLiteBlock>,

    /// When the connection was established, for `print_cn`'s uptime column
    /// (`CryptoNoteConnectionContext::m_started`).
    pub connected_at: Instant,
    /// When the last frame arrived, for the idle timeout.
    pub last_frame: Instant,
    /// When we last sent `COMMAND_TIMED_SYNC`.
    pub last_timed_sync: Instant,
    /// Blocks this peer has contributed, for the progress log.
    pub blocks_added: u64,
}

impl PeerCtx {
    pub fn new(id: ConnId, addr: SocketAddr, incoming: bool, tuning: &SyncTuning) -> Self {
        let now = Instant::now();
        Self {
            id,
            addr,
            incoming,
            peer_id: 0,
            handshake_done: false,
            version: 0,
            my_port: 0,
            state: PeerState::BeforeHandshake,
            remote_height: 0,
            last_response_height: 0,
            needed_objects: VecDeque::new(),
            requested_objects: HashSet::new(),
            remote_is_pruned: false,
            remote_pruned_height: 0,
            remote_is_lite: false,
            remote_lite_start_height: 0,
            sync_batch_size: tuning.batch_min,
            sync_failures: 0,
            orphan_retries: 0,
            avg_block_bytes: 0,
            blocks_per_second: 0.0,
            chunk_start: now,
            pipelined_objects_outstanding: false,
            discard_next_objects_response: false,
            chain_request_deadline: None,
            discard_next_chain_entry: false,
            objects_deadline: None,
            timed_sync_outstanding: false,
            deferred_objects_request: None,
            pending_lite_block: None,
            connected_at: now,
            last_frame: now,
            last_timed_sync: now,
            blocks_added: 0,
        }
    }

    /// The `is_initial` reset of `process_payload_sync_data`
    /// (`CryptoNoteProtocolHandler.cpp:449`). Deliberately not run on a timed
    /// sync: doing so wiped `m_discard_next_objects_response` mid-sync and got
    /// peers dropped for a reply we had abandoned ourselves.
    pub fn reset_sync_counters(&mut self, tuning: &SyncTuning) {
        self.sync_batch_size = tuning.batch_min;
        self.sync_failures = 0;
        self.orphan_retries = 0;
        self.blocks_per_second = 0.0;
        self.chunk_start = Instant::now();
        self.pipelined_objects_outstanding = false;
        self.discard_next_objects_response = false;
    }

    /// Record the peer's capability flags and floors.
    pub fn record_capabilities(&mut self, sync: &CoreSyncData) {
        self.remote_is_pruned = sync.capability_flags & NODE_CAPABILITY_FLAG_PRUNED != 0;
        self.remote_pruned_height = sync.pruned_node_height;
        self.remote_is_lite = sync.capability_flags & NODE_CAPABILITY_FLAG_LITE != 0;
        self.remote_lite_start_height = sync.lite_start_height;
    }

    /// `getPeerServingFloor` (`CryptoNoteProtocolHandler.cpp:1929`): the lowest
    /// height this peer can hand us a block body for; 0 when it holds
    /// everything. A node can be both lite and pruned, so the higher floor wins.
    pub fn serving_floor(&self) -> u32 {
        let mut floor = 0;
        if self.remote_is_lite && self.remote_lite_start_height != 0 {
            floor = floor.max(self.remote_lite_start_height);
        }
        if self.remote_is_pruned && self.remote_pruned_height != 0 {
            floor = floor.max(self.remote_pruned_height);
        }
        floor
    }

    /// `peerCanServeOurChain`: our height must have reached the peer's floor.
    pub fn can_serve_our_chain(&self, our_height: u32) -> bool {
        let floor = self.serving_floor();
        floor == 0 || our_height >= floor
    }

    /// `getAdaptiveBatchSize` (`:1831`), then the count and byte limits of
    /// `request_missing_objects` (`:1353`). Never 0 and never above
    /// `block_sync_size`.
    pub fn batch_size(&self, tuning: &SyncTuning) -> u32 {
        let current = if self.sync_batch_size == 0 { tuning.batch_min } else { self.sync_batch_size };
        let adaptive = tuning.batch_min.max(current.min(tuning.batch_max));
        let count_limited = 1.max(adaptive.min(tuning.block_sync_size));
        let avg = self.avg_block_bytes.max(SYNC_BLOCK_SIZE_ESTIMATE_FLOOR_BYTES);
        let bytes_limited = u32::try_from(tuning.clamped_bytes() / avg).unwrap_or(u32::MAX).max(1);
        1.max(count_limited.min(bytes_limited))
    }

    /// `onSyncChunkSuccess` (`:1843`). Called with the size of a well-formed
    /// chunk *before* its blocks are applied, because the throughput sample
    /// belongs to the request this reply answers.
    ///
    /// `sync_failures` and `orphan_retries` are deliberately not cleared here;
    /// the caller clears them once the chunk has actually been added.
    pub fn on_chunk_success(&mut self, blocks: usize, bytes: usize, tuning: &SyncTuning) {
        if blocks > 0 {
            let sample = 1.max(bytes as u64 / blocks as u64);
            self.avg_block_bytes =
                if self.avg_block_bytes == 0 { sample } else { (self.avg_block_bytes * 8 + sample * 2) / 10 };
            let elapsed = self.chunk_start.elapsed().as_secs_f32();
            if elapsed > 0.05 && elapsed < 300.0 {
                let sample_bps = blocks as f32 / elapsed;
                self.blocks_per_second = if self.blocks_per_second == 0.0 {
                    sample_bps
                } else {
                    self.blocks_per_second * 0.8 + sample_bps * 0.2
                };
            }
        }
        if self.blocks_per_second > 0.0 {
            // Target 30 seconds of blocks per batch.
            let dynamic = (self.blocks_per_second * 30.0) as u32;
            self.sync_batch_size = tuning.batch_min.max(dynamic.min(tuning.batch_max));
        } else if self.sync_batch_size < tuning.batch_max {
            let next = self.sync_batch_size + 1.max(self.sync_batch_size / 4);
            self.sync_batch_size = next.min(tuning.batch_max);
        }
    }

    /// `onSyncChunkFailure` (`:1900`): count it, halve the batch, and demote
    /// the peer once it passes the threshold. Returns true when the peer must
    /// be dropped.
    #[must_use]
    pub fn on_chunk_failure(&mut self, tuning: &SyncTuning) -> bool {
        self.sync_failures += 1;
        if self.sync_batch_size > tuning.batch_min {
            self.sync_batch_size = tuning.batch_min.max(self.sync_batch_size / 2);
        }
        if self.sync_failures >= tuning.peer_failure_threshold {
            self.state = PeerState::Shutdown;
            return true;
        }
        false
    }

    /// Forget the chain entry and the outstanding batch; used when a peer
    /// reorganised under us and when a block came back orphaned.
    pub fn clear_sync_lists(&mut self) {
        self.needed_objects.clear();
        self.requested_objects.clear();
    }

    /// A short label for the logs, in the shape the C++ `print_cn` table uses.
    pub fn label(&self) -> String {
        format!("[{} {} {}]", self.addr, if self.incoming { "in" } else { "out" }, self.state.name())
    }
}

/// `isPruneCapabilityForkActive` (`:50`): the fork is on once *either* side is
/// at or above `PRUNE_CAPABILITY_FORK_HEIGHT`.
pub fn prune_capability_fork_active(local_height: u64, remote_height: u64) -> bool {
    local_height.max(remote_height) >= wrkz_primitives::constants::PRUNE_CAPABILITY_FORK_HEIGHT
}

/// The top block index the network is believed to be at: the **median** of
/// the heights the handshaken peers claim, minus one.
///
/// `updateObservedHeight` (`:1656`) and `recalculateMaxObservedHeight` take
/// the tallest claim instead, so one peer claiming `u32::MAX` — before its
/// handshake, even — sets the progress percentage, `/info`'s `network_height`
/// and the prune-fork switch for everyone. The median needs half the
/// connections to lie; with an even count the lower middle value is taken, so
/// one peer in two cannot raise it. Recalculating from every connection keeps
/// the property the C++ recalculation exists for: a peer that switched to a
/// shorter chain leaves nothing stale behind.
pub fn recalculate_observed_height<'a>(peers: impl Iterator<Item = &'a PeerCtx>) -> u32 {
    let mut claims: Vec<u32> = peers.filter(|p| p.handshake_done).map(|p| p.remote_height.saturating_sub(1)).collect();
    if claims.is_empty() {
        return 0;
    }
    claims.sort_unstable();
    claims[(claims.len() - 1) / 2]
}

/// Whether a strict majority of the handshaken peers claim no more than one
/// block beyond `our_height` (a block count: tip index + 1).
///
/// This is what `synchronized` rests on. The C++ sets `m_synchronized` the
/// first time **any** peer reports a top block we hold, so one peer echoing
/// our own top back at us during initial sync would turn the write-ahead log
/// on for the rest of it, stop a `--exit-when-synced` run and tell wallets we
/// are synced. A majority is the other half of the median above: a peer that
/// is behind us agrees, one that is ahead does not, and with two peers both
/// must agree.
pub fn majority_not_ahead<'a>(peers: impl Iterator<Item = &'a PeerCtx>, our_height: u32) -> bool {
    let (mut total, mut agree) = (0usize, 0usize);
    for p in peers.filter(|p| p.handshake_done) {
        total += 1;
        if p.remote_height <= our_height.saturating_add(1) {
            agree += 1;
        }
    }
    total > 0 && agree * 2 > total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PeerCtx {
        PeerCtx::new(1, "1.2.3.4:17855".parse().unwrap(), false, &SyncTuning::default())
    }

    /// The batch starts at `--sync-batch-min` and is clamped by the count and
    /// the byte budget, exactly as `request_missing_objects` computes it.
    #[test]
    fn batch_size_matches_the_c_formula() {
        let t = SyncTuning::default();
        let mut c = ctx();
        assert_eq!(c.batch_size(&t), 120);
        // 16 MiB budget over the 256-byte floor is far above the count limit
        c.sync_batch_size = 600;
        assert_eq!(c.batch_size(&t), 600);
        // a chain of large blocks lowers the batch through the byte budget:
        // 16 MiB / 1 MiB = 16
        c.avg_block_bytes = 1024 * 1024;
        assert_eq!(c.batch_size(&t), 16);
        // and never to zero
        c.avg_block_bytes = u64::MAX;
        assert_eq!(c.batch_size(&t), 1);
        // above the max the adaptive value is clamped back down
        c.avg_block_bytes = 0;
        c.sync_batch_size = 10_000;
        assert_eq!(c.batch_size(&t), t.batch_max);
    }

    /// `onSyncChunkSuccess` grows the batch by 25% until throughput is known.
    #[test]
    fn chunk_success_grows_the_batch() {
        let t = SyncTuning::default();
        let mut c = ctx();
        // an instantaneous chunk gives no usable elapsed time, so the fallback
        // growth applies
        c.on_chunk_success(10, 2560, &t);
        assert_eq!(c.avg_block_bytes, 256);
        assert!(c.sync_batch_size > t.batch_min);
        assert!(c.sync_batch_size <= t.batch_max);
    }

    /// Two failures drop the peer, and each one halves the batch.
    #[test]
    fn chunk_failure_demotes_after_the_threshold() {
        let t = SyncTuning::default();
        let mut c = ctx();
        c.sync_batch_size = 600;
        assert!(!c.on_chunk_failure(&t));
        assert_eq!(c.sync_batch_size, 300);
        assert!(c.on_chunk_failure(&t));
        assert_eq!(c.state, PeerState::Shutdown);
    }

    /// The serving floor is the higher of the lite and pruned heights, and a
    /// peer whose floor is above us cannot serve our chain.
    #[test]
    fn serving_floor_and_reachability() {
        let mut c = ctx();
        assert_eq!(c.serving_floor(), 0);
        assert!(c.can_serve_our_chain(0));
        c.remote_is_lite = true;
        c.remote_lite_start_height = 4_000_000;
        c.remote_is_pruned = true;
        c.remote_pruned_height = 4_100_000;
        assert_eq!(c.serving_floor(), 4_100_000);
        assert!(!c.can_serve_our_chain(4_099_999));
        assert!(c.can_serve_our_chain(4_100_000));
        // a flag without a height means "serves everything"
        c.remote_lite_start_height = 0;
        c.remote_pruned_height = 0;
        assert_eq!(c.serving_floor(), 0);
    }

    #[test]
    fn prune_fork_is_active_from_either_side() {
        assert!(!prune_capability_fork_active(4_499_999, 4_499_999));
        assert!(prune_capability_fork_active(4_499_999, 4_500_000));
        assert!(prune_capability_fork_active(4_500_000, 0));
    }

    fn peer_at(id: u64, height: u32) -> PeerCtx {
        let mut p = PeerCtx::new(id, format!("{id}.1.1.1:1").parse().unwrap(), false, &SyncTuning::default());
        p.remote_height = height;
        p.handshake_done = true;
        p
    }

    /// The median of the handshaken peers' claims, minus one: one liar among
    /// three cannot move it in either direction, one in two cannot raise it,
    /// and a peer that has not handshaken counts for nothing. (This used to be
    /// the tallest claim, which one peer alone decided.)
    #[test]
    fn observed_height_is_the_median_claim_minus_one() {
        let (a, b) = (peer_at(1, 100), peer_at(2, 250));
        assert_eq!(recalculate_observed_height([&a].into_iter()), 99);
        assert_eq!(recalculate_observed_height([&a, &b].into_iter()), 99, "lower middle of two");
        let liar = peer_at(3, u32::MAX);
        assert_eq!(recalculate_observed_height([&a, &b, &liar].into_iter()), 249);
        let echo = peer_at(4, 1);
        assert_eq!(recalculate_observed_height([&a, &b, &echo].into_iter()), 99);
        let mut early = peer_at(5, u32::MAX);
        early.handshake_done = false;
        assert_eq!(recalculate_observed_height([&early].into_iter()), 0);
        // a peer that has not spoken yet contributes nothing, not an underflow
        assert_eq!(recalculate_observed_height([&peer_at(6, 0)].into_iter()), 0);
    }

    /// `synchronized` needs a strict majority of handshaken peers not ahead of
    /// us by more than a block; one echoing our top is not enough when another
    /// says the chain is taller.
    #[test]
    fn synchronized_needs_a_majority() {
        let ours = 6; // tip index 5
        let (at_tip, ahead_one, echo) = (peer_at(1, 6), peer_at(2, 7), peer_at(3, 1));
        let far = peer_at(4, 4_300_000);
        assert!(majority_not_ahead([&at_tip].into_iter(), ours));
        assert!(majority_not_ahead([&ahead_one].into_iter(), ours), "one block ahead is a race, not a lag");
        assert!(!majority_not_ahead([&echo, &far].into_iter(), ours), "one of two is not a majority");
        assert!(majority_not_ahead([&echo, &at_tip, &far].into_iter(), ours));
        assert!(!majority_not_ahead(std::iter::empty(), ours), "no peers, no claim");
    }

    /// A get-objects deadline grows with the batch and stays within its bounds.
    #[test]
    fn objects_timeout_scales_with_the_batch() {
        let (min, max) = (OBJECTS_TIMEOUT_MIN, OBJECTS_TIMEOUT_MAX);
        assert_eq!(objects_timeout(0, 0, min, max), min);
        assert_eq!(objects_timeout(120, 256, min, max), min, "a batch of empty blocks is quick");
        let mid = objects_timeout(600, 10 * 1024, min, max);
        assert!(mid > min && mid < max, "{mid:?}");
        assert_eq!(objects_timeout(600, 1024 * 1024, min, max), max);
        assert_eq!(objects_timeout(usize::MAX, u64::MAX, min, max), max, "no overflow");
    }
}
