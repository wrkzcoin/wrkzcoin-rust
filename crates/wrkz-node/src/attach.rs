// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-node attach <socket>`: a console for a daemon that is already
//! running, reached over its RPC IPC socket (`src/daemon/AttachConsole.cpp`).
//!
//! Every line typed here runs inside that daemon, through its own console
//! ([`crate::console::Console::run_remote`] behind `POST /console`), and what
//! the command printed comes back. It is the way in to a daemon run by a
//! service manager, which has no terminal of its own. In the C++'s order:
//!
//! 1. the address must be the daemon's `--rpc-ipc-path`: an absolute path, an
//!    `@name`, or `ipc://path` — console commands are never served over TCP;
//! 2. `help` goes first, as the connection test, and its answer is printed;
//! 3. then the `> ` prompt: a blank line is skipped, `exit` and `quit` leave
//!    without sending anything, and every other line is sent as typed. After
//!    `stop` the daemon is shutting down, and the console leaves with it. End
//!    of input leaves too, with status 0.
//!
//! The timeouts are the C++'s: two seconds to connect and to send, a day to
//! wait for an answer, because a command may run for hours. No access token is
//! sent: the socket file's mode is the gate, and a daemon started with
//! `--rpc-ipc-require-token` answers 401, which is printed.
//!
//! Not on Windows, where the IPC listener is not either ([`wrkz_rpc::ipc`]).
//!
//! # Differences from the C++
//!
//! - The prompt is drawn only when stdin is a terminal, so a script piped in
//!   gets the commands' output and nothing between it.
//! - No line editing and no history; the C++ reads through linenoise.
//! - Ctrl+C leaves at once with status 0, at the prompt or in the middle of a
//!   command. linenoise gives the C++ the same at the prompt; during a command
//!   the signal's default action kills it.
//! - A transport failure is described by the operating system's error, not
//!   `httplib`'s one-word error name.

use std::io::{BufRead, Write};
use std::time::Duration;

use wrkz_rpc::json::{self, Json, Obj, ParseLimits};

/// `CONNECT_TIMEOUT_SECONDS` (`AttachConsole.cpp:21`): connecting, and sending.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// `COMMAND_TIMEOUT_SECONDS` (`AttachConsole.cpp:26`): waiting for an answer.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest answer read.
const MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;

/// What an address that is not an IPC socket is told (`AttachConsole.cpp:105`).
pub const NOT_AN_IPC_ADDRESS: &str = "attach takes the daemon's RPC IPC socket: an absolute path, an @name or \
                                      ipc://path (the daemon's --rpc-ipc-path). Console commands are not served \
                                      over TCP.";
/// Printed under the help text (`AttachConsole.cpp:134`).
pub const LEAVING_HINT: &str = "exit or quit leaves this console. stop shuts the daemon down.";
/// Printed after a `stop` the daemon accepted (`AttachConsole.cpp:171`).
pub const DAEMON_STOPPING: &str = "The daemon is shutting down; leaving the console.";
/// A 404's reason: a daemon with no console route (`AttachConsole.cpp:69`).
pub const NEEDS_UPGRADING: &str = "this daemon does not serve console commands; it needs upgrading";

/// `Common::Ipc::unsupportedReason()` (`IpcSocket.cpp:95-105`): why this build
/// cannot attach, or `None` where it can.
pub fn unsupported_reason() -> Option<&'static str> {
    if cfg!(unix) {
        None
    } else if cfg!(windows) {
        Some(
            "IPC sockets are not available on Windows builds: the socket file carries no enforceable permissions \
             there, so the endpoint could not be restricted to its owner",
        )
    } else {
        Some("IPC sockets are not available in this build")
    }
}

/// `Utilities::isIpcDaemonAddress` (`Utilities.cpp:189`).
pub fn is_ipc_address(endpoint: &str) -> bool {
    endpoint.starts_with("ipc://") || endpoint.starts_with('/') || endpoint.starts_with('@')
}

