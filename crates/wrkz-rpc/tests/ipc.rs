// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The RPC over a local socket (`--rpc-ipc-path`): the same routes as TCP, the
//! socket file's mode as the gate, and a file that is cleared before and
//! removed after — never another process's.
#![cfg(unix)]

mod fake;

use fake::FakeNode;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wrkz_rpc::server::{self, RunningServer, ServerConfig};

/// A fresh socket path of this test's own.
fn socket_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wrkz-ipc-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.sock");
    let _ = std::fs::remove_file(&path);
    path
}

fn start(config: ServerConfig) -> RunningServer {
    server::start(Arc::new(FakeNode::default()), ServerConfig { bind: "127.0.0.1:0".into(), ..config })
        .expect("the TCP listener binds")
}

fn on(path: &Path) -> ServerConfig {
    ServerConfig { ipc_path: path.display().to_string(), ..Default::default() }
}

fn get(path: &Path, target: &str, headers: &str) -> String {
    let mut s = UnixStream::connect(path).expect("connects to the socket");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n{headers}Connection: close\r\n\r\n").as_bytes())
        .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn status_of(response: &str) -> u16 {
    response.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[test]
fn the_rpc_answers_on_the_socket_which_is_owner_only_and_removed_on_stop() {
    let path = socket_path("serve");
    let server = start(on(&path));
    assert_eq!(server.ipc_path(), Some(path.to_str().unwrap()));
    assert!(server.ipc_error().is_none());
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "owner only by default");
    let response = get(&path, "/height", "");
    assert_eq!(status_of(&response), 200, "{response}");
    drop(server);
    assert!(!path.exists(), "the socket file is removed on stop");
}

#[test]
fn a_wider_mode_is_applied_as_asked() {
    let path = socket_path("mode");
    let server = start(ServerConfig { ipc_mode: 0o660, ..on(&path) });
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o660);
    drop(server);
}

#[test]
fn a_socket_caller_needs_the_token_only_when_asked_to() {
    let path = socket_path("token");
    let server = start(ServerConfig { access_token: "s3cret".into(), ..on(&path) });
    assert_eq!(status_of(&get(&path, "/height", "")), 200, "the socket's permissions already decided");
    drop(server);

    let server = start(ServerConfig { access_token: "s3cret".into(), ipc_require_token: true, ..on(&path) });
    assert_eq!(status_of(&get(&path, "/height", "")), 401);
    assert_eq!(status_of(&get(&path, "/height", "X-API-Key: s3cret\r\n")), 200);
    drop(server);
}

#[test]
fn a_stale_socket_is_cleared_but_a_live_one_or_a_file_is_never_taken() {
    let path = socket_path("stale");
    // Left behind by a run that died: a socket file nobody listens on.
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let first = start(on(&path));
    assert!(first.ipc_path().is_some(), "{:?}", first.ipc_error());

    // A second daemon on the same path does not steal a live socket. TCP
    // still serves; the IPC failure is reported.
    let second = start(on(&path));
    assert!(second.ipc_path().is_none());
    assert!(second.ipc_error().unwrap().contains("another process is listening"), "{:?}", second.ipc_error());
    drop(second);
    assert!(path.exists(), "the loser leaves the winner's socket alone");
    assert_eq!(status_of(&get(&path, "/height", "")), 200);
    drop(first);

    // A regular file at the path is never replaced.
    std::fs::write(&path, b"not a socket").unwrap();
    let third = start(on(&path));
    assert!(third.ipc_error().unwrap().contains("not a socket"), "{:?}", third.ipc_error());
    drop(third);
    assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_relative_path_is_refused() {
    let server = start(ServerConfig { ipc_path: "relative.sock".into(), ..Default::default() });
    assert!(server.ipc_error().unwrap().contains("must be absolute"), "{:?}", server.ipc_error());
}
