// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The HTTP/1.1 this daemon speaks, written out by hand.
//!
//! The C++ uses `cpp-httplib` (`src/rpc/RpcServer.cpp:8`). This is the subset
//! of it the daemon's own routes actually need — a request line, headers, an
//! optional `Content-Length` body, a response, and keep-alive — with the limits
//! stated rather than inherited:
//!
//! - the request line and every header line are bounded ([`HttpLimits`]);
//! - the body is refused *before* it is read when `Content-Length` exceeds the
//!   cap, and the buffer grows from bytes actually read, never from the number
//!   the client declared;
//! - `Transfer-Encoding` is refused (the C++ routes take a `Content-Length`
//!   body; a chunked one is not something any client of this API sends);
//! - a read or a write that stalls hits the socket timeout the server set and
//!   the connection is dropped, so one slow client costs one worker for one
//!   timeout and nothing more;
//! - and a client that never stalls but trickles — a byte every few seconds —
//!   still has to finish: [`read_request_timed`] puts a deadline on the whole
//!   head and another, scaled to `Content-Length`, on the whole body.
//!
//! [`client`] is the same code read backwards: enough of a blocking HTTP client
//! for `wrkz-rpc-diff` to put the same request to two daemons. It speaks plain
//! `http://` only, which is what the seed nodes serve (spec/09).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// What a request may be, before any handler sees it.
#[derive(Clone, Copy, Debug)]
pub struct HttpLimits {
    /// Longest request line, in bytes (method, target and version).
    pub max_request_line: usize,
    /// Longest single header line, in bytes.
    pub max_header_line: usize,
    /// Most header lines accepted.
    pub max_headers: usize,
    /// Largest body accepted. `--rpc-max-request-body-bytes`, 2 MiB by default
    /// (spec/09 "Transport rules").
    pub max_body: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self { max_request_line: 8 * 1024, max_header_line: 8 * 1024, max_headers: 100, max_body: 2 * 1024 * 1024 }
    }
}

/// One parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// The path with any query string removed.
    pub path: String,
    /// The query string, without the `?`.
    pub query: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A header by name, matched case-insensitively as HTTP requires.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Whether the connection should be kept open after this exchange, given a
    /// server that is willing to.
    pub fn wants_keep_alive(&self) -> bool {
        match self.header("Connection") {
            Some(v) if v.eq_ignore_ascii_case("close") => false,
            Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
            _ => self.version == "HTTP/1.1",
        }
    }
}

/// Why a request could not be read. Each maps to the status the server answers
/// with before it closes the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The peer closed the connection cleanly with nothing pending.
    Closed,
    /// Malformed request line or headers → 400.
    Malformed(&'static str),
    /// The request line, a header line or the header count was over its cap → 431.
    HeadersTooLarge,
    /// `Content-Length` was over [`HttpLimits::max_body`] → 413.
    BodyTooLarge,
    /// `Transfer-Encoding` was present → 501.
    Unsupported(&'static str),
    /// The socket failed or timed out.
    Io(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Closed => write!(f, "connection closed"),
            HttpError::Malformed(w) => write!(f, "malformed request: {w}"),
            HttpError::HeadersTooLarge => write!(f, "request headers too large"),
            HttpError::BodyTooLarge => write!(f, "request body too large"),
            HttpError::Unsupported(w) => write!(f, "unsupported: {w}"),
            HttpError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        HttpError::Io(e.to_string())
    }
}

/// Read one line ending in CRLF (a bare LF is accepted, as `cpp-httplib` does),
/// bounded by `max` bytes before the LF.
///
/// Scans whatever the reader already buffered for the LF rather than asking
/// for one byte at a time; the cap is checked against what has been collected
/// so far, so a line that never ends is refused at `max + 1` bytes exactly as
/// before, whatever the buffer size.
fn read_line<R: BufRead>(r: &mut R, max: usize, first: bool) -> Result<String, HttpError> {
    let mut buf = Vec::new();
    loop {
        let available = match r.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && buf.is_empty() && first => {
                return Err(HttpError::Closed)
            }
            Err(e) => return Err(HttpError::Io(e.to_string())),
        };
        if available.is_empty() {
            if buf.is_empty() && first {
                return Err(HttpError::Closed);
            }
            return Err(HttpError::Malformed("truncated line"));
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(end) => {
                if buf.len() + end > max {
                    return Err(HttpError::HeadersTooLarge);
                }
                buf.extend_from_slice(&available[..end]);
                r.consume(end + 1);
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                return String::from_utf8(buf).map_err(|_| HttpError::Malformed("line is not UTF-8"));
            }
            None => {
                let n = available.len();
                if buf.len() + n > max {
                    return Err(HttpError::HeadersTooLarge);
                }
                buf.extend_from_slice(available);
                r.consume(n);
            }
        }
    }
}

