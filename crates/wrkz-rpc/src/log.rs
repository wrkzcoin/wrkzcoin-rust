// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A five-level logger over stderr, with the C++ daemon's level names and a
//! timestamp an operator can read.
//!
//! Deliberately not a logging crate: the node needs a level filter, a
//! timestamp, an optional file and a way not to scribble over the console's
//! prompt, and nothing else. The level is a process-wide atomic so the
//! connection threads and the engine share it without a lock.
//!
//! # The line
//!
//! ```text
//! [2026-09-10 14:03:22.123Z INFO   ] chain state opened at height 4213001
//! ```
//!
//! The level names and the 0-4 numbering are the C++ daemon's
//! (`src/logging/ILogger.cpp`, `DaemonCommandsHandler::set_log`): `--log-level 0`
//! and `set_log 0` are `ERROR`, 4 is `TRACE`, and the tag text matches so one
//! `grep WARNING` reads a log from either implementation.
//!
//! The timestamp does **not** match the C++ byte for byte. The C++ stamps
//! `2026-Sep-10 14:03:22.123456` in *local* time
//! (`src/logging/CommonLogger.cpp:40`); this is ISO-8601 in UTC, to
//! milliseconds. Local time would mean calling `localtime_r` through an
//! assumed `struct tm` layout — unsafe FFI for a log prefix — and it makes two
//! nodes' logs impossible to line up across time zones and ambiguous across a
//! DST fold. The `Z` says which it is, so nobody has to guess.
//!
//! # The file (`--log-file`)
//!
//! Every line also goes to the log file, if one is set. The C++ `FileLogger`
//! opens the file in append mode and never looks at it again, so a busy node
//! at `debug` fills the disk and takes itself down; this one carries a cap
//! ([`DEFAULT_MAX_FILE_BYTES`]) and reclaims the file when it is reached.
//! [`set_file_at`] does that by rotation and works everywhere; [`set_file`],
//! which is handed a file somebody else opened, can only truncate in place and
//! so depends on the platform. Neither can fail silently: a full disk, a
//! removed mount or a handle that will not truncate is reported once on
//! stderr — the C++ drops every line with no message at all.
//!
//! # Sharing the terminal with the console
//!
//! The daemon console (`wrkz_node::console`) writes to the same terminal these
//! lines go to, from a different thread, while blocks are arriving. One
//! process-wide [`Mutex`] therefore guards *every* write: a log line and a
//! command's output can never interleave halfway through, whichever thread got
//! there first. Nothing is ever held across anything but the write itself, and
//! a line is formatted *before* the lock is taken — so a `Display` impl that
//! logs cannot deadlock the logger against itself.
//!
//! When the console is attached to a real terminal it registers its prompt with
//! [`set_prompt`]. A log line then erases the prompt, prints itself and redraws
//! the prompt. Two limits are worth knowing, because they are properties of
//! reading stdin in the terminal's own line mode rather than in raw mode:
//!
//! - **the half-typed line is not restored.** The characters the operator had
//!   typed are in the terminal driver's buffer, not ours; we cannot read them
//!   without taking the terminal out of canonical mode. They are still there
//!   and still submitted on Enter — they are simply not on screen any more.
//! - **a typed line that wrapped leaves its earlier rows behind.** The erase
//!   clears the row the cursor is on, and nothing portable clears the rows
//!   above it.
//!
//! What is escaped depends on where the stream actually goes, decided once by
//! [`terminal`]: with stderr redirected to a file, or `TERM=dumb`, or a Windows
//! console that will not take virtual-terminal sequences, not one escape byte
//! is written and the prompt is erased with spaces or not at all.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Mirrors `Logging::Level` in `src/logging/ILogger.h`, offset the way the C++
/// daemon offsets it: `set_log 0` and `--log-level 0` are `ERROR`, not `FATAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    /// The tag in the line prefix. The text is `ILogger::LEVEL_NAMES`, padded
    /// to seven as the C++ `%L` field is (`setw(7) left`), so the message
    /// column lines up and a grep written for one log finds the other.
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR  ",
            Level::Warn => "WARNING",
            Level::Info => "INFO   ",
            Level::Debug => "DEBUG  ",
            Level::Trace => "TRACE  ",
        }
    }

    /// `0`-`4`, the numbering the C++ CLI and its `set_log` command take
    /// (`DaemonCommandsHandler::set_log`, where `0` is ERROR). Returns `None`
    /// for anything that is not one of those five numbers, including a number
    /// out of range, so a caller can tell "not a number" from "bad number"
    /// by trying [`Level::parse`] as well.
    pub fn from_number(s: &str) -> Option<Level> {
        match s.parse::<u16>() {
            Ok(0) => Some(Level::Error),
            Ok(1) => Some(Level::Warn),
            Ok(2) => Some(Level::Info),
            Ok(3) => Some(Level::Debug),
            Ok(4) => Some(Level::Trace),
            _ => None,
        }
    }

    /// `error`, `warn`, `info`, `debug` or `trace`, for the command line.
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    (level as u8) <= LEVEL.load(Ordering::Relaxed)
}

