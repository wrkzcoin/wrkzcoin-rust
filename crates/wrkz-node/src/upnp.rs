// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! UPnP port mapping: `addPortMapping` (`src/p2p/NetNode.cpp:85-146`), which
//! the C++ runs through miniupnpc 2.3.3, done here with nothing but std.
//!
//! What happens, in the C++ order:
//!
//! 1. **Discover** ([`discover`]). An SSDP `M-SEARCH` over UDP to
//!    `239.255.255.250:1900`, with a TTL of 2, for each of the four targets
//!    `upnpDiscover` searches (`miniupnpc.c:322-333`), a second each, stopping
//!    at the first that anyone answers.
//! 2. **Pick a gateway** ([`find_gateway`], `UPNP_GetValidIGD`,
//!    `miniupnpc.c:521-656`). Every answering device's description is fetched,
//!    and the choice is, in this order: an internet gateway whose WAN
//!    connection reports `Connected` (or `Up`) with a public external address;
//!    one that is connected behind a reserved address (double NAT); one that
//!    is not connected; any device at all. Only the first is mapped through.
//! 3. **Map** the P2P listening port: TCP, the same port outside and in, to the
//!    LAN address that reached the gateway, described `WRKZCoin`, with no lease
//!    limit (`UPNP_AddPortMapping`, `NetNode.cpp:104-113`).
//!
//! The log lines are the C++'s, word for word.
//!
//! # Where this differs from the C++, and why
//!
//! - **Off the start-up path.** The C++ does all of this synchronously at the
//!   end of `NodeServer::init`: up to four seconds of discovery, plus however
//!   long the gateway takes to answer, before the node does anything else.
//!   Here it runs on a thread of its own ([`PortMapper`]).
//! - **It can be turned off, and is skipped where it is pointless.**
//!   `--no-upnp` is this port's; and the daemon does not try with
//!   `--no-listen`, with `--hide-my-port` (no peer is told the port), or for a
//!   loopback listener. A listener bound to one specific address is mapped only
//!   when that is the address that reaches the gateway, since the mapping would
//!   otherwise forward to an address nothing listens on.
//! - **The mapping is removed on a clean shutdown.** The C++ never deletes it.
//!   Before deleting, this asks the gateway what the port maps to now, and a
//!   mapping it reports as another host's, or another port's, is left alone.
//! - **No `minissdpd`.** miniupnpc asks a local `minissdpd` over its socket
//!   before it tries multicast; this goes to multicast directly.
//! - **Bounded.** Every connect and every HTTP exchange has a deadline, and a
//!   description or SOAP answer a size cap; miniupnpc bounds neither the size
//!   nor, for its SOAP calls, the time.
//! - **An HTTP error with no SOAP fault is a failure.** miniupnpc counts any
//!   answer without an `errorCode` in it as success, whatever its status.
//! - **Entities are decoded** (`&amp;` in a control URL); minixml leaves them.
//!
//! The IPv6 listener is not mapped, as in the C++: an IGD port mapping is an
//! IPv4 NAT's.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::{log_debug, log_error, log_info, log_warn};

/// `CryptoNote::CRYPTONOTE_NAME` (`CryptoNoteConfig.h:442`): the description
/// the C++ gives its mapping.
pub const MAPPING_DESCRIPTION: &str = "WRKZCoin";

/// The SSDP multicast group (`UPNP_MCAST_ADDR`, `SSDP_PORT`).
pub const SSDP_MULTICAST: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));

/// What `upnpDiscover` searches for, in its order (`miniupnpc.c:322-333`).
pub const SEARCH_TARGETS: [&str; 4] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
    "upnp:rootdevice",
];

/// The service whose presence makes a device an internet gateway (`is_igd`,
/// `miniupnpc.c:575`).
const COMMON_INTERFACE: &str = "urn:schemas-upnp-org:service:WANCommonInterfaceConfig:";
/// The two services a mapping is asked of (`IGDendelt`, `igd_desc_parse.c:49`).
const WAN_IP_CONNECTION: &str = "urn:schemas-upnp-org:service:WANIPConnection:";
const WAN_PPP_CONNECTION: &str = "urn:schemas-upnp-org:service:WANPPPConnection:";

/// Devices one discovery keeps. A LAN has a handful; a flood of answers is no
/// reason to fetch a hundred descriptions.
const MAX_DEVICES: usize = 16;
/// The largest description or SOAP answer read. A gateway's are a few KiB.
const MAX_HTTP_BYTES: u64 = 256 * 1024;
/// The UPnP error a gateway answers for a port it holds no mapping for.
const NO_SUCH_ENTRY: i32 = 714;

/// How discovery and the gateway's HTTP server are reached. The defaults are
/// the C++'s `upnpDiscover(1000, NULL, NULL, 0, 0, 2, …)`; a test points
/// [`UpnpConfig::ssdp_targets`] at a fake gateway on loopback.
#[derive(Clone, Debug)]
pub struct UpnpConfig {
    /// Where the `M-SEARCH` goes: the multicast group.
    pub ssdp_targets: Vec<SocketAddr>,
    /// How long each search target waits for answers.
    pub search_wait: Duration,
    /// The multicast TTL.
    pub ttl: u32,
    /// Connecting to the gateway's HTTP server.
    pub connect_timeout: Duration,
    /// One HTTP exchange with it, from the request to the last byte.
    pub exchange_timeout: Duration,
    /// The mapping's description.
    pub description: String,
}