/// A socket whose read timeout can be changed between reads: TCP, the IPC
/// socket, and the server's own either-of-the-two.
pub trait ReadTimeout {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
}

impl ReadTimeout for TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
}

#[cfg(unix)]
impl ReadTimeout for std::os::unix::net::UnixStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, timeout)
    }
}

/// A stream with a deadline for a whole phase of the request on top of the
/// socket's per-read timeout.
///
/// A per-read timeout alone bounds how long one `read` may wait, not how long
/// a request may take: a client that sends one header byte every nine seconds
/// against a ten-second timeout never trips it, and sixteen such clients hold
/// every worker for as long as they like. Here each read waits for the
/// *shorter* of the per-read timeout and what is left of the deadline, and a
/// read that starts after the deadline fails at once with `TimedOut`.
pub struct DeadlineStream<S> {
    inner: S,
    per_read: Duration,
    deadline: Option<Instant>,
}

impl<S: ReadTimeout> DeadlineStream<S> {
    pub fn new(inner: S, per_read: Duration) -> Self {
        Self { inner, per_read, deadline: None }
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// The longest any single read may wait.
    pub fn set_per_read(&mut self, per_read: Duration) {
        self.per_read = per_read;
    }

    /// The instant after which no read may start. `None` leaves only the
    /// per-read timeout.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }
}

impl<S: Read + ReadTimeout> Read for DeadlineStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let timeout = match self.deadline {
            None => self.per_read,
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "request deadline passed"));
                }
                left.min(self.per_read)
            }
        };
        // A zero timeout means "block forever" to the OS; never hand it one.
        self.inner.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        self.inner.read(buf)
    }
}

/// The slowest body upload a deadline allows for: 32 KiB/s, a quarter of a
/// megabit. A wallet posting a 100 KB transaction on that link needs three
/// seconds; the default 2 MiB body cap needs 64 on top of the base allowance.
pub const MIN_BODY_BYTES_PER_SEC: u64 = 32 * 1024;

/// How long a body of `declared` bytes may take: the base allowance (the read
/// timeout, so a small body gets what every body got before) plus its length
/// at [`MIN_BODY_BYTES_PER_SEC`].
pub fn body_deadline(base: Duration, declared: usize) -> Duration {
    base + Duration::from_millis((declared as u64).saturating_mul(1000) / MIN_BODY_BYTES_PER_SEC)
}

/// [`read_request`] over a [`DeadlineStream`]: the request line and the
/// headers must all arrive within `head` of the call, and the body within
/// [`body_deadline`]`(body_base, Content-Length)` of the blank line. The
/// deadline is cleared again before this returns, so the response write and
/// the next request's wait are not charged to this one.
pub fn read_request_timed<S: Read + ReadTimeout>(
    r: &mut BufReader<DeadlineStream<S>>,
    limits: &HttpLimits,
    head: Duration,
    body_base: Duration,
) -> Result<Request, HttpError> {
    r.get_mut().set_deadline(Some(Instant::now() + head));
    let result = read_head(r, limits).and_then(|(mut request, declared)| {
        if declared > 0 {
            r.get_mut().set_deadline(Some(Instant::now() + body_deadline(body_base, declared)));
        }
        request.body = read_body(r, declared)?;
        Ok(request)
    });
    r.get_mut().set_deadline(None);
    result
}

/// Read one request off a connection.
pub fn read_request<R: BufRead>(r: &mut R, limits: &HttpLimits) -> Result<Request, HttpError> {
    let (mut request, declared) = read_head(r, limits)?;
    request.body = read_body(r, declared)?;
    Ok(request)
}

