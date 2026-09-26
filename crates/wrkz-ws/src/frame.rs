// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Frames (RFC 6455 §5).
//!
//! ```text
//!  0               1               2               3
//!  0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
//! +-+-+-+-+-------+-+-------------+-------------------------------+
//! |F|R|R|R| opcode|M| Payload len |    Extended payload length    |
//! |I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
//! |N|V|V|V|       |S|             |   (if payload len==126/127)   |
//! | |1|2|3|       |K|             |                               |
//! +-+-+-+-+-------+-+-------------+ - - - - - - - - - - - - - - - +
//! |     Extended payload length continued, if payload len == 127  |
//! + - - - - - - - - - - - - - - - +-------------------------------+
//! |                               |Masking-key, if MASK set to 1  |
//! +-------------------------------+-------------------------------+
//! | Masking-key (continued)       |          Payload Data         |
//! +-------------------------------- - - - - - - - - - - - - - - - +
//! ```
//!
//! # A failed read ends the connection
//!
//! [`read_frame`] reads with `read_exact`. An error part-way through a frame —
//! a read timeout included — leaves the stream somewhere inside it, with no
//! way back to a frame boundary, so the caller must drop the connection on
//! any error. Both users here do: a timeout is how they notice a dead peer.

use std::fmt;
use std::io::{self, Read};

/// The largest payload a control frame may carry (§5.5).
pub const MAX_CONTROL_PAYLOAD: usize = 125;

/// Close codes (§7.4.1) this crate's users send.
pub mod close {
    /// The purpose of the connection was fulfilled.
    pub const NORMAL: u16 = 1000;
    /// The server is going down.
    pub const GOING_AWAY: u16 = 1001;
    /// The peer broke the protocol.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// A text message that was not UTF-8.
    pub const INVALID_DATA: u16 = 1007;
    /// The peer did something this end does not allow.
    pub const POLICY: u16 = 1008;
    /// A message larger than this end accepts.
    pub const TOO_BIG: u16 = 1009;
}

/// The four-bit opcode (§5.2). The reserved values are refused, not carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xa => Opcode::Pong,
            _ => return None,
        })
    }

    fn bits(self) -> u8 {
        match self {
            Opcode::Continuation => 0x0,
            Opcode::Text => 0x1,
            Opcode::Binary => 0x2,
            Opcode::Close => 0x8,
            Opcode::Ping => 0x9,
            Opcode::Pong => 0xa,
        }
    }

    /// Close, ping and pong: never fragmented, at most 125 bytes, and allowed
    /// in the middle of a fragmented message.
    pub fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// Which end of the connection this is. It decides the masking rule (§5.1): a
/// client masks every frame it sends and a server masks none, and each end
/// closes the connection on a frame that breaks the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

/// One frame, unmasked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

/// Why a read stopped.
#[derive(Debug)]
pub enum FrameError {
    /// The stream failed or closed, a read timeout included.
    Io(io::Error),
    /// The peer broke the protocol. Close with [`close::PROTOCOL_ERROR`].
    Protocol(&'static str),
    /// A frame or message longer than this end accepts, by its declared
    /// length. Nothing was allocated for it. Close with [`close::TOO_BIG`].
    TooLarge(u64),
    /// A text message that was not UTF-8. Close with [`close::INVALID_DATA`].
    InvalidText,
}

impl FrameError {
    /// The close code to send before dropping the connection, when the
    /// connection is still able to carry one.
    pub fn close_code(&self) -> Option<u16> {
        match self {
            FrameError::Io(_) => None,
            FrameError::Protocol(_) => Some(close::PROTOCOL_ERROR),
            FrameError::TooLarge(_) => Some(close::TOO_BIG),
            FrameError::InvalidText => Some(close::INVALID_DATA),
        }
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "{e}"),
            FrameError::Protocol(what) => write!(f, "protocol error: {what}"),
            FrameError::TooLarge(len) => write!(f, "a {len}-byte frame is over the limit"),
            FrameError::InvalidText => f.write_str("a text message that is not UTF-8"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// XOR `data` with the four-byte key, cycled from its first byte (§5.3).
/// Masking and unmasking are the same operation.
pub fn apply_mask(data: &mut [u8], key: [u8; 4]) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte ^= key[i & 3];
    }
}

/// Read one frame, refusing one whose payload is over `max_payload` before
/// reading or allocating any of it. See the module docs: on any error the
/// connection is unusable.
pub fn read_frame<R: Read>(r: &mut R, role: Role, max_payload: usize) -> Result<Frame, FrameError> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head)?;
    let fin = head[0] & 0x80 != 0;
    // No extension was negotiated, so every reserved bit must be clear (§5.2).
    if head[0] & 0x70 != 0 {
        return Err(FrameError::Protocol("reserved bits set"));
    }
    let opcode = Opcode::from_bits(head[0] & 0x0f).ok_or(FrameError::Protocol("reserved opcode"))?;
    let masked = head[1] & 0x80 != 0;
    match (role, masked) {
        (Role::Server, false) => return Err(FrameError::Protocol("a client frame that is not masked")),
        (Role::Client, true) => return Err(FrameError::Protocol("a server frame that is masked")),
        _ => {}
    }
    let len = match head[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            u64::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            let len = u64::from_be_bytes(b);
            if len >> 63 != 0 {
                return Err(FrameError::Protocol("a 64-bit length with the top bit set"));
            }
            len
        }
        short => u64::from(short),
    };
    if opcode.is_control() {
        if !fin {
            return Err(FrameError::Protocol("a fragmented control frame"));
        }
        if len > MAX_CONTROL_PAYLOAD as u64 {
            return Err(FrameError::Protocol("a control frame over 125 bytes"));
        }
    }
    if len > max_payload as u64 {
        return Err(FrameError::TooLarge(len));
    }
    let key = if masked {
        let mut k = [0u8; 4];
        r.read_exact(&mut k)?;
        Some(k)
    } else {
        None
    };
    // `len <= max_payload`, a `usize`, so this cannot truncate.
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if let Some(key) = key {
        apply_mask(&mut payload, key);
    }
    Ok(Frame { fin, opcode, payload })
}

