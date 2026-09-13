// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! White and gray peer lists, the peer state file, bans and the dial
//! selection (`src/p2p/PeerListManager.cpp`, `src/p2p/NetNode.cpp`; spec/08
//! "Peer list handling", "Peer state file").
//!
//! The semantics are the C++ ones and are deliberately not simplified: every
//! other node on the network builds its lists from ours, so a peer that never
//! reaches the white list is a peer nobody learns about.
//!
//! - a peer becomes **white** after a successful outbound handshake or a
//!   successful back ping (`NetNode.cpp:2288`, `PeerListManager.cpp:242`);
//! - received entries go to **gray** only, and an address already white is
//!   left alone (`append_with_peer_gray`);
//! - white holds 1000, gray 5000, oldest `last_seen` dropped
//!   (`Peerlist::trim`);
//! - what we send is the white list sorted by `last_seen` descending with
//!   `last_seen == 0` skipped, at most 250 entries (`get_peerlist_head`);
//! - loopback is never allowed and private addresses only with
//!   `--allow-local-ip` (`is_ip_allowed`, `PeerListManager.cpp:126`).
//!
//! # Where this deliberately departs from the C++
//!
//! None of these change what the lists *send*, so a C++ node sees the same
//! peer lists from us; they change what we keep and whom we dial, which is
//! local policy against a peer that tries to fill our lists with its own
//! addresses (an eclipse):
//!
//! - a shifted `last_seen` is clamped to our clock. The C++ shifts by the
//!   peer's claimed `local_time`, so a peer claiming `local_time = u64::MAX`
//!   hands us entries that outrank every honest one at each trim;
//! - one network group ([`net_group`]: an IPv4 /16, an IPv6 /32) holds at most
//!   [`GRAY_PER_GROUP_LIMIT`] gray entries, however many a peer offers;
//! - outbound candidates are picked at random, at most one per network group
//!   that is not already connected, instead of a rotating cursor;
//! - misbehaviour is scored per address and bans last a day and survive a
//!   restart ([`BANS_FILENAME`]); the last good outbound peers are kept as
//!   anchors ([`ANCHORS_FILENAME`]) and dialled first on start. Both are
//!   separate files: `p2pstate.wrkz.bin` keeps the C++ layout byte for byte.
//!
//! # The peer state file
//!
//! `p2pstate.wrkz.bin` is written with `BinaryOutputStreamSerializer`
//! (`NetNode.cpp:827`), **not** the KV binary of the wire: every integer is a
//! varint and no field names are stored. spec/08 calls it KV binary, which is
//! wrong; the layout implemented here is the one `NodeServer::serialize`
//! (`NetNode.cpp:291`) and `PeerlistManager::serialize`
//! (`PeerListManager.cpp:14`) actually produce, so a C++ node reads a file we
//! wrote and we read one it wrote.
//!
//! ```text
//! version      varint = 1          NodeServer::serialize
//!   version    varint = 2          PeerlistManager::serialize
//!   whitelist  varint count, then count x { ip varint, port varint, id varint, last_seen varint }
//!   graylist   same
//!   whitelist6 varint count, then count x { ip 16 raw bytes, port varint, id varint, last_seen varint }
//!   graylist6  same
//! peer_id      varint
//! ```

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wrkz_p2p::msg::{PeerlistEntry, PeerlistEntry6, MAX_PEERLIST_ENTRIES};
use wrkz_primitives::constants::{P2P_LOCAL_GRAY_PEERLIST_LIMIT, P2P_LOCAL_WHITE_PEERLIST_LIMIT};
use wrkz_primitives::varint;

use crate::{log_debug, log_warn};

/// An address that failed to connect is skipped for this long
/// (`connections_maker`, spec/08).
pub const FAILED_ADDRESS_BACKOFF: Duration = Duration::from_secs(600);
/// The C++ ban length (`CryptoNoteProtocolHandler.cpp:1046`, and the console's
/// `ban add` default). The engine's own bans use [`MISBEHAVIOUR_BAN_SECONDS`].
pub const DEFAULT_BAN_SECONDS: u64 = 900;
/// How long an automatic ban lasts: a checkpoint mismatch, bad proof of work,
/// an invalid or unrequested block, a malformed message, or enough lesser
/// offences to reach [`BAN_THRESHOLD`]. The C++ bans only for a checkpoint
/// mismatch and only for 900 s, so a peer that sends garbage is merely
/// disconnected and may dial straight back in.
pub const MISBEHAVIOUR_BAN_SECONDS: u64 = 24 * 60 * 60;
/// Misbehaviour points at which an address is banned ([`Offence::points`]).
pub const BAN_THRESHOLD: u32 = 100;
/// Points are forgotten once an address has behaved for this long.
const SCORE_MEMORY: Duration = Duration::from_secs(24 * 60 * 60);
/// Addresses with a live score. Past this the stalest score is forgotten, so a
/// peer rotating through addresses cannot grow the table without bound.
const MAX_SCORED_ADDRESSES: usize = 4096;
/// Gray entries one network group ([`net_group`]) may hold. An honest group
/// is a hosting provider's /16 with a handful of nodes in it; a peer offering
/// hundreds of addresses in one group is trying to crowd the list.
pub const GRAY_PER_GROUP_LIMIT: usize = 16;
/// The ban list, next to `p2pstate.wrkz.bin`: `<ip> <unix end>` per line.
pub const BANS_FILENAME: &str = "p2pbans.wrkz.txt";
/// The anchor peers, next to `p2pstate.wrkz.bin`: one `ip:port` per line.
pub const ANCHORS_FILENAME: &str = "p2panchors.wrkz.txt";
/// Outbound peers kept as anchors across a restart.
pub const MAX_ANCHORS: usize = 3;
/// `P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT`.
const WHITE_PERCENT: usize = 70;
/// A peer state file above this is refused rather than decoded: the two lists
/// together hold 6000 entries, so a legitimate file is a few hundred kilobytes.
const MAX_STATE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Unix seconds; the file and the wire both carry `last_seen` in them.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    // A failure here would mean the OS has no entropy source; a peer id that
    // collides is only a self-connection check, so fall back to the clock
    // rather than refusing to start.
    if getrandom::fill(&mut b).is_err() {
        return now_secs().wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    u64::from_le_bytes(b)
}

/// `Common::ipAddressToString` order: the octets as they arrive on the wire.
fn ipv4_of(entry: &PeerlistEntry) -> Ipv4Addr {
    Ipv4Addr::new(entry.ip[0], entry.ip[1], entry.ip[2], entry.ip[3])
}

fn socket_addr_of(entry: &PeerlistEntry) -> Option<SocketAddr> {
    u16::try_from(entry.port).ok().map(|p| SocketAddr::new(IpAddr::V4(ipv4_of(entry)), p))
}

fn socket_addr_of6(entry: &PeerlistEntry6) -> Option<SocketAddr> {
    u16::try_from(entry.port).ok().map(|p| SocketAddr::new(IpAddr::V6(Ipv6Addr::from(entry.ip)), p))
}

/// `is_ip_allowed` (`PeerListManager.cpp:126`): loopback never, private only
/// with `--allow-local-ip`. Extended to IPv6 with the equivalent classes,
/// which the C++ only checks for IPv4 because its back ping is IPv4-only.
fn ip_allowed(ip: IpAddr, allow_local: bool) -> bool {
    match ip {
        IpAddr::V4(a) => {
            if a.is_loopback() || a.is_unspecified() || a.is_broadcast() || a.is_multicast() {
                return false;
            }
            allow_local || !(a.is_private() || a.is_link_local())
        }
        IpAddr::V6(a) => {
            if a.is_loopback() || a.is_unspecified() || a.is_multicast() {
                return false;
            }
            // fe80::/10 link-local and fc00::/7 unique-local are the v6
            // equivalents of the private ranges above.
            let local = (a.segments()[0] & 0xffc0) == 0xfe80 || (a.octets()[0] & 0xfe) == 0xfc;
            allow_local || !local
        }
    }
}