impl Default for UpnpConfig {
    fn default() -> Self {
        Self {
            ssdp_targets: vec![SSDP_MULTICAST],
            search_wait: Duration::from_secs(1),
            ttl: 2,
            connect_timeout: Duration::from_secs(2),
            exchange_timeout: Duration::from_secs(4),
            description: MAPPING_DESCRIPTION.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// discovery
// ---------------------------------------------------------------------------

/// One answer to an `M-SEARCH` (`parseMSEARCHReply`, `minissdpc.c:397`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsdpReply {
    /// Where the device description is.
    pub location: String,
    /// The search target it answered.
    pub st: String,
    /// Its unique service name; may be empty.
    pub usn: String,
}

/// The `M-SEARCH` request `ssdpDiscoverDevices` sends (`minissdpc.c:533`).
pub fn m_search(target: &str, mx: u64) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: {target}\r\nMAN: \"ssdp:discover\"\r\n\
         MX: {mx}\r\n\r\n"
    )
}

/// Read an SSDP answer: the `LOCATION`, `ST` and `USN` headers, matched without
/// regard to case. `None` unless it names both a location and a target, which
/// is also how a `NOTIFY` or another client's `M-SEARCH` is passed over.
pub fn parse_ssdp_reply(packet: &[u8]) -> Option<SsdpReply> {
    let text = String::from_utf8_lossy(packet);
    let (mut location, mut st, mut usn) = (None, None, String::new());
    for line in text.split('\n') {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("location") {
            location = Some(value);
        } else if name.eq_ignore_ascii_case("st") {
            st = Some(value);
        } else if name.eq_ignore_ascii_case("usn") {
            usn = value;
        }
    }
    match (location, st) {
        (Some(location), Some(st)) if !location.is_empty() && !st.is_empty() => Some(SsdpReply { location, st, usn }),
        _ => None,
    }
}

/// `upnpDiscover` without the `minissdpd` step: search each target in turn,
/// wait [`UpnpConfig::search_wait`] for answers, and stop after the first
/// target anyone answered. At most 16 distinct answers are kept.
pub fn discover(cfg: &UpnpConfig, stop: &AtomicBool) -> io::Result<Vec<SsdpReply>> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
    // Not fatal: a platform that refuses the option still searches, and a
    // loopback target does not need it.
    let _ = socket.set_multicast_ttl_v4(cfg.ttl);
    // `mx = delay / 1000`, at least 1 (`minissdpc.c:833`).
    let mx = cfg.search_wait.as_secs().max(1);
    let mut found: Vec<SsdpReply> = Vec::new();
    let mut buffer = [0u8; 2048];
    for target in SEARCH_TARGETS {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let request = m_search(target, mx);
        let mut sent = false;
        for to in &cfg.ssdp_targets {
            match socket.send_to(request.as_bytes(), to) {
                Ok(_) => sent = true,
                Err(e) => log_debug!("UPnP: M-SEARCH to {to} failed: {e}"),
            }
        }
        if !sent {
            continue;
        }
        let deadline = Instant::now() + cfg.search_wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(left))?;
            match socket.recv_from(&mut buffer) {
                Ok((n, from)) => match parse_ssdp_reply(&buffer[..n]) {
                    Some(reply) if found.len() < MAX_DEVICES && !found.contains(&reply) => {
                        log_debug!("UPnP: {from} answered {} at {}", reply.st, reply.location);
                        found.push(reply);
                    }
                    _ => {}
                },
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                // Windows reports an ICMP "port unreachable" for an earlier
                // datagram on the next receive.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e),
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------

/// An `http://` URL, as far as a gateway needs one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    /// As written: an IPv6 literal keeps its brackets.
    pub host: String,
    pub port: u16,
    /// Starts with `/`.
    pub path: String,
}

impl HttpUrl {
    /// `http://host[:port][/path]`. Anything else — `https`, no host, a port
    /// that is not one — is `None`.
    pub fn parse(url: &str) -> Option<Self> {
        let scheme = url.get(..7)?;
        if !scheme.eq_ignore_ascii_case("http://") {
            return None;
        }
        let rest = &url[7..];
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']')?;
            let port = match &authority[end + 1..] {
                "" => 80,
                p => p.strip_prefix(':')?.parse().ok()?,
            };
            (&authority[..=end], port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, port.parse().ok()?),
                None => (authority, 80),
            }
        };
        if host.is_empty() || host == "[]" {
            return None;
        }
        Some(Self { host: host.to_string(), port, path })
    }

    /// The `Host` header: the port only when it is not 80, as miniupnpc
    /// writes it for SOAP (`minisoap.c:89`).
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn socket_addrs(&self) -> io::Result<Vec<SocketAddr>> {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        Ok((host, self.port).to_socket_addrs()?.collect())
    }
}

/// `build_absolute_url` (`miniupnpc.c:373`): an `http://` URL is used as it
/// is; anything else is appended to the scheme and authority of `URLBase`, or
/// of the description's own URL when there is no `URLBase`. The base's path
/// is **not** kept — a relative `ctl/IPConn` under `http://gw/desc/root.xml` is
/// `http://gw/ctl/IPConn` — because that is what miniupnpc does and what
/// gateways are tested against.
pub fn absolute_url(url_base: &str, description_url: &str, url: &str) -> String {
    if url.starts_with("http://") {
        return url.to_string();
    }
    let base = if url_base.is_empty() { description_url } else { url_base };
    let authority_end = base.get(7..).and_then(|after| after.find('/')).map_or(base.len(), |i| i + 7);
    let mut out = base[..authority_end].to_string();
    if !url.starts_with('/') {
        out.push('/');
    }
    out.push_str(url);
    out
}

// ---------------------------------------------------------------------------
// XML
// ---------------------------------------------------------------------------