static JSON: AtomicBool = AtomicBool::new(false);

/// `--log-format json`: every line becomes one JSON object,
/// `{"time":"2026-09-10T14:03:22.123Z","level":"INFO","message":"…"}`, for a
/// log shipper (journald's JSON export, Loki, Elasticsearch) to read without
/// a parser for the C++-style prefix. The terminal, the log file and
/// `log_tail` all carry the same line.
pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

/// One JSON log line. `stamp` is [`stamp`]'s text; the time becomes RFC 3339
/// (`T` between date and time) and the level loses its column padding.
fn json_line(stamp: &str, level: Level, message: &str) -> String {
    let mut line = String::with_capacity(message.len() + 72);
    line.push_str("{\"time\":\"");
    line.push_str(&stamp.replacen(' ', "T", 1));
    line.push_str("\",\"level\":\"");
    line.push_str(level.tag().trim_end());
    line.push_str("\",\"message\":\"");
    for c in message.chars() {
        match c {
            '"' => line.push_str("\\\""),
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\r' => line.push_str("\\r"),
            '\t' => line.push_str("\\t"),
            c if (c as u32) < 0x20 => line.push_str(&format!("\\u{:04x}", c as u32)),
            c => line.push(c),
        }
    }
    line.push_str("\"}");
    line
}

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

/// Civil year, month, day from a count of days since 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`, which is exact for every day a `u64` of
/// seconds can name and needs no table, no leap-second list and no allocation.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `YYYY-MM-DD HH:MM:SSZ` from seconds since the epoch, UTC.
///
/// Public because the console prints block timestamps with it: a block header
/// whose `Time` column is a bare epoch is not something an operator can read.
pub fn format_time_utc(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", rest / 3600, (rest / 60) % 60, rest % 60)
}

/// The line prefix's timestamp: [`format_time_utc`] with milliseconds.
fn stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let (y, m, d) = civil_from_days((now.as_secs() / 86_400) as i64);
    let rest = now.as_secs() % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60,
        now.subsec_millis()
    )
}

// ---------------------------------------------------------------------------
// what the three standard streams actually are
// ---------------------------------------------------------------------------

/// One of the process's standard streams, for [`is_terminal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

#[cfg(unix)]
pub fn is_terminal(stream: Stream) -> bool {
    unsafe extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    let fd = match stream {
        Stream::Stdin => 0,
        Stream::Stdout => 1,
        Stream::Stderr => 2,
    };
    // SAFETY: `isatty` on a file descriptor number is always defined; it
    // reports 0 for a descriptor that is not open.
    unsafe { isatty(fd) == 1 }
}

#[cfg(windows)]
mod win {
    pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    /// `ENABLE_VIRTUAL_TERMINAL_PROCESSING`: without it a Windows console
    /// prints `\x1b[2K` as four visible characters.
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe extern "system" {
        pub fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        pub fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: u32) -> i32;
    }

    pub fn handle_of(stream: super::Stream) -> u32 {
        match stream {
            super::Stream::Stdin => STD_INPUT_HANDLE,
            super::Stream::Stdout => STD_OUTPUT_HANDLE,
            super::Stream::Stderr => STD_ERROR_HANDLE,
        }
    }

    /// The console mode of `stream`, or `None` when it is a pipe or a file.
    pub fn console_mode(stream: super::Stream) -> Option<(*mut core::ffi::c_void, u32)> {
        let mut mode: u32 = 0;
        // SAFETY: both calls take a handle the process already owns and write
        // only through `mode`, which is a live local.
        unsafe {
            let handle = GetStdHandle(handle_of(stream));
            if handle.is_null() {
                return None;
            }
            if GetConsoleMode(handle, &mut mode) == 0 {
                return None;
            }
            Some((handle, mode))
        }
    }
}

