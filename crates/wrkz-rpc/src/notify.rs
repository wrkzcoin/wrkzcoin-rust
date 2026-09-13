// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The notification hooks every program shares: `wrkz-wallet-api --tx-notify`,
//! `wrkz-service --tx-notify` / `--tx-confirmed-notify`, and the daemon's
//! `--block-notify`, `--reorg-notify` and `--tx-notify` —
//! `Tools::Notifier` (`src/common/Notifier.cpp`, `Notifier.h`).
//!
//! One [`Notifier`] is one hook. It is built once from the operator's spec and
//! handed [`Notification`]s from whatever thread learns of an event;
//! [`Notifier::notify`] only queues, so a sync thread, a request worker or a
//! block handler is never held up by the command or the webhook behind it.
//!
//! # The spec
//!
//! Trimmed of spaces, tabs and line ends; empty means the hook is off
//! (`Notifier.cpp:170`). Then:
//!
//! - **`http://` or `https://`** (any case): a *webhook*. Every notification is
//!   POSTed to the URL's path (default `/`) as `application/json`, the body
//!   `{"event":"<event>", <fields in order>}` ([`build_json`]). Timeouts of ten
//!   seconds on the connection, the write and the read; redirects are not
//!   followed; any 2xx is success, any other status is final, and a transport
//!   failure is retried once (`Notifier.cpp:552`). The built-in client speaks
//!   plain HTTP. An `https://` URL needs the host program to supply a TLS
//!   client ([`Options::post`]); without one the hook is disabled with a
//!   warning, exactly as a C++ build without OpenSSL disables it
//!   (`Notifier.cpp:188`).
//! - **anything else**: a *command template*, split once into arguments by
//!   [`tokenize`] — whitespace separates, `'` or `"` group, no escapes — with
//!   `%`-placeholders substituted inside each argument *after* the split
//!   ([`substitute`]), so a value can never add or split an argument.
//!
//! The placeholders are the caller's: `%s` hash and `%h` height for a block,
//! `%s` `%h` `%n` `%d` for a reorganisation, `%s` `%h` `%a` `%f` `%p` `%c` for a
//! wallet transaction. `%%` is a literal `%`; an unknown `%x` and a trailing
//! `%` stay as they are.
//!
//! # Running a command
//!
//! **No shell.** The program is looked up on `PATH` and started directly with
//! the substituted arguments and the program's environment — `posix_spawnp`
//! in the C++, [`std::process::Command`] here, which on Windows quotes each
//! argument for `CreateProcess` the same way `quoteWindowsArg` does
//! (`Notifier.cpp:113`). Standard output and standard error are inherited.
//!
//! Standard input is **not**: the child gets an empty one. The C++ inherits it,
//! which lets a hook swallow the line an operator was typing at the
//! `wrkz-wallet-api` console or `wrkz-node`'s prompt. That is the one deliberate
//! change to how a command runs.
//!
//! The child is polled every 50 ms. A command still running after
//! [`DEFAULT_TIMEOUT`], or when the hook is stopped, is killed; every child is
//! waited for, so none is left a zombie. Exit status 0 is success.
//!
//! # Delivery
//!
//! One worker thread per enabled hook delivers in order, one at a time. The
//! queue holds [`DEFAULT_MAX_QUEUE`] notifications; past that a new one is
//! dropped, and the drop is logged the first time and every thousandth time
//! (`Notifier.cpp:261`). [`Notifier::stop`] — and dropping the notifier —
//! discards what is queued, lets a delivery in flight finish (a command is
//! killed at once), and logs how many were sent, failed and dropped.
//!
//! A failing hook is reported **once**, not once per event: the first failure
//! of a run of failures is logged with its reason, then every hundredth, and
//! the first success afterwards says how many failed in between. The C++ logs
//! every failure, which turns a hook pointed at a missing program into one
//! warning per transaction for the life of the process.
//!
//! # Security
//!
//! A hook runs with the privileges, the working directory and the environment
//! of the program that fires it. Whoever can set the spec can run code as that
//! user, so it belongs in the same trust domain as the wallet password.
//!
//! Because no shell is involved, a placeholder value is only ever part of one
//! argument: a hash, a height or an amount cannot become a second command or a
//! redirection. **A shell is a shell**, though: a spec such as
//! `sh -c "notify %s"` or `cmd /C notify %s` hands the substituted text to a
//! shell that *does* interpret it. Every value the wallets and the daemon
//! supply today is hex or decimal, so nothing in it is special to a shell —
//! but a hook script should still quote its arguments rather than rely on that.
//!
//! On Windows, a `.bat` or `.cmd` program is run through `cmd.exe` by the
//! system, with the same caveat; Rust's standard library refuses arguments it
//! cannot quote safely for one, and such a delivery fails and is logged.
//!
//! The spec is logged in a reduced form ([`Notifier::describe`]): a command as
//! its program and argument count, a webhook without its query string. Either
//! can carry a token, and a log is not the place for one.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::log::Level;

