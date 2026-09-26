// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The opening handshake (RFC 6455 §4).
//!
//! The server side takes a request an HTTP server has already parsed —
//! headers are looked up through a closure, so this crate does not depend on
//! any particular request type — checks it (§4.2.1) and renders the `101`
//! (§4.2.2). The client side renders the request (§4.1), reads the answer's
//! head without reading past it, and checks it.

use std::fmt;
use std::io::BufRead;

use sha1::{Digest, Sha1};

/// The GUID every `Sec-WebSocket-Accept` is derived with (§1.3).
pub const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The one protocol version there is (§4.1, item 9).
pub const VERSION: &str = "13";

/// Why a handshake was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// The request was not a `GET`.
    NotGet,
    /// No `Upgrade: websocket`.
    NoUpgrade,
    /// No `upgrade` token in `Connection`.
    NoConnectionUpgrade,
    /// A `Sec-WebSocket-Version` other than 13. The server answers `426`
    /// with `Sec-WebSocket-Version: 13` (§4.4).
    BadVersion,
    /// No `Sec-WebSocket-Key`, or not the base64 of 16 bytes.
    BadKey,
    /// The server answered with this status instead of `101`.
    Status(u16),
    /// The server's `Sec-WebSocket-Accept` did not match our key.
    BadAccept,
    /// Something that cannot be put in, or read from, an HTTP head.
    Malformed(&'static str),
    /// The stream failed while the answer was being read.
    Io(String),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandshakeError::NotGet => f.write_str("a WebSocket upgrade must be a GET"),
            HandshakeError::NoUpgrade => f.write_str("no Upgrade: websocket header"),
            HandshakeError::NoConnectionUpgrade => f.write_str("no Connection: Upgrade header"),
            HandshakeError::BadVersion => f.write_str("only Sec-WebSocket-Version 13 is supported"),
            HandshakeError::BadKey => f.write_str("a missing or malformed Sec-WebSocket-Key"),
            HandshakeError::Status(status) => write!(f, "the server answered HTTP {status}, not 101"),
            HandshakeError::BadAccept => f.write_str("the server's Sec-WebSocket-Accept does not match"),
            HandshakeError::Malformed(what) => write!(f, "malformed handshake: {what}"),
            HandshakeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// `base64(SHA-1(key ‖ GUID))`, the value that proves the server read this
/// client's key (§4.2.2, item 5.4).
pub fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(GUID.as_bytes());
    base64(&sha.finalize())
}

/// Whether the request asks to be a WebSocket at all. One that does not is
/// ordinary HTTP and is answered as such; one that does and is otherwise
/// wrong is refused with [`check_request`]'s reason.
pub fn wants_upgrade<'a>(header: impl Fn(&str) -> Option<&'a str>) -> bool {
    header("Upgrade").is_some_and(|v| has_token(v, "websocket"))
}

/// Check an upgrade request (§4.2.1) and return its `Sec-WebSocket-Key`.
pub fn check_request<'a>(method: &str, header: impl Fn(&str) -> Option<&'a str>) -> Result<&'a str, HandshakeError> {
    if method != "GET" {
        return Err(HandshakeError::NotGet);
    }
    if !header("Upgrade").is_some_and(|v| has_token(v, "websocket")) {
        return Err(HandshakeError::NoUpgrade);
    }
    if !header("Connection").is_some_and(|v| has_token(v, "upgrade")) {
        return Err(HandshakeError::NoConnectionUpgrade);
    }
    if header("Sec-WebSocket-Version").map(str::trim) != Some(VERSION) {
        return Err(HandshakeError::BadVersion);
    }
    let key = header("Sec-WebSocket-Key").map(str::trim).ok_or(HandshakeError::BadKey)?;
    if !is_key(key) {
        return Err(HandshakeError::BadKey);
    }
    Ok(key)
}

/// The `101 Switching Protocols` for a checked request.
pub fn response(key: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: \
         {}\r\n\r\n",
        accept_key(key)
    )
}

/// A client's `Sec-WebSocket-Key`: sixteen random bytes, base64 (§4.1, item 7).
pub fn client_key(nonce: [u8; 16]) -> String {
    base64(&nonce)
}