/// The request line and the headers, checked; the body is left unread and its
/// declared length returned beside the request.
fn read_head<R: BufRead>(r: &mut R, limits: &HttpLimits) -> Result<(Request, usize), HttpError> {
    let line = read_line(r, limits.max_request_line, true)?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") || parts.next().is_some() {
        return Err(HttpError::Malformed("request line"));
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    let mut headers = Vec::new();
    loop {
        let line = read_line(r, limits.max_header_line, false)?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= limits.max_headers {
            return Err(HttpError::HeadersTooLarge);
        }
        let (name, value) = line.split_once(':').ok_or(HttpError::Malformed("header line"))?;
        if name.is_empty() || name.contains(' ') {
            return Err(HttpError::Malformed("header name"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }

    let find = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str());

    if find("Transfer-Encoding").is_some() {
        return Err(HttpError::Unsupported("Transfer-Encoding"));
    }

    let declared = match find("Content-Length") {
        None => 0,
        Some(v) => {
            let declared: usize = v.trim().parse().map_err(|_| HttpError::Malformed("Content-Length"))?;
            if declared > limits.max_body {
                return Err(HttpError::BodyTooLarge);
            }
            declared
        }
    };

    Ok((Request { method, path, query, version, headers, body: Vec::new() }, declared))
}

/// Exactly `declared` bytes of body. `declared` is bounded by `max_body` by
/// the time it gets here, but the buffer still grows from bytes actually read:
/// a client that promises a megabyte and sends nothing allocates nothing.
fn read_body<R: Read>(r: &mut R, declared: usize) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    if declared == 0 {
        return Ok(body);
    }
    r.take(declared as u64).read_to_end(&mut body)?;
    if body.len() != declared {
        return Err(HttpError::Malformed("short body"));
    }
    Ok(body)
}

/// One response, before it is written.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new() }
    }

    /// A JSON body, with the `Content-Type` the C++ middleware always sets
    /// (`RpcServer.cpp:534`).
    pub fn json(status: u16, body: String) -> Self {
        let mut r = Self::new(status);
        r.set_header("Content-Type", "application/json");
        r.body = body.into_bytes();
        r
    }

    pub fn set_header(&mut self, name: &str, value: &str) -> &mut Self {
        match self.headers.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some(slot) => slot.1 = value.to_string(),
            None => self.headers.push((name.to_string(), value.to_string())),
        }
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// The reason phrases `cpp-httplib` sends, for the statuses these routes use.
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Write a response, always with an explicit `Content-Length` so keep-alive
/// framing is unambiguous.
pub fn write_response<W: Write>(
    w: &mut W,
    res: &Response,
    keep_alive: bool,
    keep_alive_hint: &str,
) -> std::io::Result<()> {
    let mut head = String::with_capacity(256);
    head.push_str("HTTP/1.1 ");
    head.push_str(&res.status.to_string());
    head.push(' ');
    head.push_str(reason(res.status));
    head.push_str("\r\n");
    if keep_alive {
        head.push_str("Keep-Alive: ");
        head.push_str(keep_alive_hint);
        head.push_str("\r\n");
    } else {
        head.push_str("Connection: close\r\n");
    }
    for (k, v) in &res.headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("Content-Length: ");
    head.push_str(&res.body.len().to_string());
    head.push_str("\r\n\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(&res.body)?;
    w.flush()
}

/// A whole `Connection: close` response as bytes, for an answer written
/// outside a worker — the acceptor's `503` when it sheds a connection.
pub fn render_closing(res: &Response) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + res.body.len());
    // Writing to a `Vec` cannot fail.
    let _ = write_response(&mut out, res, false, "");
    out
}

/// Hand a connection the acceptor will not serve its pre-rendered `answer`
/// and let it go, without ever blocking the accept loop: the socket is put in
/// non-blocking mode first, so a peer whose receive window is somehow already
/// shut costs one failed `write`, not a stalled acceptor. A fresh socket's send
/// buffer is empty, and the answer is a couple of hundred bytes, so in
/// practice it always goes out whole.
pub fn shed_tcp(stream: &TcpStream, answer: &[u8]) {
    let _ = stream.set_nonblocking(true);
    let mut w = stream;
    let _ = w.write_all(answer);
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

// ---------------------------------------------------------------------------
// response compression
// ---------------------------------------------------------------------------

/// Bodies shorter than this go out as they are: gzip's own framing is about 20
/// bytes, and a short answer is not worth a worker's time.
pub const MIN_GZIP_BYTES: usize = 1024;

/// A content coding this build can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coding {
    /// What `cpp-httplib` negotiates, and what every client understands.
    Gzip,
    /// Only with the crate's `zstd` feature, and only for a client that asks
    /// for it by name. A C++ daemon never answers with this.
    #[cfg(feature = "zstd")]
    Zstd,
}

impl Coding {
    /// The `Content-Encoding` token.
    pub fn token(self) -> &'static str {
        match self {
            Coding::Gzip => "gzip",
            #[cfg(feature = "zstd")]
            Coding::Zstd => "zstd",
        }
    }
}

/// Whether a request's `Accept-Encoding` admits `coding`.
///
/// `cpp-httplib` searches the header for the substring `gzip`
/// (`httplib.h:6480`, with a TODO about `gzip;q=0`). This parses the list, so
/// `gzip;q=0` — "anything but gzip" — is honoured. A bare `*` names no coding:
/// a client that sends only that gets identity, which every client reads.
pub fn accepts_coding(req: &Request, coding: &str) -> bool {
    let Some(value) = req.header("Accept-Encoding") else { return false };
    value.split(',').any(|item| {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim();
        let matches = name.eq_ignore_ascii_case(coding)
            || (coding.eq_ignore_ascii_case("gzip") && name.eq_ignore_ascii_case("x-gzip"));
        if !matches {
            return false;
        }
        // `q=0` is "not acceptable" (RFC 9110, 12.4.2).
        !parts.any(|param| {
            let Some((key, q)) = param.split_once('=') else { return false };
            key.trim().eq_ignore_ascii_case("q") && q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0)
        })
    })
}

