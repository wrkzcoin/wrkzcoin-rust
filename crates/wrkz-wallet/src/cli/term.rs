// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The terminal: reading a line, reading a command with history, reading a
//! password without echoing it, and the three colours
//! `utilities/ColouredMsg.h` uses.
//!
//! Everything the wallet prints and reads goes through [`Terminal`], so the
//! whole interface can be driven from a script in a test with no tty, and so
//! there is exactly one place that can echo a password.
//!
//! # Commands
//!
//! The prompts that take a command read through [`Terminal::read_command`],
//! which on a terminal has the arrow-key history and in-line editing
//! zedwallet++ gets from linenoise ([`wrkz_rpc::readline`]). Every other read
//! stays a plain line, so an address, an amount or a seed is never kept for Up
//! to put back on the screen.
//!
//! # Hiding the echo
//!
//! `Tools::PasswordContainer` turns the echo off around the read. There is no
//! dependency here to do that, so the platform call is written out: `ECHO`
//! cleared in `termios::c_lflag` on unix, `ENABLE_ECHO_INPUT` cleared in the
//! console mode on Windows. Both are restored on the way out, including when
//! the read fails, and neither is fatal if it does not work — the read still
//! happens, it is simply visible, which is what the C++ does too.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use wrkz_rpc::readline::Editor;
use zeroize::Zeroizing;

////////////////////////
/* COLOUR             */
////////////////////////

/// Whether to emit ANSI colour. Off until a binary turns it on, so a scripted
/// test compares plain text.
static COLOUR: AtomicBool = AtomicBool::new(false);

/// Turn ANSI colour on or off for the whole process.
pub fn set_colour(on: bool) {
    COLOUR.store(on, Ordering::Relaxed);
}

fn paint(code: &str, msg: &str) -> String {
    if COLOUR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{msg}\x1b[0m")
    } else {
        msg.to_string()
    }
}

/// `SuccessMsg`: green.
pub fn success(msg: impl AsRef<str>) -> String {
    paint("1;32", msg.as_ref())
}

/// `WarningMsg`: red.
pub fn warning(msg: impl AsRef<str>) -> String {
    paint("1;31", msg.as_ref())
}

/// `InformationMsg`: yellow.
pub fn information(msg: impl AsRef<str>) -> String {
    paint("1;33", msg.as_ref())
}

////////////////////////
/* THE TRAIT          */
////////////////////////

/// Everything the interface does to a terminal.
pub trait Terminal {
    /// One line, with the trailing newline removed. `None` at end of input,
    /// which the C++ treats as ctrl-c: cancel, or exit.
    fn read_line(&mut self) -> Option<String>;

    /// A line at a prompt that takes a command. The same as
    /// [`Terminal::read_line`], unless the terminal has the line editor, which
    /// adds the history of the commands entered before.
    fn read_command(&mut self) -> Option<String> {
        self.read_line()
    }

    /// One line with the echo suppressed. The result is zeroized on drop and
    /// is never written back to the terminal or to a log.
    fn read_password(&mut self) -> Option<Zeroizing<String>>;

    /// Write, without a newline.
    fn write(&mut self, text: &str);

    /// Write, then a newline.
    fn line(&mut self, text: &str) {
        self.write(text);
        self.write("\n");
    }

    /// Flush anything buffered.
    fn flush(&mut self) {}
}

////////////////////////
/* THE REAL TERMINAL  */
////////////////////////

/// Standard input and standard output.
///
/// It remembers what it has written since the last line break — at a read,
/// that is the prompt — and while it waits for input it registers that text
/// with the logger ([`wrkz_rpc::log::set_prompt`]), which then takes the
/// prompt off before a log line from the sync thread and draws it again after.
/// Only on a terminal, where there is a prompt to protect.
pub struct StdTerminal {
    /// What is on the current row: everything written since the last `\n`
    /// or `\r`.
    row: String,
    /// The line editor [`Terminal::read_command`] reads through, where the
    /// terminal can take one.
    commands: Option<Editor>,
}

impl Default for StdTerminal {
    fn default() -> StdTerminal {
        StdTerminal { row: String::new(), commands: Editor::new() }
    }
}

/// The longest prompt remembered; a prompt is a few words.
const MAX_PROMPT: usize = 256;

/// Fold `text`, just written, into `row`: what is on the screen's last row.
fn track_row(row: &mut String, text: &str) {
    match text.rfind(['\n', '\r']) {
        Some(at) => {
            row.clear();
            row.push_str(&text[at + 1..]);
        }
        None => row.push_str(text),
    }
    if row.len() > MAX_PROMPT {
        let mut cut = row.len() - MAX_PROMPT;
        while !row.is_char_boundary(cut) {
            cut += 1;
        }
        row.drain(..cut);
    }
}

/// Registers the prompt for as long as a read blocks.
struct PromptShown(bool);

