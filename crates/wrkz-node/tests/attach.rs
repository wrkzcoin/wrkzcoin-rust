// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-node attach <socket>` and `--attach`, as a process: the argument
//! check, the refusals, and — where IPC sockets exist — a whole session
//! against a running daemon, from the connection test to `stop`.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn node() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wrkz-node"))
}

/// Run the binary with `stdin` piped in and wait for it.
fn run(args: &[&str], stdin: &str) -> Output {
    let mut child = node()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let mut input = child.stdin.take().expect("a stdin pipe");
    let _ = input.write_all(stdin.as_bytes());
    drop(input);
    child.wait_with_output().expect("it finishes")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn attach_takes_exactly_one_argument() {
    let exe = if cfg!(windows) { "wrkz-node.exe" } else { "wrkz-node" };
    for args in [&["attach"][..], &["attach", "/run/wrkzd.sock", "extra"][..]] {
        let output = run(args, "");
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), format!("Usage: {exe} attach <rpc ipc socket path>\n"), "{args:?}");
    }
}

#[test]
fn an_address_that_is_not_a_socket_is_refused() {
    for args in [&["attach", "127.0.0.1:17856"][..], &["--attach", "localhost:17856"][..]] {
        let output = run(args, "status\n");
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let text = stdout(&output);
        if cfg!(unix) {
            assert_eq!(
                text,
                "attach takes the daemon's RPC IPC socket: an absolute path, an @name or ipc://path (the daemon's \
                 --rpc-ipc-path). Console commands are not served over TCP.\n"
            );
        } else {
            assert!(text.starts_with("Cannot attach: "), "{text}");
        }
    }
}

#[cfg(windows)]
#[test]
fn a_windows_build_refuses_to_attach() {
    for args in [&["attach", "/run/wrkzd.sock"][..], &["--attach", "@wrkzd"][..]] {
        let output = run(args, "");
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stdout(&output),
            "Cannot attach: IPC sockets are not available on Windows builds: the socket file carries no enforceable \
             permissions there, so the endpoint could not be restricted to its owner.\n"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_socket_nobody_listens_on_is_a_failed_attach() {
    let path = std::env::temp_dir().join(format!("wrkz-attach-nobody-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let output = run(&["attach", path.to_str().unwrap()], "status\n");
    assert_eq!(output.status.code(), Some(1));
    let text = stdout(&output);
    assert!(text.starts_with(&format!("Could not attach to socket {}: ", path.display())), "{text}");
}

/// The whole thing: a daemon on its own socket, a session typed into it, and
/// `stop` ending both.
#[cfg(unix)]
#[test]
fn a_session_runs_commands_in_the_daemon_and_stop_shuts_it_down() {
    use std::time::{Duration, Instant};

    let dir = std::env::temp_dir().join(format!("wrkz-attach-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("wrkzd.sock");
    let log = dir.join("daemon.log");
    let mut daemon = node()
        .arg("--data-dir")
        .arg(&dir)
        .arg("--log-file")
        .arg(&log)
        .args(["--no-listen", "--no-default-seeds", "--no-console", "--rpc-bind-port", "0", "--rpc-ipc-path"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the daemon starts");

    // The daemon says so once its console is installed.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("Console commands are available over") {
            break;
        }
        if Instant::now() > deadline {
            let _ = daemon.kill();
            panic!("the daemon never offered its console:\n{text}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let address = format!("ipc://{}", socket.display());
    let output = run(&["attach", &address], "height\n\nno_such_command\nstop\nheight\n");
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.starts_with(&format!("Attached to socket {}\n", socket.display())), "{text}");
    assert!(text.contains("Commands:"), "help is the connection test: {text}");
    assert!(text.contains("exit or quit leaves this console. stop shuts the daemon down.\n"), "{text}");
    assert!(text.contains("Height: "), "{text}");
    assert!(text.contains("Unknown command: no_such_command\n"), "{text}");
    assert!(text.contains("EXITING"), "{text}");
    assert!(text.ends_with("The daemon is shutting down; leaving the console.\n"), "{text}");

    let deadline = Instant::now() + Duration::from_secs(60);
    while daemon.try_wait().expect("try_wait").is_none() {
        if Instant::now() > deadline {
            let _ = daemon.kill();
            panic!("`stop` did not shut the daemon down:\n{}", std::fs::read_to_string(&log).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!socket.exists(), "the socket file is removed on the way out");
}
