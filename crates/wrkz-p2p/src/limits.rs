// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Per-command inbound payload caps, checked from the Levin header before the
//! body is read.
//!
//! The C++ reader has one limit for everything, `P2P_DEFAULT_PACKET_MAX_SIZE`
//! (50 MB, [`crate::levin::DEFAULT_MAX_PAYLOAD`]), so a peer can make it buffer
//! 50 MB for a ping. Only one message on this protocol can legitimately be
//! that large — `NOTIFY_RESPONSE_GET_OBJECTS`, a batch of whole blocks — and
//! every other message has a shape whose size is bounded by something the
//! protocol already caps: a peer list by 250 entries, a hash list by 10,000
//! hashes, a block by the block size. The caps below are those bounds plus a
//! margin for the KV-binary framing, so no honest message comes near them.
//!
//! A frame over its cap is refused from the header alone, exactly like one over
//! the global limit: the connection is closed and nothing was allocated.
//!
//! | command | cap | why |
//! | --- | --- | --- |
//! | `COMMAND_HANDSHAKE`, `COMMAND_TIMED_SYNC` | 64 KiB + 250 x 40 B x 2 | node and sync data, plus a v4 and a v6 peer list of at most 250 entries |
//! | `COMMAND_PING` | 4 KiB | a status string and a peer id |
//! | `NOTIFY_REQUEST_CHAIN`, `NOTIFY_RESPONSE_CHAIN_ENTRY`, `NOTIFY_REQUEST_TX_POOL`, `NOTIFY_MISSING_TXS` | 10,000 x 32 B + 4 KiB | one hash list, capped at [`MAX_HASHES`] by the parser |
//! | `NOTIFY_REQUEST_GET_OBJECTS` | 2 x 10,000 x 32 B + 4 KiB | two hash lists (`txs`, `blocks`) |
//! | `NOTIFY_NEW_BLOCK`, `NOTIFY_NEW_LITE_BLOCK` | 8 MiB | one block and its transactions; the block size limit is ~0.9 MB at height 4.3M and grows ~100 kB a year |
//! | `NOTIFY_NEW_TRANSACTIONS` | 32 MiB | a C++ node answers `NOTIFY_REQUEST_TX_POOL` with its whole pool in one message, bounded only by its 32 MiB write buffer (`P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE`); a smaller cap would drop and score honest C++ peers during a transaction flood |
//! | `NOTIFY_RESPONSE_GET_OBJECTS` | 50 MB | a whole batch of blocks: the full packet limit |
//! | anything else | 64 KiB | no handler; the node drops the peer for it anyway |
//!
//! The same table serves both directions: a response carries the command id
//! of its request, and the only responses on this protocol (handshake, timed
//! sync, ping, and the empty `ERROR_CONNECTION_HANDLER_NOT_DEFINED`) fit the
//! request's cap.

use crate::levin::{Header, DEFAULT_MAX_PAYLOAD};
use crate::msg::{self, PeerlistEntry6, MAX_HASHES, MAX_PEERLIST_ENTRIES};

/// Room for the KV-binary section header, field names and length prefixes
/// around the one large field of a message.
const KV_MARGIN: u64 = 4 * 1024;

/// `COMMAND_HANDSHAKE` and `COMMAND_TIMED_SYNC`, either direction.
pub const HANDSHAKE_MAX_PAYLOAD: u64 = 64 * 1024 + 2 * (MAX_PEERLIST_ENTRIES * PeerlistEntry6::STRIDE) as u64;
/// `COMMAND_PING`, either direction.
pub const PING_MAX_PAYLOAD: u64 = 4 * 1024;
/// A message whose only large field is one hash list.
pub const HASH_LIST_MAX_PAYLOAD: u64 = (MAX_HASHES * 32) as u64 + KV_MARGIN;
/// `NOTIFY_REQUEST_GET_OBJECTS`: its `txs` and `blocks` hash lists.
pub const GET_OBJECTS_REQUEST_MAX_PAYLOAD: u64 = 2 * (MAX_HASHES * 32) as u64 + KV_MARGIN;
/// A relayed block or a batch of relayed transactions. Well above the block
/// size limit for decades (`max_block_cumulative_size` passes 8 MiB around
/// height 40M), and far below the 50 MB a peer could otherwise make us buffer.
pub const RELAY_MAX_PAYLOAD: u64 = 8 * 1024 * 1024;
/// `NOTIFY_NEW_TRANSACTIONS`. The C++ sends its whole pool in one of these
/// (`handleRequestTxPool`), capped by nothing but its per-connection write
/// buffer, so the cap is that buffer: the largest message an honest C++ peer
/// can send. The pool dumps and relays this node sends are split far below it.
pub const TRANSACTIONS_MAX_PAYLOAD: u64 = wrkz_primitives::constants::P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE as u64;
/// A command this node has no handler for.
pub const UNKNOWN_MAX_PAYLOAD: u64 = 64 * 1024;