#[cfg(windows)]
pub fn is_terminal(stream: Stream) -> bool {
    // A Windows console handle is the one `GetConsoleMode` succeeds on; a pipe
    // or a redirected file fails it. That is exactly the test we want.
    win::console_mode(stream).is_some()
}

/// How the prompt is taken off the screen before a line is written over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Erase {
    /// The stream is not a terminal: write nothing, ever.
    Never,
    /// A terminal that takes ANSI: carriage return and erase-to-end-of-line.
    Ansi,
    /// A terminal that does not (`TERM=dumb`, a Windows console with no
    /// virtual-terminal processing): carriage return, blanks as wide as the
    /// prompt, carriage return.
    Blanks,
}

impl Erase {
    /// Push the sequence that erases a prompt of `width` columns.
    ///
    /// `width` is clamped: a prompt is a handful of characters, and a very
    /// narrow terminal must never be handed a kilobyte of spaces to wrap.
    fn push(self, out: &mut String, width: usize) {
        match self {
            Erase::Never => {}
            Erase::Ansi => out.push_str("\r\x1b[2K"),
            Erase::Blanks => {
                out.push('\r');
                for _ in 0..width.min(MAX_ERASE_COLUMNS) {
                    out.push(' ');
                }
                out.push('\r');
            }
        }
    }
}

/// The widest prompt the [`Erase::Blanks`] fallback will blank out.
const MAX_ERASE_COLUMNS: usize = 120;

/// Whether `TERM` describes a terminal that understands ANSI sequences.
///
/// Unset is treated as "no": a process with no `TERM` is not being watched by
/// a person, and `dumb` is the value `M-x shell`, `TERM=dumb` in a CI runner
/// and a serial console all use to say exactly this.
fn ansi_from_term(term: Option<&str>) -> bool {
    !matches!(term, None | Some("") | Some("dumb"))
}

/// What each stream can be written with. Probed once: neither the descriptors
/// nor `TERM` change under a running daemon, and the answer is consulted on
/// every log line.
#[derive(Clone, Copy, Debug)]
pub struct Terminal {
    /// stdin is a terminal, so there may be an operator at a keyboard.
    pub stdin: bool,
    /// stdout is a terminal: the console's own output goes there.
    pub stdout: bool,
    /// stderr is a terminal: the log goes there.
    pub stderr: bool,
    /// How to erase a prompt on stdout.
    pub erase_stdout: Erase,
    /// How to erase a prompt on stderr. **Not** the same decision: with
    /// `wrkz-node 2>daemon.log` stdout is a terminal and stderr is a file, and
    /// writing an escape sequence or a redrawn prompt into that file is
    /// exactly the corruption this separation prevents.
    pub erase_stderr: Erase,
}

static TERMINAL: OnceLock<Terminal> = OnceLock::new();

/// What the three standard streams are, probed once per process.
pub fn terminal() -> Terminal {
    *TERMINAL.get_or_init(probe_terminal)
}

#[cfg(unix)]
fn probe_terminal() -> Terminal {
    let ansi = ansi_from_term(std::env::var("TERM").ok().as_deref());
    let mode = |tty: bool| {
        if !tty {
            Erase::Never
        } else if ansi {
            Erase::Ansi
        } else {
            Erase::Blanks
        }
    };
    let (stdout, stderr) = (is_terminal(Stream::Stdout), is_terminal(Stream::Stderr));
    Terminal {
        stdin: is_terminal(Stream::Stdin),
        stdout,
        stderr,
        erase_stdout: mode(stdout),
        erase_stderr: mode(stderr),
    }
}

