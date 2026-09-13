// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Talking to a daemon over a local socket instead of TCP — `common/IpcSocket.h`
//! and `Utilities::isIpcDaemonAddress` (`utilities/Utilities.cpp`).
//!
//! `zedwallet++` accepts an IPC endpoint anywhere it accepts a daemon address
//! (`zedwallet++/GetInput.cpp:370`, `ParseArguments.cpp:170`), skips the SSL
//! question for it (there is no TLS on a local socket; the kernel already
//! decides who may open it), and refuses it with a reason where the platform
//! has none.
//!
//! # The three forms
//!
//! | Written as | Means |
//! | --- | --- |
//! | `/run/wrkz/daemon.sock` | a filesystem socket at that absolute path |
//! | `ipc:///run/wrkz/daemon.sock` | the same, said explicitly |
//! | `@wrkzd` | the Linux abstract namespace, no file on disk |
//!
//! # Transport
//!
//! `ureq` cannot dial a Unix socket, so this does not try to bend it. The
//! request is written and the response parsed by [`exchange`], which works over
//! any `Read + Write` — a `UnixStream` here, and a `TcpStream` in the test that
//! checks the two agree.
//!
//! **What belongs in `wrkz-rpc` instead.** [`wrkz_rpc::http::client::request`]
//! already writes a request and parses a response, but it opens the
//! `TcpStream` itself and takes no extra headers, so neither a Unix socket nor
//! `X-API-Key` can go through it. Two changes there would let this module
//! delete [`exchange`] entirely and call it instead:
//!
//! 1. Split the body of `client::request` into
//!    `client::exchange<S: Read + Write>(stream: S, method, path, host, body,
//!    headers, timeout, max_response) -> Result<(u16, Vec<u8>), String>`, and
//!    leave `request` as the `TcpStream::connect` wrapper around it.
//! 2. Give both a `headers: &[(&str, &str)]` argument.
//!
//! Both are additive and break nothing. `crates/wrkz-rpc` is not ours to edit,
//! so this is a follow-up rather than a change.

use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::daemon::{
    DaemonError, GlobalIndexes, Info, RandomOuts, Result, SendResult, SyncRequest, TransactionsStatus, WalletSyncData,
    MAX_RESPONSE_BYTES,
};
use crate::sync::SyncDaemon;
use crate::transfer::TransferDaemon;

/// How long a single exchange may take. Only the unix path sets it, because
/// only the unix path opens a socket.
#[cfg_attr(not(unix), allow(dead_code))]
const TIMEOUT: Duration = Duration::from_secs(60);

////////////////////////
/* ADDRESS FORMS      */
////////////////////////

/// `Utilities::isIpcDaemonAddress`: an absolute path, an `@name`, or an
/// `ipc://` URL.
///
/// A Windows drive path (`C:\…`) is deliberately *not* one: it would collide
/// with `host:port`, and Windows has no Unix socket for the daemon to bind
/// anyway.
pub fn is_ipc_address(address: &str) -> bool {
    address.starts_with("ipc://") || address.starts_with('@') || address.starts_with('/')
}

/// The socket path an address names, with any `ipc://` prefix removed.
pub fn socket_path(address: &str) -> &str {
    address.strip_prefix("ipc://").unwrap_or(address)
}

/// Whether this build can open one at all.
///
/// Filesystem sockets need `std::os::unix`; the `@name` abstract namespace
/// additionally needs Linux, and is reported separately by
/// [`unsupported_reason`].
pub fn supported() -> bool {
    cfg!(unix)
}

/// `Common::Ipc::unsupportedReason()`: why an IPC address cannot be used here.
///
/// `None` when it can.
pub fn unsupported_reason(address: &str) -> Option<String> {
    if !cfg!(unix) {
        return Some("local IPC sockets are not available on this platform".to_string());
    }
    if socket_path(address).starts_with('@') && !cfg!(target_os = "linux") {
        return Some("the abstract socket namespace (@name) is a Linux extension".to_string());
    }
    None
}