/// One whole, unfragmented frame. `mask` is `Some` for a client, which must
/// mask every frame with a fresh unpredictable key (§5.3), and `None` for a
/// server, which must not mask at all.
pub fn encode_frame(opcode: Opcode, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode.bits());
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => out.push(mask_bit | n as u8),
        n if n <= usize::from(u16::MAX) => {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            let start = out.len();
            out.extend_from_slice(payload);
            apply_mask(&mut out[start..], key);
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// A close frame's payload: the code, then as much of `reason` as fits the
/// 125-byte control limit, cut on a character boundary.
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut cut = reason.len().min(MAX_CONTROL_PAYLOAD - 2);
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = Vec::with_capacity(2 + cut);
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&reason.as_bytes()[..cut]);
    out
}

/// A whole message, fragments joined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// The peer is closing, with its code if it gave one.
    Close(Option<u16>),
}

/// Joins frames into [`Message`]s: a fragmented message within a total cap,
/// and control frames handed over as they arrive, even between the fragments
/// of a message (§5.4). Fed by [`MessageReader`] from a stream, or frame by
/// frame from [`parse_frame`].
#[derive(Debug)]
pub struct Assembler {
    max_message: usize,
    partial: Option<(Opcode, Vec<u8>)>,
}

impl Assembler {
    /// `max_message` caps a whole message, fragments together.
    pub fn new(max_message: usize) -> Self {
        Self { max_message, partial: None }
    }

    /// The largest payload the next frame may carry. Never below a control
    /// frame's size, since one may arrive whatever is held; a data frame is
    /// checked against what is really left in [`Assembler::push`].
    pub fn frame_cap(&self) -> usize {
        self.room().max(MAX_CONTROL_PAYLOAD)
    }

    fn room(&self) -> usize {
        self.max_message.saturating_sub(self.partial.as_ref().map_or(0, |(_, buf)| buf.len()))
    }

    /// Add a frame; a message once one is complete.
    pub fn push(&mut self, frame: Frame) -> Result<Option<Message>, FrameError> {
        let room = self.room();
        match frame.opcode {
            Opcode::Ping => Ok(Some(Message::Ping(frame.payload))),
            Opcode::Pong => Ok(Some(Message::Pong(frame.payload))),
            // §5.5.1: an empty body, or a code and an optional reason.
            Opcode::Close => match frame.payload.len() {
                0 => Ok(Some(Message::Close(None))),
                1 => Err(FrameError::Protocol("a one-byte close body")),
                _ => Ok(Some(Message::Close(Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]))))),
            },
            Opcode::Text | Opcode::Binary => {
                if self.partial.is_some() {
                    return Err(FrameError::Protocol("a new message inside a fragmented one"));
                }
                if frame.payload.len() > room {
                    return Err(FrameError::TooLarge(frame.payload.len() as u64));
                }
                if frame.fin {
                    return finish(frame.opcode, frame.payload).map(Some);
                }
                self.partial = Some((frame.opcode, frame.payload));
                Ok(None)
            }
            Opcode::Continuation => {
                let Some((opcode, mut buf)) = self.partial.take() else {
                    return Err(FrameError::Protocol("a continuation with no message to continue"));
                };
                if frame.payload.len() > room {
                    return Err(FrameError::TooLarge((buf.len() + frame.payload.len()) as u64));
                }
                buf.extend_from_slice(&frame.payload);
                if frame.fin {
                    return finish(opcode, buf).map(Some);
                }
                self.partial = Some((opcode, buf));
                Ok(None)
            }
        }
    }
}

