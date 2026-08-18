use anyhow::Context;
use chrono::serde::ts_seconds_option;
use chrono::{DateTime, Utc};
use clap::{Parser, ValueEnum};
use config::ConfigHandle;
use filedescriptor::FileDescriptor;
use portable_pty::{native_pty_system, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::escape::parser::Parser as TWParser;
use termwiz::escape::Action;
#[cfg(unix)]
use unix::UnixTty as Tty;
use wezterm_term::color::ColorPalette;
#[cfg(windows)]
use win::WinTty as Tty;

/// See <https://github.com/asciinema/asciinema/blob/develop/doc/asciicast-v2.md>
/// for file format specification
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Header {
    /// Must be 2 or higher
    pub version: u32,
    /// Initial terminal width (number of columns)
    pub width: u32,
    /// Initial terminal height (number of columns)
    pub height: u32,
    /// Unix timestamp of starting time of session
    #[serde(
        default,
        with = "ts_seconds_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub timestamp: Option<DateTime<Utc>>,
    /// Duration of the whole recording in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f32>,
    /// Used to reduce terminal inactivity (delays between frames)
    /// to a maximum of this amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_time_limit: Option<f32>,
    /// Command that was recorded
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Title of the asciicast
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Map of captured environment variables
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Color theme of the recorded terminal
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
}

impl Header {
    fn new(config: &ConfigHandle, size: PtySize, prog: &[&OsStr]) -> Self {
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), config.term.to_string());
        env.insert(
            "WEZTERM_VERSION".to_string(),
            config::wezterm_version().to_string(),
        );
        env.insert(
            "WEZTERM_TARGET_TRIPLE".to_string(),
            config::wezterm_target_triple().to_string(),
        );
        if let Ok(shell) = std::env::var("SHELL") {
            env.insert("SHELL".to_string(), shell);
        }
        if let Ok(lang) = std::env::var("LANG") {
            env.insert("LANG".to_string(), lang);
        }

        let palette: ColorPalette = config.resolved_palette.clone().into();
        let ansi_colors: Vec<String> = palette.colors.0[0..16]
            .iter()
            .map(|c| c.to_rgb_string())
            .collect();

        let theme = Theme {
            fg: palette.foreground.to_rgb_string(),
            bg: palette.background.to_rgb_string(),
            palette: ansi_colors.join(":"),
        };

        let command = if prog.is_empty() {
            None
        } else {
            let args: Vec<String> = prog
                .iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect();
            Some(shell_words::join(&args))
        };

        Header {
            version: 2,
            height: size.rows.into(),
            width: size.cols.into(),
            timestamp: Some(Utc::now()),
            env,
            command,
            theme: Some(theme),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Theme {
    /// Normal text color
    pub fg: String,
    /// Normal background color
    pub bg: String,
    /// List of 8 or 16 colors separated by a colon character
    pub palette: String,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Event(pub f32, pub String, pub String);

impl Event {
    fn log_output<W: Write>(mut w: W, elapsed: f32, output: &str) -> std::io::Result<()> {
        let event = Event(elapsed, "o".to_string(), output.to_string());
        writeln!(w, "{}", serde_json::to_string(&event)?)
    }
}

fn log_utf8_output<W: Write>(
    cast_file: &mut W,
    buffer: &mut Vec<u8>,
    elapsed: f32,
    data: &mut Vec<u8>,
) -> anyhow::Result<()> {
    // The end of the data may be an incomplete utf8 sequence that straddles
    // the buffer boundary. JSON requires strings to be utf-8, so log only
    // valid portions and carry the remainder forward.
    buffer.append(data);
    match std::str::from_utf8(buffer) {
        Ok(valid) => {
            Event::log_output(cast_file, elapsed, valid)?;
            buffer.clear();
        }
        Err(error) => {
            let valid_len = error.valid_up_to();
            Event::log_output(cast_file, elapsed, unsafe {
                std::str::from_utf8_unchecked(&buffer[0..valid_len])
            })?;

            buffer.drain(0..valid_len);

            if let Some(invalid_sequence_length) = error.error_len() {
                // Invalid sequence: skip it.
                buffer.drain(0..invalid_sequence_length);
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
mod win {
    use super::*;
    use filedescriptor::AsRawFileDescriptor;
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;
    use winapi::um::consoleapi::*;
    use winapi::um::wincon::*;
    use winapi::um::winnls::CP_UTF8;

    fn check_bool(ok: i32, context: &str) -> anyhow::Result<()> {
        if ok == 0 {
            anyhow::bail!("{context}: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub struct WinTty {
        saved_input: u32,
        saved_output: u32,
        saved_cp: u32,
        read: FileDescriptor,
        write: FileDescriptor,
    }

    impl WinTty {
        pub fn new() -> anyhow::Result<Self> {
            let read =
                FileDescriptor::new(OpenOptions::new().read(true).write(true).open("CONIN$")?);
            let write =
                FileDescriptor::new(OpenOptions::new().read(true).write(true).open("CONOUT$")?);

            let mut saved_input = 0;
            let mut saved_output = 0;
            let saved_cp;
            unsafe {
                check_bool(
                    GetConsoleMode(read.as_raw_file_descriptor() as *mut _, &mut saved_input),
                    "GetConsoleMode(CONIN$) failed",
                )?;
                check_bool(
                    GetConsoleMode(write.as_raw_file_descriptor() as *mut _, &mut saved_output),
                    "GetConsoleMode(CONOUT$) failed",
                )?;
                saved_cp = GetConsoleOutputCP();
                check_bool(SetConsoleOutputCP(CP_UTF8), "SetConsoleOutputCP failed")?;
            }

            Ok(Self {
                saved_input,
                saved_output,
                saved_cp,
                read,
                write,
            })
        }

        pub fn set_cooked(&mut self) -> anyhow::Result<()> {
            let mut first_error = None;

            macro_rules! restore {
                ($expr:expr, $context:literal) => {
                    if let Err(err) = unsafe { check_bool($expr, $context) } {
                        first_error.get_or_insert(err);
                    }
                };
            }

            restore!(
                SetConsoleOutputCP(self.saved_cp),
                "SetConsoleOutputCP failed"
            );
            restore!(
                SetConsoleMode(self.read.as_raw_handle() as *mut _, self.saved_input),
                "SetConsoleMode(CONIN$ restore) failed"
            );
            restore!(
                SetConsoleMode(self.write.as_raw_handle() as *mut _, self.saved_output),
                "SetConsoleMode(CONOUT$ restore) failed"
            );

            match first_error {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        pub fn set_raw(&mut self) -> anyhow::Result<()> {
            unsafe {
                check_bool(
                    SetConsoleMode(
                        self.read.as_raw_file_descriptor() as *mut _,
                        ENABLE_VIRTUAL_TERMINAL_INPUT,
                    ),
                    "SetConsoleMode(CONIN$ raw) failed",
                )?;
                check_bool(
                    SetConsoleMode(
                        self.write.as_raw_file_descriptor() as *mut _,
                        ENABLE_PROCESSED_OUTPUT
                            | ENABLE_WRAP_AT_EOL_OUTPUT
                            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                            | DISABLE_NEWLINE_AUTO_RETURN,
                    ),
                    "SetConsoleMode(CONOUT$ raw) failed",
                )?;
            }
            Ok(())
        }

        pub fn set_bridge_mode(&mut self) -> anyhow::Result<()> {
            unsafe {
                check_bool(
                    SetConsoleMode(
                        self.read.as_raw_file_descriptor() as *mut _,
                        ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT,
                    ),
                    "SetConsoleMode(CONIN$ bridge) failed",
                )?;
                check_bool(
                    SetConsoleMode(
                        self.write.as_raw_file_descriptor() as *mut _,
                        ENABLE_PROCESSED_OUTPUT
                            | ENABLE_WRAP_AT_EOL_OUTPUT
                            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                            | DISABLE_NEWLINE_AUTO_RETURN,
                    ),
                    "SetConsoleMode(CONOUT$ bridge) failed",
                )?;
            }
            Ok(())
        }

        pub fn reset_outer_input_modes(&mut self) -> anyhow::Result<()> {
            self.write_all(
                b"\x1b[?1l\x1b[?9001l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l",
            )
        }

        pub fn get_size(&self) -> anyhow::Result<PtySize> {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                GetConsoleScreenBufferInfo(
                    self.write.as_raw_handle() as *mut _,
                    &mut info as *mut _,
                )
            };
            if ok == 0 {
                anyhow::bail!(
                    "GetConsoleScreenBufferInfo failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            let cols = 1 + (info.srWindow.Right - info.srWindow.Left);
            let rows = 1 + (info.srWindow.Bottom - info.srWindow.Top);

            Ok(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
        }

        pub fn reader(&self) -> anyhow::Result<FileDescriptor> {
            Ok(self.read.try_clone()?)
        }

        pub fn input_reader(&self) -> anyhow::Result<WinInputReader> {
            Ok(WinInputReader {
                read: self.read.try_clone()?,
            })
        }

        pub fn write_all(&mut self, data: &[u8]) -> anyhow::Result<()> {
            Ok(self.write.write_all(data)?)
        }
    }

    impl Drop for WinTty {
        fn drop(&mut self) {
            let _ = self.set_cooked();
        }
    }

    pub struct WinInputReader {
        read: FileDescriptor,
    }

    impl WinInputReader {
        pub fn read_console_input(
            &mut self,
            num_events: usize,
        ) -> anyhow::Result<Vec<INPUT_RECORD>> {
            let mut records = Vec::with_capacity(num_events);
            let empty_record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            records.resize(num_events, empty_record);

            let mut num_read = 0;
            if unsafe {
                ReadConsoleInputW(
                    self.read.as_raw_handle() as *mut _,
                    records.as_mut_ptr(),
                    num_events as u32,
                    &mut num_read,
                )
            } == 0
            {
                anyhow::bail!(
                    "ReadConsoleInputW failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            unsafe { records.set_len(num_read as usize) };
            Ok(records)
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use termios::{cfmakeraw, tcsetattr, Termios, TCSAFLUSH};

    pub struct UnixTty {
        tty: FileDescriptor,
        termios: Termios,
    }

    fn get_termios(fd: &FileDescriptor) -> anyhow::Result<Termios> {
        Termios::from_fd(fd.as_raw_fd()).context("get_termios failed")
    }

    fn set_termios(
        fd: &FileDescriptor,
        termios: &Termios,
        mode: libc::c_int,
    ) -> anyhow::Result<()> {
        tcsetattr(fd.as_raw_fd(), mode, termios).context("set_termios failed")
    }

    impl UnixTty {
        pub fn new() -> anyhow::Result<Self> {
            let tty = FileDescriptor::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/tty")?,
            );
            let termios = get_termios(&tty)?;

            Ok(Self { tty, termios })
        }

        pub fn set_raw(&mut self) -> anyhow::Result<()> {
            let mut termios = get_termios(&self.tty)?;
            cfmakeraw(&mut termios);
            set_termios(&self.tty, &termios, TCSAFLUSH)
        }

        pub fn set_cooked(&mut self) -> anyhow::Result<()> {
            set_termios(&self.tty, &self.termios, TCSAFLUSH)
        }

        pub fn get_size(&self) -> anyhow::Result<PtySize> {
            let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
            if unsafe { libc::ioctl(self.tty.as_raw_fd(), libc::TIOCGWINSZ as _, &mut size) } != 0 {
                anyhow::bail!(
                    "failed to ioctl(TIOCGWINSZ): {:#}",
                    std::io::Error::last_os_error()
                );
            }

            let size = unsafe { size.assume_init() };

            Ok(PtySize {
                rows: size.ws_row.into(),
                cols: size.ws_col.into(),
                pixel_width: size.ws_xpixel.into(),
                pixel_height: size.ws_ypixel.into(),
            })
        }

        pub fn reader(&self) -> anyhow::Result<FileDescriptor> {
            Ok(self.tty.try_clone()?)
        }

        pub fn write_all(&mut self, data: &[u8]) -> anyhow::Result<()> {
            Ok(self.tty.write_all(data)?)
        }
    }

    impl Drop for UnixTty {
        fn drop(&mut self) {
            let _ = self.set_cooked();
        }
    }
}

#[derive(Debug)]
enum Message {
    /// Input from the user
    Stdin(Vec<u8>),
    /// Output from the child tty
    Stdout(Vec<u8>),
    /// Child tty output reached EOF
    StdoutEof,
    /// Terminal window size changed
    Resize(PtySize),
    /// Child process terminated
    Terminated(portable_pty::ExitStatus),
}

#[derive(Debug, PartialEq, Eq)]
enum RecordLoopControl {
    Continue,
    Break,
}

#[derive(Debug, Default)]
struct RecordLoopState {
    child_status: Option<portable_pty::ExitStatus>,
    stdout_eof: bool,
    child_terminated_at: Option<Instant>,
}

trait RecordLoopIo {
    fn write_stdin(&mut self, data: Vec<u8>) -> anyhow::Result<()>;
    fn write_stdout(&mut self, data: Vec<u8>) -> anyhow::Result<()>;
    fn resize(&mut self, size: PtySize) -> anyhow::Result<()>;
    fn drain_pending_stdout(&mut self) -> anyhow::Result<()>;
}

fn process_record_message(
    state: &mut RecordLoopState,
    io: &mut impl RecordLoopIo,
    msg: Message,
    now: Instant,
) -> anyhow::Result<RecordLoopControl> {
    match msg {
        Message::Stdin(data) => {
            io.write_stdin(data)?;
        }
        Message::Stdout(data) => {
            io.write_stdout(data)?;
        }
        Message::StdoutEof => {
            state.stdout_eof = true;
            if state.child_status.is_some() {
                io.drain_pending_stdout()?;
                return Ok(RecordLoopControl::Break);
            }
        }
        Message::Resize(size) => {
            io.resize(size)?;
        }
        Message::Terminated(status) => {
            state.child_status.replace(status);
            state.child_terminated_at = Some(now);
            if state.stdout_eof {
                io.drain_pending_stdout()?;
                return Ok(RecordLoopControl::Break);
            }
        }
    }

    Ok(RecordLoopControl::Continue)
}

const STDOUT_DRAIN_AFTER_CHILD_EXIT: Duration = Duration::from_millis(500);

fn stdout_drain_timeout_after_child_exit(
    child_status_known: bool,
    stdout_eof: bool,
    child_terminated_at: Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    if !child_status_known || stdout_eof {
        return None;
    }

    Some(
        child_terminated_at
            .unwrap_or(now)
            .checked_add(STDOUT_DRAIN_AFTER_CHILD_EXIT)
            .unwrap_or(now)
            .saturating_duration_since(now),
    )
}

#[derive(Debug, Clone, Copy, Default, ValueEnum, PartialEq, Eq)]
enum WinInputMode {
    #[default]
    Auto,
    On,
    Off,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsInputBridgeAction {
    SetRaw,
    SetBridgeMode,
}

#[cfg(windows)]
fn configure_windows_input_bridge(
    win_input: WinInputMode,
    mut configure: impl FnMut(WindowsInputBridgeAction) -> anyhow::Result<()>,
    mut warn: impl FnMut(&anyhow::Error),
) -> anyhow::Result<bool> {
    match win_input {
        WinInputMode::Off => {
            configure(WindowsInputBridgeAction::SetRaw)?;
            Ok(false)
        }
        WinInputMode::Auto => match configure(WindowsInputBridgeAction::SetBridgeMode) {
            Ok(()) => Ok(true),
            Err(err) => {
                warn(&err);
                configure(WindowsInputBridgeAction::SetRaw)?;
                Ok(false)
            }
        },
        WinInputMode::On => {
            configure(WindowsInputBridgeAction::SetBridgeMode)?;
            Ok(true)
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseTrackingMode {
    None,
    Default,
    ButtonEvent,
    AnyEvent,
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct InnerInputState {
    application_cursor_keys: bool,
    mouse_default: bool,
    mouse_button_event: bool,
    mouse_any_event: bool,
    mouse_tracking: MouseTrackingMode,
    sgr_mouse: bool,
    focus: bool,
    win32_input: bool,
}

#[cfg(windows)]
impl Default for InnerInputState {
    fn default() -> Self {
        Self {
            application_cursor_keys: false,
            mouse_default: false,
            mouse_button_event: false,
            mouse_any_event: false,
            mouse_tracking: MouseTrackingMode::None,
            sgr_mouse: false,
            focus: false,
            win32_input: false,
        }
    }
}

#[cfg(windows)]
impl InnerInputState {
    fn set_mode(&mut self, mode: u16, enabled: bool) {
        match mode {
            1 => self.application_cursor_keys = enabled,
            1000 => self.mouse_default = enabled,
            1002 => self.mouse_button_event = enabled,
            1003 => self.mouse_any_event = enabled,
            1004 => self.focus = enabled,
            1006 => self.sgr_mouse = enabled,
            9001 => self.win32_input = enabled,
            _ => {}
        }
        self.mouse_tracking = if self.mouse_any_event {
            MouseTrackingMode::AnyEvent
        } else if self.mouse_button_event {
            MouseTrackingMode::ButtonEvent
        } else if self.mouse_default {
            MouseTrackingMode::Default
        } else {
            MouseTrackingMode::None
        };
    }
}

#[cfg(windows)]
const MAX_TRACKED_CHILD_OUTPUT_BYTES: usize = 64 * 1024;

#[cfg(windows)]
struct PassthroughControlString {
    terminates_on_bel: bool,
    pending_esc: bool,
    pending_utf8_c2: bool,
}

#[cfg(windows)]
enum Utf8SequenceLen {
    Complete(usize),
    Incomplete,
    Invalid,
}

#[cfg(windows)]
enum Utf8C1Token {
    Complete { c1: u8, len: usize },
    Incomplete,
    None,
}

#[cfg(windows)]
struct InputModeTracker {
    state: Arc<Mutex<InnerInputState>>,
    pending: Vec<u8>,
    passthrough_control_string: Option<PassthroughControlString>,
}

#[cfg(windows)]
impl InputModeTracker {
    fn new(state: Arc<Mutex<InnerInputState>>) -> Self {
        Self {
            state,
            pending: Vec::new(),
            passthrough_control_string: None,
        }
    }

    #[cfg(test)]
    fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.filter_and_get_responses(bytes).0
    }

    fn filter_and_get_responses(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut output = Vec::with_capacity(data.len());
        let mut responses = Vec::new();

        let mut i = 0;
        while i < data.len() {
            if self.passthrough_control_string.is_some() {
                i += self.passthrough_control_string(&mut output, &data[i..]);
                continue;
            }

            if data[i] != 0x1b {
                if data[i] == 0x9b {
                    match self.process_csi_sequence(&data[i..], 1, &mut output, &mut responses) {
                        Some(consumed) => i += consumed,
                        None => break,
                    }
                    continue;
                }
                match Self::utf8_c1_token(&data[i..]) {
                    Utf8C1Token::Complete { c1: 0x9b, len } => {
                        match self.process_csi_sequence(
                            &data[i..],
                            len,
                            &mut output,
                            &mut responses,
                        ) {
                            Some(consumed) => i += consumed,
                            None => break,
                        }
                        continue;
                    }
                    Utf8C1Token::Complete { c1, len: _ } if Self::starts_c1_control_string(c1) => {
                        if let Some(string_end) = Self::terminated_c1_control_string_end(&data[i..])
                        {
                            output.extend_from_slice(&data[i..i + string_end]);
                            i += string_end;
                            continue;
                        }
                        self.buffer_pending_or_passthrough(&mut output, &data[i..]);
                        break;
                    }
                    Utf8C1Token::Incomplete => {
                        self.pending.extend_from_slice(&data[i..]);
                        break;
                    }
                    Utf8C1Token::Complete { len, .. } => {
                        output.extend_from_slice(&data[i..i + len]);
                        i += len;
                        continue;
                    }
                    Utf8C1Token::None => {}
                }
                match Self::utf8_sequence_len(&data[i..]) {
                    Utf8SequenceLen::Complete(len) if len > 1 => {
                        output.extend_from_slice(&data[i..i + len]);
                        i += len;
                        continue;
                    }
                    Utf8SequenceLen::Incomplete => {
                        self.pending.extend_from_slice(&data[i..]);
                        break;
                    }
                    _ => {}
                }
                if let Some(string_end) = Self::terminated_c1_control_string_end(&data[i..]) {
                    output.extend_from_slice(&data[i..i + string_end]);
                    i += string_end;
                    continue;
                }
                if Self::starts_c1_control_string(data[i]) {
                    self.buffer_pending_or_passthrough(&mut output, &data[i..]);
                    break;
                }
                output.push(data[i]);
                i += 1;
                continue;
            }

            if i + 1 >= data.len() {
                self.buffer_pending_or_passthrough(&mut output, &data[i..]);
                break;
            }

            if let Some(string_end) = Self::terminated_control_string_end(&data[i..]) {
                output.extend_from_slice(&data[i..i + string_end]);
                i += string_end;
                continue;
            } else if Self::starts_control_string(data[i + 1]) {
                self.buffer_pending_or_passthrough(&mut output, &data[i..]);
                break;
            }

            if data[i + 1] != b'[' {
                output.push(data[i]);
                i += 1;
                continue;
            }

            match self.process_csi_sequence(&data[i..], 2, &mut output, &mut responses) {
                Some(consumed) => i += consumed,
                None => break,
            }
        }

        (output, responses)
    }

    fn process_csi_sequence(
        &mut self,
        bytes: &[u8],
        param_start: usize,
        output: &mut Vec<u8>,
        responses: &mut Vec<u8>,
    ) -> Option<usize> {
        if bytes.len() <= param_start {
            self.buffer_pending_or_passthrough(output, bytes);
            return None;
        }

        let mut end = param_start;
        let mut abort_at_esc = None;
        while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
            if bytes[end] == 0x1b {
                abort_at_esc = Some(end);
                break;
            }
            end += 1;
        }

        if let Some(esc) = abort_at_esc {
            output.extend_from_slice(&bytes[..esc]);
            return Some(esc);
        }

        if end == bytes.len() {
            self.buffer_pending_or_passthrough(output, bytes);
            return None;
        }

        let final_byte = bytes[end];
        if bytes[param_start] == b'?' && (final_byte == b'h' || final_byte == b'l') {
            let enabled = final_byte == b'h';
            match Self::split_dec_private_modes(&bytes[param_start + 1..end]) {
                Some((owned_modes, passthrough_modes)) if !owned_modes.is_empty() => {
                    if let Ok(mut state) = self.state.lock() {
                        for mode in owned_modes {
                            state.set_mode(mode, enabled);
                        }
                    }
                    if !passthrough_modes.is_empty() {
                        output.extend_from_slice(&bytes[..=param_start]);
                        output.extend_from_slice(passthrough_modes.join(";").as_bytes());
                        output.push(final_byte);
                    }
                }
                _ => output.extend_from_slice(&bytes[..=end]),
            }
        } else if let Some(response) =
            Self::response_for_terminal_query(&bytes[param_start..end], final_byte)
        {
            // Do not let the outer terminal answer DA/DSR/CPR queries. Those
            // responses arrive as console input and race with user text.
            responses.extend_from_slice(response);
        } else {
            output.extend_from_slice(&bytes[..=end]);
        }

        Some(end + 1)
    }

    fn buffer_pending_or_passthrough(&mut self, output: &mut Vec<u8>, bytes: &[u8]) {
        if self.pending.len() + bytes.len() > MAX_TRACKED_CHILD_OUTPUT_BYTES {
            output.extend_from_slice(bytes);
            self.passthrough_control_string = Self::passthrough_control_string_state(bytes);
        } else {
            self.pending.extend_from_slice(bytes);
        }
    }

    fn passthrough_control_string_state(bytes: &[u8]) -> Option<PassthroughControlString> {
        let first = bytes.first().copied()?;
        let terminates_on_bel = if first == 0x1b {
            let second = bytes.get(1).copied()?;
            if !Self::starts_control_string(second) {
                return None;
            }
            second == b']'
        } else if first == 0xc2 {
            let second = bytes.get(1).copied()?;
            if !Self::starts_c1_control_string(second) {
                return None;
            }
            second == 0x9d
        } else if Self::starts_c1_control_string(first) {
            first == 0x9d
        } else {
            return None;
        };

        Some(PassthroughControlString {
            terminates_on_bel,
            pending_esc: bytes.last().copied() == Some(0x1b),
            pending_utf8_c2: bytes.last().copied() == Some(0xc2),
        })
    }

    fn passthrough_control_string(&mut self, output: &mut Vec<u8>, bytes: &[u8]) -> usize {
        let Some(state) = self.passthrough_control_string.as_mut() else {
            return 0;
        };

        for (index, byte) in bytes.iter().copied().enumerate() {
            output.push(byte);

            if state.pending_utf8_c2 {
                state.pending_utf8_c2 = false;
                if byte == 0x9c {
                    self.passthrough_control_string = None;
                    return index + 1;
                }
            }

            if state.pending_esc {
                state.pending_esc = false;
                if byte == b'\\' {
                    self.passthrough_control_string = None;
                    return index + 1;
                }
            }

            if state.terminates_on_bel && byte == 0x07 {
                self.passthrough_control_string = None;
                return index + 1;
            }

            if byte == 0x9c {
                self.passthrough_control_string = None;
                return index + 1;
            }

            if byte == 0xc2 {
                state.pending_utf8_c2 = true;
            }

            if byte == 0x1b {
                state.pending_esc = true;
            }
        }

        bytes.len()
    }

    fn drain_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn split_dec_private_modes(params: &[u8]) -> Option<(Vec<u16>, Vec<String>)> {
        let params = std::str::from_utf8(params).ok()?;
        let mut owned_modes = Vec::new();
        let mut passthrough_modes = Vec::new();

        for param in params.split(';') {
            let mode = param.parse::<u16>().ok()?;
            if Self::owns_dec_private_mode(mode) {
                owned_modes.push(mode);
            } else {
                passthrough_modes.push(param.to_string());
            }
        }

        Some((owned_modes, passthrough_modes))
    }

    fn owns_dec_private_mode(mode: u16) -> bool {
        matches!(mode, 1 | 1000 | 1002 | 1003 | 1004 | 1006 | 9001)
    }

    fn starts_control_string(second_byte: u8) -> bool {
        matches!(second_byte, b']' | b'P' | b'_' | b'^' | b'X')
    }

    fn starts_c1_control_string(byte: u8) -> bool {
        matches!(byte, 0x90 | 0x98 | 0x9d | 0x9e | 0x9f)
    }

    fn utf8_c1_token(bytes: &[u8]) -> Utf8C1Token {
        if bytes.first().copied() != Some(0xc2) {
            return Utf8C1Token::None;
        }

        let Some(second) = bytes.get(1).copied() else {
            return Utf8C1Token::Incomplete;
        };

        if (0x80..=0x9f).contains(&second) {
            Utf8C1Token::Complete { c1: second, len: 2 }
        } else {
            Utf8C1Token::None
        }
    }

    fn utf8_sequence_len(bytes: &[u8]) -> Utf8SequenceLen {
        let Some(first) = bytes.first().copied() else {
            return Utf8SequenceLen::Invalid;
        };

        let expected = match first {
            0x00..=0x7f => return Utf8SequenceLen::Complete(1),
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Utf8SequenceLen::Invalid,
        };

        if bytes.len() < expected {
            if bytes[1..].iter().all(|byte| (0x80..=0xbf).contains(byte)) {
                return Utf8SequenceLen::Incomplete;
            }
            return Utf8SequenceLen::Invalid;
        }

        if bytes[1..expected]
            .iter()
            .all(|byte| (0x80..=0xbf).contains(byte))
        {
            Utf8SequenceLen::Complete(expected)
        } else {
            Utf8SequenceLen::Invalid
        }
    }

    fn terminated_control_string_end(bytes: &[u8]) -> Option<usize> {
        if bytes.len() < 2 || bytes[0] != 0x1b || !Self::starts_control_string(bytes[1]) {
            return None;
        }

        let terminates_on_bel = bytes[1] == b']';
        let mut index = 2;
        while index < bytes.len() {
            if terminates_on_bel && bytes[index] == 0x07 {
                return Some(index + 1);
            }
            if bytes[index] == 0x9c {
                return Some(index + 1);
            }
            if bytes[index] == 0xc2 && bytes.get(index + 1) == Some(&0x9c) {
                return Some(index + 2);
            }
            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                return Some(index + 2);
            }
            index += 1;
        }

        None
    }

    fn terminated_c1_control_string_end(bytes: &[u8]) -> Option<usize> {
        let (c1, start_len) = match Self::utf8_c1_token(bytes) {
            Utf8C1Token::Complete { c1, len } if Self::starts_c1_control_string(c1) => (c1, len),
            Utf8C1Token::Incomplete => return None,
            _ if !bytes.is_empty() && Self::starts_c1_control_string(bytes[0]) => (bytes[0], 1),
            _ => return None,
        };

        let terminates_on_bel = c1 == 0x9d;
        let mut index = start_len;
        while index < bytes.len() {
            if terminates_on_bel && bytes[index] == 0x07 {
                return Some(index + 1);
            }
            if bytes[index] == 0x9c {
                return Some(index + 1);
            }
            if bytes[index] == 0xc2 && bytes.get(index + 1) == Some(&0x9c) {
                return Some(index + 2);
            }
            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                return Some(index + 2);
            }
            index += 1;
        }

        None
    }

    fn response_for_terminal_query(params: &[u8], final_byte: u8) -> Option<&'static [u8]> {
        let params = std::str::from_utf8(params).unwrap_or("");
        match final_byte {
            b'c' if matches!(params, "" | "0") => Some(b"\x1b[?1;0c"),
            b'c' if matches!(params, ">" | ">0") => Some(b"\x1b[>0;0;0c"),
            b'n' if params == "5" => Some(b"\x1b[0n"),
            b'n' if params == "6" => Some(b"\x1b[1;1R"),
            b'n' if params == "?6" => Some(b"\x1b[?1;1R"),
            _ => None,
        }
    }
}

#[cfg(windows)]
fn filter_child_output_for_outer_terminal(
    tracker: &mut InputModeTracker,
    data: &[u8],
    child_input: &mut impl Write,
) -> std::io::Result<Vec<u8>> {
    let (filtered, responses) = tracker.filter_and_get_responses(data);
    if !responses.is_empty() {
        child_input.write_all(&responses)?;
    }
    Ok(filtered)
}

#[cfg(windows)]
#[derive(Debug)]
enum WinInputBatchMessage {
    Stdin(Vec<u8>),
    Resize(PtySize),
}

#[cfg(all(test, windows))]
impl PartialEq for WinInputBatchMessage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Stdin(a), Self::Stdin(b)) => a == b,
            (Self::Resize(a), Self::Resize(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(windows)]
fn encode_win_input_batch(
    parser: &mut termwiz::input::InputParser,
    encoder: &mut WinInputEncoder,
    records: &[winapi::um::wincon::INPUT_RECORD],
    state: &InnerInputState,
) -> Vec<WinInputBatchMessage> {
    let mut messages = Vec::new();
    let mut pending_input_records = Vec::new();

    for record in records {
        if record.EventType == winapi::um::wincon::WINDOW_BUFFER_SIZE_EVENT {
            let data = encoder.encode_records(parser, &pending_input_records, state);
            pending_input_records.clear();
            if !data.is_empty() {
                messages.push(WinInputBatchMessage::Stdin(data));
            }
            let size = unsafe { record.Event.WindowBufferSizeEvent() }.dwSize;
            messages.push(WinInputBatchMessage::Resize(PtySize {
                rows: size.Y.max(1) as u16,
                cols: size.X.max(1) as u16,
                pixel_width: 0,
                pixel_height: 0,
            }));
        } else {
            pending_input_records.push(*record);
        }
    }

    let data = encoder.encode_records(parser, &pending_input_records, state);
    if !data.is_empty() {
        messages.push(WinInputBatchMessage::Stdin(data));
    }

    messages
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct WinInputEncoder {
    last_mouse_buttons: u32,
    pending_high_surrogate: Option<u16>,
}

#[cfg(windows)]
impl WinInputEncoder {
    fn encode_records(
        &mut self,
        parser: &mut termwiz::input::InputParser,
        records: &[winapi::um::wincon::INPUT_RECORD],
        state: &InnerInputState,
    ) -> Vec<u8> {
        use termwiz::input::{KeyCodeEncodeModes, KeyboardEncoding};
        use winapi::um::wincon::*;

        let mut output = Vec::new();
        let modes = KeyCodeEncodeModes {
            encoding: KeyboardEncoding::Xterm,
            application_cursor_keys: state.application_cursor_keys,
            newline_mode: false,
            modify_other_keys: None,
        };

        let mut index = 0;
        while index < records.len() {
            let record = &records[index];
            match record.EventType {
                KEY_EVENT => {
                    if state.win32_input {
                        output.extend_from_slice(
                            Self::encode_win32_key(unsafe { record.Event.KeyEvent() }).as_bytes(),
                        );
                    } else if let Some(text) =
                        self.encode_plain_text_key(unsafe { record.Event.KeyEvent() })
                    {
                        output.extend_from_slice(text.as_bytes());
                    } else {
                        for event in
                            parser.decode_input_records_as_vec(std::slice::from_ref(record))
                        {
                            Self::append_decoded_event(&mut output, event, state, modes);
                        }
                    }
                }
                MOUSE_EVENT => {
                    if let Some(mouse) =
                        self.encode_mouse(unsafe { record.Event.MouseEvent() }, state)
                    {
                        output.extend_from_slice(&mouse);
                    }
                }
                FOCUS_EVENT => {
                    if state.focus {
                        let focus = unsafe { record.Event.FocusEvent() };
                        output.extend_from_slice(if focus.bSetFocus != 0 {
                            b"\x1b[I"
                        } else {
                            b"\x1b[O"
                        });
                    }
                }
                _ => {}
            }

            index += 1;
        }

        output
    }

    fn encode_plain_text_key(
        &mut self,
        key: &winapi::um::wincon::KEY_EVENT_RECORD,
    ) -> Option<String> {
        use winapi::um::wincon::{
            LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED,
        };

        if key.bKeyDown == 0 {
            return None;
        }

        let unicode = *unsafe { key.uChar.UnicodeChar() };
        if unicode == 0 {
            return None;
        }

        let alt_pressed = key.dwControlKeyState & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0;
        let ctrl_pressed = key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
        if alt_pressed || ctrl_pressed {
            self.pending_high_surrogate = None;
            let ch = std::char::from_u32(unicode as u32)?;
            if ctrl_pressed && !ch.is_control() {
                return None;
            }

            let text = if alt_pressed {
                format!("\x1b{ch}")
            } else {
                ch.to_string()
            };
            return Some(text.repeat(key.wRepeatCount as usize));
        }

        if (0xd800..=0xdbff).contains(&unicode) {
            self.pending_high_surrogate = Some(unicode);
            return Some(String::new());
        }

        if (0xdc00..=0xdfff).contains(&unicode) {
            let Some(high) = self.pending_high_surrogate.take() else {
                return Some(String::new());
            };
            let decoded = char::decode_utf16([high, unicode])
                .next()
                .and_then(Result::ok)?;
            return Some(decoded.to_string().repeat(key.wRepeatCount as usize));
        }

        self.pending_high_surrogate = None;
        let ch = std::char::from_u32(unicode as u32)?;
        Some(ch.to_string().repeat(key.wRepeatCount as usize))
    }

    fn append_decoded_event(
        output: &mut Vec<u8>,
        event: termwiz::input::InputEvent,
        _state: &InnerInputState,
        modes: termwiz::input::KeyCodeEncodeModes,
    ) {
        use termwiz::input::InputEvent;

        match event {
            InputEvent::Key(key) => {
                if let Ok(encoded) = key.key.encode(key.modifiers, modes, true) {
                    output.extend_from_slice(encoded.as_bytes());
                }
            }
            InputEvent::Paste(paste) => {
                output.extend_from_slice(paste.as_bytes());
            }
            _ => {}
        }
    }

    fn encode_win32_key(key: &winapi::um::wincon::KEY_EVENT_RECORD) -> String {
        let key_down = if key.bKeyDown != 0 { 1 } else { 0 };
        let unicode = *unsafe { key.uChar.UnicodeChar() } as u32;
        format!(
            "\x1b[{};{};{};{};{};{}_",
            key.wVirtualKeyCode,
            key.wVirtualScanCode,
            unicode,
            key_down,
            key.dwControlKeyState,
            key.wRepeatCount
        )
    }

    fn encode_mouse(
        &mut self,
        mouse: &winapi::um::wincon::MOUSE_EVENT_RECORD,
        state: &InnerInputState,
    ) -> Option<Vec<u8>> {
        use winapi::um::wincon::*;
        const PHYSICAL_BUTTONS: u32 =
            FROM_LEFT_1ST_BUTTON_PRESSED | FROM_LEFT_2ND_BUTTON_PRESSED | RIGHTMOST_BUTTON_PRESSED;

        if state.mouse_tracking == MouseTrackingMode::None {
            self.last_mouse_buttons = 0;
            return None;
        }

        let is_move = (mouse.dwEventFlags & MOUSE_MOVED) != 0;
        let is_wheel = (mouse.dwEventFlags & MOUSE_WHEELED) != 0;
        let is_horizontal_wheel = (mouse.dwEventFlags & MOUSE_HWHEELED) != 0;
        let physical_button_pressed = mouse.dwButtonState & PHYSICAL_BUTTONS != 0;
        let button_event = physical_button_pressed
            || self.last_mouse_buttons != 0
            || is_wheel
            || is_horizontal_wheel;

        let should_send = match state.mouse_tracking {
            MouseTrackingMode::None => false,
            MouseTrackingMode::Default => !is_move && button_event,
            MouseTrackingMode::ButtonEvent => (!is_move && button_event) || physical_button_pressed,
            MouseTrackingMode::AnyEvent => is_move || button_event,
        };

        if !should_send {
            self.last_mouse_buttons = mouse.dwButtonState & PHYSICAL_BUTTONS;
            return None;
        }

        let mut code = if is_wheel {
            let delta = ((mouse.dwButtonState >> 16) & 0xffff) as i16;
            if delta > 0 {
                64
            } else {
                65
            }
        } else if is_horizontal_wheel {
            let delta = ((mouse.dwButtonState >> 16) & 0xffff) as i16;
            if delta > 0 {
                67
            } else {
                66
            }
        } else if is_move && !physical_button_pressed {
            3
        } else {
            let buttons = if mouse.dwButtonState != 0 {
                mouse.dwButtonState
            } else {
                self.last_mouse_buttons
            };
            if (buttons & FROM_LEFT_1ST_BUTTON_PRESSED) != 0 {
                0
            } else if (buttons & FROM_LEFT_2ND_BUTTON_PRESSED) != 0 {
                1
            } else if (buttons & RIGHTMOST_BUTTON_PRESSED) != 0 {
                2
            } else {
                3
            }
        };

        let mut modifier_code = 0;
        if is_move {
            modifier_code += 32;
        }
        if (mouse.dwControlKeyState & SHIFT_PRESSED) != 0 {
            modifier_code += 4;
        }
        if (mouse.dwControlKeyState & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED)) != 0 {
            modifier_code += 8;
        }
        if (mouse.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED)) != 0 {
            modifier_code += 16;
        }
        code += modifier_code;

        let is_release = !is_move
            && !is_wheel
            && !is_horizontal_wheel
            && mouse.dwButtonState == 0
            && self.last_mouse_buttons != 0;
        self.last_mouse_buttons = mouse.dwButtonState & PHYSICAL_BUTTONS;

        if state.sgr_mouse {
            Some(
                format!(
                    "\x1b[<{};{};{}{}",
                    code,
                    mouse.dwMousePosition.X + 1,
                    mouse.dwMousePosition.Y + 1,
                    if is_release { 'm' } else { 'M' }
                )
                .into_bytes(),
            )
        } else {
            let code = if is_release { 3 + modifier_code } else { code };
            let col = i32::from(mouse.dwMousePosition.X) + 1;
            let row = i32::from(mouse.dwMousePosition.Y) + 1;
            if !(1..=223).contains(&col) || !(1..=223).contains(&row) {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                (32 + code) as u8,
                (32 + col) as u8,
                (32 + row) as u8,
            ])
        }
    }
}

#[cfg(all(test, windows))]
mod windows_input_bridge_tests;

#[derive(Debug, Parser, Clone)]
pub struct RecordCommand {
    /// Start in the specified directory, instead of
    /// the default_cwd defined by your wezterm configuration
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,

    /// Save asciicast to the specified file, instead of
    /// using a random file name in the temp directory
    #[arg(short)]
    outfile: Option<std::path::PathBuf>,

    /// Windows input bridge mode for preserving Console input semantics
    #[arg(long = "win-input", default_value = "auto")]
    win_input: WinInputMode,

    /// Start prog instead of the default_prog defined by your
    /// wezterm configuration
    #[arg(value_parser)]
    prog: Vec<OsString>,
}

struct RecordLoopRuntimeIo<'a, W: Write> {
    writer: &'a mut dyn Write,
    master: &'a mut dyn portable_pty::MasterPty,
    tty: &'a mut Tty,
    cast_file: &'a mut W,
    buffer: &'a mut Vec<u8>,
    first_output: Instant,
    #[cfg(windows)]
    input_mode_tracker: &'a mut Option<InputModeTracker>,
}

impl<W: Write> RecordLoopIo for RecordLoopRuntimeIo<'_, W> {
    fn write_stdin(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        self.writer.write_all(&data)?;
        Ok(())
    }

    fn write_stdout(&mut self, mut data: Vec<u8>) -> anyhow::Result<()> {
        let elapsed = self.first_output.elapsed().as_secs_f32();
        #[cfg(windows)]
        if let Some(tracker) = self.input_mode_tracker.as_mut() {
            data = filter_child_output_for_outer_terminal(tracker, &data, &mut self.writer)?;
        }
        if data.is_empty() {
            return Ok(());
        }
        self.tty.write_all(&data)?;
        log_utf8_output(self.cast_file, self.buffer, elapsed, &mut data)?;
        Ok(())
    }

    fn resize(&mut self, size: PtySize) -> anyhow::Result<()> {
        self.master.resize(size)?;
        Ok(())
    }

    fn drain_pending_stdout(&mut self) -> anyhow::Result<()> {
        #[cfg(windows)]
        if let Some(tracker) = self.input_mode_tracker.as_mut() {
            let mut data = tracker.drain_pending();
            if !data.is_empty() {
                let elapsed = self.first_output.elapsed().as_secs_f32();
                self.tty.write_all(&data)?;
                log_utf8_output(self.cast_file, self.buffer, elapsed, &mut data)?;
            }
        }
        Ok(())
    }
}

impl RecordCommand {
    pub fn run(&self, config: ConfigHandle) -> anyhow::Result<()> {
        let prog = self.prog.iter().map(|s| s.as_os_str()).collect::<Vec<_>>();

        let mut tty = Tty::new()?;
        let size = tty.get_size()?;

        let header = Header::new(&config, size, &prog);

        let (cast_file, cast_file_name) = match self.outfile.as_ref() {
            Some(outfile) => (
                std::fs::File::options()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(outfile)?,
                outfile.clone(),
            ),
            None => {
                tempfile::Builder::new()
                    .prefix("wezterm-recording-")
                    // We use a .txt suffix for convenice when uploading to GH
                    .suffix(".cast.txt")
                    .tempfile()?
                    .keep()?
            }
        };
        let mut cast_file = BufWriter::new(cast_file);
        writeln!(cast_file, "{}", serde_json::to_string(&header)?)?;

        let pty_system = native_pty_system();
        let mut pair = pty_system.openpty(size)?;

        let cmd = config.build_prog(
            if self.prog.is_empty() {
                None
            } else {
                Some(prog)
            },
            config.default_prog.as_ref(),
            self.cwd.as_ref().or(config.default_cwd.as_ref()),
        )?;

        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut child_output = pair.master.try_clone_reader()?;

        #[cfg(windows)]
        let use_win_input_bridge = configure_windows_input_bridge(
            self.win_input,
            |action| match action {
                WindowsInputBridgeAction::SetRaw => tty.set_raw(),
                WindowsInputBridgeAction::SetBridgeMode => tty.set_bridge_mode(),
            },
            |err| {
                eprintln!(
                    "warning: failed to initialize Windows input bridge: {err:#}; falling back to legacy input forwarding"
                );
            },
        )?;
        #[cfg(windows)]
        if use_win_input_bridge {
            let _ = tty.reset_outer_input_modes();
        }

        #[cfg(not(windows))]
        tty.set_raw()?;

        #[cfg(windows)]
        let inner_input_state = Arc::new(Mutex::new(InnerInputState::default()));
        #[cfg(windows)]
        let mut input_mode_tracker = if use_win_input_bridge {
            Some(InputModeTracker::new(Arc::clone(&inner_input_state)))
        } else {
            None
        };

        let (tx, rx) = channel();

        {
            let tx = tx.clone();
            std::thread::spawn(move || -> anyhow::Result<()> {
                let mut buf = [0u8; 8192];
                loop {
                    let size = child_output.read(&mut buf)?;
                    if size == 0 {
                        break;
                    }
                    tx.send(Message::Stdout(buf[0..size].to_vec()))?;
                }
                tx.send(Message::StdoutEof)?;
                Ok(())
            });
        }

        #[cfg(windows)]
        if use_win_input_bridge {
            let mut input = tty.input_reader()?;
            let tx = tx.clone();
            let state = Arc::clone(&inner_input_state);
            std::thread::spawn(move || -> anyhow::Result<()> {
                let mut parser = termwiz::input::InputParser::new();
                let mut encoder = WinInputEncoder::default();
                loop {
                    let records = input.read_console_input(128)?;
                    let state = state.lock().map(|state| state.clone()).unwrap_or_default();
                    for message in
                        encode_win_input_batch(&mut parser, &mut encoder, &records, &state)
                    {
                        match message {
                            WinInputBatchMessage::Stdin(data) => {
                                tx.send(Message::Stdin(data))?;
                            }
                            WinInputBatchMessage::Resize(size) => {
                                tx.send(Message::Resize(size))?;
                            }
                        }
                    }
                }
            });
        }

        #[cfg(windows)]
        if !use_win_input_bridge {
            let mut stdin = tty.reader()?;
            let tx = tx.clone();
            std::thread::spawn(move || -> anyhow::Result<()> {
                let mut buf = [0u8; 8192];
                loop {
                    let size = stdin.read(&mut buf)?;
                    if size == 0 {
                        break;
                    }
                    tx.send(Message::Stdin(buf[0..size].to_vec()))?;
                }
                Ok(())
            });
        }

        #[cfg(not(windows))]
        {
            let mut stdin = tty.reader()?;
            let tx = tx.clone();
            std::thread::spawn(move || -> anyhow::Result<()> {
                let mut buf = [0u8; 8192];
                loop {
                    let size = stdin.read(&mut buf)?;
                    if size == 0 {
                        break;
                    }
                    tx.send(Message::Stdin(buf[0..size].to_vec()))?;
                }
                Ok(())
            });
        }

        {
            let tx = tx;
            std::thread::spawn(move || -> anyhow::Result<()> {
                let status = child.wait()?;
                tx.send(Message::Terminated(status))?;
                Ok(())
            });
        }

        let first_output = Instant::now();
        let mut buffer = vec![];
        let mut writer = pair.master.take_writer()?;
        let mut loop_state = RecordLoopState::default();
        let mut loop_io = RecordLoopRuntimeIo {
            writer: writer.as_mut(),
            master: pair.master.as_mut(),
            tty: &mut tty,
            cast_file: &mut cast_file,
            buffer: &mut buffer,
            first_output,
            #[cfg(windows)]
            input_mode_tracker: &mut input_mode_tracker,
        };

        loop {
            let msg = if loop_state.child_status.is_some() && !loop_state.stdout_eof {
                match rx.recv_timeout(
                    stdout_drain_timeout_after_child_exit(
                        true,
                        loop_state.stdout_eof,
                        loop_state.child_terminated_at,
                        Instant::now(),
                    )
                    .unwrap_or_default(),
                ) {
                    Ok(msg) => msg,
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                }
            };

            if process_record_message(&mut loop_state, &mut loop_io, msg, Instant::now())?
                == RecordLoopControl::Break
            {
                break;
            }
        }

        loop_io.drain_pending_stdout()?;
        drop(loop_io);

        #[cfg(windows)]
        if use_win_input_bridge {
            let _ = tty.reset_outer_input_modes();
        }
        tty.set_cooked()?;
        eprintln!("Child status: {:?}", loop_state.child_status);
        cast_file.flush()?;
        eprintln!("*** Finished recording to {}", cast_file_name.display());

        Ok(())
    }
}

#[derive(Debug, Parser, Clone)]
pub struct PlayCommand {
    /// Explain what is being sent/received
    #[arg(long)]
    explain: bool,

    /// Don't replay, just show the explanation
    #[arg(long, conflicts_with = "explain")]
    explain_only: bool,

    /// Just emit raw escape sequences all at once, with no timing information
    #[arg(long, conflicts_with = "explain")]
    cat: bool,

    cast_file: PathBuf,
}

impl PlayCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        let mut cast_file = BufReader::new(
            std::fs::File::open(&self.cast_file)
                .with_context(|| format!("reading cast file {}", self.cast_file.display()))?,
        );
        let mut header_line = String::new();
        cast_file
            .read_line(&mut header_line)
            .context("reading Header line")?;

        let header: Header = serde_json::from_str(&header_line).context("parsing Header")?;

        if self.cat {
            for line in cast_file.lines() {
                let line = line?;
                let event: Event = serde_json::from_str(&line)?;
                if event.1 != "o" {
                    continue;
                }
                std::io::stdout().write_all(&event.2.as_bytes())?;
            }

            return Ok(());
        }

        let (tx, rx) = channel();
        let mut sent_parser = TWParser::new();
        let mut sent_actions = vec![];

        if self.explain_only {
            for line in cast_file.lines() {
                let line = line?;
                let event: Event = serde_json::from_str(&line)?;
                if event.1 != "o" {
                    continue;
                }
                sent_parser.parse(&event.2.as_bytes(), |act| sent_actions.push(act));
            }
            drop(tx);
        } else {
            let mut tty = Tty::new()?;
            let size = tty.get_size()?;
            if u32::from(size.cols) < header.width || u32::from(size.rows) < header.height {
                anyhow::bail!(
                    "{} was recorded with width={} and height={}
                     but the current screen dimensions {}x{} are
                     too small to display it",
                    self.cast_file.display(),
                    header.width,
                    header.height,
                    size.cols,
                    size.rows
                );
            }

            tty.set_raw()?;

            {
                let mut stdin = tty.reader()?;
                let tx = tx;
                std::thread::spawn(move || -> anyhow::Result<()> {
                    let mut buf = [0u8; 8192];
                    loop {
                        let size = stdin.read(&mut buf)?;
                        if size == 0 {
                            break;
                        }
                        tx.send(Message::Stdin(buf[0..size].to_vec()))?;
                    }
                    Ok(())
                });
            }

            let start = Instant::now();

            for line in cast_file.lines() {
                let line = line?;
                let event: Event = serde_json::from_str(&line)?;
                if event.1 != "o" {
                    continue;
                }
                let target = start + Duration::from_secs_f32(event.0);
                let duration = target.saturating_duration_since(Instant::now());
                std::thread::sleep(duration);

                tty.write_all(&event.2.as_bytes())?;
                sent_parser.parse(&event.2.as_bytes(), |act| sent_actions.push(act));
            }

            std::thread::sleep(Duration::from_millis(100));

            tty.set_cooked()?;
        }

        if self.explain || self.explain_only {
            println!("> SENT");
            for s in summarize(sent_actions) {
                println!("\t{:?}", s);
            }
        }

        if !self.explain_only {
            if self.explain {
                println!("< RECV");
            }
            let mut parser = TWParser::new();
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Message::Stdin(data) => {
                        if self.explain {
                            let answer_back = String::from_utf8_lossy(&data);
                            println!("\t{:?}", answer_back);
                            parser.parse(&data, |action| {
                                println!("\t{:?}", action);
                            });
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }

        Ok(())
    }
}

#[allow(dead_code)]
#[derive(Debug)]
enum Summarized {
    Action(Action),
    Print(String),
}

fn summarize(actions: Vec<Action>) -> Vec<Summarized> {
    let mut print = String::new();
    let mut res = vec![];
    for act in actions {
        match act {
            Action::Print(c) => print.push(c),
            act => {
                if !print.is_empty() {
                    res.push(Summarized::Print(print.escape_default().to_string()));
                    print.clear();
                }
                res.push(Summarized::Action(act));
            }
        }
    }
    if !print.is_empty() {
        res.push(Summarized::Print(print.escape_default().to_string()));
    }
    res
}