/// What [`xml_events`] reads.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Xml<'a> {
    /// An element opened, by its local name.
    Start(&'a str),
    /// An element closed, by its local name.
    End(&'a str),
    /// Text between tags, trimmed and entity-decoded; never empty.
    Text(String),
}

/// A tolerant reading of the XML a gateway sends — not a validating parser.
/// Elements are known by their local name, the namespace prefix dropped as
/// minixml drops it (`minixml.c:140`); attributes are skipped, quoted `>`
/// included; so are declarations, comments and doctypes; CDATA is text. A
/// document cut short simply ends.
fn xml_events(xml: &str) -> Vec<Xml<'_>> {
    let mut out = Vec::new();
    let mut rest = xml;
    loop {
        let Some(open) = rest.find('<') else {
            push_text(&mut out, rest);
            break;
        };
        push_text(&mut out, &rest[..open]);
        rest = &rest[open..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
        } else if let Some(after) = rest.strip_prefix("<![CDATA[") {
            let end = after.find("]]>").unwrap_or(after.len());
            if !after[..end].is_empty() {
                out.push(Xml::Text(after[..end].to_string()));
            }
            rest = after.get(end + 3..).unwrap_or("");
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
        } else {
            let Some(end) = tag_end(rest) else { break };
            let inner = &rest[1..end];
            rest = &rest[end + 1..];
            if let Some(closing) = inner.strip_prefix('/') {
                out.push(Xml::End(local_name(closing)));
            } else {
                let name = local_name(inner);
                out.push(Xml::Start(name));
                if inner.ends_with('/') {
                    out.push(Xml::End(name));
                }
            }
        }
    }
    out
}

/// The index of the `>` that closes the tag `tag` starts with, skipping any
/// inside a quoted attribute value.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in tag.char_indices().skip(1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i),
            None => {}
        }
    }
    None
}

/// A tag's element name, without attributes, a trailing `/` or a prefix.
fn local_name(tag: &str) -> &str {
    let name = tag.split(|c: char| c.is_ascii_whitespace() || c == '/').next().unwrap_or("");
    name.rsplit(':').next().unwrap_or(name)
}

fn push_text(out: &mut Vec<Xml<'_>>, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        out.push(Xml::Text(decode_entities(text)));
    }
}

/// The five predefined entities and numeric character references. Anything
/// else is left as it was written.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|hex| u32::from_str_radix(hex, 16).ok())
                .unwrap_or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escape a value for an element's text.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// the device description
// ---------------------------------------------------------------------------

/// One `<service>` of a description.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Service {
    pub service_type: String,
    pub control_url: String,
}

/// What `parserootdesc` keeps of a device description
/// (`igd_desc_parse.c`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Description {
    /// `<URLBase>`, empty when there is none.
    pub url_base: String,
    /// `WANCommonInterfaceConfig`: a device with one is an internet gateway.
    pub common_interface: Option<Service>,
    /// The first `WANIPConnection` or `WANPPPConnection`, in document order.
    pub first: Option<Service>,
    /// The last one after it: `IGDendelt` overwrites `second` each time.
    pub second: Option<Service>,
}

impl Description {
    pub fn parse(xml: &str) -> Self {
        let mut description = Description::default();
        let mut open: Option<&str> = None;
        let mut service = Service::default();
        for event in xml_events(xml) {
            match event {
                Xml::Start(name) => {
                    open = Some(name);
                    if name == "service" {
                        service = Service::default();
                    }
                }
                Xml::Text(text) => match open {
                    Some("URLBase") => description.url_base = text,
                    Some("serviceType") => service.service_type = text,
                    Some("controlURL") => service.control_url = text,
                    _ => {}
                },
                Xml::End(name) => {
                    open = None;
                    if name != "service" {
                        continue;
                    }
                    let done = std::mem::take(&mut service);
                    if done.service_type.starts_with(COMMON_INTERFACE) {
                        description.common_interface = Some(done);
                    } else if done.service_type.starts_with(WAN_IP_CONNECTION)
                        || done.service_type.starts_with(WAN_PPP_CONNECTION)
                    {
                        if description.first.is_none() {
                            description.first = Some(done);
                        } else {
                            description.second = Some(done);
                        }
                    }
                }
            }
        }
        description
    }

    /// Whether this device is an internet gateway.
    pub fn is_igd(&self) -> bool {
        self.common_interface.is_some()
    }
}

// ---------------------------------------------------------------------------
// SOAP
// ---------------------------------------------------------------------------

/// Why a gateway did not do what it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpnpError {
    /// It could not be reached, or did not answer in HTTP in time.
    Transport(String),
    /// An HTTP error status, with no SOAP fault to say more.
    Status(u16),
    /// A SOAP fault's `UPnPError`: `718 ConflictInMappingEntry` and the like.
    Fault { code: i32, description: String },
    /// An answer without the value the action returns.
    Incomplete(&'static str),
}

impl std::fmt::Display for UpnpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpnpError::Transport(e) => f.write_str(e),
            UpnpError::Status(status) => write!(f, "HTTP {status}"),
            UpnpError::Fault { code, description } => write!(f, "UPnP error {code} {description}"),
            UpnpError::Incomplete(what) => write!(f, "the answer carries no {what}"),
        }
    }
}

/// The SOAP request `simpleUPnPcommand` builds (`miniupnpc.c:121-199`), byte
/// for byte, with the argument values escaped.
pub fn soap_envelope(service_type: &str, action: &str, args: &[(&str, &str)]) -> String {
    let mut body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service_type}\">"
    );
    for (name, value) in args {
        body.push_str(&format!("<{name}>{}</{name}>", escape(value)));
    }
    body.push_str(&format!("</u:{action}></s:Body></s:Envelope>\r\n"));
    body
}

