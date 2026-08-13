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
use std::sync::mpsc::channel;
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
                b"\x1b[?9001l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l",
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
    /// Terminal window size changed
    Resize(PtySize),
    /// Child process terminated
    Terminated(portable_pty::ExitStatus),
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
    // TODO: Track DECCKM (?1h/l) for application cursor keys, and eventually
    // filter input-mode sequences from child output so record owns the outer
    // terminal's win32-input/mouse/paste/focus state instead of only observing it.
    fn new(state: Arc<Mutex<InnerInputState>>) -> Self {
        Self {
            state,
            pending: Vec::new(),
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);

        let mut i = 0;
        while i < data.len() {
            if data[i] != 0x1b {
                i += 1;
                continue;
            }

            if i + 2 >= data.len() {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }

            if data[i + 1] != b'[' || data[i + 2] != b'?' {
                i += 1;
                continue;
            }

            let mut end = i + 3;
            while end < data.len() && !(0x40..=0x7e).contains(&data[end]) {
                end += 1;
            }

            if end == data.len() {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }

            let final_byte = data[end];
            if final_byte == b'h' || final_byte == b'l' {
                let enabled = final_byte == b'h';
                if let Ok(params) = std::str::from_utf8(&data[i + 3..end]) {
                    if let Ok(mut state) = self.state.lock() {
                        for param in params.split(';') {
                            if let Ok(mode) = param.parse::<u16>() {
                                state.set_mode(mode, enabled);
                            }
                        }
                    }
                }
            }

            i = end + 1;
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct WinInputEncoder {
    last_mouse_buttons: u32,
}

#[cfg(windows)]
impl WinInputEncoder {
    // TODO: Add legacy mouse, horizontal wheel, focus-event, and explicit paste
    // boundary handling once the main keyboard/mouse bridge semantics are stable.
    fn encode_records(
        &mut self,
        parser: &mut termwiz::input::InputParser,
        records: &[winapi::um::wincon::INPUT_RECORD],
        state: &InnerInputState,
    ) -> Vec<u8> {
        use termwiz::input::{InputEvent, KeyCodeEncodeModes, KeyboardEncoding};
        use winapi::um::wincon::*;

        let mut non_char_key_records = Vec::new();
        let mut output = Vec::new();

        for record in records {
            match record.EventType {
                KEY_EVENT => {
                    let key = unsafe { record.Event.KeyEvent() };
                    if key.bKeyDown != 0 {
                        let unicode = *unsafe { key.uChar.UnicodeChar() };
                        if unicode != 0 {
                            // TODO: Distinguish physical printable key events from terminal
                            // response byte streams (DA/CPR). Encoding all printable chars as
                            // Win32 input corrupts terminal responses in nested record sessions.
                            if let Some(ch) = std::char::from_u32(unicode as u32) {
                                let mut buf = [0u8; 4];
                                output.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                continue;
                            }
                        }
                    }
                    non_char_key_records.push(*record);
                }
                MOUSE_EVENT => {
                    if let Some(mouse) =
                        self.encode_mouse(unsafe { record.Event.MouseEvent() }, state)
                    {
                        output.extend_from_slice(mouse.as_bytes());
                    }
                }
                _ => {}
            }
        }

        if state.win32_input {
            for record in non_char_key_records {
                if record.EventType == KEY_EVENT {
                    output.extend_from_slice(
                        Self::encode_win32_key(unsafe { record.Event.KeyEvent() }).as_bytes(),
                    );
                }
            }
            return output;
        }

        let modes = KeyCodeEncodeModes {
            encoding: KeyboardEncoding::Xterm,
            application_cursor_keys: false,
            newline_mode: false,
            modify_other_keys: None,
        };

        for event in parser.decode_input_records_as_vec(&non_char_key_records) {
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

        output
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
    ) -> Option<String> {
        use winapi::um::wincon::*;

        if state.mouse_tracking == MouseTrackingMode::None || !state.sgr_mouse {
            self.last_mouse_buttons = mouse.dwButtonState;
            return None;
        }

        let is_move = (mouse.dwEventFlags & MOUSE_MOVED) != 0;
        let is_wheel = (mouse.dwEventFlags & MOUSE_WHEELED) != 0;
        let physical_button_pressed = mouse.dwButtonState
            & (FROM_LEFT_1ST_BUTTON_PRESSED
                | FROM_LEFT_2ND_BUTTON_PRESSED
                | RIGHTMOST_BUTTON_PRESSED)
            != 0;

        let should_send = match state.mouse_tracking {
            MouseTrackingMode::None => false,
            MouseTrackingMode::Default => !is_move,
            MouseTrackingMode::ButtonEvent => !is_move || physical_button_pressed,
            MouseTrackingMode::AnyEvent => true,
        };

        if !should_send {
            self.last_mouse_buttons = mouse.dwButtonState;
            return None;
        }

        let mut code = if is_wheel {
            let delta = ((mouse.dwButtonState >> 16) & 0xffff) as i16;
            if delta > 0 {
                64
            } else {
                65
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

        if is_move {
            code += 32;
        }
        if (mouse.dwControlKeyState & SHIFT_PRESSED) != 0 {
            code += 4;
        }
        if (mouse.dwControlKeyState & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED)) != 0 {
            code += 8;
        }
        if (mouse.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED)) != 0 {
            code += 16;
        }

        let is_release =
            !is_move && !is_wheel && mouse.dwButtonState == 0 && self.last_mouse_buttons != 0;
        self.last_mouse_buttons = mouse.dwButtonState;

        Some(format!(
            "\x1b[<{};{};{}{}",
            code,
            mouse.dwMousePosition.X + 1,
            mouse.dwMousePosition.Y + 1,
            if is_release { 'm' } else { 'M' }
        ))
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
                    for record in &records {
                        if record.EventType == winapi::um::wincon::WINDOW_BUFFER_SIZE_EVENT {
                            // TODO: Re-read CONOUT$ screen buffer info here so resize uses
                            // viewport dimensions rather than the console buffer size.
                            let size = unsafe { record.Event.WindowBufferSizeEvent() };
                            tx.send(Message::Resize(PtySize {
                                rows: size.dwSize.Y as u16,
                                cols: size.dwSize.X as u16,
                                pixel_width: 0,
                                pixel_height: 0,
                            }))?;
                        }
                    }
                    let state = state.lock().map(|state| state.clone()).unwrap_or_default();
                    let data = encoder.encode_records(&mut parser, &records, &state);
                    if !data.is_empty() {
                        tx.send(Message::Stdin(data))?;
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

        for msg in rx {
            match msg {
                Message::Stdin(data) => {
                    writer.write_all(&data)?;
                }
                Message::Stdout(mut data) => {
                    let elapsed = first_output.elapsed().as_secs_f32();
                    #[cfg(windows)]
                    if let Some(tracker) = input_mode_tracker.as_mut() {
                        tracker.observe(&data);
                    }
                    tty.write_all(&data)?;

                    // The end of the data may be an incomplete utf8 sequence
                    // that straddles the buffer boundary.  JSON requires strings
                    // to be utf-8 so we need to send the currently-valid portions
                    // through to the .cast file and buffer up the remainder
                    buffer.append(&mut data);
                    match std::str::from_utf8(&buffer) {
                        Ok(valid) => {
                            Event::log_output(&mut cast_file, elapsed, valid)?;
                            buffer.clear();
                        }
                        Err(error) => {
                            let valid_len = error.valid_up_to();
                            Event::log_output(&mut cast_file, elapsed, unsafe {
                                std::str::from_utf8_unchecked(&buffer[0..valid_len])
                            })?;

                            buffer.drain(0..valid_len);

                            if let Some(invalid_sequence_length) = error.error_len() {
                                // Invalid sequence: skip it
                                buffer.drain(0..invalid_sequence_length);
                            }
                        }
                    }
                }
                Message::Resize(size) => {
                    pair.master.resize(size)?;
                }
                Message::Terminated(status) => {
                    child_status.replace(status);
                    break;
                }
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