impl PromptShown {
    fn while_reading(prompt: &str) -> PromptShown {
        let term = wrkz_rpc::log::terminal();
        if prompt.is_empty() || !term.stdin || !term.stdout {
            return PromptShown(false);
        }
        wrkz_rpc::log::set_prompt(Some(prompt.to_string()));
        PromptShown(true)
    }
}

impl Drop for PromptShown {
    fn drop(&mut self) {
        if self.0 {
            wrkz_rpc::log::set_prompt(None);
        }
    }
}

impl Terminal for StdTerminal {
    fn read_line(&mut self) -> Option<String> {
        let shown = PromptShown::while_reading(&self.row);
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        drop(shown);
        // The user's Enter ended the row.
        self.row.clear();
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(trim_newline(line)),
        }
    }

    fn read_command(&mut self) -> Option<String> {
        let Some(editor) = self.commands.as_mut() else {
            return self.read_line();
        };
        let shown = PromptShown::while_reading(&self.row);
        let line = editor.read_line(&self.row);
        drop(shown);
        self.row.clear();
        line
    }

    fn read_password(&mut self) -> Option<Zeroizing<String>> {
        let shown = PromptShown::while_reading(&self.row);
        let guard = EchoOff::new();
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        drop(guard);
        drop(shown);
        self.row.clear();
        // The newline the user typed was not echoed, so put one out ourselves;
        // otherwise the next prompt lands on the same line.
        println!();
        match read {
            Ok(0) | Err(_) => {
                line.zeroize_in_place();
                None
            }
            Ok(_) => Some(Zeroizing::new(trim_newline(line))),
        }
    }

    fn write(&mut self, text: &str) {
        track_row(&mut self.row, text);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    fn flush(&mut self) {
        let _ = std::io::stdout().flush();
    }
}

/// Overwrite a `String`'s bytes before it is dropped. `Zeroizing` does this for
/// what it owns; this is for the buffer we read into before wrapping it.
trait ZeroizeInPlace {
    fn zeroize_in_place(&mut self);
}

impl ZeroizeInPlace for String {
    fn zeroize_in_place(&mut self) {
        use zeroize::Zeroize;
        self.zeroize();
    }
}

fn trim_newline(mut line: String) -> String {
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    line
}

////////////////////////
/* SCRIPTED TERMINAL  */
////////////////////////

/// A terminal fed from a list of lines, collecting everything written.
///
/// This is how the command dispatcher is tested: the same code paths the real
/// wallet runs, with no tty, and with the output available to assert on —
/// including asserting that a secret was *not* printed.
pub struct ScriptedTerminal {
    input: VecDeque<String>,
    /// Everything written, in order.
    pub output: String,
    /// Every line read through [`Terminal::read_password`], so a test can check
    /// that a password was asked for.
    pub passwords_read: usize,
}

impl ScriptedTerminal {
    /// A terminal that will answer each prompt with the next line.
    pub fn new<I, S>(lines: I) -> ScriptedTerminal
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ScriptedTerminal {
            input: lines.into_iter().map(Into::into).collect(),
            output: String::new(),
            passwords_read: 0,
        }
    }

    /// Whether the script was fully consumed, which is how a test notices it
    /// asked fewer questions than expected.
    pub fn input_exhausted(&self) -> bool {
        self.input.is_empty()
    }

    /// Everything written so far, and clear it.
    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut self.output)
    }
}

impl Terminal for ScriptedTerminal {
    fn read_line(&mut self) -> Option<String> {
        self.input.pop_front()
    }

    fn read_password(&mut self) -> Option<Zeroizing<String>> {
        self.passwords_read += 1;
        self.input.pop_front().map(Zeroizing::new)
    }

    fn write(&mut self, text: &str) {
        self.output.push_str(text);
    }
}

////////////////////////
/* ECHO SUPPRESSION   */
////////////////////////

/// Turns the terminal echo off for as long as it lives.
struct EchoOff {
    #[cfg(any(unix, windows))]
    previous: Option<PreviousMode>,
}

impl EchoOff {
    fn new() -> EchoOff {
        #[cfg(any(unix, windows))]
        {
            EchoOff { previous: disable_echo() }
        }
        #[cfg(not(any(unix, windows)))]
        {
            EchoOff {}
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        #[cfg(any(unix, windows))]
        if let Some(previous) = self.previous.take() {
            restore_echo(previous);
        }
    }
}

// --- Windows ---------------------------------------------------------------

#[cfg(windows)]
type PreviousMode = u32;

#[cfg(windows)]
mod ffi {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetStdHandle(nStdHandle: u32) -> *mut c_void;
        pub fn GetConsoleMode(hConsoleHandle: *mut c_void, lpMode: *mut u32) -> i32;
        pub fn SetConsoleMode(hConsoleHandle: *mut c_void, dwMode: u32) -> i32;
    }

    /// `STD_INPUT_HANDLE` is `(DWORD)-10`.
    pub const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
    pub const INVALID_HANDLE_VALUE: isize = -1;
}

