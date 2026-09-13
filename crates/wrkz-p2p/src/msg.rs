// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Message shapes (`src/p2p/P2pProtocolDefinitions.h`,
//! `src/cryptonoteprotocol/CryptoNoteProtocolDefinitions.h`; spec/08).
//! Field names, KV types and declaration order are the wire contract.

use wrkz_primitives::constants::{CRYPTONOTE_NETWORK, P2P_CURRENT_VERSION};
use wrkz_primitives::kv::{self, Section, Value};
use wrkz_primitives::{Error, Result};

pub const COMMAND_HANDSHAKE: u32 = 1001;
pub const COMMAND_TIMED_SYNC: u32 = 1002;
pub const COMMAND_PING: u32 = 1003;
pub const NOTIFY_NEW_BLOCK: u32 = 2001;
pub const NOTIFY_NEW_TRANSACTIONS: u32 = 2002;
pub const NOTIFY_REQUEST_GET_OBJECTS: u32 = 2003;
pub const NOTIFY_RESPONSE_GET_OBJECTS: u32 = 2004;
pub const NOTIFY_REQUEST_CHAIN: u32 = 2006;
pub const NOTIFY_RESPONSE_CHAIN_ENTRY: u32 = 2007;
pub const NOTIFY_REQUEST_TX_POOL: u32 = 2008;
pub const NOTIFY_NEW_LITE_BLOCK: u32 = 2009;
pub const NOTIFY_MISSING_TXS: u32 = 2010;

pub const NODE_CAPABILITY_FLAG_PRUNED: u32 = 1;
pub const NODE_CAPABILITY_FLAG_LITE: u32 = 2;

/// spec/08 "Peer list handling": at most 250 entries are kept from a peer.
pub const MAX_PEERLIST_ENTRIES: usize = 250;
/// spec/08: `NOTIFY_RESPONSE_CHAIN_ENTRY` carries up to 10,000 hashes, and no
/// other hash blob on this protocol is larger.
pub const MAX_HASHES: usize = 10_000;

/// Validate a `serializeAsBinary` blob and return its element count.
///
/// The C++ reader throws "Invalid blob size given!" when the length is not a
/// multiple of `sizeof(T)`, so a partial trailing element is a protocol error,
/// not something to silently drop.
fn blob_count(blob: &[u8], stride: usize, max: usize, what: &'static str) -> Result<usize> {
    if !blob.len().is_multiple_of(stride) {
        return Err(Error::Malformed(what));
    }
    let n = blob.len() / stride;
    if n > max {
        return Err(Error::Malformed(what));
    }
    Ok(n)
}

/// `basic_node_data` (`P2pProtocolDefinitions.h:49`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasicNodeData {
    pub network_id: [u8; 16],
    pub version: u8,
    pub peer_id: u64,
    pub local_time: u64,
    pub my_port: u32,
}

impl BasicNodeData {
    pub fn ours(peer_id: u64, my_port: u32) -> Self {
        let local_time =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Self { network_id: CRYPTONOTE_NETWORK, version: P2P_CURRENT_VERSION, peer_id, local_time, my_port }
    }

    pub fn to_section(&self) -> Section {
        Section::new()
            .string("network_id", &self.network_id)
            .u8("version", self.version)
            .u64("peer_id", self.peer_id)
            .u64("local_time", self.local_time)
            .u32("my_port", self.my_port)
    }

    pub fn from_section(s: &Section) -> Result<Self> {
        let nid = s.get_bytes("network_id").ok_or(Error::Malformed("network_id"))?;
        Ok(Self {
            network_id: nid.try_into().map_err(|_| Error::Malformed("network_id length"))?,
            version: s.get_u64("version").unwrap_or(0) as u8,
            peer_id: s.get_u64("peer_id").ok_or(Error::Malformed("peer_id"))?,
            local_time: s.get_u64("local_time").ok_or(Error::Malformed("local_time"))?,
            my_port: s.get_u64("my_port").ok_or(Error::Malformed("my_port"))? as u32,
        })
    }
}

/// `CORE_SYNC_DATA` (`P2pProtocolDefinitions.h:75`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CoreSyncData {
    /// top block index + 1
    pub current_height: u32,
    pub top_id: [u8; 32],
    pub capability_flags: u32,
    pub pruned_node_height: u32,
    pub lite_start_height: u32,
}

impl CoreSyncData {
    pub fn to_section(&self) -> Section {
        Section::new()
            .u32("current_height", self.current_height)
            .string("top_id", &self.top_id)
            .u32("capability_flags", self.capability_flags)
            .u32("pruned_node_height", self.pruned_node_height)
            .u32("lite_start_height", self.lite_start_height)
    }

    pub fn from_section(s: &Section) -> Result<Self> {
        let top = s.get_bytes("top_id").ok_or(Error::Malformed("top_id"))?;
        Ok(Self {
            current_height: s.get_u64("current_height").ok_or(Error::Malformed("current_height"))? as u32,
            top_id: top.try_into().map_err(|_| Error::Malformed("top_id length"))?,
            capability_flags: s.get_u64("capability_flags").unwrap_or(0) as u32,
            pruned_node_height: s.get_u64("pruned_node_height").unwrap_or(0) as u32,
            lite_start_height: s.get_u64("lite_start_height").unwrap_or(0) as u32,
        })
    }
}