/// The network group of an address, for spreading connections and capping
/// what one group may put in the gray list.
///
/// A routable IPv4 address is grouped by its /16 and a routable IPv6 address
/// by its /32 (the usual size of one provider's allocation); an IPv4-mapped
/// IPv6 address is its IPv4 address. A private, loopback or link-local address
/// is a group of its own: those only reach the lists with `--allow-local-ip`,
/// which is a LAN or a test where every peer shares a prefix, and grouping
/// them would leave such a node one outbound peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetGroup {
    V4([u8; 2]),
    V6([u8; 4]),
    Local(IpAddr),
}

pub fn net_group(ip: IpAddr) -> NetGroup {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    if !ip_allowed(ip, false) {
        return NetGroup::Local(ip);
    }
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            NetGroup::V4([o[0], o[1]])
        }
        IpAddr::V6(a) => {
            let o = a.octets();
            NetGroup::V6([o[0], o[1], o[2], o[3]])
        }
    }
}

/// SplitMix64, seeded once per use from the OS. The dial selection shuffles
/// lists of up to 6000 entries every few ticks; one `getrandom` call per
/// shuffle rather than per swap, and nothing here needs more than "not
/// predictable by the peer that filled the list".
struct Rng(u64);

impl Rng {
    fn seeded() -> Self {
        Self(random_u64())
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Fisher-Yates.
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

/// Write `text` to `path` through a sibling and a rename, so a crash mid-write
/// never leaves half a file where the next start reads one.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Read a small text file this module wrote, refusing anything implausibly
/// large rather than reading it into memory.
fn read_small(path: &Path) -> Option<String> {
    const MAX_TEXT_FILE_BYTES: u64 = 1024 * 1024;
    match std::fs::metadata(path) {
        Ok(m) if m.len() <= MAX_TEXT_FILE_BYTES => std::fs::read_to_string(path).ok(),
        Ok(m) => {
            log_warn!("{} is {} bytes, ignoring it", path.display(), m.len());
            None
        }
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// the peer state file
// ---------------------------------------------------------------------------

struct VarintReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> VarintReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn varint(&mut self) -> io::Result<u64> {
        let (v, used) = varint::read(&self.data[self.pos..])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("peer state varint: {e}")))?;
        self.pos += used;
        Ok(v)
    }

    fn u32(&mut self) -> io::Result<u32> {
        let v = self.varint()?;
        u32::try_from(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "peer state u32 out of range"))
    }

    fn raw16(&mut self) -> io::Result<[u8; 16]> {
        let end = self.pos.checked_add(16).ok_or_else(|| io::Error::other("peer state overflow"))?;
        if end > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer state truncated"));
        }
        let out: [u8; 16] = self.data[self.pos..end].try_into().expect("16 bytes");
        self.pos = end;
        Ok(out)
    }

    /// A declared element count is never used to pre-allocate: an entry is at
    /// least four bytes, so anything the remaining input cannot hold is a
    /// corrupt file rather than a reason to reserve gigabytes.
    fn count(&mut self, min_elem_bytes: usize, limit: usize) -> io::Result<usize> {
        let n = self.varint()? as usize;
        let remaining = self.data.len() - self.pos;
        if n > limit || n.saturating_mul(min_elem_bytes) > remaining {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("peer state list of {n} entries")));
        }
        Ok(n)
    }
}

fn write_varint(out: &mut Vec<u8>, v: u64) {
    varint::write(out, v);
}

// ---------------------------------------------------------------------------

/// The in-memory host bans (`NodeServer::ban_host`, `NetNode.cpp:2391`).
///
/// Shared rather than owned by [`PeerManager`] for one reason: the engine is
/// single-threaded and owns the manager, but the daemon console runs on the
/// stdin thread and has to be able to add and remove a ban *now* — the C++
/// `ban` command calls straight into `NodeServer` from the console thread and
/// the next accept sees it. A handle behind a `Mutex` gives the same
/// immediacy without a request queue and without the console ever touching the
/// rest of the peer state.
///
/// The lock is held only for the map operation itself, never across I/O.
///
/// Unlike the C++ table, the bans outlive the process: [`PeerManager::save`]
/// writes them to [`BANS_FILENAME`] and [`PeerManager::open`] reads them back,
/// so a restart is not an amnesty. Each ban keeps its wall-clock end next to
/// the monotonic one for exactly that purpose.
#[derive(Clone, Debug, Default)]
pub struct BanList(Arc<Mutex<HashMap<IpAddr, Ban>>>);

#[derive(Clone, Copy, Debug)]
struct Ban {
    until: Instant,
    /// The same moment in Unix seconds, for the file.
    until_unix: u64,
}

impl BanList {
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Ban>> {
        // A panic in a console command must not poison the node's accept path:
        // the map is a plain table and is consistent whatever the panic was.
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `ban_host`. An existing ban is replaced, as the C++ does.
    pub fn ban(&self, ip: IpAddr, seconds: u64) {
        let ban = Ban {
            until: Instant::now() + Duration::from_secs(seconds),
            until_unix: now_secs().saturating_add(seconds),
        };
        self.map().insert(ip, ban);
    }

    /// `unban_host`. `false` when the address was not banned.
    pub fn unban(&self, ip: IpAddr) -> bool {
        self.map().remove(&ip).is_some()
    }

    pub fn is_banned(&self, ip: IpAddr) -> bool {
        self.map().get(&ip).is_some_and(|b| b.until > Instant::now())
    }

    /// Drop expired entries so the map does not grow with every address ever
    /// banned.
    pub fn expire(&self) {
        let now = Instant::now();
        self.map().retain(|_, b| b.until > now);
    }

    /// `(address, seconds remaining)`, sorted by address, for `ban list`.
    /// Expired entries are left out rather than reported as `0s`.
    pub fn entries(&self) -> Vec<(IpAddr, u64)> {
        let now = Instant::now();
        let mut rows: Vec<(IpAddr, u64)> = self
            .map()
            .iter()
            .filter(|(_, b)| b.until > now)
            .map(|(ip, b)| (*ip, b.until.saturating_duration_since(now).as_secs()))
            .collect();
        rows.sort_by_key(|(ip, _)| *ip);
        rows
    }

    /// The live bans as the text of [`BANS_FILENAME`]: `<ip> <unix end>` per
    /// line, sorted by address, so an unchanged table is an unchanged file.
    pub fn to_text(&self) -> String {
        let now = Instant::now();
        let mut rows: Vec<(IpAddr, u64)> =
            self.map().iter().filter(|(_, b)| b.until > now).map(|(ip, b)| (*ip, b.until_unix)).collect();
        rows.sort();
        rows.iter().map(|(ip, end)| format!("{ip} {end}\n")).collect()
    }

    /// Add the bans in `text` (the [`BanList::to_text`] format) that have not
    /// yet ended at `now_unix`. Lines that do not read are skipped: a damaged
    /// ban file loses those bans, never the node. Returns how many were added.
    pub fn load_text(&self, text: &str, now_unix: u64) -> usize {
        let mut added = 0;
        let mut map = self.map();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(ip), Some(end)) = (parts.next(), parts.next()) else { continue };
            let (Ok(ip), Ok(end)) = (ip.parse::<IpAddr>(), end.parse::<u64>()) else { continue };
            if end <= now_unix {
                continue;
            }
            let remaining = Duration::from_secs(end - now_unix);
            map.insert(ip, Ban { until: Instant::now() + remaining, until_unix: end });
            added += 1;
        }
        added
    }
}