/// `Utilities::ipcDaemonPath`: the socket path, `ipc://` removed.
pub fn socket_path(endpoint: &str) -> &str {
    endpoint.strip_prefix("ipc://").unwrap_or(endpoint)
}

/// The body of one request: `{"command":"<line>"}`.
pub fn request_body(line: &str) -> String {
    let mut body = Obj::new();
    body.set("command", line);
    body.build().to_string()
}

/// `runInDaemon`'s reading of an answer (`AttachConsole.cpp:47-86`): the
/// output of a 200; otherwise `HTTP <status>: ` and the body's `error`, or the
/// body itself, or — for a 404 — that the daemon needs upgrading.
pub fn read_reply(status: u16, body: &[u8]) -> Result<String, String> {
    let parsed = json::parse(body, ParseLimits { max_bytes: MAX_REPLY_BYTES, max_depth: 8 });
    if status != 200 {
        let reason = if status == 404 {
            NEEDS_UPGRADING.to_string()
        } else {
            match parsed.as_ref().ok().and_then(|j| j.get("error")).and_then(Json::as_str) {
                Some(error) => error.to_string(),
                None => String::from_utf8_lossy(body).into_owned(),
            }
        };
        return Err(format!("HTTP {status}: {reason}"));
    }
    let answer = parsed.map_err(|e| format!("unreadable reply: {e}"))?;
    match answer.get("output").and_then(Json::as_str) {
        Some(output) => Ok(output.to_string()),
        None => Err("unreadable reply: it carries no output string".to_string()),
    }
}

/// Where a command line is run: the daemon's socket, or a stand-in in a test.
pub trait Transport {
    /// Run one line. `Err` says why it did not run, in a form worth showing.
    fn run(&mut self, line: &str) -> Result<String, String>;
}

/// The console itself (`runAttachConsole`, `AttachConsole.cpp:120-176`),
/// over any transport, input and output. `describe` names the socket, as
/// `Common::Ipc::describe` does. Returns the process exit status.
pub fn session<T: Transport, R: BufRead, W: Write>(
    transport: &mut T,
    describe: &str,
    mut input: R,
    out: &mut W,
    prompt: bool,
) -> u8 {
    let help = match transport.run("help") {
        Ok(help) => help,
        Err(e) => {
            let _ = writeln!(out, "Could not attach to {describe}: {e}");
            let _ = out.flush();
            return 1;
        }
    };
    let _ = writeln!(out, "Attached to {describe}");
    print_output(out, &help);
    let _ = writeln!(out, "{LEAVING_HINT}");

    let mut line = Vec::new();
    loop {
        if prompt {
            let _ = write!(out, "> ");
        }
        let _ = out.flush();
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => {
                // Ctrl+D at a prompt: put the shell's prompt on a line of its own.
                if prompt {
                    let _ = writeln!(out);
                }
                break;
            }
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let Some(command) = text.split_whitespace().next() else { continue };
        if command == "exit" || command == "quit" {
            break;
        }
        match transport.run(text) {
            Ok(output) => print_output(out, &output),
            Err(e) => {
                let _ = writeln!(out, "Command failed: {e}");
                continue;
            }
        }
        if command == "stop" {
            let _ = writeln!(out, "{DAEMON_STOPPING}");
            break;
        }
    }
    let _ = out.flush();
    0
}

/// A command's output, with a newline added when it ends without one.
fn print_output<W: Write>(out: &mut W, output: &str) {
    let _ = write!(out, "{output}");
    if !output.is_empty() && !output.ends_with('\n') {
        let _ = writeln!(out);
    }
}