/// `PeerlistEntry` (24 bytes): ip u32 (network byte order as stored), port u32, id u64, last_seen u64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerlistEntry {
    pub ip: [u8; 4],
    pub port: u32,
    pub id: u64,
    pub last_seen: u64,
}

impl PeerlistEntry {
    pub const STRIDE: usize = 24;

    pub fn parse_blob(blob: &[u8]) -> Result<Vec<Self>> {
        blob_count(blob, Self::STRIDE, MAX_PEERLIST_ENTRIES, "local_peerlist")?;
        Ok(blob
            .as_chunks::<{ Self::STRIDE }>()
            .0
            .iter()
            .map(|c| Self {
                ip: c[0..4].try_into().unwrap(),
                port: u32::from_le_bytes(c[4..8].try_into().unwrap()),
                id: u64::from_le_bytes(c[8..16].try_into().unwrap()),
                last_seen: u64::from_le_bytes(c[16..24].try_into().unwrap()),
            })
            .collect())
    }

    pub fn addr(&self) -> String {
        format!("{}.{}.{}.{}:{}", self.ip[0], self.ip[1], self.ip[2], self.ip[3], self.port)
    }
}

/// `PeerlistEntry6`: id u64 @0, last_seen u64 @8, ip 16 bytes @16, port u32 @32.
///
/// The struct is **not** packed, so `sizeof` rounds 36 up to the 8-byte
/// alignment of its `uint64_t` members: the wire stride is **40 bytes**, with
/// four bytes of padding after `port`. `serializeAsBinary` dumps `sizeof(T)`
/// per element, so the padding is on the wire and a blob whose length is not a
/// multiple of 40 makes the C++ reader throw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerlistEntry6 {
    pub id: u64,
    pub last_seen: u64,
    pub ip: [u8; 16],
    pub port: u32,
}

impl PeerlistEntry6 {
    pub const STRIDE: usize = 40;

    pub fn parse_blob(blob: &[u8]) -> Result<Vec<Self>> {
        blob_count(blob, Self::STRIDE, MAX_PEERLIST_ENTRIES, "local_peerlist6")?;
        Ok(blob
            .as_chunks::<{ Self::STRIDE }>()
            .0
            .iter()
            .map(|c| Self {
                id: u64::from_le_bytes(c[0..8].try_into().unwrap()),
                last_seen: u64::from_le_bytes(c[8..16].try_into().unwrap()),
                ip: c[16..32].try_into().unwrap(),
                port: u32::from_le_bytes(c[32..36].try_into().unwrap()),
                // bytes 36..40 are the struct's tail padding; the C++ writer
                // emits whatever was in that memory, so they carry no meaning.
            })
            .collect())
    }

    /// The 40-byte wire form, tail padding zeroed.
    pub fn to_blob_entry(&self) -> [u8; Self::STRIDE] {
        let mut b = [0u8; Self::STRIDE];
        b[0..8].copy_from_slice(&self.id.to_le_bytes());
        b[8..16].copy_from_slice(&self.last_seen.to_le_bytes());
        b[16..32].copy_from_slice(&self.ip);
        b[32..36].copy_from_slice(&self.port.to_le_bytes());
        b
    }
}

