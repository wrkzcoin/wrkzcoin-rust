// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A line editor with history, for the prompts an operator types commands at:
//! the `wrkz-node` console, `wrkz-node attach` and `wrkz-wallet`'s command
//! prompt.
//!
//! The C++ reads those through linenoise, which gives Up and Down through the
//! lines typed before and editing inside the line. This is that and no more: no
//! tab completion, no hints, and the history lasts as long as the process.
//!
//! | key | does |
//! | --- | --- |
//! | Up, Ctrl-P / Down, Ctrl-N | the previous / next line in the history; Down past the newest is the line being typed again |
//! | Left, Ctrl-B / Right, Ctrl-F | a character left / right |
//! | Home, Ctrl-A / End, Ctrl-E | the start / end of the line |
//! | Backspace / Delete | the character before / under the cursor |
//! | Ctrl-U / Ctrl-K | everything before / from the cursor |
//! | Ctrl-W | the word before the cursor |
//! | Ctrl-D | end of input on an empty line, as in the terminal's own line mode; Delete otherwise |
//! | Enter | the line |
//!
//! A recalled line can be changed, left and come back to with the change still
//! in it. The history itself only ever gains the lines that were entered: the
//! last [`HISTORY_LEN`], with no blank lines and no line repeating the one
//! before it.
//!
//! # The terminal
//!
//! For as long as a line is being read — and no longer — the terminal is out
//! of its own line mode. On unix `ICANON`, `ECHO` and `IEXTEN` are cleared; on
//! Windows `ENABLE_LINE_INPUT` and `ENABLE_ECHO_INPUT` are, and
//! `ENABLE_VIRTUAL_TERMINAL_INPUT` is set so the arrow keys arrive as the same
//! escape sequences a unix terminal sends. Nothing else is touched: output
//! processing stays on, so a log line's `\n` still returns the carriage, and so
//! do the signal keys, so Ctrl-C still stops the daemon and still saves the
//! wallet.
//!
//! While a command runs the terminal is in its own mode again. A process that
//! ends while a line is being read must put the mode back, or the shell is left
//! with no echo: [`restore_terminal`] is registered with the C library's
//! `atexit` the first time the mode is changed, as linenoise registers its
//! own, and a program that leaves without running `atexit` handlers calls it
//! first.
//!
//! # Sharing the row with the logger
//!
//! Every redraw goes through [`crate::log::draw_prompt_row`], under the lock
//! every log line takes, so the two never interleave, and a log line arriving
//! mid-edit takes the row off and puts it back with the cursor where it was.
//! The erase before a log line clears one row, so the row must never wrap: a
//! line wider than the terminal scrolls sideways around the cursor. Every
//! character counts as one column; a double-width one, which no command or
//! address contains, puts the cursor a column out while it is on the row.
//!
//! # Where it does not run
//!
//! [`Editor::new`] returns `None`, and the caller reads in the terminal's own
//! line mode as it always has, unless stdin and stdout are both terminals,
//! stdout takes ANSI ([`crate::log::Erase::Ansi`]) and the mode can be changed:
//! not for a pipe, not with `TERM=dumb`, not on a Windows console too old for
//! virtual-terminal input.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read};

use crate::log;

/// How many entered lines are kept: linenoise's
/// `LINENOISE_DEFAULT_HISTORY_MAX_LEN`.
pub const HISTORY_LEN: usize = 100;

/// The longest line, in bytes, that can be typed or pasted; what does not fit
/// is dropped. linenoise's `LINENOISE_MAX_LINE`.
pub const MAX_LINE_BYTES: usize = 4096;

/// The width assumed when the terminal does not say.
const FALLBACK_WIDTH: usize = 80;

/// The most parameter bytes of an escape sequence kept. A longer one still
/// ends at its final byte; it just cannot be a key in the table.
const MAX_SEQUENCE: usize = 16;

/// Put the terminal back in its own line mode if a line is being read, and do
/// nothing otherwise.
///
/// Safe to call at any time, more than once, and from a signal handler: it
/// touches an atomic, a value set before any handler could need it, and
/// `tcsetattr`, which is async-signal-safe.
///
/// It is registered with `atexit`, which covers returning from `main` and, on
/// unix, `std::process::exit`. A program that leaves some other way — `_exit`
/// in a signal handler, or `std::process::exit` on Windows, which does not
/// promise to run the C runtime's handlers — calls it first.
pub fn restore_terminal() {
    sys::leave_raw();
}