/// The most payload a frame with this header may carry, never above
/// [`DEFAULT_MAX_PAYLOAD`].
pub fn max_inbound_payload(header: &Header) -> u64 {
    let cap = match header.command {
        msg::COMMAND_HANDSHAKE | msg::COMMAND_TIMED_SYNC => HANDSHAKE_MAX_PAYLOAD,
        msg::COMMAND_PING => PING_MAX_PAYLOAD,
        msg::NOTIFY_REQUEST_CHAIN
        | msg::NOTIFY_RESPONSE_CHAIN_ENTRY
        | msg::NOTIFY_REQUEST_TX_POOL
        | msg::NOTIFY_MISSING_TXS => HASH_LIST_MAX_PAYLOAD,
        msg::NOTIFY_REQUEST_GET_OBJECTS => GET_OBJECTS_REQUEST_MAX_PAYLOAD,
        msg::NOTIFY_NEW_BLOCK | msg::NOTIFY_NEW_LITE_BLOCK => RELAY_MAX_PAYLOAD,
        msg::NOTIFY_NEW_TRANSACTIONS => TRANSACTIONS_MAX_PAYLOAD,
        msg::NOTIFY_RESPONSE_GET_OBJECTS => DEFAULT_MAX_PAYLOAD,
        _ => UNKNOWN_MAX_PAYLOAD,
    };
    cap.min(DEFAULT_MAX_PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::levin::{self, OversizedFrame};
    use crate::msg::{BasicNodeData, CoreSyncData, PeerlistEntry};

    /// The largest honest message of each capped kind fits its cap: a full
    /// handshake response with both peer lists, and a full 10,000-id chain
    /// entry.
    #[test]
    fn the_largest_honest_messages_fit() {
        let v4: Vec<PeerlistEntry> = (0..MAX_PEERLIST_ENTRIES as u32)
            .map(|i| PeerlistEntry { ip: i.to_le_bytes(), port: 17855, id: u64::MAX, last_seen: u64::MAX })
            .collect();
        let v6: Vec<PeerlistEntry6> = (0..MAX_PEERLIST_ENTRIES)
            .map(|i| PeerlistEntry6 { id: u64::MAX, last_seen: u64::MAX, ip: [i as u8; 16], port: 17855 })
            .collect();
        let hs = msg::handshake_response(&BasicNodeData::ours(1, 17855), &CoreSyncData::default(), &v4, &v6);
        assert!((hs.len() as u64) < HANDSHAKE_MAX_PAYLOAD, "{} bytes", hs.len());
        let entry = msg::chain_entry(0, 10_000, &vec![[7u8; 32]; MAX_HASHES]);
        assert!((entry.len() as u64) < HASH_LIST_MAX_PAYLOAD, "{} bytes", entry.len());
        let get = msg::request_get_objects(&vec![[7u8; 32]; MAX_HASHES]);
        assert!((get.len() as u64) < GET_OBJECTS_REQUEST_MAX_PAYLOAD);
    }

    /// A ping declaring a 1 MB body is refused from the header, before any
    /// payload byte is read, and the error says which command and limit.
    #[test]
    fn an_oversized_frame_is_refused_from_its_header() {
        let mut h = Header::request(msg::COMMAND_PING, true);
        h.payload_len = 1_000_000;
        let wire = h.encode();
        let e = levin::read_frame_by(&mut &wire[..], max_inbound_payload).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        let o = OversizedFrame::of(&e).expect("typed oversize error");
        assert_eq!((o.command, o.payload_len, o.limit), (msg::COMMAND_PING, 1_000_000, PING_MAX_PAYLOAD));
        // a bad signature is an InvalidData error too, but not an oversize one
        let e = levin::read_frame_by(&mut &[0xffu8; levin::HEADER_LEN][..], max_inbound_payload).unwrap_err();
        assert!(OversizedFrame::of(&e).is_none());
        // only the get-objects response may use the full packet limit
        let mut h = Header::request(msg::NOTIFY_RESPONSE_GET_OBJECTS, false);
        h.payload_len = DEFAULT_MAX_PAYLOAD;
        assert_eq!(max_inbound_payload(&h), DEFAULT_MAX_PAYLOAD);
        h.command = 9999;
        assert_eq!(max_inbound_payload(&h), UNKNOWN_MAX_PAYLOAD);
    }
}