/// `Daemon::runAttachConsole` (`AttachConsole.cpp:90`): attach the terminal to
/// the daemon at `endpoint`. Returns the process exit status.
pub fn run(endpoint: &str) -> u8 {
    if let Some(reason) = unsupported_reason() {
        println!("Cannot attach: {reason}.");
        return 1;
    }
    if !is_ipc_address(endpoint) {
        println!("{NOT_AN_IPC_ADDRESS}");
        return 1;
    }
    run_on_socket(socket_path(endpoint))
}

#[cfg(unix)]
fn run_on_socket(path: &str) -> u8 {
    leave_on_interrupt();
    let mut transport = IpcTransport::new(path);
    let prompt = crate::log::terminal().stdin;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    session(&mut transport, &wrkz_rpc::ipc::describe(path), stdin.lock(), &mut stdout, prompt)
}

#[cfg(not(unix))]
fn run_on_socket(_path: &str) -> u8 {
    // `run` has already said this build cannot.
    1
}

/// Ctrl+C: leave at once, with status 0.
#[cfg(unix)]
fn leave_on_interrupt() {
    extern "C" fn leave(_signal: libc::c_int) {
        // SAFETY: `write` and `_exit` are async-signal-safe, and nothing else
        // is touched.
        unsafe {
            libc::write(1, b"\n".as_ptr().cast(), 1);
            libc::_exit(0);
        }
    }
    let handler = leave as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler above calls only async-signal-safe functions.
    unsafe {
        libc::signal(libc::SIGINT, handler);
    }
}

/// The daemon's IPC socket: one connection per command, as
/// `Connection: close` has it.
#[cfg(unix)]
pub struct IpcTransport {
    path: String,
}

#[cfg(unix)]
impl IpcTransport {
    pub fn new(path: &str) -> Self {
        Self { path: path.to_string() }
    }

    /// Connect within [`CONNECT_TIMEOUT`]. A local connect does not usually
    /// wait, but one to a daemon whose accept queue is full does, and an
    /// attach must not hang on it.
    fn connect(&self) -> Result<std::os::unix::net::UnixStream, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = self.path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(wrkz_rpc::ipc::connect(&path));
        });
        match rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("no connection within {} s", CONNECT_TIMEOUT.as_secs())),
        }
    }
}