/// `Notifier::DEFAULT_TIMEOUT` (`Notifier.h:67`): how long one delivery may take.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// `Notifier::DEFAULT_MAX_QUEUE` (`Notifier.h:69`).
pub const DEFAULT_MAX_QUEUE: usize = 1024;

/// How often a running command is looked at (`Notifier.cpp:741`).
const POLL: Duration = Duration::from_millis(50);

/// After the first failure in a row, how many more before it is logged again.
const FAILURE_LOG_EVERY: u64 = 100;

/// The longest status line a webhook answer may send before it is refused.
const MAX_STATUS_LINE: usize = 8 * 1024;

////////////////////////
/* NOTIFICATIONS      */
////////////////////////

/// One member of a webhook body (`Notifier::Field`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub key: String,
    /// The text, escaped when `quoted`, written as it is otherwise.
    pub value: String,
    /// A JSON string (`true`) or a number, boolean or `null` (`false`).
    pub quoted: bool,
}

impl Field {
    /// A string member.
    pub fn string(key: &str, value: impl Into<String>) -> Field {
        Field { key: key.to_string(), value: value.into(), quoted: true }
    }

    /// A number or a boolean, written as given. An empty value is `null`
    /// (`Notifier.cpp:483`).
    pub fn raw(key: &str, value: impl Into<String>) -> Field {
        Field { key: key.to_string(), value: value.into(), quoted: false }
    }
}

/// One event (`Notifier::Notification`): its name, the `%`-placeholders a
/// command gets and the members a webhook gets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    /// `block`, `reorg`, `tx`, `tx_confirmed`: the webhook's `event` member.
    pub event: String,
    /// `('s', hash)`, `('h', height)`, … for the command form.
    pub placeholders: Vec<(char, String)>,
    /// The webhook members after `event`, in this order.
    pub fields: Vec<Field>,
}

////////////////////////
/* OPTIONS            */
////////////////////////

/// Where a hook's own messages go: the level and the text, which already
/// starts with `[<name>] `.
pub type LogFn = Arc<dyn Fn(Level, &str) + Send + Sync>;

/// A webhook client: POST `body` as `application/json` to the URL, following
/// no redirect and giving up after the timeout on each step. `Ok(status)` for
/// any answer, `Err` for a transport failure, which is retried once.
pub type PostFn = Arc<dyn Fn(&str, &str, Duration) -> Result<u16, String> + Send + Sync>;

/// What can be changed about a hook. The defaults are the C++'s.
#[derive(Clone)]
pub struct Options {
    /// How long a command may run, and each step of a webhook may take.
    pub timeout: Duration,
    /// How many notifications may wait; `0` is taken as `1`.
    pub max_queue: usize,
    /// Where messages go. `None` is the process logger ([`crate::log`]).
    pub log: Option<LogFn>,
    /// The webhook client. `None` is the built-in plain-HTTP one, which
    /// disables an `https://` hook.
    pub post: Option<PostFn>,
}

impl Default for Options {
    fn default() -> Self {
        Options { timeout: DEFAULT_TIMEOUT, max_queue: DEFAULT_MAX_QUEUE, log: None, post: None }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("timeout", &self.timeout)
            .field("max_queue", &self.max_queue)
            .field("log", &self.log.is_some())
            .field("post", &self.post.is_some())
            .finish()
    }
}

////////////////////////
/* THE NOTIFIER       */
////////////////////////

/// How a hook delivers, decided once from its spec.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Disabled,
    Webhook(Url),
    /// The template, tokenised, placeholders not yet substituted.
    Command(Vec<String>),
}

/// One hook: a spec, a queue and the thread that works through it.
pub struct Notifier {
    name: String,
    spec: String,
    target: Target,
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier").field("name", &self.name).field("target", &self.describe()).finish()
    }
}

/// What the notifier and its worker share.
struct Shared {
    name: String,
    log: Option<LogFn>,
    max_queue: usize,
    state: Mutex<State>,
    wake: Condvar,
    sent: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
}

struct State {
    queue: VecDeque<Notification>,
    stopping: bool,
}