/// What a peer did wrong, and what it costs the address. An offence worth
/// [`BAN_THRESHOLD`] bans at once; the lesser ones are things an honest peer
/// does not do but a buggy or out-of-date one might, and ban only when they
/// add up within `SCORE_MEMORY`.
///
/// Only things the peer is unambiguously responsible for are here. A block
/// that fails on our clock (`TIMESTAMP_TOO_FAR_IN_FUTURE`), on a rule that
/// depends on which chain the peer is on (a spent key image, a fork's version
/// or fee rules) or on our own state is not an offence: the peer is dropped as
/// before and may come back.
///
/// An invalid block is scored, not banned at once. Past its proof of work — a
/// block that carries transactions is checked for that first — it took real
/// work to make, so the likelier cause is a disagreement between
/// implementations, around a fork or from a bug on either side, than an
/// attack. Bans outlive a restart, so an instant ban would still cut the node
/// off from honest peers for a day after the bug was fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offence {
    /// A frame or message that does not decode, or a Levin header that is not
    /// one.
    Malformed,
    /// A block that fails a rule its sender could have checked from the block
    /// and its parent alone.
    InvalidBlock,
    /// `PROOF_OF_WORK_TOO_WEAK`.
    BadProofOfWork,
    /// A block in a get-objects response that we never asked for.
    UnrequestedBlock,
    /// `CHECKPOINT_BLOCK_HASH_MISMATCH`, the one automatic ban of the C++.
    CheckpointMismatch,
    /// Protocol traffic nobody asked for: a chain entry or timed sync response
    /// without a request, an unknown command, an unexpected response, anything
    /// but a handshake or a ping before the handshake, a second handshake, or
    /// get-objects requests piling up faster than we answer them.
    Unsolicited,
    /// A frame over its command's size cap (`wrkz_p2p::limits`).
    Oversized,
    /// A relayed transaction that fails a rule no honest relay passes on: it
    /// does not parse, or its signatures or proof of work are wrong.
    InvalidTransaction,
}

impl Offence {
    pub fn points(self) -> u32 {
        match self {
            Offence::Malformed | Offence::BadProofOfWork | Offence::UnrequestedBlock | Offence::CheckpointMismatch => {
                BAN_THRESHOLD
            }
            // Not at once; see the type's docs.
            Offence::InvalidBlock | Offence::Unsolicited | Offence::Oversized => 25,
            Offence::InvalidTransaction => 20,
        }
    }
}

/// One address's misbehaviour.
#[derive(Clone, Copy, Debug)]
struct Score {
    points: u32,
    last: Instant,
}

/// The white and gray lists, the peer id, the ban table, the misbehaviour
/// scores and the anchors.
pub struct PeerManager {
    peer_id: u64,
    white: Vec<PeerlistEntry>,
    gray: Vec<PeerlistEntry>,
    white6: Vec<PeerlistEntry6>,
    gray6: Vec<PeerlistEntry6>,
    allow_local_ip: bool,
    path: Option<PathBuf>,
    /// Addresses that failed to connect, and when: skipped for
    /// [`FAILED_ADDRESS_BACKOFF`].
    failed: HashMap<SocketAddr, Instant>,
    /// Bans by address (`ban_host`, `NetNode.cpp:2391`). Shared, so the
    /// console can add and remove one without the engine's help.
    banned: BanList,
    /// The ban file's text as last written, so an unchanged table is not
    /// rewritten on every store interval.
    bans_written: String,
    /// Misbehaviour points by address ([`PeerManager::penalise`]).
    scores: HashMap<IpAddr, Score>,
    /// The anchors read at start, handed out once by
    /// [`PeerManager::take_anchors`].
    anchors: Vec<SocketAddr>,
    /// Seeds and command-line peers: always dialable, never in the lists.
    seeds: Vec<SocketAddr>,
    dirty: bool,
}

impl PeerManager {
    pub fn new(allow_local_ip: bool) -> Self {
        Self {
            peer_id: random_u64(),
            white: Vec::new(),
            gray: Vec::new(),
            white6: Vec::new(),
            gray6: Vec::new(),
            allow_local_ip,
            path: None,
            failed: HashMap::new(),
            banned: BanList::new(),
            bans_written: String::new(),
            scores: HashMap::new(),
            anchors: Vec::new(),
            seeds: Vec::new(),
            dirty: false,
        }
    }