////////////////////////
/* THE TRANSPORT      */
////////////////////////

/// One HTTP/1.1 exchange over an already-connected stream.
///
/// `Connection: close`, so the response ends with the stream and there is no
/// keep-alive state to get wrong. The body is capped at `max_response` bytes,
/// and nothing is allocated from a length the peer declared.
pub fn exchange<S: Read + Write>(
    mut stream: S,
    method: &str,
    path: &str,
    host: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
    max_response: u64,
) -> std::result::Result<(u16, Vec<u8>), String> {
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");

    stream.write_all(request.as_bytes()).map_err(|e| format!("write: {e}"))?;
    if let Some(body) = body {
        stream.write_all(body).map_err(|e| format!("write body: {e}"))?;
    }
    stream.flush().map_err(|e| format!("flush: {e}"))?;

    let mut reader = BufReader::new(stream);

    let status_line = read_line(&mut reader)?;
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .ok_or_else(|| format!("bad status line: {status_line}"))?
        .parse()
        .map_err(|e| format!("bad status: {e}"))?;

    let mut length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let line = read_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") && value.trim().eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }

    let mut out = Vec::new();
    if chunked {
        loop {
            let size_line = read_line(&mut reader)?;
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("0").trim(), 16)
                .map_err(|e| format!("bad chunk size: {e}"))?;
            if size == 0 {
                break;
            }
            if out.len() as u64 + size as u64 > max_response {
                return Err(format!("response over {max_response} bytes"));
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk).map_err(|e| format!("read chunk: {e}"))?;
            out.extend_from_slice(&chunk);
            // The CRLF that ends the chunk.
            let _ = read_line(&mut reader)?;
        }
    } else if let Some(length) = length {
        if length as u64 > max_response {
            return Err(format!("response over {max_response} bytes"));
        }
        out = vec![0u8; length];
        reader.read_exact(&mut out).map_err(|e| format!("read body: {e}"))?;
    } else {
        // No length and not chunked: read to the close, still bounded.
        reader.take(max_response + 1).read_to_end(&mut out).map_err(|e| format!("read body: {e}"))?;
        if out.len() as u64 > max_response {
            return Err(format!("response over {max_response} bytes"));
        }
    }

    Ok((status, out))
}

/// One CRLF-terminated line, trimmed, capped so a peer cannot make us allocate.
fn read_line<R: BufRead>(reader: &mut R) -> std::result::Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() >= 8 * 1024 {
                    return Err("header line too long".to_string());
                }
                line.push(byte[0]);
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    while line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|e| format!("header is not utf-8: {e}"))
}

////////////////////////
/* THE DAEMON         */
////////////////////////

/// A daemon reached over a local socket. Implements the same two traits the
/// TCP [`crate::daemon::Daemon`] does, so the front ends cannot tell them apart.
///
/// One connection per request, as `Connection: close` implies. A wallet makes a
/// handful of requests a second at most, and a local socket connect is cheap.
pub struct IpcDaemon {
    path: String,
    /// `X-API-Key`, which the C++ still requires on the IPC socket.
    api_key: Option<String>,
}

impl IpcDaemon {
    /// A daemon at `address`, in any of the three forms
    /// [`is_ipc_address`] accepts.
    ///
    /// `Err` names why the platform cannot use it, the way
    /// `Common::Ipc::unsupportedReason()` does.
    pub fn new(address: &str) -> std::result::Result<IpcDaemon, String> {
        if let Some(reason) = unsupported_reason(address) {
            return Err(reason);
        }
        Ok(IpcDaemon { path: socket_path(address).to_string(), api_key: None })
    }

    /// Send the token as `X-API-Key`.
    pub fn with_api_key(mut self, key: &str) -> IpcDaemon {
        self.api_key = Some(key.to_string());
        self
    }

    /// The socket this talks to.
    pub fn path(&self) -> &str {
        &self.path
    }

    fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> Result<T> {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(key) = &self.api_key {
            headers.push(("X-API-Key", key.as_str()));
        }

        let (status, bytes) = self
            .connect()
            .and_then(|stream| {
                exchange(stream, method, path, "localhost", body.as_deref(), &headers, MAX_RESPONSE_BYTES)
            })
            .map_err(DaemonError::Transport)?;

        // The same mapping `Daemon::map_err` makes, so the synchronizer's
        // backoff policy sees the same errors over either transport.
        match status {
            200 => serde_json::from_slice(&bytes).map_err(|e| DaemonError::Json(e.to_string())),
            429 => Err(DaemonError::RateLimited),
            400 => Err(DaemonError::BadRequest(String::from_utf8_lossy(&bytes).into_owned())),
            404 => Err(DaemonError::NotFound),
            code => Err(DaemonError::Http(code, String::from_utf8_lossy(&bytes).into_owned())),
        }
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        self.call("GET", path, None)
    }

    fn post<B: Serialize, T: for<'de> Deserialize<'de>>(&self, path: &str, body: &B) -> Result<T> {
        let bytes = serde_json::to_vec(body).map_err(|e| DaemonError::Json(e.to_string()))?;
        self.call("POST", path, Some(bytes))
    }

    fn checked<T: HasStatus>(v: T) -> Result<T> {
        if v.status() == "OK" {
            Ok(v)
        } else {
            Err(DaemonError::Status(v.status().to_string()))
        }
    }

    #[cfg(unix)]
    fn connect(&self) -> std::result::Result<std::os::unix::net::UnixStream, String> {
        use std::os::unix::net::UnixStream;

        let stream = if let Some(name) = self.path.strip_prefix('@') {
            #[cfg(target_os = "linux")]
            {
                use std::os::linux::net::SocketAddrExt;
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
                    .map_err(|e| format!("abstract name {name}: {e}"))?;
                UnixStream::connect_addr(&addr).map_err(|e| format!("connect @{name}: {e}"))?
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                return Err("the abstract socket namespace (@name) is a Linux extension".to_string());
            }
        } else {
            UnixStream::connect(&self.path).map_err(|e| format!("connect {}: {e}", self.path))?
        };

        stream.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
        Ok(stream)
    }

    #[cfg(not(unix))]
    fn connect(&self) -> std::result::Result<std::net::TcpStream, String> {
        Err("local IPC sockets are not available on this platform".to_string())
    }
}

/// The `status` field every daemon response carries.
trait HasStatus {
    fn status(&self) -> &str;
}

macro_rules! has_status {
    ($($t:ty),*) => {
        $(impl HasStatus for $t {
            fn status(&self) -> &str {
                &self.status
            }
        })*
    };
}

has_status!(Info, WalletSyncData, GlobalIndexes, RandomOuts, TransactionsStatus);

impl SyncDaemon for IpcDaemon {
    fn wallet_sync_data(&self, req: &SyncRequest) -> Result<WalletSyncData> {
        Self::checked(self.post("/getwalletsyncdata", req)?)
    }

    fn global_indexes_for_range(&self, start: u64, end: u64) -> Result<GlobalIndexes> {
        Self::checked(
            self.post("/get_global_indexes_for_range", &serde_json::json!({ "startHeight": start, "endHeight": end }))?,
        )
    }

    fn transactions_status(&self, hashes: &[String]) -> Result<TransactionsStatus> {
        Self::checked(self.post("/get_transactions_status", &serde_json::json!({ "transactionHashes": hashes }))?)
    }

    fn info(&self) -> Result<Info> {
        Self::checked(self.get("/info")?)
    }
}

impl TransferDaemon for IpcDaemon {
    fn random_outs(&self, amounts: &[u64], outs_count: u64) -> Result<RandomOuts> {
        Self::checked(
            self.post("/getrandom_outs", &serde_json::json!({ "amounts": amounts, "outs_count": outs_count }))?,
        )
    }