/// The leaf elements of a SOAP answer, by local name, in document order —
/// what `ParseNameValue` collects (`upnpreplyparse.c`). A fault's `errorCode`
/// and `errorDescription` are leaves like any other.
pub fn soap_values(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut leaf: Option<(&str, String)> = None;
    for event in xml_events(xml) {
        match event {
            Xml::Start(name) => leaf = Some((name, String::new())),
            Xml::Text(text) => {
                if let Some((_, value)) = leaf.as_mut() {
                    value.push_str(&text);
                }
            }
            Xml::End(_) => {
                if let Some((name, value)) = leaf.take() {
                    out.push((name.to_string(), value));
                }
            }
        }
    }
    out
}

/// The last value of `name`, which is the one `GetValueFromNameValueList`
/// finds first in its prepended list.
fn value_of<'a>(values: &'a [(String, String)], name: &str) -> Option<&'a str> {
    values.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// A socket whose reads and writes all end by one deadline.
struct Deadline {
    stream: TcpStream,
    until: Instant,
}

impl Deadline {
    fn left(&self) -> io::Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the gateway did not answer in time"));
        }
        Ok(left)
    }
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.left()?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

impl Write for Deadline {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.left()?;
        self.stream.set_write_timeout(Some(left))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// One HTTP answer, and this host's address on the connection it came over.
struct Answer {
    status: u16,
    body: Vec<u8>,
    local: SocketAddr,
}

fn http(
    url: &HttpUrl,
    method: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    cfg: &UpnpConfig,
) -> Result<Answer, UpnpError> {
    let addrs = url.socket_addrs().map_err(|e| UpnpError::Transport(format!("{}: {e}", url.host)))?;
    let mut failure = format!("{} resolves to no address", url.host);
    for addr in addrs {
        let stream = match TcpStream::connect_timeout(&addr, cfg.connect_timeout) {
            Ok(stream) => stream,
            Err(e) => {
                failure = format!("connect {addr}: {e}");
                continue;
            }
        };
        let local = stream.local_addr().map_err(|e| UpnpError::Transport(e.to_string()))?;
        let stream = Deadline { stream, until: Instant::now() + cfg.exchange_timeout };
        let mut all: Vec<(&str, &str)> = vec![("User-Agent", "wrkz-node UPnP/1.1")];
        all.extend_from_slice(headers);
        let host = url.host_header();
        let (status, body) =
            wrkz_rpc::http::client::exchange(stream, method, &url.path, &host, &all, body, MAX_HTTP_BYTES)
                .map_err(UpnpError::Transport)?;
        return Ok(Answer { status, body, local });
    }
    Err(UpnpError::Transport(failure))
}

/// One SOAP action on a gateway's WAN connection service.
fn soap(
    gateway: &Gateway,
    action: &str,
    args: &[(&str, &str)],
    cfg: &UpnpConfig,
) -> Result<Vec<(String, String)>, UpnpError> {
    let url = HttpUrl::parse(&gateway.control_url)
        .ok_or_else(|| UpnpError::Transport(format!("not an http:// control URL: {}", gateway.control_url)))?;
    let envelope = soap_envelope(&gateway.service_type, action, args);
    let soap_action = format!("\"{}#{action}\"", gateway.service_type);
    let headers = [("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPAction", soap_action.as_str())];
    let answer = http(&url, "POST", &headers, Some(envelope.as_bytes()), cfg)?;
    let values = soap_values(&String::from_utf8_lossy(&answer.body));
    if let Some(code) = value_of(&values, "errorCode") {
        let description = value_of(&values, "errorDescription").unwrap_or_default().to_string();
        return Err(UpnpError::Fault { code: code.trim().parse().unwrap_or(-1), description });
    }
    if answer.status != 200 {
        return Err(UpnpError::Status(answer.status));
    }
    Ok(values)
}

// ---------------------------------------------------------------------------
// the gateway
// ---------------------------------------------------------------------------

/// `UPNP_GetValidIGD`'s results (`miniupnpc.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgdStatus {
    /// `UPNP_NO_IGD`: no device description could be read.
    NoIgd,
    /// `UPNP_CONNECTED_IGD`: connected, with a public external address.
    Connected,
    /// `UPNP_PRIVATEIP_IGD`: connected, behind a reserved address.
    PrivateIp,
    /// `UPNP_DISCONNECTED_IGD`: a gateway that is not connected.
    Disconnected,
    /// `UPNP_UNKNOWN_DEVICE`: a device that is no gateway.
    UnknownDevice,
}

/// A gateway's WAN connection service, and how this host reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gateway {
    /// Where its description was read.
    pub description_url: String,
    pub control_url: String,
    pub service_type: String,
    /// This host's address on the connection that read the description:
    /// miniupnpc's `lanaddr`, and the internal client of a mapping.
    pub lan_addr: IpAddr,
    /// What `GetExternalIPAddress` answered, when it was asked.
    pub external_addr: Option<String>,
}