/// A prompt that remembers the lines entered at it.
pub struct Editor {
    /// The entered lines, oldest first.
    history: VecDeque<String>,
    /// Bytes read but not used yet: the rest of a paste, after the Enter that
    /// ended the line before.
    unread: VecDeque<u8>,
    keys: Decoder,
}

impl Editor {
    /// An editor for this process's terminal, or `None` where one cannot run
    /// (see the module documentation), in which case read the line plainly.
    pub fn new() -> Option<Editor> {
        let term = log::terminal();
        if !term.stdin || !term.stdout || term.erase_stdout != log::Erase::Ansi {
            return None;
        }
        // Tried here rather than found out at the first prompt, where the only
        // thing left to do with a failure would be to call it end of input.
        if !sys::enter_raw() {
            return None;
        }
        sys::leave_raw();
        Some(Editor { history: VecDeque::new(), unread: VecDeque::new(), keys: Decoder::default() })
    }

    /// Read a line at `prompt`, which may be painted with ANSI colour, and add
    /// it to the history. `None` at the end of input: Ctrl-D on an empty line,
    /// or a terminal that has gone away.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        if !sys::enter_raw() {
            return None;
        }
        let line = self.edit(&mut std::io::stdin(), &mut |line: &Line| {
            let (row, back) = render(prompt, line, sys::terminal_width().unwrap_or(FALLBACK_WIDTH));
            log::draw_prompt_row(&row, back);
        });
        match &line {
            Some(text) => log::end_prompt_row(&format!("{prompt}{text}"), prompt),
            None => log::set_prompt(Some(prompt.to_string())),
        }
        sys::leave_raw();
        line
    }

    /// The editing, over any input: the keys `input` sends until Enter, with
    /// the line handed to `draw` each time the editor waits for more.
    fn edit(&mut self, input: &mut impl Read, draw: &mut dyn FnMut(&Line)) -> Option<String> {
        let mut line = Line::new(&self.history);
        let mut chunk = [0u8; 256];
        loop {
            // All of what one read brought is applied before the row is drawn
            // again, so a paste is not drawn a character at a time.
            while let Some(byte) = self.unread.pop_front() {
                let Some(key) = self.keys.push(byte) else { continue };
                match line.apply(key) {
                    Step::Editing => {}
                    Step::Entered => {
                        let text = line.text();
                        remember(&mut self.history, &text);
                        return Some(text);
                    }
                    Step::Ended => return None,
                }
            }
            draw(&line);
            match input.read(&mut chunk) {
                Ok(0) => return None,
                Ok(read) => self.unread.extend(&chunk[..read]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
    }
}

/// Add an entered line to the history as `linenoiseHistoryAdd` does — not the
/// line entered just before it again, and only the newest [`HISTORY_LEN`] —
/// and not a blank line either.
fn remember(history: &mut VecDeque<String>, line: &str) {
    if line.trim().is_empty() || history.back().is_some_and(|last| last == line) {
        return;
    }
    if history.len() == HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(line.to_string());
}

// ---------------------------------------------------------------------------
// the line
// ---------------------------------------------------------------------------

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Editing,
    Entered,
    Ended,
}

/// The line being typed, and the history it can recall.
struct Line {
    chars: Vec<char>,
    /// Where the cursor is: 0 before the first character, `chars.len()` after
    /// the last.
    cursor: usize,
    /// The history, then the line being typed. Moving through it keeps what
    /// was done to each line, so a recalled line can be changed, left and come
    /// back to; the whole of it is dropped when the line is entered.
    recall: Vec<String>,
    /// Which of `recall` is on the row.
    at: usize,
}

impl Line {
    fn new(history: &VecDeque<String>) -> Line {
        let mut recall: Vec<String> = history.iter().cloned().collect();
        recall.push(String::new());
        Line { chars: Vec::new(), cursor: 0, at: recall.len() - 1, recall }
    }

    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn apply(&mut self, key: Key) -> Step {
        match key {
            Key::Enter => return Step::Entered,
            Key::CtrlD if self.chars.is_empty() => return Step::Ended,
            Key::Char(c) => self.insert(c),
            Key::CtrlD | Key::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.chars.len(),
            Key::KillToStart => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::KillToEnd => self.chars.truncate(self.cursor),
            Key::KillWord => {
                // The spaces before the cursor, then the word before them.
                let mut start = self.cursor;
                while start > 0 && self.chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && self.chars[start - 1] != ' ' {
                    start -= 1;
                }
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::Up => {
                if self.at > 0 {
                    self.recall_entry(self.at - 1);
                }
            }
            Key::Down => {
                if self.at + 1 < self.recall.len() {
                    self.recall_entry(self.at + 1);
                }
            }
        }
        Step::Editing
    }

    fn insert(&mut self, c: char) {
        let bytes: usize = self.chars.iter().map(|c| c.len_utf8()).sum();
        if bytes + c.len_utf8() <= MAX_LINE_BYTES {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Put `recall[to]` on the row, keeping what was there, with the cursor at
    /// its end as linenoise leaves it.
    fn recall_entry(&mut self, to: usize) {
        self.recall[self.at] = self.text();
        self.at = to;
        self.chars = self.recall[to].chars().collect();
        self.cursor = self.chars.len();
    }
}

/// What to draw for `line` at `prompt` on a terminal `width` columns wide: the
/// prompt and as much of the line as fits on the row, and how many columns the
/// cursor sits short of the row's end.
///
/// The window is the one linenoise's single-line mode picks: the start of the
/// line while the cursor is within reach of it, and otherwise just enough cut
/// from the left to keep the cursor on the row. One column is kept free: a
/// character in the last one leaves some terminals waiting to wrap, and the
/// next erase would take the wrong row.
fn render(prompt: &str, line: &Line, width: usize) -> (String, usize) {
    let room = width.saturating_sub(columns(prompt) + 1).max(1);
    let start = line.cursor.saturating_sub(room);
    let end = line.chars.len().min(start + room);
    let mut row = String::with_capacity(prompt.len() + room);
    row.push_str(prompt);
    row.extend(&line.chars[start..end]);
    (row, end - line.cursor)
}

/// The columns `text` takes on screen: its characters, without the ANSI colour
/// a wallet prompt is painted with.
fn columns(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // `ESC [`, the parameters, and a final character from `@` to `~`.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            count += 1;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// the keys
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    CtrlD,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    KillToStart,
    KillToEnd,
    KillWord,
}

/// Bytes from the terminal into keys. A key can arrive split across reads — an
/// escape sequence, a character of more than one byte — so what has come of
/// it so far waits in `pending`.
#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
    /// The byte before was a carriage return, so a line feed now ends the same
    /// line rather than a second, empty one.
    after_cr: bool,
}

impl Decoder {
    /// Take one byte, and return the key it completes. What is not a key — a
    /// control character with no meaning here, a sequence for a key not in the
    /// table, a malformed character — is dropped.
    fn push(&mut self, byte: u8) -> Option<Key> {
        let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
        match self.pending.first() {
            None => self.start(byte, after_cr),
            Some(&0x1b) => self.escape(byte),
            Some(_) => self.utf8(byte),
        }
    }

    fn start(&mut self, byte: u8, after_cr: bool) -> Option<Key> {
        Some(match byte {
            b'\n' if after_cr => return None,
            b'\r' | b'\n' => Key::Enter,
            0x7f | 0x08 => Key::Backspace,
            0x01 => Key::Home,
            0x02 => Key::Left,
            0x04 => Key::CtrlD,
            0x05 => Key::End,
            0x06 => Key::Right,
            0x0b => Key::KillToEnd,
            0x0e => Key::Down,
            0x10 => Key::Up,
            0x15 => Key::KillToStart,
            0x17 => Key::KillWord,
            0x20..=0x7e => Key::Char(char::from(byte)),
            // An escape, or the first byte of a character of two to four.
            0x1b | 0xc2..=0xf4 => {
                self.pending.push(byte);
                return None;
            }
            _ => return None,
        })
    }

    /// `ESC [`, parameter bytes and a final byte (CSI), or `ESC O` and one byte
    /// (SS3), which is what a terminal in application mode sends for the same
    /// keys.
    fn escape(&mut self, byte: u8) -> Option<Key> {
        if self.pending.len() == 1 {
            if byte == b'[' || byte == b'O' {
                self.pending.push(byte);
                return None;
            }
            // Escape and then anything else — Alt with a key — is not a key
            // here. The escape is dropped, and the byte is taken on its own.
            self.pending.clear();
            return self.start(byte, false);
        }
        if self.pending[1] == b'O' {
            self.pending.clear();
            return sequence_key(&[], byte);
        }
        match byte {
            0x40..=0x7e => {
                let key = sequence_key(&self.pending[2..], byte);
                self.pending.clear();
                key
            }
            0x20..=0x3f => {
                if self.pending.len() < 2 + MAX_SEQUENCE {
                    self.pending.push(byte);
                }
                None
            }
            _ => {
                self.pending.clear();
                None
            }
        }
    }

    /// The rest of a character of more than one byte.
    fn utf8(&mut self, byte: u8) -> Option<Key> {
        if byte & 0xc0 != 0x80 {
            // Not a continuation: what came before was not a character, and
            // this byte starts afresh.
            self.pending.clear();
            return self.start(byte, false);
        }
        self.pending.push(byte);
        let length = match self.pending[0] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        if self.pending.len() < length {
            return None;
        }
        let key = std::str::from_utf8(&self.pending)
            .ok()
            .and_then(|s| s.chars().next())
            .filter(|c| !c.is_control())
            .map(Key::Char);
        self.pending.clear();
        key
    }
}

/// The key a CSI or SS3 sequence names. A modifier (`ESC [ 1 ; 5 C`, which is
/// Ctrl-Right) is not told apart: the key does what it does without one.
fn sequence_key(parameters: &[u8], last: u8) -> Option<Key> {
    Some(match last {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'~' => match parameters.split(|&b| b == b';').next() {
            Some(b"1" | b"7") => Key::Home,
            Some(b"4" | b"8") => Key::End,
            Some(b"3") => Key::Delete,
            _ => return None,
        },
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// the terminal's mode
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod sys {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    /// The terminal's own mode, as the process found it the first time it was
    /// changed. Taken once rather than before every line so that a signal
    /// handler may read it: once set, a `OnceLock` is never written again.
    static COOKED: OnceLock<libc::termios> = OnceLock::new();
    /// A line is being read, so the terminal is out of its own mode.
    static RAW: AtomicBool = AtomicBool::new(false);

    pub fn enter_raw() -> bool {
        let cooked = match COOKED.get() {
            Some(cooked) => *cooked,
            None => {
                // SAFETY: an all-zero `termios` is a valid value of a plain C
                // struct, and `tcgetattr` writes one through a pointer to a
                // live local of that type.
                let mut found: libc::termios = unsafe { std::mem::zeroed() };
                if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut found) } != 0 {
                    return false;
                }
                *COOKED.get_or_init(|| {
                    // SAFETY: `at_exit` is an `extern "C" fn()` that only
                    // restores the mode.
                    unsafe {
                        libc::atexit(at_exit);
                    }
                    found
                })
            }
        };
        let mut raw = cooked;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // Marked before the switch, so a signal in between still restores.
        RAW.store(true, Ordering::SeqCst);
        // SAFETY: `tcsetattr` reads one `termios` through a pointer to a live
        // local of that type.
        let ok = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } == 0;
        if !ok {
            RAW.store(false, Ordering::SeqCst);
        }
        ok
    }

    pub fn leave_raw() {
        if !RAW.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(cooked) = COOKED.get() {
            // SAFETY: as in `enter_raw`; and `tcsetattr` is async-signal-safe,
            // which `restore_terminal` promises.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, cooked);
            }
        }
    }

    extern "C" fn at_exit() {
        leave_raw();
    }

    pub fn terminal_width() -> Option<usize> {
        let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: `TIOCGWINSZ` writes one `winsize` through a pointer to a live
        // local of that type.
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
        (ok && size.ws_col > 0).then_some(usize::from(size.ws_col))
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    /// Keys arrive as the escape sequences a unix terminal sends. Windows 10
    /// and later.
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

    /// `CONSOLE_SCREEN_BUFFER_INFO`: its `COORD`s and `SMALL_RECT` as their
    /// `i16`s. Only the visible window is read.
    #[repr(C)]
    #[derive(Default)]
    struct ScreenBufferInfo {
        _size: [i16; 2],
        _cursor: [i16; 2],
        _attributes: u16,
        /// Left, top, right, bottom.
        window: [i16; 4],
        _maximum_window: [i16; 2],
    }

    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(handle: *mut c_void, info: *mut ScreenBufferInfo) -> i32;
    }

    unsafe extern "C" {
        fn atexit(callback: extern "C" fn()) -> i32;
    }

    /// The console's own input mode, as the process found it the first time it
    /// was changed.
    static COOKED: OnceLock<u32> = OnceLock::new();
    /// A line is being read, so the console is out of its own mode.
    static RAW: AtomicBool = AtomicBool::new(false);

    pub fn enter_raw() -> bool {
        let mut mode = 0;
        // SAFETY: `GetStdHandle` takes no pointer, and `GetConsoleMode` writes
        // one `u32` through a pointer to a live local; it fails, rather than
        // misbehaves, on a handle that is not a console.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return false;
        }
        let cooked = *COOKED.get_or_init(|| {
            // SAFETY: `at_exit` is an `extern "C" fn()` that only restores the
            // mode.
            unsafe {
                atexit(at_exit);
            }
            mode
        });
        let raw = (cooked & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT;
        RAW.store(true, Ordering::SeqCst);
        // SAFETY: `handle` is the console handle `GetConsoleMode` just accepted.
        let ok = unsafe { SetConsoleMode(handle, raw) } != 0;
        if !ok {
            RAW.store(false, Ordering::SeqCst);
        }
        ok
    }

    pub fn leave_raw() {
        if !RAW.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(&cooked) = COOKED.get() {
            // SAFETY: `SetConsoleMode` takes the handle and a value; on a handle
            // that is no longer a console it fails and changes nothing.
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), cooked);
            }
        }
    }

    extern "C" fn at_exit() {
        leave_raw();
    }

    pub fn terminal_width() -> Option<usize> {
        let mut info = ScreenBufferInfo::default();
        // SAFETY: `GetConsoleScreenBufferInfo` writes one
        // `CONSOLE_SCREEN_BUFFER_INFO` through a pointer to a live local laid
        // out as one, and fails on a handle that is not a console.
        let ok = unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) } != 0;
        let width = i32::from(info.window[2]) - i32::from(info.window[0]) + 1;
        (ok && width > 0).then_some(width as usize)
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    pub fn enter_raw() -> bool {
        false
    }

    pub fn leave_raw() {}

    pub fn terminal_width() -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys `bytes` decode to, delivered a byte at a time, as a slow
    /// terminal or a split read would.
    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut decoder = Decoder::default();
        bytes.iter().filter_map(|&byte| decoder.push(byte)).collect()
    }

    fn history(lines: &[&str]) -> VecDeque<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// An editor that has had `lines` entered at it, and no terminal.
    fn editor_with(lines: &[&str]) -> Editor {
        Editor { history: history(lines), unread: VecDeque::new(), keys: Decoder::default() }
    }

    fn press(line: &mut Line, keys: &[Key]) {
        for &key in keys {
            line.apply(key);
        }
    }

    fn type_text(line: &mut Line, text: &str) {
        for c in text.chars() {
            line.apply(Key::Char(c));
        }
    }

    /// The line, with `|` where the cursor is.
    fn shown(line: &Line) -> String {
        let mut text: String = line.chars[..line.cursor].iter().collect();
        text.push('|');
        text.extend(&line.chars[line.cursor..]);
        text
    }

    #[test]
    fn a_key_is_decoded_whole_however_its_bytes_arrive() {
        assert_eq!(keys(b"\x1b[A\x1b[B\x1b[C\x1b[D"), [Key::Up, Key::Down, Key::Right, Key::Left]);
        assert_eq!(keys(b"\x1bOA\x1bOB\x1bOH\x1bOF"), [Key::Up, Key::Down, Key::Home, Key::End], "application mode");
        assert_eq!(keys(b"\x1b[1;5C"), [Key::Right], "Ctrl-Right moves as Right does");
        assert_eq!(keys(b"\x1b[H\x1b[1~\x1b[7~"), [Key::Home; 3], "each way terminals send Home");
        assert_eq!(keys(b"\x1b[F\x1b[4~\x1b[8~"), [Key::End; 3], "and End");
        assert_eq!(keys(b"\x1b[3~\x7f\x08"), [Key::Delete, Key::Backspace, Key::Backspace]);
        assert_eq!(
            keys(b"\x10\x0e\x02\x06\x01\x05\x15\x0b\x17\x04"),
            [
                Key::Up,
                Key::Down,
                Key::Left,
                Key::Right,
                Key::Home,
                Key::End,
                Key::KillToStart,
                Key::KillToEnd,
                Key::KillWord,
                Key::CtrlD
            ]
        );
        assert_eq!(keys("é€".as_bytes()), [Key::Char('é'), Key::Char('€')]);
    }

    #[test]
    fn what_is_not_a_key_is_dropped_without_taking_the_next_key_with_it() {
        assert_eq!(keys(b"\r\n\r"), [Key::Enter, Key::Enter], "CR LF ends one line, not two");
        assert_eq!(keys(b"\n\n"), [Key::Enter, Key::Enter], "but two line feeds are two lines");
        assert_eq!(keys(b"\x1b[99~a"), [Key::Char('a')], "a key not in the table");
        assert_eq!(keys(b"\x1bxy"), [Key::Char('x'), Key::Char('y')], "Alt-x is x");
        assert_eq!(keys(b"\x1b\x1b[A"), [Key::Up], "a doubled escape");
        assert_eq!(keys(b"\xc3a\xff\x00\x07\tb"), [Key::Char('a'), Key::Char('b')], "broken UTF-8 and controls");
        let long = [b"\x1b[".as_slice(), &[b'1'; 1000], b"~z"].concat();
        let mut decoder = Decoder::default();
        let decoded: Vec<Key> = long.iter().filter_map(|&byte| decoder.push(byte)).collect();
        assert_eq!(decoded, [Key::Char('z')], "a sequence of any length ends at its final byte");
        assert!(decoder.pending.capacity() < 64, "without keeping all of it");
    }

    #[test]
    fn the_editing_keys_act_where_the_cursor_is() {
        let mut line = Line::new(&VecDeque::new());
        type_text(&mut line, "print_bc 10");
        assert_eq!(shown(&line), "print_bc 10|");
        press(&mut line, &[Key::Left, Key::Left, Key::Backspace]);
        assert_eq!(shown(&line), "print_bc|10");
        type_text(&mut line, " 1");
        assert_eq!(shown(&line), "print_bc 1|10", "typing inserts");
        press(&mut line, &[Key::Home, Key::Delete, Key::Right]);
        assert_eq!(shown(&line), "r|int_bc 110");
        press(&mut line, &[Key::End, Key::KillWord]);
        assert_eq!(shown(&line), "rint_bc |", "Ctrl-W takes the word and leaves the space before it");
        press(&mut line, &[Key::Home, Key::Right, Key::Right, Key::Right, Key::Right, Key::KillToEnd]);
        assert_eq!(shown(&line), "rint|");
        press(&mut line, &[Key::Left, Key::KillToStart]);
        assert_eq!(shown(&line), "|t");
        press(&mut line, &[Key::Left, Key::Backspace, Key::End, Key::Right, Key::Delete]);
        assert_eq!(shown(&line), "t|", "nothing moves or deletes past either end");
    }

    #[test]
    fn ctrl_d_ends_the_input_only_on_an_empty_line() {
        let mut line = Line::new(&VecDeque::new());
        type_text(&mut line, "ab");
        press(&mut line, &[Key::Home]);
        assert_eq!(line.apply(Key::CtrlD), Step::Editing);
        assert_eq!(shown(&line), "|b", "on a line, it deletes");
        press(&mut line, &[Key::Delete]);
        assert_eq!(line.apply(Key::CtrlD), Step::Ended);
    }

    #[test]
    fn up_goes_back_through_the_history_and_down_returns_to_the_line_being_typed() {
        let mut line = Line::new(&history(&["status", "print_pl"]));
        type_text(&mut line, "hei");
        press(&mut line, &[Key::Up]);
        assert_eq!(shown(&line), "print_pl|", "the newest first, with the cursor at its end");
        press(&mut line, &[Key::Up, Key::Up]);
        assert_eq!(shown(&line), "status|", "and nothing older than the oldest");
        press(&mut line, &[Key::Down]);
        assert_eq!(shown(&line), "print_pl|");
        press(&mut line, &[Key::Down, Key::Down]);
        assert_eq!(shown(&line), "hei|", "what was being typed is still there");
    }

    #[test]
    fn a_recalled_line_can_be_changed_and_come_back_to_but_the_history_keeps_what_was_entered() {
        let mut editor = editor_with(&["print_bc 1 10"]);
        // Up, change the last number, go down and up again, and enter it.
        let mut input: &[u8] = b"\x1b[A\x7f\x7f20\x1b[B\x1b[A\r";
        assert_eq!(editor.edit(&mut input, &mut |_| {}).as_deref(), Some("print_bc 1 20"));
        assert_eq!(editor.history, ["print_bc 1 10", "print_bc 1 20"]);
    }

    #[test]
    fn a_pasted_block_is_read_a_line_at_a_time_and_drawn_once_per_read() {
        let mut editor = editor_with(&[]);
        let mut input: &[u8] = b"status\r\n\r\n  \rheight\r\nprint_pl";
        let mut lines = Vec::new();
        let mut draws = 0;
        while let Some(line) = editor.edit(&mut input, &mut |_| draws += 1) {
            lines.push(line);
        }
        assert_eq!(lines, ["status", "", "  ", "height"], "a line with no Enter is dropped at the end of input");
        assert_eq!(editor.history, ["status", "height"], "blank lines are not kept");
        assert_eq!(draws, 2, "before the paste arrived, and after it, not once a character");
    }

    #[test]
    fn the_history_keeps_the_newest_lines_without_blanks_or_repeats() {
        let mut kept = VecDeque::new();
        for line in ["status", "status", " ", "", "height", "status"] {
            remember(&mut kept, line);
        }
        assert_eq!(
            kept,
            ["status", "height", "status"],
            "a repeat of the line before is not kept; of an older one it is"
        );
        for i in 0..HISTORY_LEN * 2 {
            remember(&mut kept, &format!("print_block {i}"));
        }
        assert_eq!(kept.len(), HISTORY_LEN);
        assert_eq!(kept.front().map(String::as_str), Some("print_block 100"));
        assert_eq!(kept.back().map(String::as_str), Some("print_block 199"));
    }

    #[test]
    fn a_line_stops_growing_at_the_cap() {
        let mut line = Line::new(&VecDeque::new());
        type_text(&mut line, &"a".repeat(MAX_LINE_BYTES + 10));
        assert_eq!(line.chars.len(), MAX_LINE_BYTES);
        press(&mut line, &[Key::Backspace, Key::Char('é')]);
        assert_eq!(line.chars.len(), MAX_LINE_BYTES - 1, "a two-byte character does not fit in the one byte left");
    }

    #[test]
    fn a_line_wider_than_the_terminal_scrolls_to_keep_the_cursor_on_the_row() {
        let mut line = Line::new(&VecDeque::new());
        type_text(&mut line, "print_block 4213000");
        assert_eq!(render("> ", &line, 80), ("> print_block 4213000".to_string(), 0));
        // Twelve columns, two of them the prompt's and one kept free: the nine
        // characters before the cursor.
        assert_eq!(render("> ", &line, 12), ("> k 4213000".to_string(), 0));
        press(&mut line, &[Key::Home]);
        assert_eq!(render("> ", &line, 12), ("> print_blo".to_string(), 9), "the start, the cursor nine back");
        assert_eq!(render("wrkz-node> ", &line, 5), ("wrkz-node> p".to_string(), 1), "never less than a character");

        let painted = "\x1b[1;33mWhat would you like to do?: \x1b[0m";
        assert_eq!(columns(painted), "What would you like to do?: ".len(), "colour takes no columns");
    }
}
