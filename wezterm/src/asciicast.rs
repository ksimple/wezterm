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
            unsafe {
                check_bool(
                    SetConsoleOutputCP(self.saved_cp),
                    "SetConsoleOutputCP failed",
                )?;
                check_bool(
                    SetConsoleMode(self.read.as_raw_handle() as *mut _, self.saved_input),
                    "SetConsoleMode(CONIN$ restore) failed",
                )?;
                check_bool(
                    SetConsoleMode(self.write.as_raw_handle() as *mut _, self.saved_output),
                    "SetConsoleMode(CONOUT$ restore) failed",
                )?;
            }
            Ok(())
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
                b"\x1b[?1l\x1b[?9001l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l",
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
                write: self.write.try_clone()?,
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
        write: FileDescriptor,
    }

    impl WinInputReader {
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
    bracketed_paste: bool,
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
            bracketed_paste: false,
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
            2004 => self.bracketed_paste = enabled,
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
struct InputModeTracker {
    state: Arc<Mutex<InnerInputState>>,
    pending: Vec<u8>,
}

#[cfg(windows)]
impl InputModeTracker {
    fn new(state: Arc<Mutex<InnerInputState>>) -> Self {
        Self {
            state,
            pending: Vec::new(),
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
            if data[i] != 0x1b {
                output.push(data[i]);
                i += 1;
                continue;
            }

            if i + 1 >= data.len() {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }

            if let Some(string_end) = Self::terminated_control_string_end(&data[i..]) {
                output.extend_from_slice(&data[i..i + string_end]);
                i += string_end;
                continue;
            } else if Self::starts_control_string(data[i + 1]) {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }

            if data[i + 1] != b'[' {
                output.push(data[i]);
                i += 1;
                continue;
            }

            if i + 2 >= data.len() {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }

            let mut end = i + 2;
            while end < data.len() && !(0x40..=0x7e).contains(&data[end]) {
                end += 1;
            }

            if end == data.len() {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }

            let final_byte = data[end];
            if data[i + 2] == b'?' && (final_byte == b'h' || final_byte == b'l') {
                let enabled = final_byte == b'h';
                match Self::split_dec_private_modes(&data[i + 3..end]) {
                    Some((owned_modes, passthrough_modes)) if !owned_modes.is_empty() => {
                        if let Ok(mut state) = self.state.lock() {
                            for mode in owned_modes {
                                state.set_mode(mode, enabled);
                            }
                        }
                        if !passthrough_modes.is_empty() {
                            output.extend_from_slice(b"\x1b[?");
                            output.extend_from_slice(passthrough_modes.join(";").as_bytes());
                            output.push(final_byte);
                        }
                    }
                    _ => output.extend_from_slice(&data[i..=end]),
                }
            } else if let Some(response) =
                Self::response_for_terminal_query(&data[i + 2..end], final_byte)
            {
                // Do not let the outer terminal answer DA/DSR/CPR queries. Those
                // responses arrive as console input and race with user text.
                responses.extend_from_slice(response);
            } else {
                output.extend_from_slice(&data[i..=end]);
            }

            i = end + 1;
        }

        (output, responses)
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
        matches!(mode, 1 | 1000 | 1002 | 1003 | 1004 | 1006 | 2004 | 9001)
    }

    fn starts_control_string(second_byte: u8) -> bool {
        matches!(second_byte, b']' | b'P' | b'_' | b'^' | b'X')
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
    Resize,
}

#[cfg(all(test, windows))]
impl PartialEq for WinInputBatchMessage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Stdin(a), Self::Stdin(b)) => a == b,
            (Self::Resize, Self::Resize) => true,
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
            messages.push(WinInputBatchMessage::Resize);
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
                        Self::encode_plain_text_key(unsafe { record.Event.KeyEvent() })
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

    fn encode_plain_text_key(key: &winapi::um::wincon::KEY_EVENT_RECORD) -> Option<String> {
        if key.bKeyDown == 0 {
            return None;
        }

        let unicode = *unsafe { key.uChar.UnicodeChar() };
        if unicode == 0 {
            return None;
        }

        let ch = std::char::from_u32(unicode as u32)?;
        Some(ch.to_string().repeat(key.wRepeatCount as usize))
    }

    fn append_decoded_event(
        output: &mut Vec<u8>,
        event: termwiz::input::InputEvent,
        state: &InnerInputState,
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
                if state.bracketed_paste {
                    output.extend_from_slice(b"\x1b[200~");
                    output.extend_from_slice(paste.as_bytes());
                    output.extend_from_slice(b"\x1b[201~");
                } else {
                    output.extend_from_slice(paste.as_bytes());
                }
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
                66
            } else {
                67
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
mod windows_input_bridge_tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;
    use winapi::shared::minwindef::TRUE;
    use winapi::um::consoleapi::GetConsoleMode;
    use winapi::um::wincon::*;
    use winapi::um::winuser;

    fn tracker() -> (Arc<Mutex<InnerInputState>>, InputModeTracker) {
        let state = Arc::new(Mutex::new(InnerInputState::default()));
        let tracker = InputModeTracker::new(Arc::clone(&state));
        (state, tracker)
    }

    fn mouse_record(
        x: i16,
        y: i16,
        button_state: u32,
        control_state: u32,
        flags: u32,
    ) -> MOUSE_EVENT_RECORD {
        MOUSE_EVENT_RECORD {
            dwMousePosition: COORD { X: x, Y: y },
            dwButtonState: button_state,
            dwControlKeyState: control_state,
            dwEventFlags: flags,
        }
    }

    fn key_record(
        virtual_key: u16,
        scan_code: u16,
        ch: char,
        key_down: bool,
        repeat_count: u16,
        control_state: u32,
    ) -> INPUT_RECORD {
        let mut key: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
        key.bKeyDown = if key_down { TRUE } else { 0 };
        key.wRepeatCount = repeat_count;
        key.wVirtualKeyCode = virtual_key;
        key.wVirtualScanCode = scan_code;
        key.dwControlKeyState = control_state;
        *unsafe { key.uChar.UnicodeChar_mut() } = ch as u16;

        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = KEY_EVENT;
        unsafe {
            *record.Event.KeyEvent_mut() = key;
        }
        record
    }

    fn key_record_u16(
        virtual_key: u16,
        scan_code: u16,
        unicode: u16,
        key_down: bool,
        repeat_count: u16,
        control_state: u32,
    ) -> INPUT_RECORD {
        let mut key: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
        key.bKeyDown = if key_down { TRUE } else { 0 };
        key.wRepeatCount = repeat_count;
        key.wVirtualKeyCode = virtual_key;
        key.wVirtualScanCode = scan_code;
        key.dwControlKeyState = control_state;
        *unsafe { key.uChar.UnicodeChar_mut() } = unicode;

        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = KEY_EVENT;
        unsafe {
            *record.Event.KeyEvent_mut() = key;
        }
        record
    }

    fn mouse_input_record(mouse: MOUSE_EVENT_RECORD) -> INPUT_RECORD {
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = MOUSE_EVENT;
        unsafe {
            *record.Event.MouseEvent_mut() = mouse;
        }
        record
    }

    fn resize_input_record(cols: i16, rows: i16) -> INPUT_RECORD {
        let mut resize: WINDOW_BUFFER_SIZE_RECORD = unsafe { std::mem::zeroed() };
        resize.dwSize = COORD { X: cols, Y: rows };
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = WINDOW_BUFFER_SIZE_EVENT;
        unsafe {
            *record.Event.WindowBufferSizeEvent_mut() = resize;
        }
        record
    }

    fn encode_records_with_state(records: &[INPUT_RECORD], state: &InnerInputState) -> Vec<u8> {
        let mut encoder = WinInputEncoder::default();
        let mut parser = termwiz::input::InputParser::new();
        encoder.encode_records(&mut parser, records, state)
    }

    fn encode_records_default(records: &[INPUT_RECORD]) -> Vec<u8> {
        encode_records_with_state(records, &InnerInputState::default())
    }

    fn encode_mouse_with_state(
        mouse: &MOUSE_EVENT_RECORD,
        state: &InnerInputState,
    ) -> Option<Vec<u8>> {
        WinInputEncoder::default().encode_mouse(mouse, state)
    }

    fn win32_state() -> InnerInputState {
        InnerInputState {
            win32_input: true,
            ..Default::default()
        }
    }

    fn sgr_mouse_state(mouse_tracking: MouseTrackingMode) -> InnerInputState {
        InnerInputState {
            mouse_default: matches!(mouse_tracking, MouseTrackingMode::Default),
            mouse_button_event: matches!(mouse_tracking, MouseTrackingMode::ButtonEvent),
            mouse_any_event: matches!(mouse_tracking, MouseTrackingMode::AnyEvent),
            mouse_tracking,
            sgr_mouse: true,
            ..Default::default()
        }
    }

    fn legacy_mouse_state(mouse_tracking: MouseTrackingMode) -> InnerInputState {
        InnerInputState {
            mouse_default: matches!(mouse_tracking, MouseTrackingMode::Default),
            mouse_button_event: matches!(mouse_tracking, MouseTrackingMode::ButtonEvent),
            mouse_any_event: matches!(mouse_tracking, MouseTrackingMode::AnyEvent),
            mouse_tracking,
            sgr_mouse: false,
            ..Default::default()
        }
    }

    fn expected_win32_key(
        virtual_key: u16,
        scan_code: u16,
        unicode: u32,
        key_down: bool,
        control_state: u32,
        repeat_count: u16,
    ) -> Vec<u8> {
        format!(
            "\x1b[{virtual_key};{scan_code};{unicode};{};{control_state};{repeat_count}_",
            if key_down { 1 } else { 0 }
        )
        .into_bytes()
    }

    mod tracker_and_run_loop {
        use super::*;

        #[test]
        fn filters_owned_input_modes_and_updates_state() {
            let (state, mut tracker) = tracker();

            let output = tracker.filter(b"before\x1b[?9001h\x1b[?1000;1006;2004hafter");

            assert_eq!(output, b"beforeafter");
            let state = state.lock().unwrap();
            assert!(state.win32_input);
            assert_eq!(state.mouse_tracking, MouseTrackingMode::Default);
            assert!(state.sgr_mouse);
            assert!(state.bracketed_paste);
        }

        #[test]
        fn preserves_unowned_dec_private_modes_from_mixed_sequence() {
            let (state, mut tracker) = tracker();

            let output = tracker.filter(b"\x1b[?25;1000h");

            assert_eq!(output, b"\x1b[?25h");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::Default
            );
        }

        #[test]
        fn malformed_dec_private_modes_are_preserved_without_state_changes() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?1000;badh"), b"\x1b[?1000;badh");

            let state = state.lock().unwrap();
            assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
            assert!(!state.mouse_default);
        }

        #[test]
        fn non_private_sm_rm_with_owned_numbers_are_preserved() {
            let (state, mut tracker) = tracker();

            assert_eq!(
                tracker.filter(b"\x1b[1000h\x1b[2004l"),
                b"\x1b[1000h\x1b[2004l"
            );

            let state = state.lock().unwrap();
            assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
            assert!(!state.bracketed_paste);
        }

        #[test]
        fn csi_with_intermediates_or_subparams_is_preserved_without_state_changes() {
            let (state, mut tracker) = tracker();

            for sequence in [
                b"\x1b[?1000$h".as_slice(),
                b"\x1b[?1000 h",
                b"\x1b[?1000:1h",
            ] {
                assert_eq!(tracker.filter(sequence), sequence);
            }

            let state = state.lock().unwrap();
            assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
            assert!(!state.mouse_default);
        }

        #[test]
        fn unowned_mouse_modes_are_preserved_when_mixed_with_owned_modes() {
            let (state, mut tracker) = tracker();

            assert_eq!(
                tracker.filter(b"\x1b[?1000;1005;1015;1016h"),
                b"\x1b[?1005;1015;1016h"
            );

            let state = state.lock().unwrap();
            assert_eq!(state.mouse_tracking, MouseTrackingMode::Default);
            assert!(state.mouse_default);
        }

        #[test]
        fn mixed_owned_unowned_reset_filters_only_owned_modes() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?1000;1006;2004h"), b"");
            assert_eq!(tracker.filter(b"\x1b[?25;1000;1006;2004l"), b"\x1b[?25l");

            let state = state.lock().unwrap();
            assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
            assert!(!state.sgr_mouse);
            assert!(!state.bracketed_paste);
        }

        #[test]
        fn repeated_private_mode_params_are_idempotent() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?1000;1000;25;25h"), b"\x1b[?25;25h");

            let state = state.lock().unwrap();
            assert_eq!(state.mouse_tracking, MouseTrackingMode::Default);
            assert!(state.mouse_default);
        }

        #[test]
        fn set_reset_order_in_single_buffer_uses_last_state() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?1000h\x1b[?1000l"), b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::None
            );

            assert_eq!(tracker.filter(b"\x1b[?1000l\x1b[?1000h"), b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::Default
            );
        }

        #[test]
        fn filters_fragmented_input_mode_sequence() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"a\x1b[?10"), b"a");
            assert_eq!(tracker.filter(b"04hB"), b"B");

            assert!(state.lock().unwrap().focus);
        }

        #[test]
        fn filters_fragmented_mixed_input_mode_sequence() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?25;10"), b"");
            assert_eq!(tracker.filter(b"00hX"), b"\x1b[?25hX");

            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::Default
            );
        }

        #[test]
        fn preserves_fragmented_unowned_mode_sequence() {
            let (_state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?25"), b"");
            assert_eq!(tracker.filter(b"h"), b"\x1b[?25h");
        }

        #[test]
        fn drains_incomplete_pending_output() {
            let (_state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"literal\x1b[?"), b"literal");
            assert_eq!(tracker.drain_pending(), b"\x1b[?");
        }

        #[test]
        fn filters_terminal_response_queries_from_child_output() {
            let (_state, mut tracker) = tracker();

            let (output, responses) =
                tracker.filter_and_get_responses(b"a\x1b[c\x1b[>0c\x1b[6n\x1b[5nb");

            assert_eq!(output, b"ab");
            assert_eq!(responses, b"\x1b[?1;0c\x1b[>0;0;0c\x1b[1;1R\x1b[0n");
        }

        #[test]
        fn filters_fragmented_terminal_response_queries_from_child_output() {
            let (_state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"pre\x1b["), b"pre");
            let (output, responses) = tracker.filter_and_get_responses(b"6npost");
            assert_eq!(output, b"post");
            assert_eq!(responses, b"\x1b[1;1R");
            assert_eq!(tracker.drain_pending(), b"");

            assert_eq!(tracker.filter(b"\x1b[>"), b"");
            let (output, responses) = tracker.filter_and_get_responses(b"0c");
            assert_eq!(output, b"");
            assert_eq!(responses, b"\x1b[>0;0;0c");
        }

        #[test]
        fn terminal_query_near_misses_are_preserved_without_responses() {
            let (_state, mut tracker) = tracker();

            for query in [
                b"\x1b[?6n".as_slice(),
                b"\x1b[0n",
                b"\x1b[>1c",
                b"\x1b[?1;0c",
            ] {
                let (output, responses) = tracker.filter_and_get_responses(query);
                assert_eq!(output, query);
                assert_eq!(responses, b"");
            }
        }

        #[test]
        fn terminal_queries_work_when_split_at_every_byte() {
            for (query, response) in [
                (b"\x1b[c".as_slice(), b"\x1b[?1;0c".as_slice()),
                (b"\x1b[0c", b"\x1b[?1;0c"),
                (b"\x1b[>c", b"\x1b[>0;0;0c"),
                (b"\x1b[>0c", b"\x1b[>0;0;0c"),
                (b"\x1b[5n", b"\x1b[0n"),
                (b"\x1b[6n", b"\x1b[1;1R"),
            ] {
                for split_at in 1..query.len() {
                    let (_state, mut tracker) = tracker();
                    assert_eq!(tracker.filter(&query[..split_at]), b"");
                    let (output, responses) = tracker.filter_and_get_responses(&query[split_at..]);
                    assert_eq!(output, b"");
                    assert_eq!(responses, response);
                    assert_eq!(tracker.drain_pending(), b"");
                }
            }
        }

        #[test]
        fn child_output_filter_writes_query_responses_without_outer_output() {
            let (_state, mut tracker) = tracker();
            let mut child_input = Vec::new();

            let output = filter_child_output_for_outer_terminal(
                &mut tracker,
                b"before\x1b[c\x1b[5n\x1b[6nafter",
                &mut child_input,
            )
            .unwrap();

            assert_eq!(output, b"beforeafter");
            assert_eq!(child_input, b"\x1b[?1;0c\x1b[0n\x1b[1;1R");
        }

        #[test]
        fn child_output_filter_keeps_markers_and_unowned_modes_out_of_child_input() {
            let (state, mut tracker) = tracker();
            let mut child_input = Vec::new();

            let output = filter_child_output_for_outer_terminal(
                &mut tracker,
                b"start\x1b[?9001h\x1b[?25;1006hMARK\x1b[?25;1006lend",
                &mut child_input,
            )
            .unwrap();

            assert_eq!(output, b"start\x1b[?25hMARK\x1b[?25lend");
            assert_eq!(child_input, b"");
            let state = state.lock().unwrap();
            assert!(state.win32_input);
            assert!(!state.sgr_mouse);
        }

        #[test]
        fn child_output_filter_handles_fragmented_query_and_mode_sequences() {
            let (state, mut tracker) = tracker();
            let mut child_input = Vec::new();

            assert_eq!(
                filter_child_output_for_outer_terminal(&mut tracker, b"A\x1b[?", &mut child_input)
                    .unwrap(),
                b"A"
            );
            assert_eq!(
                filter_child_output_for_outer_terminal(
                    &mut tracker,
                    b"9001hB\x1b[",
                    &mut child_input
                )
                .unwrap(),
                b"B"
            );
            assert_eq!(
                filter_child_output_for_outer_terminal(&mut tracker, b"6nC", &mut child_input)
                    .unwrap(),
                b"C"
            );

            assert_eq!(child_input, b"\x1b[1;1R");
            assert!(state.lock().unwrap().win32_input);
            assert_eq!(tracker.drain_pending(), b"");
        }

        #[test]
        fn osc_payload_csi_like_bytes_are_preserved_without_state_or_responses() {
            let (state, mut tracker) = tracker();

            let (output, responses) =
                tracker.filter_and_get_responses(b"pre\x1b]0;title \x1b[6n \x1b[?1000h\x07post");

            assert_eq!(output, b"pre\x1b]0;title \x1b[6n \x1b[?1000h\x07post");
            assert_eq!(responses, b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::None
            );
        }

        #[test]
        fn dcs_payload_csi_like_bytes_are_preserved_across_fragments() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"pre\x1bPtmux;\x1b\x1b[?9001h"), b"pre");
            let (output, responses) = tracker.filter_and_get_responses(b"\x1b[6n\x1b\\post");

            assert_eq!(output, b"\x1bPtmux;\x1b\x1b[?9001h\x1b[6n\x1b\\post");
            assert_eq!(responses, b"");
            assert!(!state.lock().unwrap().win32_input);
        }

        #[test]
        fn stdout_drain_timeout_only_applies_after_child_exit_before_stdout_eof() {
            let now = Instant::now();

            assert_eq!(
                stdout_drain_timeout_after_child_exit(false, false, None, now),
                None
            );
            assert_eq!(
                stdout_drain_timeout_after_child_exit(true, true, Some(now), now),
                None
            );
            assert_eq!(
                stdout_drain_timeout_after_child_exit(true, false, Some(now), now),
                Some(STDOUT_DRAIN_AFTER_CHILD_EXIT)
            );
        }

        #[test]
        fn stdout_drain_timeout_saturates_after_grace_period() {
            let child_exit = Instant::now();
            let after_grace = child_exit + STDOUT_DRAIN_AFTER_CHILD_EXIT + Duration::from_millis(1);

            assert_eq!(
                stdout_drain_timeout_after_child_exit(true, false, Some(child_exit), after_grace),
                Some(Duration::ZERO)
            );
        }

        #[test]
        fn preserves_non_query_csi_output() {
            let (_state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[31mred"), b"\x1b[31mred");
        }

        #[test]
        fn tracks_application_cursor_mode() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?1h"), b"");
            assert!(state.lock().unwrap().application_cursor_keys);
            assert_eq!(tracker.filter(b"\x1b[?1l"), b"");
            assert!(!state.lock().unwrap().application_cursor_keys);
        }

        #[test]
        fn mouse_mode_precedence_falls_back_when_modes_reset() {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(b"\x1b[?1000;1002;1003h"), b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::AnyEvent
            );
            assert_eq!(tracker.filter(b"\x1b[?1003l"), b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::ButtonEvent
            );
            assert_eq!(tracker.filter(b"\x1b[?1002l"), b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::Default
            );
            assert_eq!(tracker.filter(b"\x1b[?1000l"), b"");
            assert_eq!(
                state.lock().unwrap().mouse_tracking,
                MouseTrackingMode::None
            );
        }
    }

    mod keyboard {
        use super::*;

        #[test]
        fn application_cursor_mode_changes_arrow_encoding() {
            let up = key_record(winuser::VK_UP as u16, 0x48, '\0', true, 1, 0);

            assert_eq!(encode_records_default(&[up]), b"\x1b[A");

            let state = InnerInputState {
                application_cursor_keys: true,
                ..Default::default()
            };
            assert_eq!(encode_records_with_state(&[up], &state), b"\x1bOA");
        }

        #[test]
        fn preserves_record_order_between_non_printable_and_printable_keys() {
            let left = key_record(winuser::VK_LEFT as u16, 0x4b, '\0', true, 1, 0);
            let a = key_record('A' as u16, 0x1e, 'a', true, 1, 0);

            assert_eq!(encode_records_default(&[left, a]), b"\x1b[Da");
        }

        #[test]
        fn repeats_plain_printable_key_records() {
            let a = key_record('A' as u16, 0x1e, 'a', true, 3, 0);

            assert_eq!(encode_records_default(&[a]), b"aaa");
        }

        #[test]
        fn win32_input_encodes_printable_key_records() {
            let a = key_record('A' as u16, 0x1e, 'a', true, 2, 0);

            assert_eq!(
                encode_records_with_state(&[a], &win32_state()),
                expected_win32_key(65, 30, 97, true, 0, 2)
            );
        }

        #[test]
        fn win32_input_preserves_surrogate_pair_code_units() {
            let high = key_record_u16(0, 0, 0xd83d, true, 1, 0);
            let low = key_record_u16(0, 0, 0xde00, true, 1, 0);

            assert_eq!(
                encode_records_with_state(&[high, low], &win32_state()),
                [
                    expected_win32_key(0, 0, 0xd83d, true, 0, 1),
                    expected_win32_key(0, 0, 0xde00, true, 0, 1)
                ]
                .concat()
            );
        }

        #[test]
        fn key_up_is_ignored_in_plain_mode_and_encoded_in_win32_mode() {
            let a_up = key_record('A' as u16, 0x1e, 'a', false, 1, 0);

            assert_eq!(encode_records_default(&[a_up]), b"");

            assert_eq!(
                encode_records_with_state(&[a_up], &win32_state()),
                expected_win32_key(65, 30, 97, false, 0, 1)
            );
        }

        #[test]
        fn repeats_non_printable_key_records() {
            let up = key_record(winuser::VK_UP as u16, 0x48, '\0', true, 3, 0);

            assert_eq!(encode_records_default(&[up]), b"\x1b[A\x1b[A\x1b[A");
        }

        #[test]
        fn plain_text_preserves_bmp_unicode_and_ignores_unpaired_surrogates() {
            let e_acute = key_record(0, 0, 'é', true, 2, 0);
            let high_surrogate = key_record_u16(0, 0, 0xd83d, true, 1, 0);

            assert_eq!(encode_records_default(&[e_acute]), "éé".as_bytes());
            assert_eq!(encode_records_default(&[high_surrogate]), b"");
        }

        #[test]
        fn response_like_text_is_not_swallowed_in_plain_mode() {
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let records = [
                key_record('[' as u16, 0, '[', true, 1, 0),
                key_record('?' as u16, 0, '?', true, 1, 0),
                key_record('6' as u16, 0, '6', true, 1, 0),
                key_record('1' as u16, 0, '1', true, 1, 0),
                key_record('c' as u16, 0, 'c', true, 1, 0),
                key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            ];

            assert_eq!(
                encoder.encode_records(&mut parser, &records, &InnerInputState::default()),
                b"[?61ca"
            );
        }

        #[test]
        fn esc_prefixed_response_like_text_is_not_swallowed() {
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let first = [
                key_record(winuser::VK_ESCAPE as u16, 1, '\x1b', true, 1, 0),
                key_record('[' as u16, 0, '[', true, 1, 0),
                key_record('6' as u16, 0, '6', true, 1, 0),
            ];
            let second = [
                key_record('n' as u16, 0, 'n', true, 1, 0),
                key_record('a' as u16, 0x1e, 'a', true, 1, 0),
            ];

            assert_eq!(
                encoder.encode_records(&mut parser, &first, &InnerInputState::default()),
                b"\x1b[6"
            );
            assert_eq!(
                encoder.encode_records(&mut parser, &second, &InnerInputState::default()),
                b"na"
            );
        }

        #[test]
        fn response_like_text_is_encoded_in_win32_mode() {
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let records = [
                key_record('[' as u16, 0, '[', true, 1, 0),
                key_record('?' as u16, 0, '?', true, 1, 0),
                key_record('6' as u16, 0, '6', true, 1, 0),
                key_record('1' as u16, 0, '1', true, 1, 0),
                key_record('c' as u16, 0, 'c', true, 1, 0),
                key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            ];
            let state = InnerInputState {
                win32_input: true,
                ..Default::default()
            };

            assert_eq!(
            encoder.encode_records(&mut parser, &records, &state),
            b"\x1b[91;0;91;1;0;1_\x1b[63;0;63;1;0;1_\x1b[54;0;54;1;0;1_\x1b[49;0;49;1;0;1_\x1b[99;0;99;1;0;1_\x1b[65;30;97;1;0;1_"
        );
        }

        #[test]
        fn split_response_like_text_is_not_swallowed() {
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let first = [
                key_record('[' as u16, 0, '[', true, 1, 0),
                key_record('?' as u16, 0, '?', true, 1, 0),
                key_record('6' as u16, 0, '6', true, 1, 0),
                key_record('1' as u16, 0, '1', true, 1, 0),
            ];
            let second = [
                key_record('c' as u16, 0, 'c', true, 1, 0),
                key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            ];

            assert_eq!(
                encoder.encode_records(&mut parser, &first, &InnerInputState::default()),
                b"[?61"
            );
            assert_eq!(
                encoder.encode_records(&mut parser, &second, &InnerInputState::default()),
                b"ca"
            );
        }
    }

    mod mouse {
        use super::*;

        #[test]
        fn encodes_sgr_horizontal_mouse_wheel() {
            let state = sgr_mouse_state(MouseTrackingMode::AnyEvent);
            let mouse = mouse_record(41, 11, 120u32 << 16, 0, MOUSE_HWHEELED);

            assert_eq!(
                encode_mouse_with_state(&mouse, &state).unwrap(),
                b"\x1b[<66;42;12M"
            );
        }

        #[test]
        fn encodes_sgr_wheel_directions() {
            let mut encoder = WinInputEncoder::default();
            let state = sgr_mouse_state(MouseTrackingMode::AnyEvent);

            assert_eq!(
                encoder
                    .encode_mouse(
                        &mouse_record(41, 11, 120u32 << 16, 0, MOUSE_WHEELED),
                        &state
                    )
                    .unwrap(),
                b"\x1b[<64;42;12M"
            );
            assert_eq!(
                encoder
                    .encode_mouse(
                        &mouse_record(41, 11, (-120i32 as u32) << 16, 0, MOUSE_WHEELED),
                        &state
                    )
                    .unwrap(),
                b"\x1b[<65;42;12M"
            );
            assert_eq!(
                encoder
                    .encode_mouse(
                        &mouse_record(41, 11, (-120i32 as u32) << 16, 0, MOUSE_HWHEELED),
                        &state
                    )
                    .unwrap(),
                b"\x1b[<67;42;12M"
            );
        }

        #[test]
        fn wheel_delta_does_not_create_later_button_release() {
            let mut encoder = WinInputEncoder::default();
            let state = sgr_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encoder
                    .encode_mouse(&mouse_record(0, 0, 120u32 << 16, 0, MOUSE_WHEELED), &state)
                    .unwrap(),
                b"\x1b[<64;1;1M"
            );
            assert_eq!(
                encoder.encode_mouse(&mouse_record(0, 0, 0, 0, 0), &state),
                None
            );
        }

        #[test]
        fn mouse_tracking_modes_gate_motion_events() {
            let hover = mouse_record(0, 0, 0, 0, MOUSE_MOVED);
            let drag = mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, MOUSE_MOVED);
            let press = mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0);

            let mut encoder = WinInputEncoder::default();
            let state = sgr_mouse_state(MouseTrackingMode::Default);
            assert!(encoder.encode_mouse(&hover, &state).is_none());
            assert_eq!(
                encoder.encode_mouse(&press, &state).unwrap(),
                b"\x1b[<0;1;1M"
            );

            let state = sgr_mouse_state(MouseTrackingMode::ButtonEvent);
            assert!(encoder.encode_mouse(&hover, &state).is_none());
            assert_eq!(
                encoder.encode_mouse(&drag, &state).unwrap(),
                b"\x1b[<32;1;1M"
            );

            let state = sgr_mouse_state(MouseTrackingMode::AnyEvent);
            assert_eq!(
                encoder.encode_mouse(&hover, &state).unwrap(),
                b"\x1b[<35;1;1M"
            );
        }

        #[test]
        fn sgr_mouse_press_and_release_preserve_button() {
            let mut encoder = WinInputEncoder::default();
            let state = sgr_mouse_state(MouseTrackingMode::Default);
            let records = [
                mouse_input_record(mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0)),
                mouse_input_record(mouse_record(0, 0, 0, 0, 0)),
            ];
            let mut parser = termwiz::input::InputParser::new();

            assert_eq!(
                encoder.encode_records(&mut parser, &records, &state),
                b"\x1b[<0;1;1M\x1b[<0;1;1m"
            );
        }

        #[test]
        fn tracked_mouse_modes_drive_encoded_mouse_records() {
            let (state, mut tracker) = tracker();
            assert_eq!(tracker.filter(b"\x1b[?1000;1006h"), b"");

            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let mouse = mouse_input_record(mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0));
            let state_snapshot = state.lock().unwrap().clone();
            assert_eq!(
                encoder.encode_records(&mut parser, &[mouse], &state_snapshot),
                b"\x1b[<0;1;1M"
            );

            assert_eq!(tracker.filter(b"\x1b[?1000l"), b"");
            let state_snapshot = state.lock().unwrap().clone();
            assert_eq!(
                encoder.encode_records(&mut parser, &[mouse], &state_snapshot),
                b""
            );
        }

        #[test]
        fn tracked_win32_mode_drives_encoded_key_records() {
            let (state, mut tracker) = tracker();
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let key = key_record('A' as u16, 0x1e, 'a', true, 1, 0);

            assert_eq!(tracker.filter(b"\x1b[?9001h"), b"");
            let state_snapshot = state.lock().unwrap().clone();
            assert_eq!(
                encoder.encode_records(&mut parser, &[key], &state_snapshot),
                b"\x1b[65;30;97;1;0;1_"
            );

            assert_eq!(tracker.filter(b"\x1b[?9001l"), b"");
            let state_snapshot = state.lock().unwrap().clone();
            assert_eq!(
                encoder.encode_records(&mut parser, &[key], &state_snapshot),
                b"a"
            );
        }
    }

    mod batching {
        use super::*;

        #[test]
        fn win_input_batch_preserves_resize_order() {
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let records = [
                key_record('A' as u16, 0x1e, 'a', true, 1, 0),
                resize_input_record(120, 40),
                key_record('B' as u16, 0x30, 'b', true, 1, 0),
            ];

            assert_eq!(
                encode_win_input_batch(
                    &mut parser,
                    &mut encoder,
                    &records,
                    &InnerInputState::default()
                ),
                vec![
                    WinInputBatchMessage::Stdin(b"a".to_vec()),
                    WinInputBatchMessage::Resize,
                    WinInputBatchMessage::Stdin(b"b".to_vec()),
                ]
            );
        }
    }

    mod mouse_legacy_and_edges {
        use super::*;

        #[test]
        fn encodes_legacy_mouse_when_sgr_is_not_enabled() {
            let state = legacy_mouse_state(MouseTrackingMode::Default);
            let mouse = mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0);

            assert_eq!(
                encode_mouse_with_state(&mouse, &state).unwrap(),
                vec![0x1b, b'[', b'M', 32, 33, 33]
            );
        }

        #[test]
        fn encodes_legacy_mouse_release_as_button_three() {
            let mut encoder = WinInputEncoder::default();
            encoder.last_mouse_buttons = FROM_LEFT_1ST_BUTTON_PRESSED;
            let state = InnerInputState {
                mouse_default: true,
                mouse_tracking: MouseTrackingMode::Default,
                sgr_mouse: false,
                ..Default::default()
            };
            let mouse = mouse_record(0, 0, 0, 0, 0);

            assert_eq!(
                encoder.encode_mouse(&mouse, &state).unwrap(),
                vec![0x1b, b'[', b'M', 35, 33, 33]
            );
        }

        #[test]
        fn mouse_modifiers_are_encoded() {
            let state = sgr_mouse_state(MouseTrackingMode::Default);
            let mouse = mouse_record(
                0,
                0,
                FROM_LEFT_1ST_BUTTON_PRESSED,
                SHIFT_PRESSED | LEFT_ALT_PRESSED | RIGHT_CTRL_PRESSED,
                0,
            );

            assert_eq!(
                encode_mouse_with_state(&mouse, &state).unwrap(),
                b"\x1b[<28;1;1M"
            );
        }

        #[test]
        fn legacy_mouse_rejects_coordinates_outside_protocol_range() {
            let state = legacy_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(222, 222, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0),
                    &state
                ),
                Some(vec![0x1b, b'[', b'M', 32, 255, 255])
            );
            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(223, 222, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0),
                    &state
                ),
                None
            );
            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(222, 223, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0),
                    &state
                ),
                None
            );
        }

        #[test]
        fn mouse_disabled_while_button_held_does_not_emit_stale_release() {
            let mut encoder = WinInputEncoder::default();
            let enabled = sgr_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encoder
                    .encode_mouse(
                        &mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0),
                        &enabled
                    )
                    .unwrap(),
                b"\x1b[<0;1;1M"
            );

            assert_eq!(
                encoder.encode_mouse(
                    &mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0),
                    &InnerInputState::default()
                ),
                None
            );
            assert_eq!(
                encoder.encode_mouse(&mouse_record(0, 0, 0, 0, 0), &enabled),
                None
            );
        }

        #[test]
        fn encodes_right_and_middle_mouse_buttons() {
            let state = sgr_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(0, 0, RIGHTMOST_BUTTON_PRESSED, 0, 0),
                    &state
                )
                .unwrap(),
                b"\x1b[<2;1;1M"
            );
            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(0, 0, FROM_LEFT_2ND_BUTTON_PRESSED, 0, 0),
                    &state
                )
                .unwrap(),
                b"\x1b[<1;1;1M"
            );
        }

        #[test]
        fn double_click_uses_button_press_encoding() {
            let state = sgr_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, DOUBLE_CLICK),
                    &state
                )
                .unwrap(),
                b"\x1b[<0;1;1M"
            );
        }

        #[test]
        fn wheel_modifiers_are_encoded() {
            let state = sgr_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encode_mouse_with_state(
                    &mouse_record(
                        0,
                        0,
                        120u32 << 16,
                        SHIFT_PRESSED | RIGHT_CTRL_PRESSED,
                        MOUSE_WHEELED,
                    ),
                    &state
                )
                .unwrap(),
                b"\x1b[<84;1;1M"
            );
        }
    }

    mod focus {
        use super::*;

        #[test]
        fn encodes_focus_events_when_enabled() {
            let state = InnerInputState {
                focus: true,
                ..Default::default()
            };
            let mut focus: FOCUS_EVENT_RECORD = unsafe { std::mem::zeroed() };
            focus.bSetFocus = TRUE;
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            record.EventType = FOCUS_EVENT;
            unsafe {
                *record.Event.FocusEvent_mut() = focus;
            }
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();

            assert_eq!(
                encoder.encode_records(&mut parser, &[record], &state),
                b"\x1b[I"
            );
        }

        #[test]
        fn focus_disabled_emits_nothing_and_focus_lost_encodes() {
            let mut focus: FOCUS_EVENT_RECORD = unsafe { std::mem::zeroed() };
            focus.bSetFocus = 0;
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            record.EventType = FOCUS_EVENT;
            unsafe {
                *record.Event.FocusEvent_mut() = focus;
            }
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();

            assert_eq!(
                encoder.encode_records(&mut parser, &[record], &InnerInputState::default()),
                b""
            );

            let state = InnerInputState {
                focus: true,
                ..Default::default()
            };
            assert_eq!(
                encoder.encode_records(&mut parser, &[record], &state),
                b"\x1b[O"
            );
        }
    }

    mod windows_console_integration {
        use super::*;
        use std::path::PathBuf;
        use std::process::Command;

        fn debug_wezterm_exe() -> PathBuf {
            let mut path = std::env::current_exe().unwrap();
            while let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
                if file_name == "deps" {
                    path.pop();
                    path.push("wezterm.exe");
                    return path;
                }
                if !path.pop() {
                    break;
                }
            }
            panic!("could not locate target debug wezterm.exe from current test executable");
        }

        fn cast_payload(path: &std::path::Path) -> anyhow::Result<String> {
            let contents = std::fs::read_to_string(path)?;
            let mut payload = String::new();
            for line in contents.lines().skip(1).filter(|line| !line.is_empty()) {
                let event: Event = serde_json::from_str(line)?;
                if event.1 == "o" {
                    payload.push_str(&event.2);
                }
            }
            Ok(payload)
        }

        #[test]
        #[ignore = "requires an interactive Windows console input buffer"]
        fn write_console_input_records_are_read_and_encoded_by_bridge_reader() -> anyhow::Result<()>
        {
            let conin = OpenOptions::new().read(true).write(true).open("CONIN$")?;
            let conin_handle = conin.as_raw_handle() as *mut _;
            let mut original_mode = 0;
            assert_ne!(
                unsafe { GetConsoleMode(conin_handle, &mut original_mode) },
                0
            );
            assert_ne!(unsafe { FlushConsoleInputBuffer(conin_handle) }, 0);

            let mut tty = super::super::win::WinTty::new()?;
            tty.set_bridge_mode()?;
            let mut bridge_mode = 0;
            assert_ne!(unsafe { GetConsoleMode(conin_handle, &mut bridge_mode) }, 0);
            assert_eq!(
                bridge_mode & (ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT),
                ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT
            );

            let records = [
                key_record('A' as u16, 0x1e, 'a', true, 1, 0),
                mouse_input_record(mouse_record(
                    0,
                    0,
                    FROM_LEFT_1ST_BUTTON_PRESSED,
                    SHIFT_PRESSED,
                    0,
                )),
                resize_input_record(120, 40),
            ];

            let mut written = 0;
            let ok = unsafe {
                WriteConsoleInputW(
                    conin_handle,
                    records.as_ptr() as *mut _,
                    records.len() as u32,
                    &mut written,
                )
            };
            assert_ne!(ok, 0);
            assert_eq!(written, records.len() as u32);

            let mut input = tty.input_reader()?;
            let read = input.read_console_input(records.len())?;
            assert_eq!(read.len(), records.len());
            assert_eq!(read[0].EventType, KEY_EVENT);
            assert_eq!(read[1].EventType, MOUSE_EVENT);
            assert_eq!(read[2].EventType, WINDOW_BUFFER_SIZE_EVENT);
            let key = unsafe { read[0].Event.KeyEvent() };
            assert_eq!(key.bKeyDown, TRUE);
            assert_eq!(key.wVirtualKeyCode, 'A' as u16);
            assert_eq!(key.wVirtualScanCode, 0x1e);
            assert_eq!(*unsafe { key.uChar.UnicodeChar() }, 'a' as u16);
            let mouse = unsafe { read[1].Event.MouseEvent() };
            assert_eq!(mouse.dwMousePosition.X, 0);
            assert_eq!(mouse.dwMousePosition.Y, 0);
            assert_eq!(mouse.dwButtonState, FROM_LEFT_1ST_BUTTON_PRESSED);
            assert_eq!(mouse.dwControlKeyState, SHIFT_PRESSED);
            let resize = unsafe { read[2].Event.WindowBufferSizeEvent() };
            assert_eq!(resize.dwSize.X, 120);
            assert_eq!(resize.dwSize.Y, 40);

            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let state = InnerInputState {
                mouse_default: true,
                mouse_tracking: MouseTrackingMode::Default,
                sgr_mouse: true,
                ..Default::default()
            };
            assert_eq!(
                encoder.encode_records(&mut parser, &read[..2], &state),
                b"a\x1b[<4;1;1M"
            );

            tty.set_cooked()?;
            let mut restored_mode = 0;
            assert_ne!(
                unsafe { GetConsoleMode(conin_handle, &mut restored_mode) },
                0
            );
            assert_eq!(restored_mode, original_mode);
            assert_ne!(unsafe { FlushConsoleInputBuffer(conin_handle) }, 0);

            Ok(())
        }

        #[test]
        #[ignore = "spawns wezterm record and requires an interactive Windows console"]
        fn wezterm_record_win_input_on_filters_owned_modes_in_cast() -> anyhow::Result<()> {
            if std::env::var_os("WEZTERM_PANE").is_none() {
                eprintln!("skipping: this e2e requires a real WezTerm pane");
                return Ok(());
            }

            let cast = tempfile::Builder::new()
                .prefix("wezterm-record-win-input-e2e-")
                .suffix(".cast.txt")
                .tempfile()?
                .into_temp_path();
            let cast_path = cast.to_path_buf();
            let script = "$e=[char]27; [Console]::Out.Write(\"${e}[?9001h${e}[?1006h${e}[?25hE2E_CAST_MARK${e}[?25l${e}[?1006l${e}[?9001l`r`n\")";

            let status = Command::new(debug_wezterm_exe())
                .arg("record")
                .arg("--win-input=on")
                .arg("-o")
                .arg(&cast_path)
                .arg("--")
                .arg("powershell.exe")
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(script)
                .status()?;

            assert!(
                status.success(),
                "wezterm record failed: status={:?}",
                status.code()
            );

            let payload = cast_payload(&cast_path)?;
            assert!(payload.contains("E2E_CAST_MARK"));
            assert!(!payload.contains("\x1b[?9001h"));
            assert!(!payload.contains("\x1b[?9001l"));
            assert!(!payload.contains("\x1b[?1006h"));
            assert!(!payload.contains("\x1b[?1006l"));
            assert!(payload.contains("\x1b[?25h"));
            assert!(payload.contains("\x1b[?25l"));

            Ok(())
        }
    }
}

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
        let pair = pty_system.openpty(size)?;

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
        let use_win_input_bridge = match self.win_input {
            WinInputMode::Off => {
                tty.set_raw()?;
                false
            }
            WinInputMode::Auto => match tty.set_bridge_mode() {
                Ok(()) => true,
                Err(err) => {
                    eprintln!(
                        "warning: failed to initialize Windows input bridge: {err:#}; falling back to legacy input forwarding"
                    );
                    tty.set_raw()?;
                    false
                }
            },
            WinInputMode::On => {
                tty.set_bridge_mode()?;
                true
            }
        };
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
                            WinInputBatchMessage::Resize => {
                                tx.send(Message::Resize(input.get_size()?))?;
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

        let mut child_status = None;
        let first_output = Instant::now();
        let mut buffer = vec![];
        let mut writer = pair.master.take_writer()?;
        let mut stdout_eof = false;
        let mut child_terminated_at = None;

        loop {
            let msg = if child_status.is_some() && !stdout_eof {
                match rx.recv_timeout(
                    stdout_drain_timeout_after_child_exit(
                        true,
                        stdout_eof,
                        child_terminated_at,
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

            match msg {
                Message::Stdin(data) => {
                    writer.write_all(&data)?;
                }
                Message::Stdout(mut data) => {
                    let elapsed = first_output.elapsed().as_secs_f32();
                    #[cfg(windows)]
                    if let Some(tracker) = input_mode_tracker.as_mut() {
                        data = filter_child_output_for_outer_terminal(tracker, &data, &mut writer)?;
                    }
                    if data.is_empty() {
                        continue;
                    }
                    tty.write_all(&data)?;
                    log_utf8_output(&mut cast_file, &mut buffer, elapsed, &mut data)?;
                }
                Message::StdoutEof => {
                    stdout_eof = true;
                    if child_status.is_some() {
                        #[cfg(windows)]
                        if let Some(tracker) = input_mode_tracker.as_mut() {
                            let mut data = tracker.drain_pending();
                            if !data.is_empty() {
                                let elapsed = first_output.elapsed().as_secs_f32();
                                tty.write_all(&data)?;
                                log_utf8_output(&mut cast_file, &mut buffer, elapsed, &mut data)?;
                            }
                        }
                        break;
                    }
                }
                Message::Resize(size) => {
                    pair.master.resize(size)?;
                }
                Message::Terminated(status) => {
                    child_status.replace(status);
                    child_terminated_at = Some(Instant::now());
                    if stdout_eof {
                        #[cfg(windows)]
                        if let Some(tracker) = input_mode_tracker.as_mut() {
                            let mut data = tracker.drain_pending();
                            if !data.is_empty() {
                                let elapsed = first_output.elapsed().as_secs_f32();
                                tty.write_all(&data)?;
                                log_utf8_output(&mut cast_file, &mut buffer, elapsed, &mut data)?;
                            }
                        }
                        break;
                    }
                }
            }
        }

        #[cfg(windows)]
        if let Some(tracker) = input_mode_tracker.as_mut() {
            let mut data = tracker.drain_pending();
            if !data.is_empty() {
                let elapsed = first_output.elapsed().as_secs_f32();
                tty.write_all(&data)?;
                log_utf8_output(&mut cast_file, &mut buffer, elapsed, &mut data)?;
            }
        }

        #[cfg(windows)]
        if use_win_input_bridge {
            let _ = tty.reset_outer_input_modes();
        }
        tty.set_cooked()?;
        eprintln!("Child status: {:?}", child_status);
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
