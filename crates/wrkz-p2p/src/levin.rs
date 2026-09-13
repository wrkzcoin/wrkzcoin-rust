// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Levin framing (`src/p2p/LevinProtocol.cpp`, spec/08 "Levin framing").

use std::io::{self, Read, Write};
use wrkz_primitives::constants::{LEVIN_MAX_PACKET_SIZE, LEVIN_SIGNATURE};

pub const HEADER_LEN: usize = 33;
pub const PACKET_REQUEST: u32 = 1;
pub const PACKET_RESPONSE: u32 = 2;
pub const PROTOCOL_VERSION: u32 = 1;
pub const RETCODE_SUCCESS: i32 = 1;
/// `ERROR_CONNECTION_HANDLER_NOT_DEFINED`, sent for an unknown command.
pub const ERROR_HANDLER_NOT_DEFINED: i32 = -6;

/// Payload size [`read_frame`] accepts, `P2P_DEFAULT_PACKET_MAX_SIZE` in the
/// C++ node (spec/08, "Timeouts and limits"). `LEVIN_MAX_PACKET_SIZE` (100 MB)
/// is only the hard ceiling above which the connection is closed; the node
/// never configures anything near it, and accepting 100 MB per connection
/// would let a peer claim gigabytes across the 15 allowed connections.
pub const DEFAULT_MAX_PAYLOAD: u64 = 50 * 1024 * 1024;

/// Payloads are read in chunks this size so the buffer grows with the bytes
/// that actually arrive rather than with the peer's declared length.
const READ_CHUNK: usize = 64 * 1024;

/// Below this, header and payload are concatenated into one `write_all`; above
/// it they are written separately rather than copied into a temporary buffer.
const COMBINED_WRITE_LIMIT: usize = 64 * 1024;

/// `bucket_head2`, packed little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub payload_len: u64,
    pub have_to_return_data: bool,
    pub command: u32,
    pub return_code: i32,
    pub flags: u32,
    pub protocol_version: u32,
}

impl Header {
    /// A request (or, with `need_response` false, a notification).
    /// `payload_len` is filled in by [`write_frame`] from the payload itself.
    pub fn request(command: u32, need_response: bool) -> Self {
        Self {
            payload_len: 0,
            have_to_return_data: need_response,
            command,
            return_code: 0,
            flags: PACKET_REQUEST,
            protocol_version: PROTOCOL_VERSION,
        }
    }

    /// `payload_len` is filled in by [`write_frame`] from the payload itself.
    pub fn response(command: u32, return_code: i32) -> Self {
        Self {
            payload_len: 0,
            have_to_return_data: false,
            command,
            return_code,
            flags: PACKET_RESPONSE,
            protocol_version: PROTOCOL_VERSION,
        }
    }

    pub fn is_notification(&self) -> bool {
        !self.have_to_return_data && self.flags & PACKET_RESPONSE == 0
    }

