// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The RPC over a local socket: `common/IpcSocket` and the IPC half of the C++
//! `RpcServer` (`--rpc-ipc-path`, `--rpc-ipc-mode`, `--rpc-ipc-group`,
//! `--rpc-ipc-require-token`).
//!
//! An IPC endpoint is an `AF_UNIX` stream socket serving exactly the routes the
//! TCP listener serves, off the same workers. Who may use it is decided by the
//! mode on the socket file, which the kernel enforces — owner only (`0600`)
//! unless the operator widens it. So, as in the C++:
//!
//! - an IPC caller is not asked for `--rpc-access-token` unless
//!   `--rpc-ipc-require-token` is set, and is never rate limited: it is neither
//!   anonymous nor remote;
//! - `@name` binds in Linux's abstract namespace, which has no file and so no
//!   permissions at all — every process in the network namespace can connect;
//! - Windows is not supported, for the C++'s reason: `AF_UNIX` exists there,
//!   but with no dependable permission enforcement on the socket file a socket
//!   would be reachable by every process on the machine.
//!
//! The wallets reach it with an IPC daemon address (`/path`, `ipc:///path` or
//! `@name`; `wrkz_wallet::ipc`), and so does `wrkz-node attach <socket>`.
//!
//! One route is served here and on no TCP listener: `POST /console`
//! ([`crate::console`]), which runs a console command inside the daemon —
//! `stop` included — as `Wrkzd attach` does. The socket file's permissions are
//! all that stand between a local user and it.

/// The peer name an IPC connection is dispatched with. Not an IP address, so
/// it never meets the rate limiter or `X-Forwarded-For`.
pub const IPC_PEER: &str = "ipc";

/// `Common::Ipc::DEFAULT_MODE`: owner only. With no token in front of the
/// socket this mode is the whole security model.
pub const DEFAULT_MODE: u32 = 0o600;

/// `Common::Ipc::unsupportedReason()` on a platform without it.
pub const UNSUPPORTED: &str = "local IPC sockets are not available on this platform";

/// Whether this build can serve an IPC socket at all.
pub fn supported() -> bool {
    cfg!(unix)
}

/// `Common::Ipc::isAbstract`: `@name`, Linux's abstract namespace.
pub fn is_abstract(path: &str) -> bool {
    path.starts_with('@')
}

/// `Common::Ipc::parseMode`: `600`, `0600` or `0o600`. Anything that is not a
/// whole octal permission triple is refused.
pub fn parse_mode(text: &str) -> Option<u32> {
    let t = text.trim();
    let digits = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")).unwrap_or(t);
    let digits = if digits.len() == 4 { digits.strip_prefix('0')? } else { digits };
    if digits.len() != 3 || !digits.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(digits, 8).ok()
}