    /// HTTP 200 either way; `status` is `OK` or `Failed` with `error`.
    fn send_raw_transaction(&self, tx_hex: &str) -> Result<SendResult> {
        self.post("/sendrawtransaction", &serde_json::json!({ "tx_as_hex": tx_hex }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_address_forms_are_recognised() {
        assert!(is_ipc_address("/run/wrkz/daemon.sock"));
        assert!(is_ipc_address("ipc:///run/wrkz/daemon.sock"));
        assert!(is_ipc_address("@wrkzd"));

        // A host, a host:port and a bracketed IPv6 are not.
        assert!(!is_ipc_address("127.0.0.1"));
        assert!(!is_ipc_address("node-fin.wrkz.work:17856"));
        assert!(!is_ipc_address("[::1]:17856"));
        // Nor a Windows drive path, which would collide with host:port.
        assert!(!is_ipc_address(r"C:\run\wrkz.sock"));
    }

    #[test]
    fn the_ipc_prefix_is_stripped_and_the_rest_left_alone() {
        assert_eq!(socket_path("ipc:///run/w.sock"), "/run/w.sock");
        assert_eq!(socket_path("/run/w.sock"), "/run/w.sock");
        assert_eq!(socket_path("@wrkzd"), "@wrkzd");
    }

    #[test]
    fn an_unsupported_platform_says_why() {
        let reason = unsupported_reason("/run/w.sock");
        if cfg!(unix) {
            assert!(reason.is_none());
        } else {
            assert_eq!(reason.as_deref(), Some("local IPC sockets are not available on this platform"));
            assert!(IpcDaemon::new("/run/w.sock").is_err());
        }
    }

    #[test]
    fn the_abstract_namespace_is_linux_only() {
        let reason = unsupported_reason("@wrkzd");
        if cfg!(target_os = "linux") {
            assert!(reason.is_none());
        } else {
            assert!(reason.is_some(), "a non-Linux platform must refuse @name with a reason");
        }
    }

    /// The transport, over a TCP socket rather than a Unix one, so it runs on
    /// every platform. The Unix path differs only in how the stream is opened.
    #[test]
    fn the_exchange_reads_a_content_length_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let trimmed = line.trim_end().to_string();
                request.push_str(&line);
                if trimmed.is_empty() {
                    break;
                }
            }
            let body = br#"{"status":"OK"}"#;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            request
        });

        let stream = std::net::TcpStream::connect(addr).expect("connect");
        let (status, body) = exchange(
            stream,
            "POST",
            "/getwalletsyncdata",
            "localhost",
            Some(b"{\"blockCount\":100}"),
            &[("X-API-Key", "secret")],
            MAX_RESPONSE_BYTES,
        )
        .expect("exchange");

        assert_eq!(status, 200);
        assert_eq!(body, br#"{"status":"OK"}"#);

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /getwalletsyncdata HTTP/1.1\r\n"));
        assert!(request.contains("X-API-Key: secret\r\n"), "{request}");
        assert!(request.contains("Content-Length: 18\r\n"), "{request}");
        assert!(request.contains("Connection: close\r\n"));
    }

    #[test]
    fn the_exchange_reads_a_chunked_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim_end().is_empty() {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1,\r\n6\r\n\"b\":2}\r\n0\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
        });

        let stream = std::net::TcpStream::connect(addr).expect("connect");
        let (status, body) =
            exchange(stream, "GET", "/info", "localhost", None, &[], MAX_RESPONSE_BYTES).expect("exchange");
        assert_eq!(status, 200);
        assert_eq!(body, br#"{"a":1,"b":2}"#);
    }

    #[test]
    fn an_oversized_body_is_refused_rather_than_allocated() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim_end().is_empty() {
                    break;
                }
            }
            // A declared length far above the cap. Nothing is sent after it.
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999\r\n\r\n");
            let _ = stream.flush();
        });

        let stream = std::net::TcpStream::connect(addr).expect("connect");
        let error = exchange(stream, "GET", "/info", "localhost", None, &[], 1024).expect_err("should refuse");
        assert!(error.contains("over 1024 bytes"), "{error}");
    }
}