/// `addr_is_reserved` (`addr_is_reserved.c`): whether an external address is
/// one no peer could reach — private, shared, loopback, link-local,
/// documentation, multicast or reserved. Anything that is not an IPv4
/// address counts as reserved.
pub fn addr_is_reserved(addr: &str) -> bool {
    const fn v4(a: u32, b: u32, c: u32, d: u32) -> u32 {
        (a << 24) | (b << 16) | (c << 8) | d
    }
    const RESERVED: [(u32, u32); 18] = [
        (v4(0, 0, 0, 0), 8),
        (v4(10, 0, 0, 0), 8),
        (v4(100, 64, 0, 0), 10),
        (v4(127, 0, 0, 0), 8),
        (v4(169, 254, 0, 0), 16),
        (v4(172, 16, 0, 0), 12),
        (v4(192, 0, 0, 0), 24),
        (v4(192, 0, 2, 0), 24),
        (v4(192, 31, 196, 0), 24),
        (v4(192, 52, 193, 0), 24),
        (v4(192, 88, 99, 0), 24),
        (v4(192, 168, 0, 0), 16),
        (v4(192, 175, 48, 0), 24),
        (v4(198, 18, 0, 0), 15),
        (v4(198, 51, 100, 0), 24),
        (v4(203, 0, 113, 0), 24),
        (v4(224, 0, 0, 0), 4),
        (v4(240, 0, 0, 0), 4),
    ];
    let Ok(ip) = addr.parse::<Ipv4Addr>() else { return true };
    let ip = u32::from(ip);
    RESERVED.iter().any(|&(net, bits)| ip >> (32 - bits) == net >> (32 - bits))
}

/// `UPNP_GetStatusInfo` then the check `UPNPIGD_IsConnected` makes
/// (`miniupnpc.c:489`): `Connected`, or `Up`.
fn is_connected(gateway: &Gateway, cfg: &UpnpConfig) -> bool {
    match soap(gateway, "GetStatusInfo", &[], cfg) {
        Ok(values) => matches!(value_of(&values, "NewConnectionStatus"), Some("Connected" | "Up")),
        Err(e) => {
            log_debug!("UPnP: GetStatusInfo at {}: {e}", gateway.control_url);
            false
        }
    }
}

/// `UPNP_GetExternalIPAddress`.
pub fn external_ip_address(gateway: &Gateway, cfg: &UpnpConfig) -> Result<String, UpnpError> {
    let values = soap(gateway, "GetExternalIPAddress", &[], cfg)?;
    value_of(&values, "NewExternalIPAddress").map(str::to_string).ok_or(UpnpError::Incomplete("NewExternalIPAddress"))
}

/// `UPNP_GetValidIGD` (`miniupnpc.c:521-656`): read every device's
/// description, then take the first, in device order and `first` before
/// `second` service, that is a connected gateway with a public address; else
/// the first connected one; else the first gateway; else the first device.
pub fn find_gateway(devices: &[SsdpReply], cfg: &UpnpConfig, stop: &AtomicBool) -> (IgdStatus, Option<Gateway>) {
    struct Read {
        url: String,
        lan: IpAddr,
        description: Description,
    }
    let mut read: Vec<Read> = Vec::new();
    for device in devices {
        if stop.load(Ordering::Relaxed) {
            return (IgdStatus::NoIgd, None);
        }
        if read.iter().any(|r| r.url == device.location) {
            continue;
        }
        let Some(url) = HttpUrl::parse(&device.location) else {
            log_debug!("UPnP: {} is not an http:// location", device.location);
            continue;
        };
        match http(&url, "GET", &[], None, cfg) {
            Ok(answer) if answer.status == 200 => read.push(Read {
                url: device.location.clone(),
                lan: answer.local.ip(),
                description: Description::parse(&String::from_utf8_lossy(&answer.body)),
            }),
            Ok(answer) => log_debug!("UPnP: {} answered HTTP {}", device.location, answer.status),
            Err(e) => log_debug!("UPnP: could not read {}: {e}", device.location),
        }
    }
    let gateway = |r: &Read, service: &Service| Gateway {
        description_url: r.url.clone(),
        control_url: absolute_url(&r.description.url_base, &r.url, &service.control_url),
        service_type: service.service_type.clone(),
        lan_addr: r.lan,
        external_addr: None,
    };

    let mut behind_nat: Option<Gateway> = None;
    for r in read.iter().filter(|r| r.description.is_igd()) {
        for service in [&r.description.first, &r.description.second].into_iter().flatten() {
            if stop.load(Ordering::Relaxed) {
                return (IgdStatus::NoIgd, None);
            }
            let candidate = gateway(r, service);
            if !is_connected(&candidate, cfg) {
                continue;
            }
            match external_ip_address(&candidate, cfg) {
                Ok(ip) if !addr_is_reserved(&ip) => {
                    return (IgdStatus::Connected, Some(Gateway { external_addr: Some(ip), ..candidate }));
                }
                Ok(ip) => {
                    behind_nat.get_or_insert(Gateway { external_addr: Some(ip), ..candidate });
                }
                Err(e) => {
                    log_debug!("UPnP: GetExternalIPAddress at {}: {e}", candidate.control_url);
                    behind_nat.get_or_insert(candidate);
                }
            }
        }
    }
    if let Some(found) = behind_nat {
        return (IgdStatus::PrivateIp, Some(found));
    }
    let first_service = |r: &Read| gateway(r, &r.description.first.clone().unwrap_or_default());
    if let Some(r) = read.iter().find(|r| r.description.is_igd()) {
        return (IgdStatus::Disconnected, Some(first_service(r)));
    }
    match read.first() {
        Some(r) => (IgdStatus::UnknownDevice, Some(first_service(r))),
        None => (IgdStatus::NoIgd, None),
    }
}