#[cfg(unix)]
impl Transport for IpcTransport {
    fn run(&mut self, line: &str) -> Result<String, String> {
        let stream = self.connect()?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(COMMAND_TIMEOUT)).map_err(|e| e.to_string())?;
        let body = request_body(line);
        let headers = [("Content-Type", "application/json")];
        let (status, reply) = wrkz_rpc::http::client::exchange(
            stream,
            "POST",
            "/console",
            "localhost",
            &headers,
            Some(body.as_bytes()),
            MAX_REPLY_BYTES as u64,
        )?;
        read_reply(status, &reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Answers in order, and remembers what it was sent.
    #[derive(Default)]
    struct Scripted {
        answers: VecDeque<Result<String, String>>,
        sent: Vec<String>,
    }

    impl Transport for Scripted {
        fn run(&mut self, line: &str) -> Result<String, String> {
            self.sent.push(line.to_string());
            self.answers.pop_front().unwrap_or_else(|| Ok(String::new()))
        }
    }

    fn scripted(answers: &[Result<&str, &str>]) -> Scripted {
        let answers = answers.iter().map(|a| a.map(str::to_string).map_err(str::to_string)).collect();
        Scripted { answers, sent: Vec::new() }
    }

    fn attach(transport: &mut Scripted, input: &str, prompt: bool) -> (u8, String) {
        let mut out = Vec::new();
        let status = session(transport, "socket /run/wrkzd.sock", input.as_bytes(), &mut out, prompt);
        (status, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_session_runs_lines_in_the_daemon_until_stop() {
        let mut daemon = scripted(&[
            Ok("wrkz-node 1.0.0\nCommands:\n  help\n"),
            Ok("Height: 5 / 5 (100.00%)"),
            Err("HTTP 503: The daemon console is not available yet, please retry in a moment"),
            Ok("== EXITING ==\n"),
        ]);
        let (status, out) = attach(&mut daemon, "height\n\n   \nprint_bc 1 2\r\nstop\nstatus\n", false);
        assert_eq!(status, 0);
        assert_eq!(
            daemon.sent,
            ["help", "height", "print_bc 1 2", "stop"],
            "blank lines and what follows stop are not sent"
        );
        assert_eq!(
            out,
            "Attached to socket /run/wrkzd.sock\nwrkz-node 1.0.0\nCommands:\n  help\n\
             exit or quit leaves this console. stop shuts the daemon down.\n\
             Height: 5 / 5 (100.00%)\n\
             Command failed: HTTP 503: The daemon console is not available yet, please retry in a moment\n\
             == EXITING ==\n\
             The daemon is shutting down; leaving the console.\n"
        );
    }

    #[test]
    fn exit_and_quit_leave_without_troubling_the_daemon() {
        for word in ["exit", "quit", "  quit now"] {
            let mut daemon = scripted(&[Ok("help text\n")]);
            let (status, out) = attach(&mut daemon, &format!("{word}\nstatus\n"), true);
            assert_eq!(status, 0);
            assert_eq!(daemon.sent, ["help"], "{word}");
            assert!(out.ends_with("stop shuts the daemon down.\n> "), "{out:?}");
        }
        let mut daemon = scripted(&[Ok("help text\n")]);
        let (status, out) = attach(&mut daemon, "", true);
        assert_eq!(status, 0, "end of input leaves");
        assert!(out.ends_with("> \n"), "a prompt left by Ctrl+D gets its newline: {out:?}");
    }

    #[test]
    fn a_failed_connection_test_is_status_1() {
        let mut daemon = scripted(&[Err("HTTP 404: this daemon does not serve console commands; it needs upgrading")]);
        let (status, out) = attach(&mut daemon, "status\n", false);
        assert_eq!(status, 1);
        assert_eq!(
            out,
            "Could not attach to socket /run/wrkzd.sock: HTTP 404: this daemon does not serve console commands; \
             it needs upgrading\n"
        );
        assert_eq!(daemon.sent, ["help"]);
    }

    #[test]
    fn replies_are_read_as_run_in_daemon_reads_them() {
        assert_eq!(read_reply(200, br#"{"output":"ok\n","status":"OK"}"#), Ok("ok\n".to_string()));
        assert_eq!(read_reply(503, br#"{"error":"not yet","status":"Failed"}"#), Err("HTTP 503: not yet".to_string()));
        assert_eq!(read_reply(401, b"Unauthorized"), Err("HTTP 401: Unauthorized".to_string()), "a bare body");
        assert_eq!(read_reply(404, b""), Err(format!("HTTP 404: {NEEDS_UPGRADING}")));
        assert!(read_reply(200, b"not json").unwrap_err().starts_with("unreadable reply: "));
        assert!(read_reply(200, br#"{"status":"OK"}"#).unwrap_err().starts_with("unreadable reply: "));
        assert_eq!(request_body("print_block \"x\""), r#"{"command":"print_block \"x\""}"#);
    }

    #[test]
    fn only_an_ipc_address_is_attached_to() {
        for ok in ["/run/wrkz/wrkzd.sock", "@wrkzd", "ipc:///run/wrkzd.sock"] {
            assert!(is_ipc_address(ok), "{ok}");
        }
        for refused in ["127.0.0.1:17856", "localhost", "", "run/wrkzd.sock", "C:\\wrkz\\wrkzd.sock"] {
            assert!(!is_ipc_address(refused), "{refused}");
        }
        assert_eq!(socket_path("ipc:///run/wrkzd.sock"), "/run/wrkzd.sock");
        assert_eq!(socket_path("@wrkzd"), "@wrkzd");
        assert_eq!(unsupported_reason().is_none(), cfg!(unix));
    }
}