/// A client's upgrade request. `host` is the `Host` header (`host:port`) and
/// `target` the path with its query. Every value is refused if it could end a
/// header line, so a configured host or key can never inject a header.
pub fn client_request(
    host: &str,
    target: &str,
    key: &str,
    extra_headers: &[(&str, &str)],
) -> Result<String, HandshakeError> {
    let clean = |s: &str| !s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0);
    if !clean(host) || !clean(target) || !clean(key) || target.contains(' ') || !target.starts_with('/') {
        return Err(HandshakeError::Malformed("a host, path or key that cannot go in a request"));
    }
    let mut out = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: \
         {key}\r\nSec-WebSocket-Version: {VERSION}\r\n"
    );
    for (name, value) in extra_headers {
        if !clean(name) || !clean(value) || name.is_empty() || name.contains(':') {
            return Err(HandshakeError::Malformed("a header that cannot go in a request"));
        }
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    Ok(out)
}

/// The status and headers of the server's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// A header by name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Read the answer's head, and not a byte more: frames may follow straight
/// after it, and they stay in `r` for the frame reader. At most `max_bytes`
/// are read for the whole head.
pub fn read_response_head<R: BufRead>(r: &mut R, max_bytes: usize) -> Result<ResponseHead, HandshakeError> {
    let mut left = max_bytes;
    let mut line = Vec::new();
    let mut next_line = |r: &mut R, line: &mut Vec<u8>| -> Result<(), HandshakeError> {
        line.clear();
        // Through `&mut R`, so the reader is lent to the `Take`, not moved.
        let n = <&mut R as std::io::Read>::take(&mut *r, left as u64)
            .read_until(b'\n', line)
            .map_err(|e| HandshakeError::Io(e.to_string()))?;
        if n == 0 {
            return Err(HandshakeError::Io("the connection closed during the handshake".into()));
        }
        if line.last() != Some(&b'\n') {
            return Err(HandshakeError::Malformed("an answer head over the size limit"));
        }
        left -= n;
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(())
    };

    next_line(r, &mut line)?;
    let status_line = std::str::from_utf8(&line).map_err(|_| HandshakeError::Malformed("a status line"))?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(HandshakeError::Malformed("not an HTTP/1.x answer"));
    }
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(HandshakeError::Malformed("a status line without a status"))?;

    let mut headers = Vec::new();
    loop {
        next_line(r, &mut line)?;
        if line.is_empty() {
            return Ok(ResponseHead { status, headers });
        }
        let text = std::str::from_utf8(&line).map_err(|_| HandshakeError::Malformed("a header line"))?;
        let (name, value) = text.split_once(':').ok_or(HandshakeError::Malformed("a header without a colon"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
}

/// Check the server's answer to our `key` (§4.1, the client's checks).
pub fn check_response(head: &ResponseHead, key: &str) -> Result<(), HandshakeError> {
    if head.status != 101 {
        return Err(HandshakeError::Status(head.status));
    }
    if !head.header("Upgrade").is_some_and(|v| has_token(v, "websocket")) {
        return Err(HandshakeError::NoUpgrade);
    }
    if !head.header("Connection").is_some_and(|v| has_token(v, "upgrade")) {
        return Err(HandshakeError::NoConnectionUpgrade);
    }
    if head.header("Sec-WebSocket-Accept") != Some(accept_key(key).as_str()) {
        return Err(HandshakeError::BadAccept);
    }
    Ok(())
}

/// Whether a comma-separated header value carries `token`, case-insensitively
/// (`Connection: keep-alive, Upgrade`).
fn has_token(value: &str, token: &str) -> bool {
    value.split(',').any(|t| t.trim().eq_ignore_ascii_case(token))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding. Only ever given 16 or 20 bytes.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> shift) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The base64 of exactly sixteen bytes: 22 alphabet characters, the last of
/// which carries only four bits, then `==` (§4.2.1, item 5).
fn is_key(key: &str) -> bool {
    let b = key.as_bytes();
    b.len() == 24
        && b.ends_with(b"==")
        && b[..22].iter().all(|c| ALPHABET.contains(c))
        && ALPHABET.iter().position(|c| *c == b[21]).is_some_and(|v| v & 0x0f == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
        move |name: &str| pairs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
    }

    #[test]
    fn the_rfc_accept_value() {
        // RFC 6455 §1.3.
        assert_eq!(accept_key(RFC_KEY), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn base64_matches_rfc_4648() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(client_key(*b"the sample nonce"), RFC_KEY);
    }

    #[test]
    fn a_browser_request_is_accepted() {
        let request = [
            ("Host", "server.example.com"),
            ("Upgrade", "websocket"),
            ("Connection", "keep-alive, Upgrade"),
            ("Sec-WebSocket-Key", RFC_KEY),
            ("Sec-WebSocket-Version", "13"),
        ];
        assert!(wants_upgrade(headers(&request)));
        assert_eq!(check_request("GET", headers(&request)), Ok(RFC_KEY));
        assert!(response(RFC_KEY).contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
    }

    #[test]
    fn each_missing_piece_is_named() {
        let full = [
            ("Upgrade", "WebSocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", RFC_KEY),
            ("Sec-WebSocket-Version", "13"),
        ];
        assert_eq!(check_request("POST", headers(&full)), Err(HandshakeError::NotGet));
        assert_eq!(check_request("GET", headers(&full[1..])), Err(HandshakeError::NoUpgrade));
        let no_conn = [full[0], full[2], full[3]];
        assert_eq!(check_request("GET", headers(&no_conn)), Err(HandshakeError::NoConnectionUpgrade));
        let old = [full[0], full[1], full[2], ("Sec-WebSocket-Version", "8")];
        assert_eq!(check_request("GET", headers(&old)), Err(HandshakeError::BadVersion));
        for bad in ["", "short==", "dGhlIHNhbXBsZSBub25jZR==", "dGhlIHNhbXBsZSBub25jZQ=A", "!GhlIHNhbXBsZSBub25jZQ=="] {
            let keyed = [full[0], full[1], ("Sec-WebSocket-Key", bad), full[3]];
            assert_eq!(check_request("GET", headers(&keyed)), Err(HandshakeError::BadKey), "{bad:?}");
        }
        assert!(!wants_upgrade(headers(&[("Upgrade", "h2c")])));
    }

    #[test]
    fn the_client_request_refuses_header_injection() {
        let ok = client_request("node:17856", "/ws?topics=hashblock", RFC_KEY, &[("X-API-Key", "secret")]).unwrap();
        assert!(ok.starts_with("GET /ws?topics=hashblock HTTP/1.1\r\nHost: node:17856\r\n"));
        assert!(ok.contains("X-API-Key: secret\r\n") && ok.ends_with("\r\n\r\n"));
        assert!(client_request("node\r\nEvil: 1", "/ws", RFC_KEY, &[]).is_err());
        assert!(client_request("node", "/ws HTTP/1.0", RFC_KEY, &[]).is_err());
        assert!(client_request("node", "/ws", RFC_KEY, &[("X-API-Key", "a\nb")]).is_err());
    }

    #[test]
    fn the_answer_head_is_read_without_the_frame_after_it() {
        let mut bytes = response(RFC_KEY).into_bytes();
        bytes.extend_from_slice(&[0x81, 0x01, b'x']);
        let mut reader = std::io::BufReader::new(&bytes[..]);
        let head = read_response_head(&mut reader, 4096).unwrap();
        assert_eq!(head.status, 101);
        assert_eq!(check_response(&head, RFC_KEY), Ok(()));
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut rest).unwrap();
        assert_eq!(rest, [0x81, 0x01, b'x']);
    }

    #[test]
    fn a_wrong_answer_is_refused() {
        let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let head = read_response_head(&mut &not_found[..], 4096).unwrap();
        assert_eq!(check_response(&head, RFC_KEY), Err(HandshakeError::Status(404)));

        let other_key = response("AAAAAAAAAAAAAAAAAAAAAA==");
        let head = read_response_head(&mut other_key.as_bytes(), 4096).unwrap();
        assert_eq!(check_response(&head, RFC_KEY), Err(HandshakeError::BadAccept));

        let endless = [b'a'; 100];
        assert!(read_response_head(&mut &endless[..], 64).is_err());
        assert!(read_response_head(&mut &b"garbage\r\n\r\n"[..], 64).is_err());
    }
}