/// Whether a request's `Accept-Encoding` admits gzip.
pub fn accepts_gzip(req: &Request) -> bool {
    accepts_coding(req, "gzip")
}

/// The coding to answer `req` with, or `None` for identity.
///
/// zstd first when this build has it and the client named it — a wallet sync
/// body is hex and JSON and zstd beats gzip on both size and speed — then
/// gzip, which is what `cpp-httplib` negotiates and what everything else
/// understands. The three "not worth it" rules are the C++'s: a short body, a
/// body already encoded, and a content type that is not text or JSON.
pub fn negotiated_coding(req: &Request, res: &Response) -> Option<Coding> {
    if res.body.len() < MIN_GZIP_BYTES
        || res.header("Content-Encoding").is_some()
        || !res.header("Content-Type").is_some_and(compressible)
    {
        return None;
    }
    #[cfg(feature = "zstd")]
    if accepts_coding(req, "zstd") {
        return Some(Coding::Zstd);
    }
    if accepts_coding(req, "gzip") {
        return Some(Coding::Gzip);
    }
    None
}

/// `can_compress_content_type` (`httplib.h:6439`), reduced to the types these
/// routes produce: JSON and `text/*`.
fn compressible(content_type: &str) -> bool {
    let media = content_type.split(';').next().unwrap_or("").trim();
    media.eq_ignore_ascii_case("application/json") || media.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("text/"))
}

/// Compress `res` in place when the client accepts a coding and the body is
/// worth it — what `cpp-httplib` does after every handler when the C++ is
/// built with zlib, which is how public nodes are built (their `/info` says
/// `"compression":"gzip"`). Returns the coding used.
///
/// The fastest level, for both codings. A wallet sync body is hex and JSON
/// punctuation, which the fastest level already shrinks by most of what the
/// slowest would, and a worker busy compressing is a worker not answering the
/// next wallet.
pub fn compress_if_accepted(req: &Request, res: &mut Response) -> Option<Coding> {
    let coding = negotiated_coding(req, res)?;
    let compressed = compress(&res.body, coding)?;
    res.body = compressed;
    res.set_header("Content-Encoding", coding.token());
    res.set_header("Vary", "Accept-Encoding");
    Some(coding)
}

/// [`compress_if_accepted`] restricted to gzip, which is the coding the C++
/// comparison and the transport tests talk about.
pub fn gzip_if_accepted(req: &Request, res: &mut Response) -> bool {
    if !accepts_coding(req, "gzip") || negotiated_coding(req, res).is_none() {
        return false;
    }
    let Some(compressed) = compress(&res.body, Coding::Gzip) else { return false };
    res.body = compressed;
    res.set_header("Content-Encoding", "gzip");
    res.set_header("Vary", "Accept-Encoding");
    true
}

/// One buffer in, one buffer out. `None` when the encoder failed, which for an
/// in-memory writer means an allocation failure.
fn compress(body: &[u8], coding: Coding) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len() / 4);
    {
        let mut sink = encoder(&mut out, coding);
        sink.write_all(body).ok()?;
        sink.finish().ok()?;
    }
    Some(out)
}

/// A compressing sink over `w`. Boxed rather than generic because the two
/// codings are different types and no caller cares which it holds.
fn encoder<'a, W: Write + 'a>(w: W, coding: Coding) -> Box<dyn Encoder + 'a> {
    match coding {
        Coding::Gzip => Box::new(flate2::write::GzEncoder::new(w, flate2::Compression::fast())),
        #[cfg(feature = "zstd")]
        Coding::Zstd => {
            Box::new(ZstdEncoder(Some(zstd::stream::write::Encoder::new(w, 1).expect("zstd level 1 is valid"))))
        }
    }
}

/// What both encoders offer: write, then a finish that flushes the trailer.
trait Encoder: Write {
    fn finish(self: Box<Self>) -> std::io::Result<()>;
}

impl<W: Write> Encoder for flate2::write::GzEncoder<W> {
    fn finish(self: Box<Self>) -> std::io::Result<()> {
        (*self).finish().map(|_| ())
    }
}

/// `zstd::stream::write::Encoder::finish` takes `self` by value, so the
/// encoder lives in an `Option` that `finish` empties.
#[cfg(feature = "zstd")]
struct ZstdEncoder<'a, W: Write>(Option<zstd::stream::write::Encoder<'a, W>>);