/// `COMMAND_HANDSHAKE::request`.
pub fn handshake_request(node: &BasicNodeData, sync: &CoreSyncData) -> Vec<u8> {
    kv::encode(&Section::new().object("node_data", node.to_section()).object("payload_data", sync.to_section()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandshakeResponse {
    pub node_data: BasicNodeData,
    pub payload_data: CoreSyncData,
    pub local_peerlist: Vec<PeerlistEntry>,
    pub local_peerlist6: Vec<PeerlistEntry6>,
}

pub fn parse_handshake_response(payload: &[u8]) -> Result<HandshakeResponse> {
    let s = kv::decode(payload)?;
    Ok(HandshakeResponse {
        node_data: BasicNodeData::from_section(s.get_object("node_data").ok_or(Error::Malformed("node_data"))?)?,
        payload_data: CoreSyncData::from_section(
            s.get_object("payload_data").ok_or(Error::Malformed("payload_data"))?,
        )?,
        local_peerlist: PeerlistEntry::parse_blob(s.get_bytes("local_peerlist").unwrap_or(&[]))?,
        local_peerlist6: PeerlistEntry6::parse_blob(s.get_bytes("local_peerlist6").unwrap_or(&[]))?,
    })
}

/// `COMMAND_TIMED_SYNC::request { payload_data }`.
pub fn timed_sync_request(sync: &CoreSyncData) -> Vec<u8> {
    kv::encode(&Section::new().object("payload_data", sync.to_section()))
}

/// `COMMAND_TIMED_SYNC::response { local_time, payload_data, local_peerlist, local_peerlist6 }`.
pub fn timed_sync_response(local_time: u64, sync: &CoreSyncData, peerlist: &[u8], peerlist6: &[u8]) -> Vec<u8> {
    kv::encode(
        &Section::new()
            .u64("local_time", local_time)
            .object("payload_data", sync.to_section())
            .string("local_peerlist", peerlist)
            .string("local_peerlist6", peerlist6),
    )
}

/// `COMMAND_PING::response { status: "OK", peer_id }`.
pub fn ping_response(peer_id: u64) -> Vec<u8> {
    // `status` is a std::string, not a POD blob: always emitted.
    kv::encode(&Section::new().text("status", b"OK").u64("peer_id", peer_id))
}

fn hashes_blob(hashes: &[[u8; 32]]) -> Vec<u8> {
    hashes.iter().flatten().copied().collect()
}

fn blob_hashes(blob: &[u8], what: &'static str) -> Result<Vec<[u8; 32]>> {
    blob_count(blob, 32, MAX_HASHES, what)?;
    Ok(blob.as_chunks::<32>().0.to_vec())
}

/// `NOTIFY_REQUEST_CHAIN { block_ids }`: the sparse chain, genesis last.
pub fn request_chain(block_ids: &[[u8; 32]]) -> Vec<u8> {
    kv::encode(&Section::new().string("block_ids", &hashes_blob(block_ids)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainEntry {
    pub start_height: u32,
    pub total_height: u32,
    pub block_ids: Vec<[u8; 32]>,
}

pub fn parse_chain_entry(payload: &[u8]) -> Result<ChainEntry> {
    let s = kv::decode(payload)?;
    let block_ids = blob_hashes(s.get_bytes("m_block_ids").unwrap_or(&[]), "m_block_ids")?;
    // `handle_response_chain_entry` requires a non-empty list; without it
    // `last_response_height = start_height + n - 1` underflows (spec/08).
    if block_ids.is_empty() {
        return Err(Error::Malformed("m_block_ids empty"));
    }
    Ok(ChainEntry {
        start_height: s.get_u64("start_height").ok_or(Error::Malformed("start_height"))? as u32,
        total_height: s.get_u64("total_height").ok_or(Error::Malformed("total_height"))? as u32,
        block_ids,
    })
}

/// `NOTIFY_REQUEST_GET_OBJECTS { txs, blocks }`; `txs` is always empty on this network.
pub fn request_get_objects(blocks: &[[u8; 32]]) -> Vec<u8> {
    kv::encode(&Section::new().string("txs", &[]).string("blocks", &hashes_blob(blocks)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlockLegacy {
    pub block: Vec<u8>,
    pub txs: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetObjectsResponse {
    pub txs: Vec<Vec<u8>>,
    pub blocks: Vec<RawBlockLegacy>,
    pub missed_ids: Vec<[u8; 32]>,
    pub current_blockchain_height: u32,
}

/// A KV array of strings. A non-string element is a protocol error: dropping
/// it would silently shorten a transaction list and mis-pair it against the
/// block's `tx_hashes`.
fn string_array(v: &[Value], what: &'static str) -> Result<Vec<Vec<u8>>> {
    v.iter()
        .map(|e| match e {
            Value::String(s) => Ok(s.clone()),
            _ => Err(Error::Malformed(what)),
        })
        .collect()
}

pub fn parse_get_objects_response(payload: &[u8]) -> Result<GetObjectsResponse> {
    let s = kv::decode(payload)?;
    let mut blocks = Vec::new();
    for v in s.get_array("blocks") {
        let Value::Object(o) = v else { return Err(Error::Malformed("blocks entry")) };
        blocks.push(RawBlockLegacy {
            block: o.get_bytes("block").ok_or(Error::Malformed("block"))?.to_vec(),
            txs: string_array(o.get_array("txs"), "block txs")?,
        });
    }
    Ok(GetObjectsResponse {
        txs: string_array(s.get_array("txs"), "txs")?,
        blocks,
        missed_ids: blob_hashes(s.get_bytes("missed_ids").unwrap_or(&[]), "missed_ids")?,
        current_blockchain_height: s.get_u64("current_blockchain_height").unwrap_or(0) as u32,
    })
}

/// `NOTIFY_REQUEST_TX_POOL { txs }`.
pub fn request_tx_pool(hashes: &[[u8; 32]]) -> Vec<u8> {
    kv::encode(&Section::new().string("txs", &hashes_blob(hashes)))
}

/// A lite block notification (`NOTIFY_NEW_LITE_BLOCK`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiteBlock {
    pub current_blockchain_height: u32,
    pub hop: u32,
    pub block_template: Vec<u8>,
}

pub fn parse_lite_block(payload: &[u8]) -> Result<LiteBlock> {
    let s = kv::decode(payload)?;
    Ok(LiteBlock {
        current_blockchain_height: s.get_u64("current_blockchain_height").unwrap_or(0) as u32,
        hop: s.get_u64("hop").unwrap_or(0) as u32,
        block_template: s.get_bytes("blockTemplate").ok_or(Error::Malformed("blockTemplate"))?.to_vec(),
    })
}

/// The block indices `Core::doBuildSparseChain` (`Core.cpp:4081`) asks for,
/// newest first, genesis last.
///
/// The deployed loop is `for (i = 1; i < blockIndex; i *= 2)` over
/// `blockIndex - i`, so it is powers of two only — the "first 10 are
/// sequential" of the `NOTIFY_REQUEST_CHAIN` comment in
/// `CryptoNoteProtocolDefinitions.h` describes an older CryptoNote and is not
/// what this code produces. Genesis is appended whenever the *top* is not
/// genesis, which is the C++ test (`sparseChain[0] != genesisBlockHash`), so
/// at a tip of 1 or 2 the list simply ends at index 0 without a duplicate.
///
/// The list is at most 2 + log2(tip) entries: 24 at the current mainnet
/// height, which is why the receiver's 10,000-hash cap is never near.
pub fn sparse_chain_indices(tip: u32) -> Vec<u32> {
    let mut out = vec![tip];
    let mut i: u32 = 1;
    while i < tip {
        out.push(tip - i);
        match i.checked_mul(2) {
            Some(next) => i = next,
            None => break,
        }
    }
    if tip != 0 {
        out.push(0);
    }
    // `tip - i` reaches 0 only when i == tip, which the loop condition
    // excludes, so genesis is never pushed twice.
    out.dedup();
    out
}

/// [`sparse_chain_indices`] resolved against a chain held in memory, oldest
/// first with genesis at 0. A node with a real chain uses the index list and
/// looks each hash up instead of materialising 4.2 million of them.
pub fn sparse_chain(chain: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let Some(tip) = chain.len().checked_sub(1) else { return Vec::new() };
    sparse_chain_indices(tip as u32).into_iter().map(|i| chain[i as usize]).collect()
}

// ---------------------------------------------------------------------------
// The serving side of the 1000-series, and the notifications the sync needs.
//
// Every encoder below writes the fields in the C++ declaration order
// (`P2pProtocolDefinitions.h`, and the hand-written serializers at
// `CryptoNoteProtocolHandler.cpp:101-199`). The KV reader on the other end is
// order-independent, but the *type tags* are not: a field written with the
// wrong width is silently narrowed by the C++ reader.
// ---------------------------------------------------------------------------

impl PeerlistEntry {
    /// The 24-byte wire form.
    pub fn to_blob_entry(&self) -> [u8; Self::STRIDE] {
        let mut b = [0u8; Self::STRIDE];
        b[0..4].copy_from_slice(&self.ip);
        b[4..8].copy_from_slice(&self.port.to_le_bytes());
        b[8..16].copy_from_slice(&self.id.to_le_bytes());
        b[16..24].copy_from_slice(&self.last_seen.to_le_bytes());
        b
    }
}

/// `serializeAsBinary` of a peer list: the concatenated fixed-size entries.
/// At most [`MAX_PEERLIST_ENTRIES`] are written, which is what the C++
/// `get_peerlist_head` sends and what its reader keeps.
pub fn peerlist_blob(entries: &[PeerlistEntry]) -> Vec<u8> {
    entries.iter().take(MAX_PEERLIST_ENTRIES).flat_map(|e| e.to_blob_entry()).collect()
}

/// [`peerlist_blob`] for the 40-byte IPv6 entries.
pub fn peerlist6_blob(entries: &[PeerlistEntry6]) -> Vec<u8> {
    entries.iter().take(MAX_PEERLIST_ENTRIES).flat_map(|e| e.to_blob_entry()).collect()
}

/// `COMMAND_HANDSHAKE::request` as the receiving side reads it
/// (`handle_handshake`, `NetNode.cpp:2240`).
pub fn parse_handshake_request(payload: &[u8]) -> Result<(BasicNodeData, CoreSyncData)> {
    let s = kv::decode(payload)?;
    Ok((
        BasicNodeData::from_section(s.get_object("node_data").ok_or(Error::Malformed("node_data"))?)?,
        CoreSyncData::from_section(s.get_object("payload_data").ok_or(Error::Malformed("payload_data"))?)?,
    ))
}

/// `COMMAND_HANDSHAKE::response`.
pub fn handshake_response(
    node: &BasicNodeData,
    sync: &CoreSyncData,
    peerlist: &[PeerlistEntry],
    peerlist6: &[PeerlistEntry6],
) -> Vec<u8> {
    kv::encode(
        &Section::new()
            .object("node_data", node.to_section())
            .object("payload_data", sync.to_section())
            .string("local_peerlist", &peerlist_blob(peerlist))
            .string("local_peerlist6", &peerlist6_blob(peerlist6)),
    )
}

/// `COMMAND_TIMED_SYNC::request { payload_data }` as the receiving side reads it.
pub fn parse_timed_sync_request(payload: &[u8]) -> Result<CoreSyncData> {
    let s = kv::decode(payload)?;
    CoreSyncData::from_section(s.get_object("payload_data").ok_or(Error::Malformed("payload_data"))?)
}

/// `COMMAND_TIMED_SYNC::response`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedSyncResponse {
    pub local_time: u64,
    pub payload_data: CoreSyncData,
    pub local_peerlist: Vec<PeerlistEntry>,
    pub local_peerlist6: Vec<PeerlistEntry6>,
}

pub fn parse_timed_sync_response(payload: &[u8]) -> Result<TimedSyncResponse> {
    let s = kv::decode(payload)?;
    Ok(TimedSyncResponse {
        local_time: s.get_u64("local_time").ok_or(Error::Malformed("local_time"))?,
        payload_data: CoreSyncData::from_section(
            s.get_object("payload_data").ok_or(Error::Malformed("payload_data"))?,
        )?,
        local_peerlist: PeerlistEntry::parse_blob(s.get_bytes("local_peerlist").unwrap_or(&[]))?,
        local_peerlist6: PeerlistEntry6::parse_blob(s.get_bytes("local_peerlist6").unwrap_or(&[]))?,
    })
}

/// [`timed_sync_response`] from typed peer lists.
pub fn timed_sync_response_from(
    local_time: u64,
    sync: &CoreSyncData,
    peerlist: &[PeerlistEntry],
    peerlist6: &[PeerlistEntry6],
) -> Vec<u8> {
    timed_sync_response(local_time, sync, &peerlist_blob(peerlist), &peerlist6_blob(peerlist6))
}

/// `PING_OK_RESPONSE_STATUS_TEXT` (`P2pProtocolDefinitions.h`).
pub const PING_OK_RESPONSE_STATUS_TEXT: &[u8] = b"OK";

/// `COMMAND_PING::request`: an empty object.
pub fn ping_request() -> Vec<u8> {
    kv::encode(&Section::new())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PingResponse {
    pub status: Vec<u8>,
    pub peer_id: u64,
}

impl PingResponse {
    /// What `try_ping` requires: `status == "OK"` and the peer id the handshake
    /// claimed (`NetNode.cpp:2196`).
    pub fn is_ok_for(&self, expected_peer_id: u64) -> bool {
        self.status == PING_OK_RESPONSE_STATUS_TEXT && self.peer_id == expected_peer_id
    }
}

pub fn parse_ping_response(payload: &[u8]) -> Result<PingResponse> {
    let s = kv::decode(payload)?;
    Ok(PingResponse {
        status: s.get_bytes("status").ok_or(Error::Malformed("status"))?.to_vec(),
        // A node that answers `OK` without a peer id fails `try_ping` anyway;
        // read 0 rather than refusing to decode, so the caller reports the
        // mismatch and not a parse error.
        peer_id: s.get_u64("peer_id").unwrap_or(0),
    })
}

/// `NOTIFY_REQUEST_CHAIN { block_ids }` as the serving side reads it.
///
/// `handle_request_chain` (`CryptoNoteProtocolHandler.cpp:1308`) drops the peer
/// on an empty list, so an empty one is a protocol error here.
pub fn parse_request_chain(payload: &[u8]) -> Result<Vec<[u8; 32]>> {
    let s = kv::decode(payload)?;
    let ids = blob_hashes(s.get_bytes("block_ids").unwrap_or(&[]), "block_ids")?;
    if ids.is_empty() {
        return Err(Error::Malformed("block_ids empty"));
    }
    Ok(ids)
}

/// `NOTIFY_RESPONSE_CHAIN_ENTRY`.
pub fn chain_entry(start_height: u32, total_height: u32, block_ids: &[[u8; 32]]) -> Vec<u8> {
    kv::encode(
        &Section::new()
            .u32("start_height", start_height)
            .u32("total_height", total_height)
            .string("m_block_ids", &hashes_blob(block_ids)),
    )
}

/// `NOTIFY_REQUEST_GET_OBJECTS` as the serving side reads it; the `txs` blob is
/// always empty on this network and is only checked for shape.
pub fn parse_request_get_objects(payload: &[u8]) -> Result<Vec<[u8; 32]>> {
    let s = kv::decode(payload)?;
    blob_hashes(s.get_bytes("txs").unwrap_or(&[]), "txs")?;
    blob_hashes(s.get_bytes("blocks").unwrap_or(&[]), "blocks")
}

impl RawBlockLegacy {
    /// `{ block, txs }`; the writer omits `txs` when the block has none.
    pub fn to_section(&self) -> Section {
        Section::new().string("block", &self.block).string_array("txs", &self.txs)
    }
}

/// `NOTIFY_RESPONSE_GET_OBJECTS`.
pub fn get_objects_response(
    blocks: &[RawBlockLegacy],
    missed_ids: &[[u8; 32]],
    current_blockchain_height: u32,
) -> Vec<u8> {
    kv::encode(
        &Section::new()
            // `txs` at top level is a `std::vector<std::string>`: the writer
            // omits an empty array, and this network never fills it.
            .string_array("txs", &[])
            .object_array("blocks", blocks.iter().map(|b| b.to_section()).collect())
            .string("missed_ids", &hashes_blob(missed_ids))
            .u32("current_blockchain_height", current_blockchain_height),
    )
}

/// `NOTIFY_REQUEST_TX_POOL { txs }` as the serving side reads it.
pub fn parse_request_tx_pool(payload: &[u8]) -> Result<Vec<[u8; 32]>> {
    let s = kv::decode(payload)?;
    blob_hashes(s.get_bytes("txs").unwrap_or(&[]), "txs")
}

/// `NOTIFY_NEW_BLOCK { b, current_blockchain_height, hop }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBlock {
    pub block: RawBlockLegacy,
    pub current_blockchain_height: u32,
    pub hop: u32,
}

pub fn new_block(nb: &NewBlock) -> Vec<u8> {
    kv::encode(
        &Section::new()
            .object("b", nb.block.to_section())
            .u32("current_blockchain_height", nb.current_blockchain_height)
            .u32("hop", nb.hop),
    )
}

pub fn parse_new_block(payload: &[u8]) -> Result<NewBlock> {
    let s = kv::decode(payload)?;
    let b = s.get_object("b").ok_or(Error::Malformed("b"))?;
    Ok(NewBlock {
        block: RawBlockLegacy {
            block: b.get_bytes("block").ok_or(Error::Malformed("block"))?.to_vec(),
            txs: string_array(b.get_array("txs"), "block txs")?,
        },
        current_blockchain_height: s.get_u64("current_blockchain_height").unwrap_or(0) as u32,
        hop: s.get_u64("hop").unwrap_or(0) as u32,
    })
}

/// `NOTIFY_NEW_LITE_BLOCK { current_blockchain_height, hop, blockTemplate }`.
pub fn lite_block(lb: &LiteBlock) -> Vec<u8> {
    kv::encode(
        &Section::new()
            .u32("current_blockchain_height", lb.current_blockchain_height)
            .u32("hop", lb.hop)
            .string("blockTemplate", &lb.block_template),
    )
}

/// `NOTIFY_NEW_TRANSACTIONS { txs }`.
pub fn new_transactions(txs: &[Vec<u8>]) -> Vec<u8> {
    kv::encode(&Section::new().string_array("txs", txs))
}

/// The writer omits an empty `txs` array entirely, so a missing key is an empty
/// list and not an error (spec/04 "KV binary", arrays).
pub fn parse_new_transactions(payload: &[u8]) -> Result<Vec<Vec<u8>>> {
    let s = kv::decode(payload)?;
    string_array(s.get_array("txs"), "txs")
}

/// `NOTIFY_MISSING_TXS { current_blockchain_height, blockHash, missing_txs }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingTxs {
    pub current_blockchain_height: u32,
    pub block_hash: [u8; 32],
    pub missing_txs: Vec<[u8; 32]>,
}

pub fn missing_txs(m: &MissingTxs) -> Vec<u8> {
    kv::encode(
        &Section::new()
            .u32("current_blockchain_height", m.current_blockchain_height)
            .string("blockHash", &m.block_hash)
            .string("missing_txs", &hashes_blob(&m.missing_txs)),
    )
}

pub fn parse_missing_txs(payload: &[u8]) -> Result<MissingTxs> {
    let s = kv::decode(payload)?;
    let h = s.get_bytes("blockHash").ok_or(Error::Malformed("blockHash"))?;
    Ok(MissingTxs {
        current_blockchain_height: s.get_u64("current_blockchain_height").unwrap_or(0) as u32,
        block_hash: h.try_into().map_err(|_| Error::Malformed("blockHash length"))?,
        missing_txs: blob_hashes(s.get_bytes("missing_txs").unwrap_or(&[]), "missing_txs")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() {
        let node = BasicNodeData {
            network_id: CRYPTONOTE_NETWORK,
            version: 19,
            peer_id: 42,
            local_time: 1_700_000_000,
            my_port: 17855,
        };
        let sync = CoreSyncData { current_height: 1, top_id: [7u8; 32], ..Default::default() };
        let bytes = handshake_request(&node, &sync);
        let s = kv::decode(&bytes).unwrap();
        assert_eq!(BasicNodeData::from_section(s.get_object("node_data").unwrap()).unwrap(), node);
        assert_eq!(CoreSyncData::from_section(s.get_object("payload_data").unwrap()).unwrap(), sync);
        // declaration order and widths: version is uint8 (type 8), my_port uint32 (type 6)
        let nd = s.get_object("node_data").unwrap();
        assert_eq!(
            nd.entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["network_id", "version", "peer_id", "local_time", "my_port"]
        );
        assert!(matches!(nd.get("version"), Some(Value::Uint(19, 8))));
        assert!(matches!(nd.get("my_port"), Some(Value::Uint(17855, 6))));
    }

    /// The index sequence `doBuildSparseChain` produces, hand-checked at the
    /// small tips where the strict `i < blockIndex` bound matters.
    #[test]
    fn sparse_chain_shape() {
        assert_eq!(sparse_chain_indices(0), vec![0]);
        assert_eq!(sparse_chain_indices(1), vec![1, 0]);
        assert_eq!(sparse_chain_indices(2), vec![2, 1, 0]);
        assert_eq!(sparse_chain_indices(3), vec![3, 2, 1, 0]);
        // i = 4 is not < 4, so index 0 comes only from the genesis append
        assert_eq!(sparse_chain_indices(4), vec![4, 3, 2, 0]);
        assert_eq!(sparse_chain_indices(10), vec![10, 9, 8, 6, 2, 0]);
        // it stays logarithmic at mainnet scale, well under the 10,000 cap
        assert_eq!(sparse_chain_indices(4_213_650).len(), 25, "the tip, 23 powers of two and genesis");
        assert_eq!(*sparse_chain_indices(4_213_650).last().unwrap(), 0);

        let chain: Vec<[u8; 32]> = (0..100u8).map(|i| [i; 32]).collect();
        let sc = sparse_chain(&chain);
        assert_eq!(sc[0], [99; 32]);
        assert_eq!(sc[1], [98; 32]);
        assert_eq!(sc[2], [97; 32]);
        assert_eq!(sc[3], [95; 32]);
        assert_eq!(*sc.last().unwrap(), [0; 32]);
        assert_eq!(sparse_chain(&chain[..1]), vec![[0; 32]]);
        assert!(sparse_chain(&[]).is_empty());
    }

    #[test]
    fn peerlist_blobs_use_the_c_struct_strides() {
        assert_eq!(PeerlistEntry::STRIDE, 24);
        assert_eq!(PeerlistEntry6::STRIDE, 40);

        let mut v4 = vec![0u8; 24];
        v4[0..4].copy_from_slice(&[1, 2, 3, 4]);
        v4[4..8].copy_from_slice(&17855u32.to_le_bytes());
        v4[8..16].copy_from_slice(&42u64.to_le_bytes());
        v4[16..24].copy_from_slice(&7u64.to_le_bytes());
        let p = PeerlistEntry::parse_blob(&v4).unwrap();
        assert_eq!(p[0].addr(), "1.2.3.4:17855");
        assert_eq!((p[0].id, p[0].last_seen), (42, 7));

        let e6 = PeerlistEntry6 { id: 9, last_seen: 8, ip: [0xab; 16], port: 17855 };
        let mut blob = e6.to_blob_entry().to_vec();
        blob.extend_from_slice(&[0xcc; 4]); // padding of a second entry's worth of junk
                                            // 44 bytes is not a whole number of 40-byte entries
        assert_eq!(PeerlistEntry6::parse_blob(&blob), Err(Error::Malformed("local_peerlist6")));
        assert_eq!(PeerlistEntry6::parse_blob(&e6.to_blob_entry()).unwrap(), vec![e6.clone()]);
        // the C++ writer leaves junk in the tail padding; it must be ignored
        let mut dirty = e6.to_blob_entry();
        dirty[36..40].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(PeerlistEntry6::parse_blob(&dirty).unwrap()[0], e6);

        // a partial trailing v4 entry is rejected, not silently dropped
        assert_eq!(PeerlistEntry::parse_blob(&v4[..23]), Err(Error::Malformed("local_peerlist")));
        assert!(PeerlistEntry::parse_blob(&[]).unwrap().is_empty());
        // and the 250-entry cap holds
        assert!(PeerlistEntry::parse_blob(&vec![0u8; 24 * (MAX_PEERLIST_ENTRIES + 1)]).is_err());
    }

    #[test]
    fn chain_entry_rejects_malformed_blobs() {
        let good =
            kv::encode(&Section::new().u32("start_height", 0).u32("total_height", 2).string("m_block_ids", &[9u8; 64]));
        assert_eq!(parse_chain_entry(&good).unwrap().block_ids.len(), 2);

        let empty = kv::encode(&Section::new().u32("start_height", 0).u32("total_height", 1));
        assert_eq!(parse_chain_entry(&empty), Err(Error::Malformed("m_block_ids empty")));

        let ragged =
            kv::encode(&Section::new().u32("start_height", 0).u32("total_height", 1).string("m_block_ids", &[9u8; 33]));
        assert_eq!(parse_chain_entry(&ragged), Err(Error::Malformed("m_block_ids")));

        let too_many = kv::encode(
            &Section::new()
                .u32("start_height", 0)
                .u32("total_height", 1)
                .string("m_block_ids", &vec![0u8; 32 * (MAX_HASHES + 1)]),
        );
        assert_eq!(parse_chain_entry(&too_many), Err(Error::Malformed("m_block_ids")));
    }

    /// Every encoder added for the sync round trips through its own decoder,
    /// and the KV types are the ones the C++ struct declares.
    #[test]
    fn notification_round_trips() {
        let nb = NewBlock {
            block: RawBlockLegacy { block: b"blockblob".to_vec(), txs: vec![b"t1".to_vec(), b"t2".to_vec()] },
            current_blockchain_height: 4_213_650,
            hop: 3,
        };
        assert_eq!(parse_new_block(&new_block(&nb)).unwrap(), nb);
        // a block with no transactions omits `txs` entirely; the reader must
        // still see an empty list rather than fail
        let empty = NewBlock { block: RawBlockLegacy { block: b"b".to_vec(), txs: vec![] }, ..nb.clone() };
        assert_eq!(parse_new_block(&new_block(&empty)).unwrap(), empty);
        let s = kv::decode(&new_block(&nb)).unwrap();
        assert!(matches!(s.get("current_blockchain_height"), Some(Value::Uint(4_213_650, 6))));
        assert!(matches!(s.get("hop"), Some(Value::Uint(3, 6))));

        let lb = LiteBlock { current_blockchain_height: 17, hop: 1, block_template: b"tmpl".to_vec() };
        assert_eq!(parse_lite_block(&lite_block(&lb)).unwrap(), lb);

        let txs = vec![b"a".to_vec(), b"bb".to_vec()];
        assert_eq!(parse_new_transactions(&new_transactions(&txs)).unwrap(), txs);
        assert!(parse_new_transactions(&new_transactions(&[])).unwrap().is_empty());

        let m = MissingTxs { current_blockchain_height: 9, block_hash: [5u8; 32], missing_txs: vec![[6u8; 32]] };
        assert_eq!(parse_missing_txs(&missing_txs(&m)).unwrap(), m);

        let ids: Vec<[u8; 32]> = (0..3u8).map(|i| [i; 32]).collect();
        let e = parse_chain_entry(&chain_entry(4, 100, &ids)).unwrap();
        assert_eq!((e.start_height, e.total_height, e.block_ids), (4, 100, ids.clone()));
        assert_eq!(parse_request_chain(&request_chain(&ids)).unwrap(), ids);
        assert_eq!(parse_request_get_objects(&request_get_objects(&ids)).unwrap(), ids);
        assert_eq!(parse_request_tx_pool(&request_tx_pool(&ids)).unwrap(), ids);

        let blocks = vec![RawBlockLegacy { block: b"raw".to_vec(), txs: vec![b"x".to_vec()] }];
        let r = parse_get_objects_response(&get_objects_response(&blocks, &ids, 42)).unwrap();
        assert_eq!((r.blocks, r.missed_ids, r.current_blockchain_height), (blocks, ids, 42));
    }

    /// The 1000-series commands round trip in both directions, which is what a
    /// node serving a C++ peer needs.
    #[test]
    fn command_round_trips() {
        let node = BasicNodeData::ours(7, 17855);
        let sync = CoreSyncData { current_height: 5, top_id: [1u8; 32], capability_flags: 1, ..Default::default() };
        let (n, p) = parse_handshake_request(&handshake_request(&node, &sync)).unwrap();
        assert_eq!((n, p), (node.clone(), sync.clone()));

        let p4 = vec![PeerlistEntry { ip: [1, 2, 3, 4], port: 17855, id: 8, last_seen: 9 }];
        let p6 = vec![PeerlistEntry6 { id: 1, last_seen: 2, ip: [3u8; 16], port: 17855 }];
        let hs = parse_handshake_response(&handshake_response(&node, &sync, &p4, &p6)).unwrap();
        assert_eq!(
            hs,
            HandshakeResponse {
                node_data: node,
                payload_data: sync.clone(),
                local_peerlist: p4.clone(),
                local_peerlist6: p6.clone()
            }
        );

        assert_eq!(parse_timed_sync_request(&timed_sync_request(&sync)).unwrap(), sync);
        let ts = parse_timed_sync_response(&timed_sync_response_from(1234, &sync, &p4, &p6)).unwrap();
        assert_eq!(
            ts,
            TimedSyncResponse { local_time: 1234, payload_data: sync, local_peerlist: p4, local_peerlist6: p6 }
        );

        assert!(kv::decode(&ping_request()).unwrap().entries.is_empty());
        let pr = parse_ping_response(&ping_response(99)).unwrap();
        assert!(pr.is_ok_for(99));
        assert!(!pr.is_ok_for(98));
    }

    #[test]
    fn get_objects_response_rejects_a_non_string_tx() {
        let bad = kv::encode(&Section::new().object_array(
            "blocks",
            vec![Section::new().string("block", b"raw").object_array("txs", vec![Section::new().u8("nope", 1)])],
        ));
        assert_eq!(parse_get_objects_response(&bad), Err(Error::Malformed("block txs")));

        let good = kv::encode(&Section::new().object_array(
            "blocks",
            vec![Section::new().string("block", b"raw").string_array("txs", &[b"a".to_vec()])],
        ));
        let r = parse_get_objects_response(&good).unwrap();
        assert_eq!(r.blocks[0].block, b"raw");
        assert_eq!(r.blocks[0].txs, vec![b"a".to_vec()]);
    }
}