impl Shared {
    fn new(name: &str, options: &Options) -> Shared {
        Shared {
            name: name.to_string(),
            log: options.log.clone(),
            max_queue: options.max_queue.max(1),
            state: Mutex::new(State { queue: VecDeque::new(), stopping: false }),
            wake: Condvar::new(),
            sent: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing a panic can leave half-done lives behind this lock.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log(&self, level: Level, message: &str) {
        let line = format!("[{}] {message}", self.name);
        match &self.log {
            Some(log) => log(level, &line),
            None => crate::log::log(level, format_args!("{line}")),
        }
    }

    fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// `Notifier::notify` (`Notifier.cpp:246`): queue, or count a drop.
    /// Returns whether it was queued.
    fn enqueue(&self, notification: Notification) -> bool {
        let mut state = self.lock();
        if state.stopping {
            return false;
        }
        if state.queue.len() >= self.max_queue {
            drop(state);
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(1000) {
                self.log(
                    Level::Warn,
                    &format!("queue full ({}), dropping notifications. Dropped so far: {dropped}", self.max_queue),
                );
            }
            return false;
        }
        state.queue.push_back(notification);
        drop(state);
        self.wake.notify_one();
        true
    }

    /// The next notification, or `None` once the hook is stopping.
    fn next(&self) -> Option<Notification> {
        let mut state = self.lock();
        loop {
            if state.stopping {
                return None;
            }
            if let Some(n) = state.queue.pop_front() {
                return Some(n);
            }
            state = self.wake.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }
}

impl Notifier {
    /// A hook named `name` (used in its log lines, e.g. `tx-notify`) from
    /// `spec`, with the C++ defaults. An empty spec gives a disabled hook.
    pub fn new(name: &str, spec: &str) -> Notifier {
        Notifier::with_options(name, spec, Options::default())
    }

    /// [`Notifier::new`] with the timeout, the queue, the log sink or the
    /// webhook client changed. A usable spec starts the worker thread straight
    /// away, as the C++ constructor does.
    pub fn with_options(name: &str, spec: &str, options: Options) -> Notifier {
        let spec = spec.trim_matches([' ', '\t', '\r', '\n']).to_string();
        let shared = Arc::new(Shared::new(name, &options));
        let target = parse_target(&spec, options.post.is_some(), &shared);

        let notifier = Notifier { name: name.to_string(), spec, target, shared, worker: Mutex::new(None) };

        if notifier.target != Target::Disabled {
            let delivery = Delivery {
                target: notifier.target.clone(),
                timeout: options.timeout,
                post: options.post.clone(),
                shared: Arc::clone(&notifier.shared),
            };
            let worker = std::thread::Builder::new().name(format!("{name} notifier")).spawn(move || delivery.run());
            match worker {
                Ok(handle) => {
                    *notifier.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
                    let kind = if notifier.is_webhook() { "webhook" } else { "command" };
                    notifier.shared.log(Level::Info, &format!("{kind} notifier enabled: {}", notifier.describe()));
                }
                Err(e) => {
                    notifier.shared.log(Level::Warn, &format!("cannot start the notifier thread: {e}"));
                    notifier.shared.lock().stopping = true;
                }
            }
        }

        notifier
    }

    /// Whether the spec was usable, so the caller can skip building
    /// notifications nobody will receive (`Notifier::enabled`).
    pub fn enabled(&self) -> bool {
        self.target != Target::Disabled && !self.shared.stopping()
    }

    /// Whether this hook POSTs rather than runs a command.
    pub fn is_webhook(&self) -> bool {
        matches!(self.target, Target::Webhook(_))
    }

    /// The label its log lines carry.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The spec as the operator gave it, trimmed. It may carry a token; print
    /// [`Notifier::describe`] instead.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// The spec with nothing in it a log should keep: a command's program and
    /// how many arguments follow it, or a webhook's scheme, host, port and path
    /// without the query string.
    pub fn describe(&self) -> String {
        match &self.target {
            Target::Disabled => "disabled".to_string(),
            Target::Command(argv) => match argv.len() {
                1 => argv[0].clone(),
                n => format!("{} with {} argument(s)", argv[0], n - 1),
            },
            Target::Webhook(url) => {
                let path = url.path.split('?').next().unwrap_or("/");
                let query = if url.path.contains('?') { "?…" } else { "" };
                format!("{}://{}{path}{query}", url.scheme, url.authority)
            }
        }
    }

    /// Queue a notification. Never blocks on the delivery; a full queue or a
    /// stopped hook drops it (`Notifier::notify`).
    pub fn notify(&self, notification: Notification) {
        if self.target == Target::Disabled {
            return;
        }
        self.shared.enqueue(notification);
    }

    /// Stop the worker: queued notifications are discarded and a delivery in
    /// flight finishes or, for a command, is killed. Idempotent, and called on
    /// drop (`Notifier::stop`).
    pub fn stop(&self) {
        {
            let mut state = self.shared.lock();
            if state.stopping {
                return;
            }
            state.stopping = true;
            state.queue.clear();
        }
        self.shared.wake.notify_all();

        let handle = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }

        if self.target != Target::Disabled {
            self.shared.log(
                Level::Info,
                &format!("stopped. Sent={}, failed={}, dropped={}", self.sent(), self.failed(), self.dropped()),
            );
        }
    }

    /// Deliveries that succeeded.
    pub fn sent(&self) -> u64 {
        self.shared.sent.load(Ordering::Relaxed)
    }

    /// Deliveries that failed.
    pub fn failed(&self) -> u64 {
        self.shared.failed.load(Ordering::Relaxed)
    }

    /// Notifications turned away by a full queue.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Decide what a trimmed spec is, logging why when it is unusable
/// (`Notifier.cpp:180-211`).
fn parse_target(spec: &str, have_post: bool, shared: &Shared) -> Target {
    if spec.is_empty() {
        return Target::Disabled;
    }
    if is_url(spec) {
        let Some(url) = Url::parse(spec) else {
            shared.log(Level::Warn, "invalid webhook URL; notifier disabled");
            return Target::Disabled;
        };
        if url.scheme == "https" && !have_post {
            shared.log(
                Level::Warn,
                &format!(
                    "https webhook configured but this program has no TLS client; notifier disabled: https://{}",
                    url.authority
                ),
            );
            return Target::Disabled;
        }
        return Target::Webhook(url);
    }
    let argv = tokenize(spec);
    if argv.is_empty() {
        shared.log(Level::Warn, "empty command; notifier disabled");
        return Target::Disabled;
    }
    Target::Command(argv)
}

////////////////////////
/* THE WORKER         */
////////////////////////

/// Everything the worker thread owns.
struct Delivery {
    target: Target,
    timeout: Duration,
    post: Option<PostFn>,
    shared: Arc<Shared>,
}

impl Delivery {
    /// `Notifier::workerLoop` (`Notifier.cpp:491`).
    fn run(self) {
        let mut failures_in_a_row: u64 = 0;

        while let Some(notification) = self.shared.next() {
            // A panic in a delivery must not take the hook down for the rest
            // of the run, which is what the C++'s catch-all is for.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.deliver(&notification)))
                .unwrap_or_else(|_| Err("delivery panicked".to_string()));

            match outcome {
                Ok(()) => {
                    self.shared.sent.fetch_add(1, Ordering::Relaxed);
                    if failures_in_a_row > 0 {
                        self.shared.log(
                            Level::Info,
                            &format!("delivering again after {failures_in_a_row} failed notification(s)"),
                        );
                    }
                    failures_in_a_row = 0;
                }
                Err(reason) => {
                    self.shared.failed.fetch_add(1, Ordering::Relaxed);
                    failures_in_a_row += 1;
                    if failures_in_a_row == 1 {
                        self.shared.log(Level::Warn, &reason);
                    } else if failures_in_a_row.is_multiple_of(FAILURE_LOG_EVERY) {
                        self.shared.log(Level::Warn, &format!("{reason} ({failures_in_a_row} failures in a row)"));
                    }
                }
            }
        }
    }

    /// `Notifier::deliver` (`Notifier.cpp:535`).
    fn deliver(&self, notification: &Notification) -> Result<(), String> {
        match &self.target {
            Target::Disabled => Ok(()),
            Target::Webhook(url) => self.post_webhook(url, &build_json(notification)),
            Target::Command(template) => {
                let argv: Vec<String> = template.iter().map(|t| substitute(t, &notification.placeholders)).collect();
                self.run_command(&argv)
            }
        }
    }

    /// `Notifier::postWebhook` (`Notifier.cpp:552`): one retry on a transport
    /// failure, none on an HTTP status — the receiver answered.
    fn post_webhook(&self, url: &Url, body: &str) -> Result<(), String> {
        let mut last = String::new();
        for _attempt in 0..2 {
            let answer = match &self.post {
                Some(post) => post(&url.full(), body, self.timeout),
                None => post_http(url, body, self.timeout),
            };
            match answer {
                Ok(status) if (200..300).contains(&status) => return Ok(()),
                Ok(status) => {
                    return Err(format!("webhook {}://{} answered HTTP {status}", url.scheme, url.authority));
                }
                Err(e) => last = e,
            }
            if self.shared.stopping() {
                break;
            }
        }
        Err(format!("webhook {}://{} failed: {last}", url.scheme, url.authority))
    }

    /// `Notifier::runCommand` (`Notifier.cpp:608`).
    fn run_command(&self, argv: &[String]) -> Result<(), String> {
        let Some((program, args)) = argv.split_first() else {
            return Err("empty command".to_string());
        };

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to start '{program}': {e}"))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return exit_outcome(program, status),
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("waiting for '{program}' failed: {e}"));
                }
            }

            if Instant::now() >= deadline || self.shared.stopping() {
                let _ = child.kill();
                // Reaped here, whatever happened, so no child outlives its
                // delivery as a zombie.
                let _ = child.wait();
                return Err(format!(
                    "'{program}' did not finish within {}s or shutdown was requested; killed",
                    self.timeout.as_secs_f32()
                ));
            }

            std::thread::sleep(POLL);
        }
    }
}