/// `UPNP_AddPortMapping(controlURL, servicetype, port, port, lanaddr,
/// description, "TCP", NULL, "0")` (`NetNode.cpp:104-113`).
pub fn add_port_mapping(gateway: &Gateway, port: u16, description: &str, cfg: &UpnpConfig) -> Result<(), UpnpError> {
    let port = port.to_string();
    let client = gateway.lan_addr.to_string();
    let args = [
        ("NewRemoteHost", ""),
        ("NewExternalPort", port.as_str()),
        ("NewProtocol", "TCP"),
        ("NewInternalPort", port.as_str()),
        ("NewInternalClient", client.as_str()),
        ("NewEnabled", "1"),
        ("NewPortMappingDescription", description),
        ("NewLeaseDuration", "0"),
    ];
    soap(gateway, "AddPortMapping", &args, cfg).map(|_| ())
}

/// `UPNP_DeletePortMapping(controlURL, servicetype, port, "TCP", NULL)`.
pub fn delete_port_mapping(gateway: &Gateway, port: u16, cfg: &UpnpConfig) -> Result<(), UpnpError> {
    let port = port.to_string();
    let args = [("NewRemoteHost", ""), ("NewExternalPort", port.as_str()), ("NewProtocol", "TCP")];
    soap(gateway, "DeletePortMapping", &args, cfg).map(|_| ())
}

/// What a gateway says a TCP port maps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortMappingEntry {
    pub internal_client: String,
    pub internal_port: String,
    pub description: String,
}

/// `UPNP_GetSpecificPortMappingEntry(controlURL, servicetype, port, "TCP",
/// NULL, …)`.
pub fn port_mapping_entry(gateway: &Gateway, port: u16, cfg: &UpnpConfig) -> Result<PortMappingEntry, UpnpError> {
    let port = port.to_string();
    let args = [("NewRemoteHost", ""), ("NewExternalPort", port.as_str()), ("NewProtocol", "TCP")];
    let values = soap(gateway, "GetSpecificPortMappingEntry", &args, cfg)?;
    let internal_client = value_of(&values, "NewInternalClient").ok_or(UpnpError::Incomplete("NewInternalClient"))?;
    Ok(PortMappingEntry {
        internal_client: internal_client.trim().to_string(),
        internal_port: value_of(&values, "NewInternalPort").unwrap_or_default().trim().to_string(),
        description: value_of(&values, "NewPortMappingDescription").unwrap_or_default().to_string(),
    })
}

/// Delete a mapping this node made, after asking the gateway what the port
/// maps to now: one it reports as another host's or another port's is left
/// alone.
pub fn remove_port_mapping(gateway: &Gateway, port: u16, cfg: &UpnpConfig) {
    let ours = |entry: &PortMappingEntry| {
        entry.internal_client == gateway.lan_addr.to_string() && entry.internal_port == port.to_string()
    };
    match port_mapping_entry(gateway, port, cfg) {
        Ok(entry) if ours(&entry) => match delete_port_mapping(gateway, port, cfg) {
            Ok(()) => log_info!("Removed IGD port mapping."),
            Err(e) => log_warn!("UPnP: could not remove the mapping of TCP port {port}: {e}"),
        },
        Ok(entry) => log_info!(
            "UPnP: TCP port {port} now maps to {}:{}, not to this node; the mapping is left in place",
            entry.internal_client,
            entry.internal_port
        ),
        Err(UpnpError::Fault { code: NO_SUCH_ENTRY, .. }) => {
            log_debug!("UPnP: the mapping of TCP port {port} is already gone")
        }
        Err(e) => log_warn!("UPnP: could not read back the mapping of TCP port {port} ({e}); it is left in place"),
    }
}

// ---------------------------------------------------------------------------
// the daemon's mapper
// ---------------------------------------------------------------------------

/// The port mapping the daemon asks for, worked out on a thread of its own.
///
/// [`PortMapper::shutdown`] takes the mapping down again. It gives an attempt
/// still under way a moment to finish; one that finishes later anyway sees the
/// shutdown and removes its own mapping, so none is left behind by a race.
pub struct PortMapper {
    shared: Arc<Shared>,
}

struct Shared {
    cfg: UpnpConfig,
    stop: AtomicBool,
    state: Mutex<MapperState>,
    finished: Condvar,
}

#[derive(Default)]
struct MapperState {
    done: bool,
    mapped: Option<(Gateway, u16)>,
}