#[cfg(feature = "zstd")]
impl<W: Write> Write for ZstdEncoder<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.as_mut() {
            Some(e) => e.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.as_mut() {
            Some(e) => e.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(feature = "zstd")]
impl<W: Write> Encoder for ZstdEncoder<'_, W> {
    fn finish(mut self: Box<Self>) -> std::io::Result<()> {
        match self.0.take() {
            Some(e) => e.finish().map(|_| ()),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// streaming a compressed body
// ---------------------------------------------------------------------------

/// `Transfer-Encoding: chunked` over `W`: each `write` becomes one chunk, and
/// [`ChunkedWriter::finish`] writes the terminating zero-length chunk.
struct ChunkedWriter<W: Write> {
    inner: W,
    done: bool,
}

impl<W: Write> ChunkedWriter<W> {
    fn new(inner: W) -> Self {
        ChunkedWriter { inner, done: false }
    }

    fn finish(mut self) -> std::io::Result<()> {
        if !self.done {
            self.done = true;
            self.inner.write_all(b"0\r\n\r\n")?;
        }
        Ok(())
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            // A zero-length chunk is the terminator; never emit one here.
            return Ok(0);
        }
        self.inner.write_all(format!("{:x}\r\n", buf.len()).as_bytes())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// How much plaintext goes to the encoder at a time when streaming.
pub const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Write `res` compressed with `coding` straight to the socket, framed with
/// `Transfer-Encoding: chunked`.
///
/// The point is the copy that never exists. [`compress_if_accepted`] builds
/// the whole compressed body in a second buffer before a byte goes out, so a
/// worker answering a 30 MiB `/getrawblocks` holds the plaintext *and* the
/// compressed copy at once, times however many workers are busy. Here the
/// encoder writes into the socket as it goes, and the extra memory is one
/// [`STREAM_CHUNK_BYTES`] window plus the encoder's own state.
///
/// The cost is the framing: `cpp-httplib` always sends `Content-Length`, so a
/// response written this way is not byte-identical to the C++ daemon's. That
/// is why it is off by default and behind
/// [`crate::server::ServerConfig::stream_threshold_bytes`] — an operator
/// running a busy public node turns it on knowingly. Every HTTP/1.1 client
/// must accept chunked, and this port's wallet, `curl` and `cpp-httplib`'s own
/// client all do.
pub fn write_response_streamed<W: Write>(
    w: &mut W,
    res: &Response,
    coding: Coding,
    keep_alive: bool,
    keep_alive_hint: &str,
) -> std::io::Result<()> {
    let mut head = String::with_capacity(256);
    head.push_str("HTTP/1.1 ");
    head.push_str(&res.status.to_string());
    head.push(' ');
    head.push_str(reason(res.status));
    head.push_str("\r\n");
    if keep_alive {
        head.push_str("Keep-Alive: ");
        head.push_str(keep_alive_hint);
        head.push_str("\r\n");
    } else {
        head.push_str("Connection: close\r\n");
    }
    for (k, v) in &res.headers {
        if k.eq_ignore_ascii_case("Content-Encoding")
            || k.eq_ignore_ascii_case("Vary")
            || k.eq_ignore_ascii_case("Content-Length")
        {
            continue;
        }
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("Content-Encoding: ");
    head.push_str(coding.token());
    head.push_str("\r\nVary: Accept-Encoding\r\nTransfer-Encoding: chunked\r\n\r\n");
    w.write_all(head.as_bytes())?;

    let mut chunked = ChunkedWriter::new(&mut *w);
    {
        let mut sink = encoder(&mut chunked, coding);
        for part in res.body.chunks(STREAM_CHUNK_BYTES) {
            sink.write_all(part)?;
        }
        sink.finish()?;
    }
    chunked.finish()?;
    w.flush()
}

// ---------------------------------------------------------------------------
// a minimal client, for wrkz-rpc-diff
// ---------------------------------------------------------------------------

/// Enough of an HTTP client to put one request to a daemon and read the answer.
pub mod client {
    use super::*;

    /// A parsed `http://host:port` base URL.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct BaseUrl {
        pub host: String,
        pub port: u16,
    }

    /// Parse `http://host[:port]`, with a trailing slash allowed. `https://` is
    /// refused: this client has no TLS, and the daemons speak plain HTTP.
    pub fn parse_base(url: &str) -> Result<BaseUrl, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("{url}: only http:// is supported (this client has no TLS)"))?;
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            return Err(format!("{url}: no host"));
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (h.to_string(), p.parse::<u16>().map_err(|e| format!("{url}: bad port: {e}"))?)
            }
            _ => (rest.to_string(), 80u16),
        };
        Ok(BaseUrl { host, port })
    }