#[cfg(windows)]
fn probe_terminal() -> Terminal {
    // A Windows console does not process virtual-terminal sequences unless
    // someone turns it on, and every console since Windows 10 1511 can be
    // asked to. Ask once, here; if the console refuses, fall back to blanks
    // rather than printing `←[2K` at the operator.
    //
    // `TERM` is normally unset on Windows, which says nothing; `TERM=dumb`
    // from a CI shell or a mintty session still means it.
    let term = std::env::var("TERM").ok();
    let dumb = term.is_some() && !ansi_from_term(term.as_deref());
    let mode = |stream: Stream| -> Erase {
        let Some((handle, console_mode)) = win::console_mode(stream) else { return Erase::Never };
        if dumb {
            return Erase::Blanks;
        }
        if console_mode & win::ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return Erase::Ansi;
        }
        // SAFETY: `handle` came from `GetStdHandle` a moment ago and is a
        // console handle, which `GetConsoleMode` just proved.
        let ok = unsafe { win::SetConsoleMode(handle, console_mode | win::ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0 };
        if ok {
            Erase::Ansi
        } else {
            Erase::Blanks
        }
    };
    Terminal {
        stdin: is_terminal(Stream::Stdin),
        stdout: is_terminal(Stream::Stdout),
        stderr: is_terminal(Stream::Stderr),
        erase_stdout: mode(Stream::Stdout),
        erase_stderr: mode(Stream::Stderr),
    }
}

// ---------------------------------------------------------------------------
// the log file
// ---------------------------------------------------------------------------

/// How large `--log-file` is allowed to get before the logger reclaims it.
///
/// The C++ has no such limit — `FileLogger::init` opens the file in append mode
/// and nothing ever stats, renames or truncates it, so a busy node at `debug`
/// fills the disk and takes itself down. A bounded log loses old lines; an
/// unbounded one loses the node.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// How many of the most recent lines [`recent`] keeps in memory.
pub const RECENT_CAPACITY: usize = 200;

/// The longest a line kept for [`recent`] may be; anything past this is cut.
/// A block of hex in a `debug` line must not be able to pin megabytes.
const RECENT_LINE_MAX: usize = 512;

/// The file `--log-file` named, and what is known about it.
struct LogFile {
    file: std::fs::File,
    /// The path, when the logger opened the file itself. `None` when it was
    /// handed an already-open handle, which cannot be rotated by rename.
    path: Option<PathBuf>,
    /// Bytes in the file, tracked rather than stat-ed per line.
    bytes: u64,
    /// The cap, from [`DEFAULT_MAX_FILE_BYTES`] or [`set_file_at`].
    max_bytes: u64,
    /// A write failure has already been reported; do not say it again on every
    /// line for the rest of the run.
    reported_error: bool,
}

impl LogFile {
    fn new(file: std::fs::File, path: Option<PathBuf>, max_bytes: u64) -> Self {
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Self { file, path, bytes, max_bytes, reported_error: false }
    }

    /// Append one line, and reclaim the file if it has grown past its cap.
    ///
    /// Returns a message for stderr the first time something goes wrong — a
    /// failed write, or a file that can be neither rotated nor truncated. The
    /// caller prints it, because it already holds the stream, and it is said
    /// once rather than once per line for the rest of the run.
    fn write_line(&mut self, line: &str) -> Option<String> {
        match writeln!(self.file, "{line}") {
            Ok(()) => {
                self.bytes += line.len() as u64 + 1;
                if self.bytes > self.max_bytes {
                    return self.reclaim();
                }
                None
            }
            Err(e) => self.report(format!(
                "log file: {e}. Nothing more will be written to it; stderr and the console are unaffected."
            )),
        }
    }

    /// Say something on stderr once, and never again for this file.
    fn report(&mut self, what: String) -> Option<String> {
        if self.reported_error {
            return None;
        }
        self.reported_error = true;
        Some(what)
    }

    /// Bring the file back under its cap.
    ///
    /// With a path, one generation is kept: `daemon.log` becomes
    /// `daemon.log.1` and a new `daemon.log` is opened, so the operator still
    /// has the window before the rotation. That is the path [`set_file_at`]
    /// takes, and it works everywhere.
    ///
    /// Without one — [`set_file`] was handed a handle somebody else opened —
    /// there is no name to rename, so the only move left is to truncate the
    /// file in place. That works on a POSIX handle (`O_APPEND` is a write mode
    /// and `ftruncate` is content with it) and **not** on a Windows one:
    /// `FILE_APPEND_DATA` alone does not carry the right `SetEndOfFile` needs.
    /// When it fails, the file is left to grow and the operator is told so
    /// once, because a log that silently ignores its own cap is worse than one
    /// that admits it has none. The daemon itself is never affected either way.
    fn reclaim(&mut self) -> Option<String> {
        if let Some(path) = self.path.clone() {
            let previous = {
                let mut name = path.clone().into_os_string();
                name.push(".1");
                PathBuf::from(name)
            };
            let _ = std::fs::remove_file(&previous);
            if std::fs::rename(&path, &previous).is_ok() {
                if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    self.file = file;
                    let note = format!("[log rotated: the previous file is {}]", previous.display());
                    self.bytes = match writeln!(self.file, "{note}") {
                        Ok(()) => note.len() as u64 + 1,
                        Err(_) => 0,
                    };
                    return None;
                }
            }
        }
        let kept = self.bytes;
        if self.file.set_len(0).is_ok() {
            self.bytes = 0;
            let note = format!("[log truncated at {kept} bytes: the cap is {} bytes]", self.max_bytes);
            if writeln!(self.file, "{note}").is_ok() {
                self.bytes = note.len() as u64 + 1;
            }
            return None;
        }
        // Nothing can bring this file back under its cap. Stop checking, and
        // say so exactly once.
        self.max_bytes = u64::MAX;
        self.report(format!(
            "log file: it passed {kept} bytes and this handle can be neither rotated nor truncated, \
             so it will keep growing. Rotate it from outside the daemon (copytruncate, not rename: \
             the handle stays open)."
        ))
    }
}

