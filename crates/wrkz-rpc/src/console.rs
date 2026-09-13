// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `POST /console`: console commands over the local IPC socket
//! (`RpcServer::console`, `RpcServer.cpp:1062-1088`), which is what
//! `wrkz-node attach <socket>` talks to.
//!
//! The route exists **only on the IPC listener**, as it exists only on the
//! C++'s (`RpcServer.cpp:318-326`): console commands change the log level, ban
//! peers and stop the node, and the mode on the socket file — the same people
//! who could type at the daemon's own terminal — decides who may send them.
//! Over TCP the path is the 404 of any unrouted one, token or no token.
//!
//! Past the route it goes through the middleware like any route with a body:
//! the body cap; the access token only when `--rpc-ipc-require-token` asks for
//! it on the socket; no rate limit, which an IPC caller never has; a body that
//! is not JSON is the usual 400. No sync is required. Then:
//!
//! | Request | Answer |
//! | --- | --- |
//! | `{"command":"status"}` | 200 `{"output":"…","status":"OK"}` |
//! | no `command` | 400 `Missing JSON parameter: 'command'` |
//! | `command` not a string | 500 `Internal server error: …` |
//! | a well-formed request before the console is ready | 503 [`NOT_READY`] |
//!
//! This crate knows nothing of commands. The daemon installs a
//! [`ConsoleExecutor`] in the server's [`ConsoleSlot`] once its console exists,
//! and clears it when the engine stops (`Daemon.cpp:1168`, `:1199`); before,
//! and after, the route answers 503. The output is whatever the executor
//! returns.

use std::sync::{Arc, RwLock, RwLockWriteGuard};

use crate::handlers;
use crate::http::Response;
use crate::json::{Json, Obj};

/// The 503 while no console is installed (`RpcServer.cpp:1078`).
pub const NOT_READY: &str = "The daemon console is not available yet, please retry in a moment";

/// Runs one command line and returns everything the command printed
/// (`RpcServer::ConsoleExecutor`, `RpcServer.h:145`).
pub type ConsoleExecutor = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Where the daemon puts its [`ConsoleExecutor`], shared by every worker.
///
/// A worker clones the executor out under the lock and runs it after letting
/// go, as the C++ copies it out of `m_consoleExecutorMutex`, so a command that
/// runs for minutes holds up neither an install nor a clear.
#[derive(Clone, Default)]
pub struct ConsoleSlot(Arc<RwLock<Option<ConsoleExecutor>>>);

impl ConsoleSlot {
    /// `setConsoleExecutor(executor)`.
    pub fn install(&self, executor: ConsoleExecutor) {
        *self.write() = Some(executor);
    }

    /// `setConsoleExecutor(nullptr)`: the route answers 503 again.
    pub fn clear(&self) {
        *self.write() = None;
    }

    /// The executor, when one is installed.
    pub fn executor(&self) -> Option<ConsoleExecutor> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn is_installed(&self) -> bool {
        self.0.read().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    fn write(&self) -> RwLockWriteGuard<'_, Option<ConsoleExecutor>> {
        self.0.write().unwrap_or_else(|p| p.into_inner())
    }
}

impl std::fmt::Debug for ConsoleSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleSlot").field("installed", &self.is_installed()).finish()
    }
}

/// `RpcServer::console`: the command is read first — `getStringFromJSON`
/// throws before the executor is looked at — then run.
pub(crate) fn handle(slot: &ConsoleSlot, body: &Json) -> Response {
    let command = match handlers::str_of(body, "command") {
        Ok(command) => command,
        Err(e) => return e.response(),
    };
    let Some(executor) = slot.executor() else {
        return handlers::fail_request(503, NOT_READY);
    };
    let mut answer = Obj::new();
    answer.set("output", executor(command)).set("status", "OK");
    Response::json(200, answer.build().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> Json {
        crate::json::parse(text.as_bytes(), Default::default()).unwrap()
    }

    fn text(res: &Response) -> String {
        String::from_utf8(res.body.clone()).unwrap()
    }

    #[test]
    fn the_route_answers_503_until_a_console_is_installed_and_after_it_is_cleared() {
        let slot = ConsoleSlot::default();
        let res = handle(&slot, &body(r#"{"command":"status"}"#));
        assert_eq!(res.status, 503);
        assert_eq!(text(&res), format!(r#"{{"error":"{NOT_READY}","status":"Failed"}}"#));

        slot.install(Arc::new(|line: &str| format!("ran {line}\n")));
        assert!(slot.is_installed());
        let res = handle(&slot, &body(r#"{"command":"print_cn  x"}"#));
        assert_eq!(res.status, 200);
        assert_eq!(text(&res), r#"{"output":"ran print_cn  x\n","status":"OK"}"#);

        slot.clear();
        assert_eq!(handle(&slot, &body(r#"{"command":"status"}"#)).status, 503);
        assert_eq!(format!("{slot:?}"), "ConsoleSlot { installed: false }");
    }

    #[test]
    fn a_bad_command_is_refused_before_the_console_is_looked_at() {
        let slot = ConsoleSlot::default();
        let missing = handle(&slot, &body("{}"));
        assert_eq!(missing.status, 400);
        assert_eq!(text(&missing), r#"{"error":"Missing JSON parameter: 'command'","status":"Failed"}"#);
        let wrong = handle(&slot, &body(r#"{"command":7}"#));
        assert_eq!(wrong.status, 500);
        assert!(text(&wrong).contains("Internal server error: "), "{}", text(&wrong));
    }
}