/// Success for exit status 0; otherwise what the C++ logs for it.
fn exit_outcome(program: &str, status: ExitStatus) -> Result<(), String> {
    if status.success() {
        return Ok(());
    }
    if let Some(code) = status.code() {
        return Err(format!("'{program}' exited with code {code}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Err(format!("'{program}' terminated by signal {signal}"));
        }
    }
    Err(format!("'{program}' did not exit normally"))
}

////////////////////////
/* THE TEMPLATE       */
////////////////////////

/// `Notifier::isUrl` (`Notifier.cpp:321`): `http://` or `https://`, any case.
pub fn is_url(spec: &str) -> bool {
    starts_with_no_case(spec, "http://") || starts_with_no_case(spec, "https://")
}

fn starts_with_no_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// `Notifier::tokenize` (`Notifier.cpp:329`): split a command line into
/// arguments.
///
/// Whitespace separates. A `'` or a `"` opens a quote that only the same
/// character closes, and may sit in the middle of a word (`a"b c"d` is one
/// argument, `ab cd`); `""` is an empty argument; a quote left open runs to
/// the end. There are no escape characters, so a Windows path keeps its
/// backslashes.
pub fn tokenize(command_line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;

    for c in command_line.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }

        if c == '"' || c == '\'' {
            quote = Some(c);
            in_token = true;
            continue;
        }

        // `std::isspace` in the "C" locale.
        if matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r') {
            if in_token {
                tokens.push(std::mem::take(&mut current));
                in_token = false;
            }
            continue;
        }

        current.push(c);
        in_token = true;
    }

    if in_token {
        tokens.push(current);
    }

    tokens
}