/// Everything a write to the terminal needs, behind one lock.
#[derive(Default)]
struct Out {
    /// The `--log-file` sink, when there is one.
    file: Option<LogFile>,
    /// The console's prompt, when one is drawn on the terminal right now.
    prompt: Option<String>,
    /// The last [`RECENT_CAPACITY`] lines, for the console's `log_tail`. The
    /// only way an operator whose stderr is redirected can see what the node
    /// has just said without leaving the console.
    recent: VecDeque<String>,
}

/// The one lock every writer to the terminal takes: the logger here, and the
/// console's own output. Nothing is held across anything but the write itself,
/// and every line is formatted before it is taken.
static OUT: Mutex<Out> = Mutex::new(Out { file: None, prompt: None, recent: VecDeque::new() });

fn out() -> MutexGuard<'static, Out> {
    // A panic while a line was being written must not silence the log for the
    // rest of the run: the state behind this lock is a file handle, a prompt
    // and a ring of strings, none of which a panic can leave inconsistent.
    OUT.lock().unwrap_or_else(|p| p.into_inner())
}

/// Append every line to this file as well as to stderr (`--log-file`).
///
/// The handle is used as it was given, and the logger does not know its name,
/// so it cannot be rotated: past [`DEFAULT_MAX_FILE_BYTES`] the only move left
/// is truncating it in place, which POSIX allows on an appending descriptor and
/// Windows does not. **Prefer [`set_file_at`]**, which owns the path, keeps one
/// previous generation and is bounded on every platform.
pub fn set_file(file: std::fs::File) {
    out().file = Some(LogFile::new(file, None, DEFAULT_MAX_FILE_BYTES));
}

/// Append every line to the file at `path`, capped at `max_bytes`.
///
/// Creates the file and its parent directory. Because the logger knows the
/// path, reaching the cap rotates: `path` becomes `path.1` and logging carries
/// on in a fresh `path`.
pub fn set_file_at(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    // The log carries peer addresses and this node's peer id, so it is not
    // world-readable. Only applies when we create it; an existing file keeps
    // whatever the operator chose.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o640);
    }
    let file = opts.open(path)?;
    out().file = Some(LogFile::new(file, Some(path.to_path_buf()), max_bytes.max(1024)));
    Ok(())
}

/// Flush the log file, if there is one. Called on the way out.
pub fn flush_file() {
    if let Some(f) = out().file.as_mut() {
        let _ = f.file.flush();
    }
}

/// The last `count` lines the logger emitted, oldest first, capped at
/// [`RECENT_CAPACITY`]. The console's `log_tail` prints these.
pub fn recent(count: usize) -> Vec<String> {
    let guard = out();
    let take = count.min(guard.recent.len());
    guard.recent.iter().skip(guard.recent.len() - take).cloned().collect()
}

/// Tell the logger a prompt is on screen, so it can redraw it after each line.
///
/// `None` (the default) means no prompt: no escape sequence is ever written.
/// The console sets it only when stdin *and* stdout are terminals, and clears
/// it when the reader stops.
pub fn set_prompt(prompt: Option<String>) {
    out().prompt = prompt;
}