#[cfg(windows)]
fn disable_echo() -> Option<PreviousMode> {
    unsafe {
        let handle = ffi::GetStdHandle(ffi::STD_INPUT_HANDLE);
        if handle.is_null() || handle as isize == ffi::INVALID_HANDLE_VALUE {
            return None;
        }
        let mut mode: u32 = 0;
        if ffi::GetConsoleMode(handle, &mut mode) == 0 {
            // Not a console — a pipe, which never echoes anyway.
            return None;
        }
        if ffi::SetConsoleMode(handle, mode & !ffi::ENABLE_ECHO_INPUT) == 0 {
            return None;
        }
        Some(mode)
    }
}

#[cfg(windows)]
fn restore_echo(previous: PreviousMode) {
    unsafe {
        let handle = ffi::GetStdHandle(ffi::STD_INPUT_HANDLE);
        if !handle.is_null() && handle as isize != ffi::INVALID_HANDLE_VALUE {
            ffi::SetConsoleMode(handle, previous);
        }
    }
}

// --- unix ------------------------------------------------------------------

/// A `struct termios` as an opaque buffer, which is all this needs: the only
/// field it touches is `c_lflag`, whose offset and width are the two things
/// that differ between the platforms below. Sized well above every real
/// `struct termios` (Linux's is 60 bytes, macOS's 72) so `tcgetattr` cannot
/// write past it.
#[cfg(unix)]
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct PreviousMode([u8; 128]);

#[cfg(unix)]
mod ffi {
    use std::ffi::c_int;

    extern "C" {
        pub fn tcgetattr(fd: c_int, termios_p: *mut u8) -> c_int;
        pub fn tcsetattr(fd: c_int, optional_actions: c_int, termios_p: *const u8) -> c_int;
    }

    pub const STDIN_FILENO: c_int = 0;
    /// `TCSANOW`: apply immediately. Zero on Linux and on the BSDs.
    pub const TCSANOW: c_int = 0;
    /// `ECHO`, which is `0o10` everywhere this compiles.
    pub const ECHO: u64 = 0x0000_0008;
}

/// The byte offset of `c_lflag` inside `struct termios`, and its width.
///
/// Linux and Android: four `tcflag_t` (`unsigned int`) fields, so `c_lflag`
/// starts at 12 and is 4 bytes. The BSDs and macOS use `unsigned long`, so it
/// starts at 24 and is 8.
#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
const C_LFLAG: (usize, usize) = (12, 4);
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const C_LFLAG: (usize, usize) = (24, 8);

#[cfg(unix)]
fn disable_echo() -> Option<PreviousMode> {
    unsafe {
        let mut current = PreviousMode([0u8; 128]);
        if ffi::tcgetattr(ffi::STDIN_FILENO, current.0.as_mut_ptr()) != 0 {
            // Not a tty. Nothing to turn off, and nothing to restore.
            return None;
        }

        let saved = current;
        let (offset, width) = C_LFLAG;
        let mut flags = [0u8; 8];
        flags[..width].copy_from_slice(&current.0[offset..offset + width]);
        let value = u64::from_ne_bytes(flags) & !ffi::ECHO;
        current.0[offset..offset + width].copy_from_slice(&value.to_ne_bytes()[..width]);

        if ffi::tcsetattr(ffi::STDIN_FILENO, ffi::TCSANOW, current.0.as_ptr()) != 0 {
            return None;
        }
        Some(saved)
    }
}

#[cfg(unix)]
fn restore_echo(previous: PreviousMode) {
    unsafe {
        ffi::tcsetattr(ffi::STDIN_FILENO, ffi::TCSANOW, previous.0.as_ptr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_under_the_cursor_is_what_follows_the_last_line_break() {
        let mut row = String::new();
        track_row(&mut row, "Welcome\nWhat would you ");
        track_row(&mut row, "like to do?: ");
        assert_eq!(row, "What would you like to do?: ", "a prompt written in pieces is one row");
        track_row(&mut row, "\r4213000/4213500 ");
        assert_eq!(row, "4213000/4213500 ", "a carriage return starts the row again");
        track_row(&mut row, "done\n");
        assert_eq!(row, "", "a newline leaves nothing to redraw");
        track_row(&mut row, &"é".repeat(400));
        assert!(row.len() <= MAX_PROMPT + 1 && row.chars().all(|c| c == 'é'), "bounded, on a character boundary");
    }

    #[test]
    fn a_scripted_terminal_answers_in_order_and_records_output() {
        let mut term = ScriptedTerminal::new(["one", "two"]);
        term.write("prompt: ");
        assert_eq!(term.read_line().as_deref(), Some("one"));
        assert_eq!(term.read_password().as_deref().map(String::as_str), Some("two"));
        assert_eq!(term.read_line(), None);
        assert_eq!(term.output, "prompt: ");
        assert_eq!(term.passwords_read, 1);
        assert!(term.input_exhausted());
    }

    #[test]
    fn colour_is_off_by_default_so_output_is_comparable() {
        assert_eq!(success("ok"), "ok");
        assert_eq!(warning("no"), "no");
        assert_eq!(information("hm"), "hm");
    }
}