/// `Common::Ipc::formatMode`: `0600`.
pub fn format_mode(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

/// `Common::Ipc::describe`, for logs.
pub fn describe(path: &str) -> String {
    if is_abstract(path) {
        format!("abstract socket {path}")
    } else {
        format!("socket {path}")
    }
}

#[cfg(unix)]
pub use unix::{bind, cleanup, connect};

#[cfg(unix)]
mod unix {
    use std::ffi::CString;
    use std::io::{Error, ErrorKind, Result};
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    fn invalid(message: String) -> Error {
        Error::new(ErrorKind::InvalidInput, message)
    }

    /// `Common::Ipc::bindServer`: a listener at `path` with `mode` and `group`
    /// in force from the first instant.
    ///
    /// The socket is created owner-only — the process umask is narrowed to
    /// `0177` around the `bind`, as the C++ narrows it — then given its group,
    /// and only then widened to `mode`. At no point is it more open than asked,
    /// or open to the daemon user's primary group when another group was named.
    /// The umask is process-wide, which is why [`crate::server::start`] binds
    /// here before any of its own threads exist.
    pub fn bind(path: &str, mode: u32, group: &str) -> Result<UnixListener> {
        if mode & !0o777 != 0 {
            return Err(invalid(format!("mode {} is not a permission triple", super::format_mode(mode))));
        }
        if let Some(name) = path.strip_prefix('@') {
            return bind_abstract(name);
        }
        if !path.starts_with('/') {
            return Err(invalid(format!("{path}: an IPC socket path must be absolute, or @name")));
        }
        remove_stale(path)?;
        let gid = if group.is_empty() { None } else { Some(group_id(group)?) };

        // SAFETY: `umask` only swaps the process's file-creation mask and
        // cannot fail; the old mask is put back straight after the bind.
        let old = unsafe { libc::umask(0o177 as libc::mode_t) };
        let bound = UnixListener::bind(path);
        // SAFETY: as above.
        unsafe { libc::umask(old) };
        let listener = bound?;

        let finish = || -> Result<()> {
            if let Some(gid) = gid {
                std::os::unix::fs::chown(path, None, Some(gid))?;
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        };
        if let Err(e) = finish() {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        Ok(listener)
    }

    #[cfg(target_os = "linux")]
    fn bind_abstract(name: &str) -> Result<UnixListener> {
        use std::os::linux::net::SocketAddrExt;
        if name.is_empty() {
            return Err(invalid("an abstract socket needs a name after the @".to_string()));
        }
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
        UnixListener::bind_addr(&addr)
    }

    #[cfg(not(target_os = "linux"))]
    fn bind_abstract(_name: &str) -> Result<UnixListener> {
        Err(Error::new(ErrorKind::Unsupported, "the abstract socket namespace (@name) is a Linux extension"))
    }

    /// `Common::Ipc::removeStaleSocket`: clear a socket file a previous run
    /// left behind. Anything that is not a socket is refused, and so is a
    /// socket another process is still listening on.
    fn remove_stale(path: &str) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(meta) if !meta.file_type().is_socket() => Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("{path} exists and is not a socket; refusing to replace it"),
            )),
            Ok(_) => match UnixStream::connect(path) {
                Ok(_) => Err(Error::new(ErrorKind::AddrInUse, format!("another process is listening on {path}"))),
                Err(_) => std::fs::remove_file(path),
            },
        }
    }

    fn group_id(name: &str) -> Result<libc::gid_t> {
        let c = CString::new(name).map_err(|_| invalid(format!("group name {name:?} holds a NUL byte")))?;
        // SAFETY: `getgrnam` returns null or a pointer into static storage,
        // which is read once, straight away, before anything else can call it
        // on this thread; the daemon resolves the group once, at start-up.
        let entry = unsafe { libc::getgrnam(c.as_ptr()) };
        if entry.is_null() {
            return Err(Error::new(ErrorKind::NotFound, format!("no group named {name}")));
        }
        // SAFETY: non-null, and valid until the next `getgr*` call.
        Ok(unsafe { (*entry).gr_gid })
    }

    /// A connection to the socket, used to wake the acceptor at shutdown.
    pub fn connect(path: &str) -> Result<UnixStream> {
        match path.strip_prefix('@') {
            Some(name) => connect_abstract(name),
            None => UnixStream::connect(path),
        }
    }

    #[cfg(target_os = "linux")]
    fn connect_abstract(name: &str) -> Result<UnixStream> {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
        UnixStream::connect_addr(&addr)
    }

    #[cfg(not(target_os = "linux"))]
    fn connect_abstract(_name: &str) -> Result<UnixStream> {
        Err(Error::new(ErrorKind::Unsupported, "the abstract socket namespace (@name) is a Linux extension"))
    }

    /// `Common::Ipc::cleanup`: best-effort removal at shutdown. An abstract
    /// socket has no file.
    pub fn cleanup(path: &str) {
        if !super::is_abstract(path) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_as_the_cpp_reads_them() {
        for text in ["600", "0600", "0o600", " 0600 "] {
            assert_eq!(parse_mode(text), Some(0o600), "{text:?}");
        }
        assert_eq!(parse_mode("660"), Some(0o660));
        for bad in ["", "6", "60", "0999", "7777", "06000", "rw-", "0x600"] {
            assert_eq!(parse_mode(bad), None, "{bad:?}");
        }
        assert_eq!(format_mode(0o600), "0600");
        assert_eq!(format_mode(0o660), "0660");
    }

    #[test]
    fn an_abstract_name_is_described_as_one() {
        assert!(is_abstract("@wrkzd"));
        assert!(!is_abstract("/run/wrkzd.sock"));
        assert_eq!(describe("@wrkzd"), "abstract socket @wrkzd");
        assert_eq!(describe("/run/wrkzd.sock"), "socket /run/wrkzd.sock");
    }
}