/// Draw the prompt on stdout again with nothing above it.
///
/// The console calls this after a command that printed nothing — a blank line —
/// so that pressing Enter does not leave the operator staring at a bare row.
pub fn redraw_prompt() {
    let guard = out();
    if !terminal().stdout {
        return;
    }
    if let Some(prompt) = guard.prompt.as_deref() {
        let mut sink = std::io::stdout().lock();
        let _ = sink.write_all(prompt.as_bytes());
        let _ = sink.flush();
    }
    drop(guard);
}

impl Out {
    /// Write `line` (no trailing newline) to stderr, redrawing the prompt
    /// around it. The caller holds the lock.
    fn emit(&mut self, line: &str) {
        self.remember(line);

        let term = terminal();
        // The prompt was drawn on stdout; it may only be erased and redrawn
        // here when stderr is the same kind of place — a terminal. With stderr
        // going to a file or a journal, this writes the bare line and leaves
        // the prompt on the operator's screen untouched.
        let redraw = self.prompt.as_deref().filter(|_| term.stderr);
        let mut buffer = String::with_capacity(line.len() + 32);
        if let Some(prompt) = redraw {
            term.erase_stderr.push(&mut buffer, prompt.chars().count());
        }
        buffer.push_str(line);
        buffer.push('\n');
        if let Some(prompt) = redraw {
            buffer.push_str(prompt);
        }

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(buffer.as_bytes());
        if redraw.is_some() {
            // Nothing follows the prompt, so nothing would flush it.
            let _ = err.flush();
        }
        drop(err);

        // A log file that cannot be written must not take the node down with
        // it, and must not print a second error per line either.
        if let Some(f) = self.file.as_mut() {
            if let Some(problem) = f.write_line(line) {
                let _ = writeln!(std::io::stderr().lock(), "[{} ERROR  ] {problem}", stamp());
            }
        }
    }

    /// Keep the line for `log_tail`, bounded in both directions.
    fn remember(&mut self, line: &str) {
        let kept = if line.len() > RECENT_LINE_MAX {
            let mut cut = RECENT_LINE_MAX;
            while cut > 0 && !line.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &line[..cut])
        } else {
            line.to_string()
        };
        if self.recent.len() == RECENT_CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back(kept);
    }
}

/// Write one line if `level` passes the filter. Called through the macros.
pub fn log(level: Level, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    // Formatted *before* the lock: a `Display` impl that logs would otherwise
    // re-enter a mutex it already holds, and `std::sync::Mutex` is not
    // reentrant — it would deadlock the node rather than print twice.
    let line = if JSON.load(Ordering::Relaxed) {
        json_line(&stamp(), level, &args.to_string())
    } else {
        format!("[{} {}] {}", stamp(), level.tag(), args)
    };
    out().emit(&line);
}

/// Write a block of console output to **stdout**, under the same lock the log
/// lines take, so the two can never interleave mid-line.
///
/// `text` is written as-is and a newline is added if it does not end with one;
/// the prompt is redrawn afterwards when there is one. Nothing goes to the log
/// file or to [`recent`]: `--log-file` is the daemon's log, not a transcript of
/// what an operator typed at it.
pub fn console_print(text: &str) {
    // Held for the whole write, which is the point: a log line arriving on
    // another thread waits here rather than landing in the middle of a table.
    let guard = out();
    let term = terminal();
    let prompt = guard.prompt.as_deref().filter(|_| term.stdout);
    let mut sink = std::io::stdout().lock();
    if let Some(prompt) = prompt {
        let mut erase = String::new();
        term.erase_stdout.push(&mut erase, prompt.chars().count());
        let _ = sink.write_all(erase.as_bytes());
    }
    let _ = sink.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = sink.write_all(b"\n");
    }
    if let Some(prompt) = prompt {
        let _ = sink.write_all(prompt.as_bytes());
    }
    let _ = sink.flush();
    drop(guard);
}