/// `Notifier::substitute` (`Notifier.cpp:381`): replace `%x` in one argument.
///
/// `%%` is `%`. A `%` before a character with no placeholder, or at the very
/// end, is kept, and the character after it is read normally. A substituted
/// value is never scanned again.
pub fn substitute(token: &str, placeholders: &[(char, String)]) -> String {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(&key) = chars.peek() else {
            out.push('%');
            continue;
        };
        if key == '%' {
            out.push('%');
            chars.next();
            continue;
        }
        match placeholders.iter().find(|(k, _)| *k == key) {
            Some((_, value)) => {
                out.push_str(value);
                chars.next();
            }
            // Unknown: the `%` stays, and `key` is read on the next turn.
            None => out.push('%'),
        }
    }

    out
}

/// `Notifier::jsonEscape` (`Notifier.cpp:422`).
pub fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `Notifier::buildJson` (`Notifier.cpp:470`): `event` first, then the fields
/// in the order given — not sorted, unlike [`crate::json::Json`], so the body
/// is the C++'s byte for byte.
pub fn build_json(notification: &Notification) -> String {
    let mut out = format!("{{\"event\":\"{}\"", json_escape(&notification.event));
    for field in &notification.fields {
        out.push_str(",\"");
        out.push_str(&json_escape(&field.key));
        out.push_str("\":");
        if field.quoted {
            out.push('"');
            out.push_str(&json_escape(&field.value));
            out.push('"');
        } else if field.value.is_empty() {
            out.push_str("null");
        } else {
            out.push_str(&field.value);
        }
    }
    out.push('}');
    out
}

////////////////////////
/* THE WEBHOOK        */
////////////////////////

/// A webhook URL, split the way `splitUrl` splits it (`Notifier.cpp:62`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Url {
    /// `http` or `https`, lower case.
    scheme: String,
    /// `host`, `host:port`, `[v6]` or `[v6]:port`, as written.
    authority: String,
    /// The host to connect to, without brackets.
    host: String,
    port: u16,
    /// `/path?query`; `/` when the URL had none.
    path: String,
}