    pub fn is_response(&self) -> bool {
        self.flags & PACKET_RESPONSE != 0
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&LEVIN_SIGNATURE.to_le_bytes());
        b[8..16].copy_from_slice(&self.payload_len.to_le_bytes());
        b[16] = self.have_to_return_data as u8;
        b[17..21].copy_from_slice(&self.command.to_le_bytes());
        b[21..25].copy_from_slice(&self.return_code.to_le_bytes());
        b[25..29].copy_from_slice(&self.flags.to_le_bytes());
        b[29..33].copy_from_slice(&self.protocol_version.to_le_bytes());
        b
    }

    /// Returns `None` for a wrong signature, an oversized payload, or flags
    /// that are neither request nor response; the peer closes the connection
    /// in the first two cases (`LevinProtocol.cpp:67`).
    pub fn decode(b: &[u8; HEADER_LEN]) -> Option<Self> {
        if u64::from_le_bytes(b[0..8].try_into().unwrap()) != LEVIN_SIGNATURE {
            return None;
        }
        let payload_len = u64::from_le_bytes(b[8..16].try_into().unwrap());
        if payload_len > LEVIN_MAX_PACKET_SIZE {
            return None;
        }
        let flags = u32::from_le_bytes(b[25..29].try_into().unwrap());
        // The C++ reader does not check this, but a frame that is neither a
        // request nor a response has no handler on either side, and accepting
        // it would make `is_notification` true for flags the peer never sends.
        if flags & (PACKET_REQUEST | PACKET_RESPONSE) == 0 {
            return None;
        }
        Some(Self {
            payload_len,
            have_to_return_data: b[16] != 0,
            command: u32::from_le_bytes(b[17..21].try_into().unwrap()),
            return_code: i32::from_le_bytes(b[21..25].try_into().unwrap()),
            flags,
            protocol_version: u32::from_le_bytes(b[29..33].try_into().unwrap()),
        })
    }

    /// Advisory: spec/08 fixes `m_protocol_version` at 1. Kept out of
    /// [`Header::decode`] so a future peer bumping it is logged rather than
    /// silently dropped, as the C++ node also does not check it.
    pub fn is_expected_version(&self) -> bool {
        self.protocol_version == PROTOCOL_VERSION
    }
}

/// Write one frame. `payload_len` is taken from `payload`, so the header and
/// the body can never disagree — a mismatch would desynchronize the peer's
/// reader permanently.
pub fn write_frame<W: Write>(w: &mut W, header: &Header, payload: &[u8]) -> io::Result<()> {
    let mut h = *header;
    h.payload_len = payload.len() as u64;
    let hb = h.encode();
    if payload.len() <= COMBINED_WRITE_LIMIT {
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(&hb);
        buf.extend_from_slice(payload);
        w.write_all(&buf)
    } else {
        w.write_all(&hb)?;
        w.write_all(payload)
    }
}

/// Read one frame, accepting payloads up to [`DEFAULT_MAX_PAYLOAD`].
///
/// `Err(InvalidData)` for a bad signature, bad flags or an over-limit size.
///
/// The caller owns the timeout: set one on the socket (or wrap the reader).
/// Any error from this function — a timeout included — leaves the stream at an
/// unknown offset, so the connection MUST be closed rather than retried.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<(Header, Vec<u8>)> {
    read_frame_limited(r, DEFAULT_MAX_PAYLOAD)
}

/// [`read_frame`] with an explicit payload limit, capped at the protocol's
/// hard `LEVIN_MAX_PACKET_SIZE`. The payload buffer grows in
/// `READ_CHUNK`-sized steps, so a peer that declares a large frame and then
/// sends nothing costs one chunk rather than the whole declared length.
pub fn read_frame_limited<R: Read>(r: &mut R, max_payload: u64) -> io::Result<(Header, Vec<u8>)> {
    read_frame_by(r, |_| max_payload)
}

/// A frame whose declared payload is over the limit for its command. Carried
/// inside the `InvalidData` error [`read_frame_by`] returns, so a caller that
/// cares — the node scores an oversized frame differently from garbage — can
/// tell the two apart with `get_ref()` and `downcast_ref`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OversizedFrame {
    pub command: u32,
    pub payload_len: u64,
    pub limit: u64,
}

impl std::fmt::Display for OversizedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "levin payload {} for command {} over the {} byte limit", self.payload_len, self.command, self.limit)
    }
}

impl std::error::Error for OversizedFrame {}

impl OversizedFrame {
    /// The [`OversizedFrame`] inside an error from [`read_frame_by`], if that
    /// is what it was.
    pub fn of(e: &io::Error) -> Option<Self> {
        e.get_ref().and_then(|inner| inner.downcast_ref::<Self>()).copied()
    }
}