#[macro_export]
macro_rules! log_error {
    ($($a:tt)*) => { $crate::log::log($crate::log::Level::Error, format_args!($($a)*)) };
}
#[macro_export]
macro_rules! log_warn {
    ($($a:tt)*) => { $crate::log::log($crate::log::Level::Warn, format_args!($($a)*)) };
}
#[macro_export]
macro_rules! log_info {
    ($($a:tt)*) => { $crate::log::log($crate::log::Level::Info, format_args!($($a)*)) };
}
#[macro_export]
macro_rules! log_debug {
    ($($a:tt)*) => { $crate::log::log($crate::log::Level::Debug, format_args!($($a)*)) };
}
#[macro_export]
macro_rules! log_trace {
    ($($a:tt)*) => { $crate::log::log($crate::log::Level::Trace, format_args!($($a)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_filter() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("nonsense"), None);
        set_level(Level::Warn);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Warn));
        assert!(!enabled(Level::Info));
        set_level(Level::Info);
    }

    #[test]
    fn the_level_tags_are_the_cpp_names_in_one_column() {
        // src/logging/ILogger.cpp: LEVEL_NAMES, in `setw(7) left`.
        assert_eq!(Level::Error.tag().trim_end(), "ERROR");
        assert_eq!(Level::Warn.tag().trim_end(), "WARNING");
        assert_eq!(Level::Info.tag().trim_end(), "INFO");
        assert_eq!(Level::Debug.tag().trim_end(), "DEBUG");
        assert_eq!(Level::Trace.tag().trim_end(), "TRACE");
        for level in [Level::Error, Level::Warn, Level::Info, Level::Debug, Level::Trace] {
            assert_eq!(level.tag().len(), 7, "{level:?} does not line up");
        }
        // And the numbering the C++ `set_log` and `--log-level` share.
        assert_eq!(Level::Error as u8, 0);
        assert_eq!(Level::Trace as u8, 4);
    }

    #[test]
    fn a_json_line_is_one_escaped_object() {
        let line = json_line("2026-09-10 14:03:22.123Z", Level::Warn, "peer \"a\\b\"\nsaid\t\u{1}ok");
        assert_eq!(
            line,
            r#"{"time":"2026-09-10T14:03:22.123Z","level":"WARNING","message":"peer \"a\\b\"\nsaid\t\u0001ok"}"#
        );
        assert!(!line.contains('\n'), "one physical line, whatever the message holds");
    }

    #[test]
    fn the_timestamp_is_a_date_a_person_can_read() {
        assert_eq!(format_time_utc(0), "1970-01-01 00:00:00Z");
        // The three every calendar implementation gets wrong if it is wrong.
        assert_eq!(format_time_utc(951_782_400), "2000-02-29 00:00:00Z", "2000 is a leap year");
        assert_eq!(format_time_utc(1_000_000_000), "2001-09-09 01:46:40Z");
        assert_eq!(format_time_utc(1_700_000_000), "2023-11-14 22:13:20Z");
        // 2100 is not a leap year: 2100-02-28 plus a day is March.
        assert_eq!(format_time_utc(4_107_542_400), "2100-03-01 00:00:00Z");
        // Every second of a day round-trips to the right wall clock.
        assert_eq!(format_time_utc(86_399), "1970-01-01 23:59:59Z");
        assert_eq!(format_time_utc(86_400), "1970-01-02 00:00:00Z");
        // And the live stamp has the shape the log line promises.
        let now = stamp();
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert!(now.starts_with("20"), "{now}");
    }

    #[test]
    fn a_dumb_or_redirected_stream_gets_no_escape_bytes() {
        assert!(!ansi_from_term(None), "no TERM is nobody watching");
        assert!(!ansi_from_term(Some("")), "an empty TERM is the same");
        assert!(!ansi_from_term(Some("dumb")));
        assert!(ansi_from_term(Some("xterm-256color")));

        let mut s = String::new();
        Erase::Never.push(&mut s, 11);
        assert_eq!(s, "", "a redirected stream never gets an escape or a blank");

        s.clear();
        Erase::Ansi.push(&mut s, 11);
        assert_eq!(s, "\r\x1b[2K");

        s.clear();
        Erase::Blanks.push(&mut s, 11);
        assert_eq!(s, "\r           \r", "a dumb terminal is blanked, not escaped");
        assert!(!s.contains('\x1b'));

        // A prompt no terminal is that wide for cannot become a kilobyte of
        // spaces to wrap over the operator's scrollback.
        s.clear();
        Erase::Blanks.push(&mut s, 100_000);
        assert_eq!(s.len(), MAX_ERASE_COLUMNS + 2);
    }

    #[test]
    fn the_recent_ring_is_bounded_in_both_directions() {
        // Held for the whole check: the ring is process-wide and another test
        // in this binary may be logging into it at the same time.
        let mut guard = out();
        guard.recent.clear();
        for i in 0..RECENT_CAPACITY * 2 {
            guard.remember(&format!("line {i}"));
        }
        assert_eq!(guard.recent.len(), RECENT_CAPACITY, "the ring does not grow");
        assert_eq!(guard.recent.back().map(String::as_str), Some("line 399"), "newest last");
        assert_eq!(guard.recent.front().map(String::as_str), Some("line 200"), "oldest first, and the rest dropped");

        // A very long line is cut, and cut on a character boundary.
        guard.remember(&"é".repeat(4000));
        let kept = guard.recent.back().unwrap();
        assert!(kept.len() <= RECENT_LINE_MAX + 4, "a long line is not kept whole: {}", kept.len());
        assert!(kept.ends_with('…'), "and says it was cut");
        guard.recent.clear();
        drop(guard);

        // Asking for more than there is, or for everything, is not a panic.
        assert!(recent(usize::MAX).len() <= RECENT_CAPACITY);
        assert!(recent(0).is_empty());
    }

    #[test]
    fn the_log_file_is_bounded_and_keeps_one_generation() {
        let dir = std::env::temp_dir().join(format!("wrkz-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("deep").join("daemon.log");

        // Small enough to reach in a test; `set_file_at` floors the cap at 1 KiB.
        // The lines go through `LogFile` directly rather than through `emit`,
        // which would also put a hundred lines on the test runner's stderr.
        let write_lines = |count: usize| {
            let mut guard = out();
            let file = guard.file.as_mut().expect("a log file is set");
            for i in 0..count {
                file.write_line(&format!("{i:04} {}", "x".repeat(60)));
            }
        };

        set_file_at(&path, 1024).expect("the file and its directory are created");
        write_lines(60);
        let previous = dir.join("deep").join("daemon.log.1");
        assert!(path.exists(), "the live file is still there");
        assert!(previous.exists(), "one generation is kept, so the window before the rotation survives");
        let live = std::fs::metadata(&path).unwrap().len();
        assert!(live <= 1024 + 200, "the live file is bounded, not {live} bytes");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[log rotated:"), "the rotation says what happened:\n{text}");
        let kept = text + &std::fs::read_to_string(&previous).unwrap();
        assert!(kept.contains("0059 "), "the newest line is in one of the two generations");

        out().file = None;

        // A handle the logger did not open has no name to rename, so the most
        // it can do is truncate itself. Where that works — a POSIX `O_APPEND`
        // descriptor — the file stays under its cap; where it does not — a
        // Windows `FILE_APPEND_DATA` handle — the operator is told once, rather
        // than left with a cap that quietly does nothing. Both are asserted
        // here because both are shipped.
        let plain = dir.join("plain.log");
        let handle = std::fs::OpenOptions::new().create(true).append(true).open(&plain).unwrap();
        let mut file = LogFile::new(handle, None, 1024);
        let mut said = None;
        for i in 0..60 {
            said = file.write_line(&format!("{i:04} {}", "x".repeat(60))).or(said);
        }
        drop(file);
        let size = std::fs::metadata(&plain).unwrap().len();
        let text = std::fs::read_to_string(&plain).unwrap();
        if size <= 1024 + 200 {
            assert!(text.contains("[log truncated at"), "the truncation says what happened:\n{text}");
            assert!(said.is_none(), "nothing went wrong, so nothing was reported: {said:?}");
        } else {
            let said = said.expect("a log file that cannot be capped is reported, not silently grown");
            assert!(said.contains("keep growing"), "{said}");
            assert!(said.contains("copytruncate"), "and says what to do about it: {said}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prompt_is_never_redrawn_onto_a_stream_that_is_not_a_terminal() {
        // The whole point of the split: `cargo test` has stderr on a pipe, so
        // whatever the probe found for stdout, stderr must take no escapes.
        let term = terminal();
        if !term.stderr {
            assert_eq!(term.erase_stderr, Erase::Never);
        }
        if !term.stdout {
            assert_eq!(term.erase_stdout, Erase::Never);
        }
        // Emitting with a prompt registered writes the bare line to the ring:
        // the decoration is never part of the line, so it can never reach a
        // log file or `log_tail` either.
        set_prompt(Some("wrkz-node> ".to_string()));
        {
            let mut guard = out();
            guard.recent.clear();
            guard.emit("a line while a prompt is registered");
            assert!(guard.recent.iter().all(|l| !l.contains('\x1b')), "{:?}", guard.recent);
            guard.recent.clear();
        }
        set_prompt(None);
    }
}