/// Reads [`Message`]s off one connection with [`read_frame`]. See the module
/// docs: any error, a read timeout included, ends the connection.
#[derive(Debug)]
pub struct MessageReader {
    role: Role,
    assembler: Assembler,
}

impl MessageReader {
    /// `max_message` caps a whole message, fragments together.
    pub fn new(role: Role, max_message: usize) -> Self {
        Self { role, assembler: Assembler::new(max_message) }
    }

    /// The next message. On any error the connection must be dropped.
    pub fn read<R: Read>(&mut self, r: &mut R) -> Result<Message, FrameError> {
        loop {
            let frame = read_frame(r, self.role, self.assembler.frame_cap())?;
            if let Some(message) = self.assembler.push(frame)? {
                return Ok(message);
            }
        }
    }
}

/// One frame from the front of `buf`, and how many bytes it took; `None` when
/// the frame is not all there yet. For a reader that collects bytes under a
/// short timeout: a timeout then only means "nothing more yet", never a
/// stream stopped inside a frame.
pub fn parse_frame(buf: &[u8], role: Role, max_payload: usize) -> Result<Option<(Frame, usize)>, FrameError> {
    let mut rest = buf;
    match read_frame(&mut rest, role, max_payload) {
        Ok(frame) => Ok(Some((frame, buf.len() - rest.len()))),
        Err(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

fn finish(opcode: Opcode, payload: Vec<u8>) -> Result<Message, FrameError> {
    match opcode {
        Opcode::Text => String::from_utf8(payload).map(Message::Text).map_err(|_| FrameError::InvalidText),
        _ => Ok(Message::Binary(payload)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    fn read(bytes: &[u8], role: Role) -> Result<Frame, FrameError> {
        read_frame(&mut &bytes[..], role, 1 << 20)
    }

    // The examples of RFC 6455 §5.7, byte for byte.
    #[test]
    fn the_rfc_examples_decode_and_encode() {
        let plain = [0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        assert_eq!(
            read(&plain, Role::Client).unwrap(),
            Frame { fin: true, opcode: Opcode::Text, payload: b"Hello".to_vec() }
        );
        assert_eq!(encode_frame(Opcode::Text, b"Hello", None), plain);

        let masked = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        assert_eq!(read(&masked, Role::Server).unwrap().payload, b"Hello");
        assert_eq!(encode_frame(Opcode::Text, b"Hello", Some(MASK)), masked);

        let ping = [0x89, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        assert_eq!(read(&ping, Role::Client).unwrap().opcode, Opcode::Ping);

        let long = encode_frame(Opcode::Binary, &[0u8; 256], None);
        assert_eq!(&long[..4], &[0x82, 0x7e, 0x01, 0x00]);
        let longer = encode_frame(Opcode::Binary, &[0u8; 65536], None);
        assert_eq!(&longer[..10], &[0x82, 0x7f, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(read(&longer, Role::Client).unwrap().payload.len(), 65536);
    }

    #[test]
    fn each_end_refuses_the_other_masking_rule() {
        let plain = encode_frame(Opcode::Text, b"x", None);
        let masked = encode_frame(Opcode::Text, b"x", Some(MASK));
        assert!(matches!(read(&plain, Role::Server), Err(FrameError::Protocol(_))));
        assert!(matches!(read(&masked, Role::Client), Err(FrameError::Protocol(_))));
    }

    #[test]
    fn a_declared_length_over_the_cap_is_refused_before_it_is_read() {
        // Claims 2^62 bytes and carries none: must fail on the length alone.
        let mut bytes = vec![0x82, 0x7f];
        bytes.extend_from_slice(&(1u64 << 62).to_be_bytes());
        assert!(matches!(read_frame(&mut &bytes[..], Role::Client, 1024), Err(FrameError::TooLarge(_))));
        let mut top = vec![0x82, 0x7f];
        top.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(read(&top, Role::Client), Err(FrameError::Protocol(_))));
    }

    #[test]
    fn reserved_bits_opcodes_and_bad_control_frames_are_refused() {
        assert!(matches!(read(&[0xc1, 0x00], Role::Client), Err(FrameError::Protocol(_))));
        assert!(matches!(read(&[0x83, 0x00], Role::Client), Err(FrameError::Protocol(_))));
        // A ping without FIN, and a 126-byte ping.
        assert!(matches!(read(&[0x09, 0x00], Role::Client), Err(FrameError::Protocol(_))));
        let mut big_ping = vec![0x89, 0x7e, 0x00, 0x7e];
        big_ping.extend_from_slice(&[0u8; 126]);
        assert!(matches!(read(&big_ping, Role::Client), Err(FrameError::Protocol(_))));
    }

    #[test]
    fn a_truncated_frame_is_an_io_error() {
        assert!(matches!(read(&[0x81, 0x05, 0x48], Role::Client), Err(FrameError::Io(_))));
        assert!(matches!(read(&[], Role::Client), Err(FrameError::Io(_))));
    }

    #[test]
    fn fragments_join_and_control_frames_pass_between_them() {
        // RFC §5.7: "Hel" then "lo", with a ping in between.
        let bytes = [
            0x01, 0x03, 0x48, 0x65, 0x6c, // text, not final
            0x89, 0x00, // ping
            0x80, 0x02, 0x6c, 0x6f, // continuation, final
        ];
        let mut reader = MessageReader::new(Role::Client, 64);
        let mut input = &bytes[..];
        assert_eq!(reader.read(&mut input).unwrap(), Message::Ping(Vec::new()));
        assert_eq!(reader.read(&mut input).unwrap(), Message::Text("Hello".into()));
    }

    #[test]
    fn the_message_cap_counts_every_fragment() {
        let mut bytes = encode_frame(Opcode::Text, b"12345", None);
        bytes[0] &= 0x7f; // not final
        let mut rest = encode_frame(Opcode::Continuation, b"67890", None);
        bytes.append(&mut rest);
        let mut reader = MessageReader::new(Role::Client, 8);
        assert!(matches!(reader.read(&mut &bytes[..]), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn message_order_errors_and_bad_text() {
        let stray = encode_frame(Opcode::Continuation, b"x", None);
        assert!(matches!(MessageReader::new(Role::Client, 64).read(&mut &stray[..]), Err(FrameError::Protocol(_))));

        let mut nested = encode_frame(Opcode::Text, b"a", None);
        nested[0] &= 0x7f;
        nested.extend(encode_frame(Opcode::Text, b"b", None));
        assert!(matches!(MessageReader::new(Role::Client, 64).read(&mut &nested[..]), Err(FrameError::Protocol(_))));

        let bad = encode_frame(Opcode::Text, &[0xff, 0xfe], None);
        assert!(matches!(MessageReader::new(Role::Client, 64).read(&mut &bad[..]), Err(FrameError::InvalidText)));
    }

    #[test]
    fn a_frame_is_parsed_only_once_it_is_all_there() {
        let mut bytes = encode_frame(Opcode::Text, b"Hello", None);
        bytes.extend(encode_frame(Opcode::Ping, b"", None));
        for cut in 0..7 {
            assert_eq!(parse_frame(&bytes[..cut], Role::Client, 64).unwrap(), None, "cut at {cut}");
        }
        let (frame, used) = parse_frame(&bytes, Role::Client, 64).unwrap().unwrap();
        assert_eq!((frame.payload.as_slice(), used), (&b"Hello"[..], 7));
        let (ping, used) = parse_frame(&bytes[7..], Role::Client, 64).unwrap().unwrap();
        assert_eq!((ping.opcode, used), (Opcode::Ping, 2));
        // A bad header is an error at once, however little follows it.
        assert!(parse_frame(&[0xc1], Role::Client, 64).unwrap().is_none());
        assert!(parse_frame(&[0xc1, 0x00], Role::Client, 64).is_err());
        let mut assembler = Assembler::new(64);
        assert_eq!(assembler.push(frame).unwrap(), Some(Message::Text("Hello".into())));
    }

    #[test]
    fn close_bodies() {
        let with_code = encode_frame(Opcode::Close, &close_payload(close::GOING_AWAY, "bye"), None);
        assert_eq!(
            MessageReader::new(Role::Client, 64).read(&mut &with_code[..]).unwrap(),
            Message::Close(Some(close::GOING_AWAY))
        );
        let empty = encode_frame(Opcode::Close, &[], None);
        assert_eq!(MessageReader::new(Role::Client, 64).read(&mut &empty[..]).unwrap(), Message::Close(None));
        let one = encode_frame(Opcode::Close, &[3], None);
        assert!(MessageReader::new(Role::Client, 64).read(&mut &one[..]).is_err());

        // A long reason is cut to fit, on a character boundary.
        let long = close_payload(close::NORMAL, &"é".repeat(100));
        assert!(long.len() <= MAX_CONTROL_PAYLOAD);
        assert!(std::str::from_utf8(&long[2..]).is_ok());
    }
}