impl PortMapper {
    /// Start mapping TCP `port`, which the P2P listener bound on `bind`.
    pub fn spawn(port: u16, bind: IpAddr, cfg: UpnpConfig) -> Self {
        let shared = Arc::new(Shared {
            cfg,
            stop: AtomicBool::new(false),
            state: Mutex::new(MapperState::default()),
            finished: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new().name("wrkz-upnp".into()).spawn(move || {
            worker.map(port, bind);
            worker.finish();
        });
        if let Err(e) = spawned {
            log_warn!("UPnP: could not start the port mapping thread: {e}");
            shared.finish();
        }
        Self { shared }
    }

    /// Wait up to `timeout` for the attempt to end. True when it has.
    pub fn wait(&self, timeout: Duration) -> bool {
        let state = self.shared.lock();
        let (state, _) =
            self.shared.finished.wait_timeout_while(state, timeout, |s| !s.done).unwrap_or_else(|p| p.into_inner());
        state.done
    }

    /// The gateway and port, once a mapping has been made.
    pub fn mapping(&self) -> Option<(Gateway, u16)> {
        self.shared.lock().mapped.clone()
    }

    /// Stop, and remove the mapping if one was made, waiting at most `wait`
    /// for an attempt still under way.
    pub fn shutdown(self, wait: Duration) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.wait(wait);
        let mapped = self.shared.lock().mapped.take();
        if let Some((gateway, port)) = mapped {
            remove_port_mapping(&gateway, port, &self.shared.cfg);
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, MapperState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn finish(&self) {
        self.lock().done = true;
        self.finished.notify_all();
    }

    /// `addPortMapping`, with the C++'s log lines.
    fn map(&self, port: u16, bind: IpAddr) {
        log_info!("Attempting to add IGD port mapping.");
        let devices = discover(&self.cfg, &self.stop).unwrap_or_else(|e| {
            log_debug!("UPnP: discovery failed: {e}");
            Vec::new()
        });
        if self.stopping() {
            return;
        }
        match find_gateway(&devices, &self.cfg, &self.stop) {
            (IgdStatus::Connected, Some(gateway)) => self.add(gateway, port, bind),
            (IgdStatus::PrivateIp, _) => log_info!("IGD was found but its external address is reserved (double NAT)."),
            (IgdStatus::Disconnected, _) => log_info!("IGD was found but reported as not connected."),
            (IgdStatus::UnknownDevice, _) => log_info!("UPnP device was found but not recognized as IGD."),
            _ => log_info!("No IGD was found."),
        }
    }

    fn add(&self, gateway: Gateway, port: u16, bind: IpAddr) {
        if !bind.is_unspecified() && bind != gateway.lan_addr {
            log_info!(
                "UPnP: not mapping TCP port {port}: the P2P listener is bound to {bind}, but {} is the address that \
                 reaches the gateway",
                gateway.lan_addr
            );
            return;
        }
        if self.stopping() {
            return;
        }
        match add_port_mapping(&gateway, port, &self.cfg.description, &self.cfg) {
            Ok(()) => {
                log_info!("Added IGD port mapping.");
                log_debug!(
                    "UPnP: TCP port {port} on {} maps to {}:{port}",
                    gateway.external_addr.as_deref().unwrap_or("the gateway"),
                    gateway.lan_addr
                );
                let mut state = self.lock();
                if self.stopping() {
                    drop(state);
                    remove_port_mapping(&gateway, port, &self.cfg);
                } else {
                    state.mapped = Some((gateway, port));
                }
            }
            Err(e) => {
                log_error!("UPNP_AddPortMapping failed.");
                log_info!("UPnP: {} would not map TCP port {port} to {}: {e}", gateway.control_url, gateway.lan_addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssdp_answer_is_read_as_miniupnpc_reads_it() {
        let packet = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\n\
                       st: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
                       USN: uuid:1234::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
                       Location:   http://192.168.1.1:5000/rootDesc.xml\r\nSERVER: x\r\n\r\n";
        let reply = parse_ssdp_reply(packet).expect("an answer");
        assert_eq!(reply.location, "http://192.168.1.1:5000/rootDesc.xml");
        assert_eq!(reply.st, "urn:schemas-upnp-org:device:InternetGatewayDevice:1");
        assert!(reply.usn.starts_with("uuid:1234"));

        let no_usn =
            parse_ssdp_reply(b"HTTP/1.1 200 OK\nST: upnp:rootdevice\nLOCATION: http://10.0.0.1/d.xml\n").unwrap();
        assert_eq!((no_usn.usn.as_str(), no_usn.location.as_str()), ("", "http://10.0.0.1/d.xml"));
        // Another client's search names a target and no location.
        assert!(parse_ssdp_reply(&m_search(SEARCH_TARGETS[0], 1).into_bytes()).is_none());
        assert!(parse_ssdp_reply(b"NOTIFY * HTTP/1.1\r\nNT: upnp:rootdevice\r\nLOCATION: http://x/\r\n").is_none());
        assert!(parse_ssdp_reply(b"HTTP/1.1 200 OK\r\nST: x\r\nLOCATION:\r\n").is_none(), "an empty location");
    }

    #[test]
    fn the_search_request_is_the_one_miniupnpc_sends() {
        assert_eq!(
            m_search("upnp:rootdevice", 1),
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: upnp:rootdevice\r\n\
             MAN: \"ssdp:discover\"\r\nMX: 1\r\n\r\n"
        );
    }

    /// A description with a default namespace, a prefixed one, a comment,
    /// nested devices, `URLBase`, two WAN connection services and one more
    /// service that is neither.
    #[test]
    fn a_description_keeps_what_parserootdesc_keeps() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- a gateway -->
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:d="urn:schemas-upnp-org:device-1-0">
 <specVersion><major>1</major><minor>0</minor></specVersion>
 <URLBase>http://192.168.1.1:49152/</URLBase>
 <device>
  <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
  <deviceList><device>
   <d:serviceList>
    <d:service>
     <d:serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</d:serviceType>
     <d:controlURL>/upnp/control/WANCommonIFC1</d:controlURL>
    </d:service>
   </d:serviceList>
   <deviceList><device><serviceList>
    <service>
     <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
     <controlURL>ctl/PPP?a=1&amp;b=2</controlURL>
    </service>
    <service>
     <serviceType>urn:schemas-upnp-org:service:WANIPv6FirewallControl:1</serviceType>
     <controlURL>/ctl/6FC</controlURL>
    </service>
    <service>
     <serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType>
     <controlURL>/ctl/IPConn</controlURL>
     <presentation note="a > in an attribute"/>
    </service>
   </serviceList></device></deviceList>
  </device></deviceList>
 </device>
</root>"#;
        let d = Description::parse(xml);
        assert!(d.is_igd());
        assert_eq!(d.url_base, "http://192.168.1.1:49152/");
        let first = d.first.clone().unwrap();
        assert_eq!(first.service_type, "urn:schemas-upnp-org:service:WANPPPConnection:1");
        assert_eq!(first.control_url, "ctl/PPP?a=1&b=2", "entities are decoded");
        assert_eq!(d.second.unwrap().control_url, "/ctl/IPConn");
        assert_eq!(
            absolute_url(&d.url_base, "http://192.168.1.1:49152/rootDesc.xml", &first.control_url),
            "http://192.168.1.1:49152/ctl/PPP?a=1&b=2"
        );

        let bare = Description::parse("<root><device><serviceList></serviceList></device></root>");
        assert!(!bare.is_igd() && bare.first.is_none());
        assert_eq!(
            Description::parse("<root><URLBase>http://x/</URL"),
            Description { url_base: "http://x/".into(), ..Default::default() }
        );
    }

    #[test]
    fn control_urls_resolve_as_build_absolute_url_resolves_them() {
        let desc = "http://192.168.1.1:5000/rootDesc.xml";
        assert_eq!(absolute_url("", desc, "/ctl/IPConn"), "http://192.168.1.1:5000/ctl/IPConn");
        assert_eq!(absolute_url("", desc, "ctl/IPConn"), "http://192.168.1.1:5000/ctl/IPConn");
        assert_eq!(
            absolute_url("http://10.0.0.1:80/base/", desc, "ctl"),
            "http://10.0.0.1:80/ctl",
            "the base's path is dropped"
        );
        assert_eq!(absolute_url("http://10.0.0.1", desc, "/x"), "http://10.0.0.1/x");
        assert_eq!(absolute_url("", desc, "http://10.9.9.9:1/c"), "http://10.9.9.9:1/c");
    }

    #[test]
    fn http_urls_parse_with_and_without_a_port() {
        let url = HttpUrl::parse("http://192.168.1.1:5000/ctl/IPConn").unwrap();
        assert_eq!((url.host.as_str(), url.port, url.path.as_str()), ("192.168.1.1", 5000, "/ctl/IPConn"));
        assert_eq!(url.host_header(), "192.168.1.1:5000");
        let plain = HttpUrl::parse("HTTP://gw.lan").unwrap();
        assert_eq!((plain.port, plain.path.as_str(), plain.host_header().as_str()), (80, "/", "gw.lan"));
        let v6 = HttpUrl::parse("http://[fe80::1]:2869/desc").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("[fe80::1]", 2869));
        for bad in ["https://gw/", "http://", "http://gw:port/", "http://[::1/", "ftp://gw/", "http"] {
            assert!(HttpUrl::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_soap_request_is_the_one_miniupnpc_builds() {
        let service = "urn:schemas-upnp-org:service:WANIPConnection:1";
        assert_eq!(
            soap_envelope(service, "GetExternalIPAddress", &[]),
            "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
             <u:GetExternalIPAddress xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">\
             </u:GetExternalIPAddress></s:Body></s:Envelope>\r\n"
        );
        let body = soap_envelope(
            service,
            "DeletePortMapping",
            &[("NewRemoteHost", ""), ("NewExternalPort", "17855"), ("NewProtocol", "TCP")],
        );
        let expected = "<u:DeletePortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">\
                        <NewRemoteHost></NewRemoteHost><NewExternalPort>17855</NewExternalPort>\
                        <NewProtocol>TCP</NewProtocol></u:DeletePortMapping>";
        assert!(body.contains(expected), "{body}");
        assert!(soap_envelope(service, "X", &[("NewPortMappingDescription", "a<b&c")]).contains(">a&lt;b&amp;c<"));
    }

    #[test]
    fn soap_answers_and_faults_are_read_by_local_name() {
        let ok = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <u:GetStatusInfoResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
            <NewConnectionStatus>Connected</NewConnectionStatus>
            <NewLastConnectionError>ERROR_NONE</NewLastConnectionError>
            <NewUptime>42</NewUptime></u:GetStatusInfoResponse></s:Body></s:Envelope>"#;
        let values = soap_values(ok);
        assert_eq!(value_of(&values, "NewConnectionStatus"), Some("Connected"));
        assert_eq!(value_of(&values, "NewUptime"), Some("42"));
        assert_eq!(value_of(&values, "GetStatusInfoResponse"), None, "only leaves are values");

        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault>
            <faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>
            <UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>718</errorCode>
            <errorDescription>ConflictInMappingEntry</errorDescription></UPnPError>
            </detail></s:Fault></s:Body></s:Envelope>"#;
        let values = soap_values(fault);
        assert_eq!(value_of(&values, "errorCode"), Some("718"));
        assert_eq!(value_of(&values, "errorDescription"), Some("ConflictInMappingEntry"));
        let empty = soap_values("<a><NewRemoteHost></NewRemoteHost><NewRemoteHost/></a>");
        assert_eq!(empty, [("NewRemoteHost".to_string(), String::new()), ("NewRemoteHost".to_string(), String::new())]);
    }

    #[test]
    fn entities_decode_and_junk_is_left_alone() {
        assert_eq!(decode_entities("a&amp;b&lt;&gt;&quot;&apos;&#65;&#x42;"), "a&b<>\"'AB");
        assert_eq!(decode_entities("fish & chips &unknown; &#xZZ; &"), "fish & chips &unknown; &#xZZ; &");
    }

    #[test]
    fn reserved_addresses_are_miniupnpcs() {
        for reserved in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.168.1.1",
            "192.175.48.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "",
            "not an address",
            "::1",
        ] {
            assert!(addr_is_reserved(reserved), "{reserved}");
        }
        for public in ["1.1.1.1", "93.184.216.34", "100.128.0.1", "172.32.0.1", "198.20.0.1", "223.255.255.255"] {
            assert!(!addr_is_reserved(public), "{public}");
        }
    }
}