impl Url {
    fn parse(spec: &str) -> Option<Url> {
        let scheme_end = spec.find("://")?;
        let scheme = spec[..scheme_end].to_ascii_lowercase();
        let rest = &spec[scheme_end + 3..];

        let authority_end = if rest.starts_with('[') {
            let close = rest.find(']')?;
            rest[close..].find(['/', '?']).map(|i| close + i)
        } else {
            rest.find(['/', '?'])
        };
        let (authority, path) = match authority_end {
            None => (rest, "/".to_string()),
            Some(i) if rest[i..].starts_with('?') => (&rest[..i], format!("/{}", &rest[i..])),
            Some(i) => (&rest[..i], rest[i..].to_string()),
        };

        // `user:pass@host` is not something the C++ client parses either, and
        // a credential in a spec is one more thing a log must never show.
        if authority.is_empty() || authority.contains('@') {
            return None;
        }

        let default_port = if scheme == "https" { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let close = v6.find(']')?;
            let port = match &v6[close + 1..] {
                "" => default_port,
                p => p.strip_prefix(':')?.parse().ok()?,
            };
            (v6[..close].to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (authority.to_string(), default_port),
            }
        };
        if host.is_empty() || path.chars().any(|c| c.is_ascii_whitespace() || c.is_ascii_control()) {
            return None;
        }

        Some(Url { scheme, authority: authority.to_string(), host, port, path })
    }

    fn full(&self) -> String {
        format!("{}://{}{}", self.scheme, self.authority, self.path)
    }
}