/// [`read_frame_limited`] with the limit chosen from the decoded header, so a
/// per-command cap ([`crate::limits::max_inbound_payload`]) is applied before
/// a single payload byte is read or buffered. The limit is still capped at the
/// hard `LEVIN_MAX_PACKET_SIZE`.
pub fn read_frame_by<R: Read>(r: &mut R, limit: impl Fn(&Header) -> u64) -> io::Result<(Header, Vec<u8>)> {
    let mut hb = [0u8; HEADER_LEN];
    r.read_exact(&mut hb)?;
    let h = Header::decode(&hb).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad levin header"))?;
    let max = limit(&h).min(LEVIN_MAX_PACKET_SIZE);
    if h.payload_len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            OversizedFrame { command: h.command, payload_len: h.payload_len, limit: max },
        ));
    }
    let want = h.payload_len as usize;
    let mut payload: Vec<u8> = Vec::new();
    while payload.len() < want {
        let start = payload.len();
        let chunk = READ_CHUNK.min(want - start);
        payload.resize(start + chunk, 0);
        r.read_exact(&mut payload[start..])?;
    }
    Ok((h, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout() {
        let mut h = Header::request(1001, true);
        h.payload_len = 5;
        let b = h.encode();
        assert_eq!(&b[..8], &[0x01, 0x21, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]);
        assert_eq!(&b[8..16], &5u64.to_le_bytes());
        assert_eq!(b[16], 1);
        assert_eq!(&b[17..21], &1001u32.to_le_bytes());
        assert_eq!(&b[25..29], &1u32.to_le_bytes());
        assert_eq!(&b[29..33], &1u32.to_le_bytes());
        assert_eq!(Header::decode(&b), Some(h));
        assert!(h.is_expected_version());
        let mut bad = b;
        bad[0] = 0;
        assert_eq!(Header::decode(&bad), None);
        let mut no_flags = b;
        no_flags[25..29].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Header::decode(&no_flags), None);
        let mut huge = b;
        huge[8..16].copy_from_slice(&(LEVIN_MAX_PACKET_SIZE + 1).to_le_bytes());
        assert_eq!(Header::decode(&huge), None);
        assert!(Header::request(2001, false).is_notification());
        assert!(Header::response(1001, 1).is_response());
    }

    /// `write_frame` takes the length from the payload, and `read_frame` reads
    /// exactly that back.
    #[test]
    fn frame_round_trip() {
        let mut wire = Vec::new();
        let mut h = Header::request(1001, true);
        h.payload_len = 999; // deliberately wrong; write_frame must override it
        write_frame(&mut wire, &h, b"hello").unwrap();
        assert_eq!(wire.len(), HEADER_LEN + 5);
        let (got, body) = read_frame(&mut &wire[..]).unwrap();
        assert_eq!(got.payload_len, 5);
        assert_eq!(body, b"hello");

        // and a payload above the combined-write threshold
        let big = vec![7u8; COMBINED_WRITE_LIMIT + 1];
        let mut wire = Vec::new();
        write_frame(&mut wire, &Header::request(2004, false), &big).unwrap();
        let (got, body) = read_frame(&mut &wire[..]).unwrap();
        assert_eq!(got.payload_len as usize, big.len());
        assert_eq!(body, big);
    }

    /// A frame over the configured limit is rejected from the header alone,
    /// before any payload byte is read or allocated.
    #[test]
    fn oversized_payload_is_rejected_from_the_header() {
        let mut h = Header::request(1001, false);
        h.payload_len = DEFAULT_MAX_PAYLOAD + 1;
        let hb = h.encode();
        let e = read_frame(&mut &hb[..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        // a limit above the hard ceiling is clamped to it
        let mut h = Header::request(1001, false);
        h.payload_len = LEVIN_MAX_PACKET_SIZE;
        let hb = h.encode();
        assert_eq!(read_frame_limited(&mut &hb[..], u64::MAX).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A peer that declares a huge payload and sends nothing must not make us
    /// allocate the declared length.
    #[test]
    fn truncated_payload_errors_without_preallocating() {
        let mut h = Header::request(1001, false);
        h.payload_len = DEFAULT_MAX_PAYLOAD;
        let mut wire = h.encode().to_vec();
        wire.extend_from_slice(&[0u8; 10]);
        assert_eq!(read_frame(&mut &wire[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}