    /// One HTTP exchange: `(status, body)`.
    ///
    /// `body` is capped at `max_response` bytes; a longer one is an error
    /// rather than an unbounded allocation.
    pub fn request(
        base: &BaseUrl,
        method: &str,
        path: &str,
        body: Option<&str>,
        timeout: Duration,
        max_response: usize,
    ) -> Result<(u16, Vec<u8>), String> {
        let addr = format!("{}:{}", base.host, base.port);
        let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
        stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_nodelay(true).ok();
        let headers: &[(&str, &str)] = if body.is_some() { &[("Content-Type", "application/json")] } else { &[] };
        exchange(stream, method, path, &addr, headers, body.map(str::as_bytes), max_response as u64)
    }

    /// One request and its answer, over a stream the caller has connected and
    /// given its timeouts: TCP for the comparison tool and a UPnP gateway, the
    /// IPC socket for `wrkz-node attach`.
    ///
    /// The request carries `Host`, `Connection: close`, then `headers`, then a
    /// `Content-Length` when there is a body. The answer's body is read to its
    /// `Content-Length`, chunk by chunk when it is chunked, and otherwise to the
    /// end of the stream. Past `max_response` bytes it is an error, and nothing
    /// is allocated from a length the peer declared beyond that cap.
    pub fn exchange<S: Read + Write>(
        mut stream: S,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        max_response: u64,
    ) -> Result<(u16, Vec<u8>), String> {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(b) = body {
            head.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).map_err(|e| format!("write: {e}"))?;
        if let Some(b) = body {
            stream.write_all(b).map_err(|e| format!("write: {e}"))?;
        }
        stream.flush().map_err(|e| format!("flush: {e}"))?;

        let mut reader = BufReader::new(stream);
        let status_line = read_line(&mut reader, 8 * 1024, true).map_err(|e| e.to_string())?;
        let status: u16 = status_line
            .split(' ')
            .nth(1)
            .ok_or_else(|| format!("bad status line: {status_line}"))?
            .parse()
            .map_err(|e| format!("bad status: {e}"))?;

        let mut length: Option<usize> = None;
        let mut chunked = false;
        loop {
            let line = read_line(&mut reader, 8 * 1024, false).map_err(|e| e.to_string())?;
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse().ok();
                } else if k.eq_ignore_ascii_case("transfer-encoding") && v.trim().eq_ignore_ascii_case("chunked") {
                    chunked = true;
                }
            }
        }

        let mut body = Vec::new();
        if chunked {
            loop {
                let size_line = read_line(&mut reader, 64, false).map_err(|e| e.to_string())?;
                let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("0").trim(), 16)
                    .map_err(|e| format!("bad chunk size: {e}"))?;
                if size == 0 {
                    break;
                }
                if body.len() as u64 + size as u64 > max_response {
                    return Err(format!("response over {max_response} bytes"));
                }
                let mut chunk = vec![0u8; size];
                reader.read_exact(&mut chunk).map_err(|e| format!("read chunk: {e}"))?;
                body.extend_from_slice(&chunk);
                read_line(&mut reader, 8, false).map_err(|e| e.to_string())?;
            }
        } else if let Some(length) = length {
            if length as u64 > max_response {
                return Err(format!("response over {max_response} bytes"));
            }
            // Grown from the bytes that arrive, not from the declared length.
            reader.take(length as u64).read_to_end(&mut body).map_err(|e| format!("read body: {e}"))?;
            if body.len() < length {
                return Err(format!("read body: the connection closed after {} of {length} bytes", body.len()));
            }
        } else {
            reader.take(max_response + 1).read_to_end(&mut body).map_err(|e| format!("read body: {e}"))?;
            if body.len() as u64 > max_response {
                return Err(format!("response over {max_response} bytes"));
            }
        }
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &str, limits: HttpLimits) -> Result<Request, HttpError> {
        let mut r = BufReader::new(raw.as_bytes());
        read_request(&mut r, &limits)
    }

    /// A stream in memory: reads come from `input`, writes collect in `output`.
    struct Duplex {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Duplex {
        fn answering(answer: &[u8]) -> Self {
            Self { input: std::io::Cursor::new(answer.to_vec()), output: Vec::new() }
        }
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_client_frames_a_request_and_reads_every_kind_of_body() {
        let json = [("Content-Type", "application/json")];
        let mut sized = Duplex::answering(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello, and no more");
        let answer = client::exchange(&mut sized, "POST", "/console", "localhost", &json, Some(&b"{}"[..]), 1024);
        assert_eq!(answer, Ok((200, b"hello".to_vec())), "read to its Content-Length");
        assert_eq!(
            String::from_utf8(sized.output).unwrap(),
            "POST /console HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\n\
             Content-Length: 2\r\n\r\n{}"
        );

        let get = |answer: &[u8], max: u64| {
            client::exchange(&mut Duplex::answering(answer), "GET", "/", "gw", &[], None, max)
        };
        let chunked = b"HTTP/1.1 500 Error\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(get(chunked, 1024), Ok((500, b"abcde".to_vec())));
        let to_close = get(b"HTTP/1.0 200 OK\r\n\r\nall of it", 64);
        assert_eq!(to_close, Ok((200, b"all of it".to_vec())), "no length: read to the end");
        let declared = get(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n", 1024);
        assert_eq!(declared, Err("response over 1024 bytes".to_string()));
        let short = get(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc", 64);
        assert!(short.unwrap_err().contains("closed after 3 of 10 bytes"));
        let overflow = get(b"HTTP/1.1 200 OK\r\n\r\n0123456789", 4);
        assert_eq!(overflow, Err("response over 4 bytes".to_string()));
    }

    #[test]
    fn a_plain_get_parses() {
        let req = read("GET /info?x=1 HTTP/1.1\r\nHost: a\r\nX-API-Key: k\r\n\r\n", HttpLimits::default()).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/info");
        assert_eq!(req.query, "x=1");
        assert_eq!(req.header("x-api-key"), Some("k"));
        assert!(req.body.is_empty());
        assert!(req.wants_keep_alive());
    }

    #[test]
    fn a_post_body_is_read_and_bounded() {
        let req = read("POST /height HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}", HttpLimits::default()).unwrap();
        assert_eq!(req.body, b"{}");
        let limits = HttpLimits { max_body: 1, ..HttpLimits::default() };
        assert_eq!(read("POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\n123456789", limits), Err(HttpError::BodyTooLarge));
        // A declared length the client never sends is a short body, not a hang
        // on a pre-sized buffer.
        assert_eq!(
            read("POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\n12", HttpLimits::default()),
            Err(HttpError::Malformed("short body"))
        );
    }

    #[test]
    fn hostile_heads_are_refused() {
        let limits = HttpLimits { max_headers: 2, max_header_line: 32, max_request_line: 32, ..Default::default() };
        let many = "GET / HTTP/1.1\r\na: 1\r\nb: 2\r\nc: 3\r\n\r\n";
        assert_eq!(read(many, limits), Err(HttpError::HeadersTooLarge));
        let long_line = format!("GET / HTTP/1.1\r\na: {}\r\n\r\n", "x".repeat(200));
        assert_eq!(read(&long_line, limits), Err(HttpError::HeadersTooLarge));
        let long_target = format!("GET /{} HTTP/1.1\r\n\r\n", "x".repeat(200));
        assert_eq!(read(&long_target, limits), Err(HttpError::HeadersTooLarge));
        assert_eq!(read("nonsense\r\n\r\n", HttpLimits::default()), Err(HttpError::Malformed("request line")));
        assert_eq!(read("", HttpLimits::default()), Err(HttpError::Closed));
        assert_eq!(
            read("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", HttpLimits::default()),
            Err(HttpError::Unsupported("Transfer-Encoding"))
        );
    }

    #[test]
    fn lines_split_across_buffer_refills_parse_and_keep_their_cap() {
        // A four-byte buffer forces every line across several refills.
        let raw = "POST /height HTTP/1.1\r\nContent-Length: 2\r\nX-Long: abcdefghij\r\n\r\n{}";
        let mut r = BufReader::with_capacity(4, raw.as_bytes());
        let req = read_request(&mut r, &HttpLimits::default()).unwrap();
        assert_eq!(req.header("x-long"), Some("abcdefghij"));
        assert_eq!(req.body, b"{}");
        // The cap counts bytes before the LF, the CR included, as it always
        // did: exactly `max` fits, one more does not.
        let limits = HttpLimits { max_header_line: 9, ..Default::default() };
        let fits = "GET / HTTP/1.1\r\nA: 123456\r\n\r\n"; // "A: 123456\r" is 10 bytes
        let mut r = BufReader::with_capacity(3, fits.as_bytes());
        assert_eq!(read_request(&mut r, &limits), Err(HttpError::HeadersTooLarge));
        let limits = HttpLimits { max_header_line: 10, ..Default::default() };
        let mut r = BufReader::with_capacity(3, fits.as_bytes());
        assert!(read_request(&mut r, &limits).is_ok());
    }

    #[test]
    fn a_passed_deadline_fails_the_read_without_waiting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (served, _) = listener.accept().unwrap();
        let mut stream = DeadlineStream::new(served, Duration::from_secs(30));
        stream.set_deadline(Some(Instant::now()));
        let started = Instant::now();
        let e = stream.read(&mut [0u8; 8]).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        // A body's allowance grows with its declared length.
        assert_eq!(body_deadline(Duration::from_secs(10), 0), Duration::from_secs(10));
        assert_eq!(body_deadline(Duration::from_secs(10), 2 * 1024 * 1024), Duration::from_secs(74));
    }

    #[test]
    fn responses_carry_their_own_length() {
        let res = Response::json(200, "{\"status\":\"OK\"}".into());
        let mut out = Vec::new();
        write_response(&mut out, &res, true, "timeout=3, max=1000").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Keep-Alive: timeout=3, max=1000\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.ends_with("Content-Length: 15\r\n\r\n{\"status\":\"OK\"}"));
        let mut out = Vec::new();
        write_response(&mut out, &res, false, "").unwrap();
        assert!(String::from_utf8(out).unwrap().contains("Connection: close\r\n"));
    }

    fn request_with(accept: Option<&str>) -> Request {
        let mut headers = Vec::new();
        if let Some(a) = accept {
            headers.push(("Accept-Encoding".to_string(), a.to_string()));
        }
        Request {
            method: "POST".into(),
            path: "/getwalletsyncdata".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn accept_encoding_is_parsed_not_searched() {
        assert!(accepts_gzip(&request_with(Some("gzip"))));
        assert!(accepts_gzip(&request_with(Some("deflate, GZIP"))));
        assert!(accepts_gzip(&request_with(Some("br;q=1.0, gzip;q=0.5"))));
        assert!(accepts_gzip(&request_with(Some("x-gzip"))));
        assert!(!accepts_gzip(&request_with(None)));
        assert!(!accepts_gzip(&request_with(Some("identity"))));
        assert!(!accepts_gzip(&request_with(Some("gzip;q=0"))), "q=0 is a refusal");
        assert!(!accepts_gzip(&request_with(Some("gzip; q=0.000"))));
        assert!(!accepts_gzip(&request_with(Some("*"))), "a wildcard gets identity, which every client reads");
        assert!(!accepts_gzip(&request_with(Some("notgzip"))));
    }

    #[test]
    fn a_json_body_round_trips_through_gzip() {
        let body = format!("{{\"items\":[{}]}}", vec!["\"00112233445566778899aabbccddeeff\""; 200].join(","));
        let mut res = Response::json(200, body.clone());
        assert!(gzip_if_accepted(&request_with(Some("gzip")), &mut res));
        assert_eq!(res.header("Content-Encoding"), Some("gzip"));
        assert_eq!(res.header("Vary"), Some("Accept-Encoding"));
        assert!(res.body.len() < body.len() / 4, "hex compresses: {} of {}", res.body.len(), body.len());
        let mut plain = String::new();
        flate2::read::GzDecoder::new(res.body.as_slice()).read_to_string(&mut plain).unwrap();
        assert_eq!(plain, body);
        // Never twice.
        let once = res.body.clone();
        assert!(!gzip_if_accepted(&request_with(Some("gzip")), &mut res));
        assert_eq!(res.body, once);
    }

    #[test]
    fn small_bodies_other_types_and_unwilling_clients_go_out_as_they_are() {
        let big = "x".repeat(MIN_GZIP_BYTES * 2);
        let mut small = Response::json(200, "{\"status\":\"OK\"}".into());
        assert!(!gzip_if_accepted(&request_with(Some("gzip")), &mut small));
        assert_eq!(small.header("Content-Encoding"), None);
        let mut untyped = Response::new(200);
        untyped.body = big.clone().into_bytes();
        assert!(!gzip_if_accepted(&request_with(Some("gzip")), &mut untyped), "no Content-Type, no guess");
        let mut json = Response::json(200, big.clone());
        assert!(!gzip_if_accepted(&request_with(None), &mut json));
        assert_eq!(json.body, big.as_bytes());
    }

    #[test]
    fn base_urls_parse() {
        use client::{parse_base, BaseUrl};
        assert_eq!(
            parse_base("http://node-fin.wrkz.work:17856"),
            Ok(BaseUrl { host: "node-fin.wrkz.work".into(), port: 17856 })
        );
        assert_eq!(parse_base("http://127.0.0.1:8/"), Ok(BaseUrl { host: "127.0.0.1".into(), port: 8 }));
        assert_eq!(parse_base("http://example.com"), Ok(BaseUrl { host: "example.com".into(), port: 80 }));
        assert!(parse_base("https://example.com").is_err());
        assert!(parse_base("example.com:80").is_err());
    }
}