/// One POST over plain HTTP/1.1, reading no more than the status line.
fn post_http(url: &Url, body: &str, timeout: Duration) -> Result<u16, String> {
    let addrs: Vec<SocketAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", url.host))?
        .collect();

    let mut last = format!("{} resolved to no address", url.host);
    let mut connected = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(e) => last = format!("connect {addr}: {e}"),
        }
    }
    let mut stream = connected.ok_or(last)?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        url.path,
        url.authority,
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("write: {e}"))?;

    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err("the connection closed before a status line".to_string()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if line.len() >= MAX_STATUS_LINE {
                    return Err("the status line is too long".to_string());
                }
                line.push(byte[0]);
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let line = String::from_utf8_lossy(&line);
    let mut parts = line.trim_end().split(' ');
    let version = parts.next().unwrap_or("");
    let status = parts.next().and_then(|s| s.parse::<u16>().ok());
    match status {
        Some(status) if version.starts_with("HTTP/") => Ok(status),
        _ => Err(format!("not an HTTP status line: {}", line.trim_end())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> Options {
        Options { log: Some(Arc::new(|_: Level, _: &str| {})), ..Options::default() }
    }

    /// Wait up to ten seconds for `done`.
    fn eventually(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    #[test]
    fn the_tokenizer_splits_the_way_the_cpp_does() {
        assert_eq!(tokenize("notify  %s\t%h\n"), ["notify", "%s", "%h"]);
        // Quotes group, may sit mid-word, and either kind closes only itself.
        assert_eq!(tokenize(r#"a"b c"d"#), ["ab cd"]);
        assert_eq!(tokenize(r#"say 'it''s "fine"'"#), ["say", r#"its "fine""#]);
        // An empty pair is an empty argument; an open quote runs to the end.
        assert_eq!(tokenize(r#"x "" y"#), ["x", "", "y"]);
        assert_eq!(tokenize("x 'open to the end"), ["x", "open to the end"]);
        // No escapes: a Windows path keeps its backslashes.
        assert_eq!(tokenize(r#""C:\Program Files\hook.exe" %s"#), [r"C:\Program Files\hook.exe", "%s"]);
        assert_eq!(tokenize(r"a\ b"), [r"a\", "b"]);
        assert!(tokenize("   \t ").is_empty());
    }

    #[test]
    fn placeholders_are_substituted_inside_one_argument() {
        let values = [('s', "ab cd".to_string()), ('h', "42".to_string()), ('p', "%h".to_string())];
        assert_eq!(substitute("%s", &values), "ab cd", "a value with a space stays one argument");
        assert_eq!(substitute("--height=%h", &values), "--height=42");
        assert_eq!(substitute("100%%", &values), "100%");
        assert_eq!(substitute("%%s", &values), "%s", "%% is consumed before the s is looked at");
        assert_eq!(substitute("%x%h", &values), "%x42", "an unknown placeholder stays and the next one still works");
        assert_eq!(substitute("tail%", &values), "tail%");
        assert_eq!(substitute("%p", &values), "%h", "a substituted value is not scanned again");
        assert_eq!(substitute("é%hé", &values), "é42é");

        // The whole pipeline: tokens first, then values.
        let argv: Vec<String> = tokenize("notify %s '%h blocks'").iter().map(|t| substitute(t, &values)).collect();
        assert_eq!(argv, ["notify", "ab cd", "42 blocks"]);
    }

    #[test]
    fn the_webhook_body_is_the_cpp_body() {
        let n = Notification {
            event: "tx".into(),
            placeholders: Vec::new(),
            fields: vec![
                Field::string("hash", "ab\"c\\d"),
                Field::raw("height", "7"),
                Field::raw("fee", ""),
                Field::raw("confirmed", "true"),
                Field::string("paymentId", "line\nbreak\u{1}"),
            ],
        };
        assert_eq!(
            build_json(&n),
            r#"{"event":"tx","hash":"ab\"c\\d","height":7,"fee":null,"confirmed":true,"paymentId":"line\nbreak\u0001"}"#
        );
    }

    #[test]
    fn urls_are_recognised_and_split_the_way_the_cpp_splits_them() {
        assert!(is_url("HTTP://example.com"));
        assert!(is_url("https://example.com/x"));
        assert!(!is_url("httpd --serve"));
        assert!(!is_url("/usr/bin/notify"));

        let u = Url::parse("http://example.com").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("example.com", 80, "/"));
        let u = Url::parse("http://example.com:8080?token=1").unwrap();
        assert_eq!((u.port, u.path.as_str()), (8080, "/?token=1"));
        let u = Url::parse("https://[::1]:9443/hook/tx?a=b").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("::1", 9443, "/hook/tx?a=b"));
        let u = Url::parse("https://[::1]/").unwrap();
        assert_eq!(u.port, 443);

        for bad in ["http://", "http:///path", "http://user:pw@host/", "http://host:notaport/", "http://[::1/"] {
            assert!(Url::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_spec_is_trimmed_and_an_unusable_one_disables_the_hook() {
        let off = Notifier::with_options("tx-notify", " \t\r\n", quiet());
        assert!(!off.enabled());
        off.notify(Notification::default());
        assert_eq!(off.dropped(), 0, "a disabled hook does not even count");

        let command = Notifier::with_options("tx-notify", "  notify %s  ", quiet());
        assert!(command.enabled());
        assert!(!command.is_webhook());
        assert_eq!(command.spec(), "notify %s");

        // No TLS client supplied: an https hook is off, as in a C++ build with
        // no OpenSSL. With one, it is on.
        assert!(!Notifier::with_options("tx-notify", "https://example.com/", quiet()).enabled());
        let post: PostFn = Arc::new(|_: &str, _: &str, _: Duration| Ok(200));
        let tls = Notifier::with_options("tx-notify", "https://example.com/", Options { post: Some(post), ..quiet() });
        assert!(tls.enabled() && tls.is_webhook());

        assert!(!Notifier::with_options("tx-notify", "http://user:secret@example.com/", quiet()).enabled());
    }

    #[test]
    fn the_description_leaves_out_what_a_log_should_not_keep() {
        let hook = Notifier::with_options("tx-notify", "http://example.com:81/tx?token=hunter2", quiet());
        assert_eq!(hook.describe(), "http://example.com:81/tx?…");
        let hook = Notifier::with_options("tx-notify", "notify --key hunter2 %s", quiet());
        assert_eq!(hook.describe(), "notify with 3 argument(s)");
        assert!(!format!("{hook:?}").contains("hunter2"));
    }

    #[test]
    fn a_full_queue_drops_and_counts_rather_than_blocking() {
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&said);
        let options = Options {
            max_queue: 2,
            log: Some(Arc::new(move |_: Level, line: &str| sink.lock().unwrap().push(line.to_string()))),
            ..Options::default()
        };
        // Driven through `Shared` directly: no worker is draining it.
        let shared = Shared::new("tx-notify", &options);
        assert!(shared.enqueue(Notification::default()));
        assert!(shared.enqueue(Notification::default()));
        for _ in 0..1500 {
            assert!(!shared.enqueue(Notification::default()));
        }
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 1500);
        let said = said.lock().unwrap();
        assert_eq!(said.len(), 2, "the first drop and the thousandth, not every one: {said:?}");
        assert!(said[0].starts_with("[tx-notify] queue full (2)"), "{}", said[0]);

        shared.lock().stopping = true;
        assert!(!shared.enqueue(Notification::default()), "a stopped hook queues nothing");
    }

    /// Read one whole request — head, then `Content-Length` bytes of body — so
    /// the server never closes with unread input, which some stacks answer
    /// with a reset the client would see as a transport failure.
    fn read_request(stream: &mut TcpStream) -> String {
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let text = String::from_utf8_lossy(&got).into_owned();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if got.len() >= end + 4 + length {
                    return text;
                }
            }
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                return String::from_utf8_lossy(&got).into_owned();
            }
            got.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn a_webhook_posts_the_body_to_the_path() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").unwrap();
            request
        });

        let hook = Notifier::with_options("block-notify", &format!("http://127.0.0.1:{port}/hooks/block?x=1"), quiet());
        hook.notify(Notification {
            event: "block".into(),
            placeholders: vec![('s', "ab".into())],
            fields: vec![Field::raw("height", "5"), Field::string("hash", "ab")],
        });

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /hooks/block?x=1 HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Content-Type: application/json\r\n"), "{request}");
        assert!(request.ends_with(r#"{"event":"block","height":5,"hash":"ab"}"#), "{request}");
        assert!(eventually(|| hook.sent() == 1), "a 2xx is a success");
    }

    #[test]
    fn a_webhook_that_answers_an_error_status_fails_without_a_retry() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&accepted);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                count.fetch_add(1, Ordering::SeqCst);
                read_request(&mut stream);
                let _ = stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
            }
        });

        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&said);
        let options = Options {
            log: Some(Arc::new(move |_: Level, line: &str| sink.lock().unwrap().push(line.to_string()))),
            ..Options::default()
        };
        let hook = Notifier::with_options("tx-notify", &format!("http://127.0.0.1:{port}/"), options);
        for _ in 0..3 {
            hook.notify(Notification { event: "tx".into(), ..Default::default() });
        }
        assert!(eventually(|| hook.failed() == 3));
        assert_eq!(accepted.load(Ordering::SeqCst), 3, "an HTTP status is final: one request per notification");
        let warnings: Vec<String> =
            said.lock().unwrap().iter().filter(|l| l.contains("answered HTTP 500")).cloned().collect();
        assert_eq!(warnings.len(), 1, "a run of failures is reported once: {warnings:?}");
    }

    #[test]
    fn a_program_that_is_not_there_fails_and_says_so() {
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&said);
        let options = Options {
            log: Some(Arc::new(move |_: Level, line: &str| sink.lock().unwrap().push(line.to_string()))),
            ..Options::default()
        };
        let hook = Notifier::with_options("tx-notify", "wrkz-no-such-program-anywhere %s", options);
        hook.notify(Notification::default());
        assert!(eventually(|| hook.failed() == 1));
        assert!(
            said.lock().unwrap().iter().any(|l| l.contains("failed to start 'wrkz-no-such-program-anywhere'")),
            "{:?}",
            said.lock().unwrap()
        );
    }

    /// A shell-free command that writes its substituted arguments to a file.
    #[cfg(unix)]
    fn recorder(out: &std::path::Path) -> String {
        format!("sh -c 'printf \"%%s|%%s\" \"$1\" \"$2\" > {}' notify %s '%h blocks'", out.display())
    }

    #[cfg(windows)]
    fn recorder(out: &std::path::Path) -> String {
        // `cmd /C` takes the rest of its command line as one command; the two
        // values carry no character cmd.exe treats specially.
        format!("cmd /C echo %s^|%h> {}", out.display())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_command_runs_with_its_placeholders_substituted() {
        let dir = std::env::temp_dir().join(format!("wrkz-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("args.txt");
        let _ = std::fs::remove_file(&out);

        let hook = Notifier::with_options("tx-notify", &recorder(&out), quiet());
        hook.notify(Notification {
            event: "tx".into(),
            placeholders: vec![('s', "deadbeef".into()), ('h', "4213000".into())],
            fields: Vec::new(),
        });
        assert!(eventually(|| hook.sent() == 1), "sent={} failed={}", hook.sent(), hook.failed());

        let written = std::fs::read_to_string(&out).unwrap();
        #[cfg(unix)]
        assert_eq!(written, "deadbeef|4213000 blocks");
        #[cfg(windows)]
        assert_eq!(written.trim_end(), "deadbeef|4213000");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_command_that_outlives_its_timeout_is_killed() {
        #[cfg(unix)]
        let spec = "sleep 30";
        #[cfg(windows)]
        let spec = "ping -n 30 127.0.0.1";
        let options = Options { timeout: Duration::from_millis(300), ..quiet() };
        let hook = Notifier::with_options("tx-notify", spec, options);
        let started = Instant::now();
        hook.notify(Notification::default());
        assert!(eventually(|| hook.failed() == 1));
        assert!(started.elapsed() < Duration::from_secs(10), "killed at the timeout, not left to finish");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn stopping_kills_a_command_in_flight_and_discards_the_queue() {
        #[cfg(unix)]
        let spec = "sleep 30";
        #[cfg(windows)]
        let spec = "ping -n 30 127.0.0.1";
        let hook = Notifier::with_options("tx-notify", spec, quiet());
        for _ in 0..5 {
            hook.notify(Notification::default());
        }
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        hook.stop();
        assert!(started.elapsed() < Duration::from_secs(5), "stop does not wait out a 30 s command");
        assert!(!hook.enabled());
        assert_eq!(hook.sent(), 0);
        assert!(hook.failed() <= 1, "only the delivery in flight ran");
        hook.notify(Notification::default());
        hook.stop();
    }
}