    /// Read `path` if it exists, otherwise start empty with a fresh peer id.
    /// `reset` is `--p2p-reset-peerstate`: a new peer id and empty lists.
    ///
    /// A file that does not decode is logged and replaced rather than fatal:
    /// the C++ does the same (`init_config` catches and calls
    /// `make_default_config`), and refusing to start over a peer list would
    /// turn a corrupt cache into an outage.
    ///
    /// The ban list ([`BANS_FILENAME`]) is read from the same directory even
    /// with `reset`: forgetting who we know is not forgiving who misbehaved.
    /// The anchors ([`ANCHORS_FILENAME`]) are part of the peer state and are
    /// not.
    pub fn open(path: &Path, allow_local_ip: bool, reset: bool) -> Self {
        let mut pm = Self::new(allow_local_ip);
        pm.path = Some(path.to_path_buf());
        if let Some(text) = read_small(&path.with_file_name(BANS_FILENAME)) {
            let n = pm.banned.load_text(&text, now_secs());
            if n > 0 {
                log_debug!("{n} bans loaded from {BANS_FILENAME}");
            }
            pm.bans_written = pm.banned.to_text();
        }
        if reset {
            return pm;
        }
        if let Some(text) = read_small(&path.with_file_name(ANCHORS_FILENAME)) {
            pm.anchors = text.lines().filter_map(|l| l.trim().parse().ok()).take(MAX_ANCHORS).collect();
        }
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_STATE_FILE_BYTES => {
                log_warn!("peer state file {} is {} bytes, ignoring it", path.display(), m.len());
                return pm;
            }
            Ok(_) => {}
            Err(_) => return pm,
        }
        match std::fs::read(path).and_then(|b| pm.decode(&b)) {
            Ok(()) => log_debug!(
                "peer state loaded from {}: {} white, {} gray ({} white6, {} gray6)",
                path.display(),
                pm.white.len(),
                pm.gray.len(),
                pm.white6.len(),
                pm.gray6.len()
            ),
            Err(e) => log_warn!("peer state file {} is unusable ({e}); starting with empty lists", path.display()),
        }
        pm
    }

    pub fn peer_id(&self) -> u64 {
        self.peer_id
    }

    pub fn white_count(&self) -> usize {
        self.white.len() + self.white6.len()
    }

    pub fn gray_count(&self) -> usize {
        self.gray.len() + self.gray6.len()
    }

    /// The white list as `ip:port`, which is what `/peers` prints
    /// (`RpcServer::peers`, `RpcServer.cpp:1016`).
    pub fn white_addresses(&self) -> Vec<SocketAddr> {
        self.white.iter().filter_map(socket_addr_of).chain(self.white6.iter().filter_map(socket_addr_of6)).collect()
    }

    /// The gray list as `ip:port`, for `/peers`'s `peers_gray`.
    pub fn gray_addresses(&self) -> Vec<SocketAddr> {
        self.gray.iter().filter_map(socket_addr_of).chain(self.gray6.iter().filter_map(socket_addr_of6)).collect()
    }

    /// Addresses that are dialled regardless of the lists (`--seed` and the
    /// compiled-in seed nodes, already resolved).
    pub fn set_seeds(&mut self, seeds: Vec<SocketAddr>) {
        self.seeds = seeds;
    }

    pub fn seeds(&self) -> &[SocketAddr] {
        &self.seeds
    }

    // -- the file ------------------------------------------------------------

    /// `NodeServer::serialize` through `BinaryOutputStreamSerializer`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + 24 * (self.white.len() + self.gray.len()));
        write_varint(&mut out, 1); // NodeServer version
        write_varint(&mut out, 2); // PeerlistManager version
        for list in [&self.white, &self.gray] {
            write_varint(&mut out, list.len() as u64);
            for e in list {
                write_varint(&mut out, u32::from_le_bytes(e.ip) as u64);
                write_varint(&mut out, e.port as u64);
                write_varint(&mut out, e.id);
                write_varint(&mut out, e.last_seen);
            }
        }
        for list in [&self.white6, &self.gray6] {
            write_varint(&mut out, list.len() as u64);
            for e in list {
                out.extend_from_slice(&e.ip);
                write_varint(&mut out, e.port as u64);
                write_varint(&mut out, e.id);
                write_varint(&mut out, e.last_seen);
            }
        }
        write_varint(&mut out, self.peer_id);
        out
    }

    /// The reverse of [`PeerManager::encode`], tolerating the C++ version 1
    /// file (no IPv6 lists) exactly as `PeerlistManager::serialize` does.
    pub fn decode(&mut self, data: &[u8]) -> io::Result<()> {
        let mut r = VarintReader::new(data);
        let node_version = r.varint()?;
        if node_version != 1 {
            // `NodeServer::serialize` throws "Unsupported version".
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("peer state version {node_version}")));
        }
        let list_version = r.varint()?;
        if list_version < 1 {
            // `PeerlistManager::serialize` returns without reading anything.
            return Ok(());
        }
        let mut v4 = [Vec::new(), Vec::new()];
        for slot in &mut v4 {
            let n = r.count(4, P2P_LOCAL_GRAY_PEERLIST_LIMIT)?;
            for _ in 0..n {
                let ip = r.u32()?.to_le_bytes();
                let port = r.u32()?;
                let id = r.varint()?;
                let last_seen = r.varint()?;
                slot.push(PeerlistEntry { ip, port, id, last_seen });
            }
        }
        let mut v6 = [Vec::new(), Vec::new()];
        if list_version >= 2 {
            for slot in &mut v6 {
                let n = r.count(19, P2P_LOCAL_GRAY_PEERLIST_LIMIT)?;
                for _ in 0..n {
                    let ip = r.raw16()?;
                    let port = r.u32()?;
                    let id = r.varint()?;
                    let last_seen = r.varint()?;
                    slot.push(PeerlistEntry6 { id, last_seen, ip, port });
                }
            }
        }
        // The peer id is last; a file truncated before it keeps the fresh one
        // rather than losing the lists.
        if let Ok(id) = r.varint() {
            self.peer_id = id;
        }
        let [white, gray] = v4;
        let [white6, gray6] = v6;
        self.white = white;
        self.gray = gray;
        self.white6 = white6;
        self.gray6 = gray6;
        self.trim_white();
        self.trim_gray();
        Ok(())
    }

    /// Write the file if anything changed since the last write
    /// (`store_config`, called on the peerlist store interval and at exit),
    /// and the ban list beside it if that changed.
    pub fn save(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else { return Ok(()) };
        self.save_bans()?;
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write to a sibling and rename, so a crash mid-write cannot leave a
        // half-written list where the C++ node (or we) would read one.
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, self.encode())?;
        std::fs::rename(&tmp, &path)?;
        self.dirty = false;
        Ok(())
    }

    /// Write [`BANS_FILENAME`] if the table changed since it was last written.
    /// Called by [`PeerManager::save`], and by the engine right after an
    /// automatic ban so a crash does not lose it.
    pub fn save_bans(&mut self) -> io::Result<()> {
        let Some(path) = self.path.as_ref().map(|p| p.with_file_name(BANS_FILENAME)) else { return Ok(()) };
        self.banned.expire();
        let text = self.banned.to_text();
        if text == self.bans_written {
            return Ok(());
        }
        write_atomically(&path, &text)?;
        self.bans_written = text;
        Ok(())
    }

    /// The anchors read at start: the outbound peers this node was connected
    /// to when it last saved, to be dialled before anything else. Handed out
    /// once.
    pub fn take_anchors(&mut self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.anchors)
    }

    /// Record the current anchors ([`ANCHORS_FILENAME`]): at most
    /// [`MAX_ANCHORS`] outbound peers that completed a handshake, best first.
    /// An empty list leaves the file alone, so a node that stops while
    /// offline keeps the anchors of its last good run.
    pub fn save_anchors(&self, anchors: &[SocketAddr]) -> io::Result<()> {
        let Some(path) = self.path.as_ref().map(|p| p.with_file_name(ANCHORS_FILENAME)) else { return Ok(()) };
        if anchors.is_empty() {
            return Ok(());
        }
        let text: String = anchors.iter().take(MAX_ANCHORS).map(|a| format!("{a}\n")).collect();
        write_atomically(&path, &text)
    }

    // -- list maintenance ----------------------------------------------------

    fn trim_white(&mut self) {
        trim(&mut self.white, P2P_LOCAL_WHITE_PEERLIST_LIMIT, |e| e.last_seen);
        trim(&mut self.white6, P2P_LOCAL_WHITE_PEERLIST_LIMIT, |e| e.last_seen);
    }

    fn trim_gray(&mut self) {
        trim(&mut self.gray, P2P_LOCAL_GRAY_PEERLIST_LIMIT, |e| e.last_seen);
        trim(&mut self.gray6, P2P_LOCAL_GRAY_PEERLIST_LIMIT, |e| e.last_seen);
    }

    /// `append_with_peer_white`: insert or update in white, and remove the same
    /// address from gray.
    pub fn append_white(&mut self, entry: PeerlistEntry) {
        if !ip_allowed(IpAddr::V4(ipv4_of(&entry)), self.allow_local_ip) {
            return;
        }
        match self.white.iter_mut().find(|e| e.ip == entry.ip && e.port == entry.port) {
            Some(slot) => *slot = entry.clone(),
            None => {
                self.white.push(entry.clone());
                self.trim_white();
            }
        }
        self.gray.retain(|e| !(e.ip == entry.ip && e.port == entry.port));
        self.dirty = true;
    }

    /// `append_with_peer_white6`.
    pub fn append_white6(&mut self, entry: PeerlistEntry6) {
        if !ip_allowed(IpAddr::V6(Ipv6Addr::from(entry.ip)), self.allow_local_ip) {
            return;
        }
        match self.white6.iter_mut().find(|e| e.ip == entry.ip && e.port == entry.port) {
            Some(slot) => *slot = entry.clone(),
            None => {
                self.white6.push(entry.clone());
                self.trim_white();
            }
        }
        self.gray6.retain(|e| !(e.ip == entry.ip && e.port == entry.port));
        self.dirty = true;
    }

    /// `--add-peer` (`parsePeersAndAddToPeerListContainer`,
    /// `NetNodeConfig.cpp:41`, then `append_with_peer_white`,
    /// `NetNode.cpp:744`): on the white list with a random peer id and
    /// `last_seen = 0`, so it is dialled like any white peer and not handed to
    /// others until a handshake gives it a real `last_seen`. An address the
    /// lists do not take — loopback, or a private one without
    /// `--allow-local-ip` — is left out, as the C++ leaves it out.
    ///
    /// Unlike the C++, an address already on the white list keeps its entry:
    /// overwriting it would zero a `last_seen` a real handshake earned.
    pub fn add_command_line_peer(&mut self, addr: SocketAddr) {
        let port = u32::from(addr.port());
        match addr.ip() {
            IpAddr::V4(ip) => {
                if !self.white.iter().any(|e| e.ip == ip.octets() && e.port == port) {
                    self.append_white(PeerlistEntry { ip: ip.octets(), port, id: random_u64(), last_seen: 0 });
                }
            }
            IpAddr::V6(ip) => {
                if !self.white6.iter().any(|e| e.ip == ip.octets() && e.port == port) {
                    self.append_white6(PeerlistEntry6 { id: random_u64(), last_seen: 0, ip: ip.octets(), port });
                }
            }
        }
    }

    /// `append_with_peer_gray`: an address already white is left alone.
    pub fn append_gray(&mut self, entry: PeerlistEntry) {
        if !ip_allowed(IpAddr::V4(ipv4_of(&entry)), self.allow_local_ip) {
            return;
        }
        if self.white.iter().any(|e| e.ip == entry.ip && e.port == entry.port) {
            return;
        }
        match self.gray.iter_mut().find(|e| e.ip == entry.ip && e.port == entry.port) {
            Some(slot) => *slot = entry,
            None => self.gray.push(entry),
        }
        self.dirty = true;
    }

    /// `append_with_peer_gray6`.
    pub fn append_gray6(&mut self, entry: PeerlistEntry6) {
        if !ip_allowed(IpAddr::V6(Ipv6Addr::from(entry.ip)), self.allow_local_ip) {
            return;
        }
        if self.white6.iter().any(|e| e.ip == entry.ip && e.port == entry.port) {
            return;
        }
        match self.gray6.iter_mut().find(|e| e.ip == entry.ip && e.port == entry.port) {
            Some(slot) => *slot = entry,
            None => self.gray6.push(entry),
        }
        self.dirty = true;
    }

    /// `set_peer_just_seen`: a successful outbound handshake makes the peer
    /// white with `last_seen = now`.
    pub fn set_peer_just_seen(&mut self, peer_id: u64, addr: SocketAddr, now: u64) {
        match addr.ip() {
            IpAddr::V4(ip) => self.append_white(PeerlistEntry {
                ip: ip.octets(),
                port: addr.port() as u32,
                id: peer_id,
                last_seen: now,
            }),
            IpAddr::V6(ip) => self.append_white6(PeerlistEntry6 {
                id: peer_id,
                last_seen: now,
                ip: ip.octets(),
                port: addr.port() as u32,
            }),
        }
        self.failed.remove(&addr);
    }

    /// `handle_remote_peerlist` (`NetNode.cpp:1912`): keep at most 250 entries,
    /// reject the whole list if any `last_seen` is in the sender's future, then
    /// shift every `last_seen` by `now - sender local_time` and merge to gray.
    ///
    /// Two departures, both local policy (see the module docs): the shifted
    /// `last_seen` is clamped to `now`, because `local_time` is the peer's
    /// claim and `u64::MAX` would otherwise make its entries outrank every
    /// honest one at each trim; and a new entry is skipped when its network
    /// group already holds [`GRAY_PER_GROUP_LIMIT`] gray entries. The shift is
    /// computed without signed arithmetic, which a `local_time` of 2^63 or
    /// more used to overflow.
    ///
    /// Returns false when the list was rejected, which drops the connection in
    /// the C++ handshake path.
    pub fn merge_peerlist(&mut self, entries: &[PeerlistEntry], sender_local_time: u64, now: u64) -> bool {
        let kept = &entries[..entries.len().min(MAX_PEERLIST_ENTRIES)];
        if kept.iter().any(|e| e.last_seen > sender_local_time) {
            return false;
        }
        let shift = |last_seen: u64| -> u64 {
            let shifted = if now >= sender_local_time {
                last_seen.saturating_add(now - sender_local_time)
            } else {
                last_seen.saturating_sub(sender_local_time - now)
            };
            shifted.min(now)
        };
        let mut per_group = group_counts(self.gray.iter().map(|e| IpAddr::V4(ipv4_of(e))));
        for e in kept {
            let mut e = e.clone();
            e.last_seen = shift(e.last_seen);
            let known = self.gray.iter().any(|g| g.ip == e.ip && g.port == e.port);
            if !known && !admit_to_group(&mut per_group, IpAddr::V4(ipv4_of(&e))) {
                continue;
            }
            self.append_gray(e);
        }
        self.trim_gray();
        true
    }

    /// `handle_remote_peerlist6`: no time check, because `PeerlistEntry6` never
    /// goes through `fix_time_delta` in the C++ (`NetNode.cpp:1936`). The
    /// `last_seen` of a v6 entry is therefore the peer's word as it stands, and
    /// is clamped to our clock for the reason [`PeerManager::merge_peerlist`]
    /// clamps the shifted one; the per-group cap applies the same way.
    pub fn merge_peerlist6(&mut self, entries: &[PeerlistEntry6]) {
        let now = now_secs();
        let mut per_group = group_counts(self.gray6.iter().map(|e| IpAddr::V6(Ipv6Addr::from(e.ip))));
        for e in entries.iter().take(MAX_PEERLIST_ENTRIES) {
            let mut e = e.clone();
            e.last_seen = e.last_seen.min(now);
            let known = self.gray6.iter().any(|g| g.ip == e.ip && g.port == e.port);
            if !known && !admit_to_group(&mut per_group, IpAddr::V6(Ipv6Addr::from(e.ip))) {
                continue;
            }
            self.append_gray6(e);
        }
        self.trim_gray();
    }

    /// `get_peerlist_head`: white sorted by `last_seen` descending, entries
    /// with `last_seen == 0` skipped, at most `depth`.
    pub fn peerlist_head(&mut self, depth: usize) -> Vec<PeerlistEntry> {
        self.white.sort_by_key(|e| std::cmp::Reverse(e.last_seen));
        self.white.iter().filter(|e| e.last_seen != 0).take(depth).cloned().collect()
    }

    /// `get_peerlist6_head`; only sent to peers with `version >= 19`.
    pub fn peerlist6_head(&mut self, depth: usize) -> Vec<PeerlistEntry6> {
        self.white6.sort_by_key(|e| std::cmp::Reverse(e.last_seen));
        self.white6.iter().filter(|e| e.last_seen != 0).take(depth).cloned().collect()
    }

    // -- bans and failures ---------------------------------------------------

    /// `ban_host`. Applied on accept and before dialling.
    pub fn ban(&mut self, ip: IpAddr, seconds: u64) {
        self.banned.ban(ip, seconds);
    }

    pub fn is_banned(&self, ip: IpAddr) -> bool {
        self.banned.is_banned(ip)
    }

    /// The shared ban table, for a caller on another thread — the daemon
    /// console's `ban` command.
    pub fn bans(&self) -> BanList {
        self.banned.clone()
    }

    /// Record a failed dial; the address is skipped for
    /// [`FAILED_ADDRESS_BACKOFF`].
    pub fn note_dial_failure(&mut self, addr: SocketAddr) {
        self.failed.insert(addr, Instant::now() + FAILED_ADDRESS_BACKOFF);
    }

    /// Drop expired ban, backoff and score entries so no map grows with the
    /// number of addresses ever seen.
    pub fn expire(&mut self) {
        let now = Instant::now();
        self.failed.retain(|_, until| *until > now);
        self.banned.expire();
        self.scores.retain(|_, s| now.duration_since(s.last) <= SCORE_MEMORY);
    }

    /// Charge `points` of misbehaviour to `ip`. Returns true when this took the
    /// address to [`BAN_THRESHOLD`], in which case it is now banned for
    /// [`MISBEHAVIOUR_BAN_SECONDS`] and its score starts again from zero.
    ///
    /// Points are per address, not per connection, so a peer cannot shed them
    /// by reconnecting; they are forgotten after `SCORE_MEMORY` of good
    /// behaviour. Whether an address may be scored at all (loopback, in tests)
    /// is the caller's decision.
    pub fn penalise(&mut self, ip: IpAddr, points: u32) -> bool {
        let now = Instant::now();
        if !self.scores.contains_key(&ip) && self.scores.len() >= MAX_SCORED_ADDRESSES {
            self.scores.retain(|_, s| now.duration_since(s.last) <= SCORE_MEMORY);
            if self.scores.len() >= MAX_SCORED_ADDRESSES {
                if let Some(stalest) = self.scores.iter().min_by_key(|(_, s)| s.last).map(|(ip, _)| *ip) {
                    self.scores.remove(&stalest);
                }
            }
        }
        let score = self.scores.entry(ip).or_insert(Score { points: 0, last: now });
        if now.duration_since(score.last) > SCORE_MEMORY {
            score.points = 0;
        }
        score.points = score.points.saturating_add(points);
        score.last = now;
        if score.points < BAN_THRESHOLD {
            return false;
        }
        self.scores.remove(&ip);
        self.banned.ban(ip, MISBEHAVIOUR_BAN_SECONDS);
        true
    }

    /// The live misbehaviour points of `ip`, 0 when it has none.
    pub fn score(&self, ip: IpAddr) -> u32 {
        self.scores.get(&ip).filter(|s| s.last.elapsed() <= SCORE_MEMORY).map_or(0, |s| s.points)
    }

    fn dialable(&self, addr: SocketAddr, busy: &[SocketAddr]) -> bool {
        !busy.contains(&addr)
            && !self.is_banned(addr.ip())
            && !self.failed.get(&addr).is_some_and(|until| *until > Instant::now())
            && ip_allowed(addr.ip(), self.allow_local_ip)
    }

    /// [`PeerManager::dial_candidates_for`] with every busy address counted as
    /// an outbound connection.
    pub fn dial_candidates(&self, want: usize, busy: &[SocketAddr]) -> Vec<SocketAddr> {
        self.dial_candidates_for(want, busy, busy)
    }

    /// Up to `want` addresses to dial, `WHITE_PERCENT` of them from the white
    /// list, then gray, then the seeds (`connections_maker`, spec/08). `busy`
    /// is what is already connected or being dialled; `outbound` is the part
    /// of it that is ours to choose — outbound connections and dials in flight.
    ///
    /// The C++ picks at random from the whole list. This picks at random too,
    /// but at most one address per network group ([`net_group`]) and none from
    /// a group an outbound connection already covers, so a peer that owns one
    /// /16 and has filled our lists from it gets one outbound slot, not all of
    /// them. The seeds are exempt: they are the operator's choice.
    pub fn dial_candidates_for(&self, want: usize, busy: &[SocketAddr], outbound: &[SocketAddr]) -> Vec<SocketAddr> {
        if want == 0 {
            return Vec::new();
        }
        let mut rng = Rng::seeded();
        let white_target = (want * WHITE_PERCENT).div_ceil(100);
        let mut out: Vec<SocketAddr> = Vec::new();
        let mut groups: HashSet<NetGroup> = outbound.iter().map(|a| net_group(a.ip())).collect();

        let take = |src: &[SocketAddr], limit: usize, out: &mut Vec<SocketAddr>, groups: &mut HashSet<NetGroup>| {
            for a in src {
                if out.len() >= limit {
                    break;
                }
                if !out.contains(a) && groups.insert(net_group(a.ip())) {
                    out.push(*a);
                }
            }
        };

        let mut white: Vec<SocketAddr> = self
            .white
            .iter()
            .filter_map(socket_addr_of)
            .chain(self.white6.iter().filter_map(socket_addr_of6))
            .filter(|a| self.dialable(*a, busy))
            .collect();
        let mut gray: Vec<SocketAddr> = self
            .gray
            .iter()
            .filter_map(socket_addr_of)
            .chain(self.gray6.iter().filter_map(socket_addr_of6))
            .filter(|a| self.dialable(*a, busy))
            .collect();
        rng.shuffle(&mut white);
        rng.shuffle(&mut gray);

        take(&white, white_target.min(want), &mut out, &mut groups);
        take(&gray, want, &mut out, &mut groups);
        take(&white, want, &mut out, &mut groups);
        if out.len() < want {
            // Seeds are the bootstrap of last resort: only dialled when the
            // lists cannot fill the target, which is the "lists are empty or
            // the node is stuck" rule of spec/08.
            let mut seeds: Vec<SocketAddr> = self.seeds.iter().copied().filter(|a| self.dialable(*a, busy)).collect();
            rng.shuffle(&mut seeds);
            for a in seeds {
                if out.len() >= want {
                    break;
                }
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
        out
    }

    /// Whether `addr` may be dialled now: allowed, not banned, not backing off.
    /// For the anchors, which are dialled outside the candidate selection.
    pub fn may_dial(&self, addr: SocketAddr) -> bool {
        self.dialable(addr, &[])
    }
}

/// Entries per network group in a list.
fn group_counts(ips: impl Iterator<Item = IpAddr>) -> HashMap<NetGroup, usize> {
    let mut counts = HashMap::new();
    for ip in ips {
        *counts.entry(net_group(ip)).or_insert(0) += 1;
    }
    counts
}

/// Count one more entry for `ip`'s group, unless the group is full.
fn admit_to_group(counts: &mut HashMap<NetGroup, usize>, ip: IpAddr) -> bool {
    let n = counts.entry(net_group(ip)).or_insert(0);
    if *n >= GRAY_PER_GROUP_LIMIT {
        return false;
    }
    *n += 1;
    true
}

/// `Peerlist::trim`: sort by `last_seen` descending and drop the tail.
fn trim<T, F: Fn(&T) -> u64>(list: &mut Vec<T>, max: usize, key: F) {
    if list.len() <= max {
        return;
    }
    list.sort_by_key(|e| std::cmp::Reverse(key(e)));
    list.truncate(max);
}

/// Resolve a `host:port` string, and a bare host against the default P2P port.
/// DNS seeds (`DNS_SEED_NODES`) are ordinary hostnames whose A **and AAAA**
/// records are the seed addresses, so an IPv6 seed resolves through the same
/// path and lands in the same list (`resolve_seed_nodes`, `NetNode.cpp:498`,
/// which likewise sorts what the resolver returned into `nodes4` and `nodes6`).
///
/// The forms accepted, and why the colon count is not enough on its own: an
/// IPv6 literal is full of colons, so `2a01:4f8::1` has to be bracketed before
/// a port can be appended, and `[2a01:4f8::1]` has to be recognised as an
/// address that still needs one.
///
/// - `host` or `1.2.3.4` — the default port is appended;
/// - `host:port`, `1.2.3.4:port` — used as given;
/// - `[::1]:port` — used as given;
/// - `[::1]` — the default port is appended;
/// - `::1`, `2a01:4f8::1` — bracketed, then the default port is appended.
pub fn resolve(target: &str, default_port: u16) -> io::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    let target = target.trim();
    let with_port = if let Some(end) = target.strip_prefix('[').and_then(|_| target.rfind(']')) {
        // A bracketed IPv6 literal: it either already carries `:port` or needs one.
        if target[end + 1..].starts_with(':') {
            target.to_string()
        } else {
            format!("{target}:{default_port}")
        }
    } else if target.matches(':').count() > 1 {
        // More than one colon and no brackets: a bare IPv6 literal.
        format!("[{target}]:{default_port}")
    } else if target.contains(':') && !target.ends_with(':') {
        target.to_string()
    } else {
        let host = target.strip_suffix(':').unwrap_or(target);
        format!("{host}:{default_port}")
    };
    Ok(with_port.to_socket_addrs()?.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(ip: [u8; 4], port: u32, id: u64, last_seen: u64) -> PeerlistEntry {
        PeerlistEntry { ip, port, id, last_seen }
    }

    /// The file we write decodes back to the same lists and peer id, and the
    /// byte layout is the varint one `BinaryOutputStreamSerializer` produces.
    #[test]
    fn peer_state_file_round_trip() {
        let mut pm = PeerManager::new(true);
        pm.peer_id = 0x0123_4567_89ab_cdef;
        pm.append_white(e([1, 2, 3, 4], 17855, 11, 1000));
        pm.append_white(e([5, 6, 7, 8], 17855, 12, 900));
        pm.append_gray(e([9, 10, 11, 12], 17855, 13, 800));
        pm.append_white6(PeerlistEntry6 { id: 21, last_seen: 700, ip: [0x20; 16], port: 17855 });
        pm.append_gray6(PeerlistEntry6 { id: 22, last_seen: 600, ip: [0x21; 16], port: 17856 });

        let bytes = pm.encode();
        // version 1, then the peerlist version 2, then the white count
        assert_eq!(&bytes[..3], &[1, 2, 2]);

        let mut back = PeerManager::new(true);
        back.decode(&bytes).unwrap();
        assert_eq!(back.peer_id, pm.peer_id);
        assert_eq!(back.white, pm.white);
        assert_eq!(back.gray, pm.gray);
        assert_eq!(back.white6, pm.white6);
        assert_eq!(back.gray6, pm.gray6);
        assert_eq!(back.encode(), bytes);
    }

    /// A version-1 file (no IPv6 lists), which an older C++ node writes, loads
    /// with empty v6 lists instead of failing.
    #[test]
    fn peer_state_file_version_1() {
        let mut bytes = Vec::new();
        write_varint(&mut bytes, 1);
        write_varint(&mut bytes, 1);
        write_varint(&mut bytes, 1); // one white entry
        write_varint(&mut bytes, u32::from_le_bytes([1, 2, 3, 4]) as u64);
        write_varint(&mut bytes, 17855);
        write_varint(&mut bytes, 7);
        write_varint(&mut bytes, 42);
        write_varint(&mut bytes, 0); // empty gray
        write_varint(&mut bytes, 99); // peer id
        let mut pm = PeerManager::new(true);
        pm.decode(&bytes).unwrap();
        assert_eq!(pm.peer_id, 99);
        assert_eq!(pm.white, vec![e([1, 2, 3, 4], 17855, 7, 42)]);
        assert!(pm.white6.is_empty());
    }

    /// A corrupt or hostile file is an error, never an allocation from its
    /// declared counts.
    #[test]
    fn peer_state_file_rejects_nonsense() {
        let mut pm = PeerManager::new(true);
        assert!(pm.decode(&[]).is_err());
        assert!(pm.decode(&[9]).is_err(), "unsupported NodeServer version");
        let mut huge = Vec::new();
        write_varint(&mut huge, 1);
        write_varint(&mut huge, 2);
        write_varint(&mut huge, u32::MAX as u64); // claimed white entries
        assert!(pm.decode(&huge).is_err());
        assert!(pm.white.is_empty());
    }

    /// The white/gray rules of `PeerListManager.cpp`.
    #[test]
    fn white_and_gray_semantics() {
        let mut pm = PeerManager::new(true);
        pm.append_gray(e([1, 1, 1, 1], 17855, 1, 10));
        assert_eq!(pm.gray_count(), 1);
        // promoting the same address moves it out of gray
        pm.append_white(e([1, 1, 1, 1], 17855, 1, 20));
        assert_eq!((pm.white_count(), pm.gray_count()), (1, 0));
        // an address already white is not re-added to gray
        pm.append_gray(e([1, 1, 1, 1], 17855, 1, 30));
        assert_eq!((pm.white_count(), pm.gray_count()), (1, 0));
        // loopback is never accepted, even in gray
        let mut strict = PeerManager::new(false);
        strict.append_white(e([127, 0, 0, 1], 17855, 1, 10));
        strict.append_gray(e([192, 168, 0, 5], 17855, 2, 10));
        assert_eq!((strict.white_count(), strict.gray_count()), (0, 0));
    }

    /// `--add-peer` is listed with `last_seen = 0`, so it is not handed on,
    /// and never over an entry a handshake already earned.
    #[test]
    fn a_command_line_peer_is_listed_once_and_never_over_a_real_entry() {
        let mut pm = PeerManager::new(false);
        let addr: SocketAddr = "45.10.0.1:17855".parse().unwrap();
        pm.add_command_line_peer(addr);
        assert_eq!((pm.white.len(), pm.white[0].last_seen), (1, 0));
        assert!(pm.peerlist_head(MAX_PEERLIST_ENTRIES).is_empty(), "not handed on before a handshake");
        pm.set_peer_just_seen(7, addr, 1000);
        pm.add_command_line_peer(addr);
        assert_eq!((pm.white.len(), pm.white[0].last_seen, pm.white[0].id), (1, 1000, 7));
        pm.add_command_line_peer("[2a01:4f8::1]:17855".parse().unwrap());
        assert_eq!(pm.white6.len(), 1);
        pm.add_command_line_peer("127.0.0.1:17855".parse().unwrap());
        pm.add_command_line_peer("192.168.1.2:17855".parse().unwrap());
        assert_eq!(pm.white_count(), 2, "loopback never, and private only with --allow-local-ip");
    }

    /// `get_peerlist_head`: newest first, `last_seen == 0` skipped, capped.
    #[test]
    fn peerlist_head_shape() {
        let mut pm = PeerManager::new(true);
        pm.append_white(e([1, 0, 0, 1], 1, 1, 5));
        pm.append_white(e([1, 0, 0, 2], 1, 2, 50));
        pm.append_white(e([1, 0, 0, 3], 1, 3, 0));
        let head = pm.peerlist_head(MAX_PEERLIST_ENTRIES);
        assert_eq!(head.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 1]);
        assert_eq!(pm.peerlist_head(1).len(), 1);
    }

    /// `fix_time_delta`: a future `last_seen` rejects the whole list, and the
    /// rest are shifted by the clock difference.
    #[test]
    fn merge_shifts_and_rejects() {
        let mut pm = PeerManager::new(true);
        assert!(!pm.merge_peerlist(&[e([2, 2, 2, 2], 1, 1, 200)], 100, 100));
        assert_eq!(pm.gray_count(), 0);
        // sender says it is 100, we are at 1000: shift by +900
        assert!(pm.merge_peerlist(&[e([2, 2, 2, 2], 1, 1, 50)], 100, 1000));
        assert_eq!(pm.gray[0].last_seen, 950);
    }

    /// The trims hold at the C++ limits.
    #[test]
    fn trims_at_the_c_limits() {
        let mut pm = PeerManager::new(true);
        // 10.0.0.0/8, so every address is a distinct private one: allowed with
        // `allow_local_ip` and never loopback, broadcast or multicast.
        let addr = |i: u32| [10, (i >> 16) as u8, (i >> 8) as u8, i as u8];
        for i in 0..(P2P_LOCAL_WHITE_PEERLIST_LIMIT + 10) as u32 {
            pm.append_white(e(addr(i), 1, i as u64, i as u64 + 1));
        }
        assert_eq!(pm.white.len(), P2P_LOCAL_WHITE_PEERLIST_LIMIT);
        // the oldest were dropped
        assert!(pm.white.iter().all(|e| e.last_seen >= 11));
        for i in 0..(P2P_LOCAL_GRAY_PEERLIST_LIMIT + 10) as u32 {
            pm.append_gray(e(addr(2_000_000 + i), 1, i as u64, i as u64 + 1));
        }
        pm.trim_gray();
        assert_eq!(pm.gray.len(), P2P_LOCAL_GRAY_PEERLIST_LIMIT);
    }

    /// Bans and dial backoff keep an address out of the candidate list.
    #[test]
    fn dial_candidates_respect_bans_and_failures() {
        let mut pm = PeerManager::new(true);
        pm.append_white(e([10, 0, 0, 1], 17855, 1, 10));
        pm.append_gray(e([10, 0, 0, 2], 17855, 2, 10));
        let a1: SocketAddr = "10.0.0.1:17855".parse().unwrap();
        let a2: SocketAddr = "10.0.0.2:17855".parse().unwrap();
        let got = pm.dial_candidates(4, &[]);
        assert!(got.contains(&a1) && got.contains(&a2));
        pm.note_dial_failure(a1);
        pm.ban(a2.ip(), 60);
        assert!(pm.dial_candidates(4, &[]).is_empty());
        // already-connected addresses are skipped too
        pm.expire();
        let mut fresh = PeerManager::new(true);
        fresh.append_white(e([10, 0, 0, 1], 17855, 1, 10));
        assert!(fresh.dial_candidates(4, &[a1]).is_empty());
    }

    /// Seeds fill the target only when the lists cannot.
    #[test]
    fn seeds_are_the_fallback() {
        let mut pm = PeerManager::new(true);
        let seed: SocketAddr = "10.9.9.9:17855".parse().unwrap();
        pm.set_seeds(vec![seed]);
        assert_eq!(pm.dial_candidates(1, &[]), vec![seed]);
        pm.append_white(e([10, 0, 0, 1], 17855, 1, 10));
        assert_eq!(pm.dial_candidates(1, &[]), vec!["10.0.0.1:17855".parse().unwrap()]);
    }

    #[test]
    fn resolve_adds_the_default_port() {
        let a = resolve("127.0.0.1", 17855).unwrap();
        assert_eq!(a[0], "127.0.0.1:17855".parse::<SocketAddr>().unwrap());
        let b = resolve("127.0.0.1:1234", 17855).unwrap();
        assert_eq!(b[0].port(), 1234);
    }

    /// A peer claiming `local_time = u64::MAX` (or any time far ahead of ours)
    /// no longer hands us entries from the far future: every shifted
    /// `last_seen` is at most our clock. A `local_time` of 2^63 used to
    /// overflow the signed shift.
    #[test]
    fn merge_clamps_last_seen_to_now() {
        let now = 1_700_000_000;
        let mut pm = PeerManager::new(true);
        assert!(pm.merge_peerlist(&[e([2, 2, 2, 2], 1, 1, u64::MAX)], u64::MAX, now));
        assert_eq!(pm.gray[0].last_seen, now);
        // a sender whose clock is behind ours shifts forward, never past now
        assert!(pm.merge_peerlist(&[e([2, 2, 2, 3], 1, 2, 100)], 100, now));
        assert_eq!(pm.gray.iter().find(|g| g.id == 2).unwrap().last_seen, now);
        assert!(pm.merge_peerlist(&[e([2, 2, 2, 4], 1, 3, 5)], 1 << 63, now));
        assert!(pm.gray.iter().all(|g| g.last_seen <= now));
        // v6 entries are not shifted at all, so they are clamped as they come
        pm.merge_peerlist6(&[PeerlistEntry6 { id: 9, last_seen: u64::MAX, ip: [0x20; 16], port: 1 }]);
        assert!(pm.gray6[0].last_seen <= now_secs());
    }

    /// One network group may put at most `GRAY_PER_GROUP_LIMIT` entries in the
    /// gray list, however many a peer offers; other groups are unaffected, and
    /// refreshing an entry already there is always allowed.
    #[test]
    fn one_group_cannot_flood_the_gray_list() {
        let now = 1_700_000_000;
        let mut pm = PeerManager::new(false);
        let flood: Vec<PeerlistEntry> = (0..MAX_PEERLIST_ENTRIES as u32)
            .map(|i| e([45, 10, (i >> 8) as u8, i as u8], 17855, i as u64, now))
            .collect();
        assert!(pm.merge_peerlist(&flood, now, now));
        assert_eq!(pm.gray_count(), GRAY_PER_GROUP_LIMIT);
        assert!(pm.merge_peerlist(&[e([46, 1, 0, 1], 17855, 1, now)], now, now));
        assert_eq!(pm.gray_count(), GRAY_PER_GROUP_LIMIT + 1);
        let first = pm.gray[0].clone();
        assert!(pm.merge_peerlist(&[e(first.ip, first.port, 77, now)], now, now));
        assert!(pm.gray.iter().any(|g| g.id == 77), "an existing entry is refreshed");
    }

    /// The dial selection takes one address per network group and none from a
    /// group an outbound connection already covers; within a group the choice
    /// is random, so every member is eventually tried.
    #[test]
    fn dial_candidates_spread_across_groups_at_random() {
        let mut pm = PeerManager::new(false);
        pm.append_white(e([45, 10, 0, 1], 17855, 1, 10));
        pm.append_white(e([45, 10, 0, 2], 17855, 2, 10));
        pm.append_white(e([45, 11, 0, 1], 17855, 3, 10));
        let got = pm.dial_candidates(3, &[]);
        assert_eq!(got.len(), 2, "two groups, two candidates: {got:?}");
        let other: SocketAddr = "45.11.0.1:17855".parse().unwrap();
        assert!(got.contains(&other));
        // an outbound peer in 45.11/16 takes that group
        let busy: SocketAddr = "45.11.9.9:17855".parse().unwrap();
        let got = pm.dial_candidates_for(3, &[busy], &[busy]);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0].ip().to_string()[..6], "45.10.");
        let mut seen = HashSet::new();
        for _ in 0..200 {
            seen.extend(pm.dial_candidates_for(1, &[busy], &[busy]));
        }
        assert_eq!(seen.len(), 2, "both members of 45.10/16 get picked: {seen:?}");
    }

    #[test]
    fn network_groups() {
        let g = |s: &str| net_group(s.parse().unwrap());
        assert_eq!(g("45.10.1.2"), g("45.10.200.7"));
        assert_ne!(g("45.10.1.2"), g("45.11.1.2"));
        assert_eq!(g("2a01:4f8:1::1"), g("2a01:4f8:ffff::2"));
        assert_ne!(g("2a01:4f8::1"), g("2a01:4f9::1"));
        assert_eq!(g("::ffff:45.10.1.2"), g("45.10.3.4"), "a mapped address is its IPv4 address");
        // private and loopback addresses are each their own group
        assert_ne!(g("10.0.0.1"), g("10.0.0.2"));
        assert_ne!(g("127.0.0.1"), g("127.0.0.2"));
    }

    /// Lesser offences add up to a ban; a ban-worthy one bans at once; the ban
    /// lasts `MISBEHAVIOUR_BAN_SECONDS` and the score starts again after it.
    #[test]
    fn misbehaviour_adds_up_to_a_ban() {
        let mut pm = PeerManager::new(false);
        let ip: IpAddr = "45.10.0.1".parse().unwrap();
        for _ in 0..3 {
            assert!(!pm.penalise(ip, Offence::Unsolicited.points()));
        }
        assert_eq!(pm.score(ip), 75);
        assert!(!pm.is_banned(ip));
        assert!(pm.penalise(ip, Offence::Unsolicited.points()), "the fourth reaches the threshold");
        assert!(pm.is_banned(ip));
        assert_eq!(pm.score(ip), 0);
        let (_, remaining) = pm.bans().entries()[0];
        assert!(remaining > MISBEHAVIOUR_BAN_SECONDS - 5 && remaining <= MISBEHAVIOUR_BAN_SECONDS);
        let other: IpAddr = "45.10.0.2".parse().unwrap();
        assert!(pm.penalise(other, Offence::BadProofOfWork.points()));
    }

    /// The ban list survives a restart through its own file, expired bans are
    /// dropped on the way in, and `p2pstate.wrkz.bin` is untouched by it.
    #[test]
    fn bans_persist_in_their_own_file() {
        let dir = std::env::temp_dir().join(format!("wrkz-peers-bans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("p2pstate.wrkz.bin");
        let mut pm = PeerManager::open(&state, false, false);
        let ip: IpAddr = "45.10.0.1".parse().unwrap();
        pm.ban(ip, 3600);
        pm.save().unwrap();
        let text = std::fs::read_to_string(dir.join(BANS_FILENAME)).unwrap();
        assert!(text.starts_with("45.10.0.1 "), "{text}");
        assert!(!state.exists(), "no peer-list change, so no state file write");

        let back = PeerManager::open(&state, false, true);
        assert!(back.is_banned(ip), "a ban outlives the process, even across --p2p-reset-peerstate");
        let (_, remaining) = back.bans().entries()[0];
        assert!(remaining > 3590 && remaining <= 3600);

        let list = BanList::new();
        let now = now_secs();
        let loaded = list.load_text(&format!("45.10.0.2 {}\n45.10.0.3 {}\nnot a line\n", now - 1, now + 60), now);
        assert_eq!(loaded, 1, "the expired ban and the junk line are skipped");
        assert!(list.is_banned("45.10.0.3".parse().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Anchors are written beside the state file and handed out once on the
    /// next start; `--p2p-reset-peerstate` forgets them.
    #[test]
    fn anchors_round_trip() {
        let dir = std::env::temp_dir().join(format!("wrkz-peers-anchors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("p2pstate.wrkz.bin");
        let pm = PeerManager::open(&state, false, false);
        let anchors: Vec<SocketAddr> = ["45.10.0.1:17855", "[2a01:4f8::1]:17855", "46.1.0.1:1", "47.1.0.1:1"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        pm.save_anchors(&anchors).unwrap();
        pm.save_anchors(&[]).unwrap(); // an empty list keeps the last good one
        let mut back = PeerManager::open(&state, false, false);
        assert_eq!(back.take_anchors(), anchors[..MAX_ANCHORS].to_vec());
        assert!(back.take_anchors().is_empty(), "handed out once");
        assert!(PeerManager::open(&state, false, true).anchors.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
