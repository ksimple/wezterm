use super::*;
use std::fs::OpenOptions;
use std::os::windows::io::AsRawHandle;
use winapi::shared::minwindef::TRUE;
use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
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

fn focus_input_record(focused: bool) -> INPUT_RECORD {
    let mut focus: FOCUS_EVENT_RECORD = unsafe { std::mem::zeroed() };
    focus.bSetFocus = if focused { TRUE } else { 0 };
    let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
    record.EventType = FOCUS_EVENT;
    unsafe {
        *record.Event.FocusEvent_mut() = focus;
    }
    record
}

fn menu_input_record() -> INPUT_RECORD {
    let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
    record.EventType = MENU_EVENT;
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

fn encode_mouse_with_state(mouse: &MOUSE_EVENT_RECORD, state: &InnerInputState) -> Option<Vec<u8>> {
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

mod cli_flags {
    use super::*;
    use std::cell::Cell;

    fn configure_with_counts(
        mode: WinInputMode,
        bridge_fails: bool,
        raw_fails: bool,
    ) -> anyhow::Result<(bool, u32, u32, u32)> {
        let raw_calls = Cell::new(0);
        let bridge_calls = Cell::new(0);
        let warnings = Cell::new(0);

        let enabled = configure_windows_input_bridge(
            mode,
            |action| {
                match action {
                    WindowsInputBridgeAction::SetRaw => {
                        raw_calls.set(raw_calls.get() + 1);
                        if raw_fails {
                            anyhow::bail!("raw setup failed");
                        }
                    }
                    WindowsInputBridgeAction::SetBridgeMode => {
                        bridge_calls.set(bridge_calls.get() + 1);
                        if bridge_fails {
                            anyhow::bail!("bridge setup failed");
                        }
                    }
                }
                Ok(())
            },
            |_| warnings.set(warnings.get() + 1),
        )?;

        Ok((enabled, raw_calls.get(), bridge_calls.get(), warnings.get()))
    }

    #[test]
    fn off_uses_legacy_raw_input_without_trying_bridge() {
        assert_eq!(
            configure_with_counts(WinInputMode::Off, true, false).unwrap(),
            (false, 1, 0, 0)
        );
    }

    #[test]
    fn auto_uses_bridge_when_setup_succeeds() {
        assert_eq!(
            configure_with_counts(WinInputMode::Auto, false, false).unwrap(),
            (true, 0, 1, 0)
        );
    }

    #[test]
    fn auto_warns_and_falls_back_to_raw_when_bridge_setup_fails() {
        assert_eq!(
            configure_with_counts(WinInputMode::Auto, true, false).unwrap(),
            (false, 1, 1, 1)
        );
    }

    #[test]
    fn off_propagates_raw_setup_failure() {
        let err = configure_with_counts(WinInputMode::Off, false, true).unwrap_err();

        assert_eq!(err.to_string(), "raw setup failed");
    }

    #[test]
    fn auto_propagates_raw_fallback_failure_after_bridge_warning() {
        let err = configure_with_counts(WinInputMode::Auto, true, true).unwrap_err();

        assert_eq!(err.to_string(), "raw setup failed");
    }

    #[test]
    fn on_requires_bridge_setup_and_does_not_fall_back() {
        let raw_calls = Cell::new(0);
        let bridge_calls = Cell::new(0);
        let warnings = Cell::new(0);

        let err = configure_windows_input_bridge(
            WinInputMode::On,
            |action| match action {
                WindowsInputBridgeAction::SetRaw => {
                    raw_calls.set(raw_calls.get() + 1);
                    Ok(())
                }
                WindowsInputBridgeAction::SetBridgeMode => {
                    bridge_calls.set(bridge_calls.get() + 1);
                    anyhow::bail!("bridge setup failed")
                }
            },
            |_| warnings.set(warnings.get() + 1),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "bridge setup failed");
        assert_eq!(raw_calls.get(), 0);
        assert_eq!(bridge_calls.get(), 1);
        assert_eq!(warnings.get(), 0);
    }

    #[test]
    fn record_command_parses_win_input_values() {
        assert_eq!(
            RecordCommand::try_parse_from(["record"]).unwrap().win_input,
            WinInputMode::Auto
        );
        assert_eq!(
            RecordCommand::try_parse_from(["record", "--win-input", "auto"])
                .unwrap()
                .win_input,
            WinInputMode::Auto
        );
        assert_eq!(
            RecordCommand::try_parse_from(["record", "--win-input", "on"])
                .unwrap()
                .win_input,
            WinInputMode::On
        );
        assert_eq!(
            RecordCommand::try_parse_from(["record", "--win-input", "off"])
                .unwrap()
                .win_input,
            WinInputMode::Off
        );
    }

    #[test]
    fn record_command_rejects_invalid_win_input_value() {
        assert!(RecordCommand::try_parse_from(["record", "--win-input", "maybe"]).is_err());
    }
}

mod tracker_and_run_loop {
    use super::*;

    #[derive(Default)]
    struct TestRecordLoopIo {
        stdin_writes: Vec<Vec<u8>>,
        stdout_writes: Vec<Vec<u8>>,
        resizes: Vec<PtySize>,
        tracker: Option<InputModeTracker>,
    }

    impl TestRecordLoopIo {
        fn with_tracker(tracker: InputModeTracker) -> Self {
            Self {
                tracker: Some(tracker),
                ..Default::default()
            }
        }
    }

    impl RecordLoopIo for TestRecordLoopIo {
        fn write_stdin(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
            self.stdin_writes.push(data);
            Ok(())
        }

        fn write_stdout(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
            let data = if let Some(tracker) = self.tracker.as_mut() {
                let mut responses = Vec::new();
                let data = filter_child_output_for_outer_terminal(tracker, &data, &mut responses)?;
                if !responses.is_empty() {
                    self.stdin_writes.push(responses);
                }
                data
            } else {
                data
            };
            if !data.is_empty() {
                self.stdout_writes.push(data);
            }
            Ok(())
        }

        fn resize(&mut self, size: PtySize) -> anyhow::Result<()> {
            self.resizes.push(size);
            Ok(())
        }

        fn drain_pending_stdout(&mut self) -> anyhow::Result<()> {
            if let Some(tracker) = self.tracker.as_mut() {
                let data = tracker.drain_pending();
                if !data.is_empty() {
                    self.stdout_writes.push(data);
                }
            }
            Ok(())
        }
    }

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
    fn unowned_modes_are_preserved_with_button_and_any_event_modes() {
        let (state, mut tracker) = tracker();

        assert_eq!(
            tracker.filter(b"\x1b[?25;1002;1003;1016h"),
            b"\x1b[?25;1016h"
        );
        assert_eq!(
            state.lock().unwrap().mouse_tracking,
            MouseTrackingMode::AnyEvent
        );

        assert_eq!(tracker.filter(b"\x1b[?25;1003;1016l"), b"\x1b[?25;1016l");
        assert_eq!(
            state.lock().unwrap().mouse_tracking,
            MouseTrackingMode::ButtonEvent
        );
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
    fn dec_private_modes_work_when_split_at_every_byte() {
        for (sequence, expected_output, assert_state) in [
            (
                b"\x1b[?9001h".as_slice(),
                b"".as_slice(),
                Box::new(|state: &InnerInputState| assert!(state.win32_input))
                    as Box<dyn Fn(&InnerInputState)>,
            ),
            (
                b"\x1b[?25;1000;1006h".as_slice(),
                b"\x1b[?25h".as_slice(),
                Box::new(|state: &InnerInputState| {
                    assert_eq!(state.mouse_tracking, MouseTrackingMode::Default);
                    assert!(state.sgr_mouse);
                }),
            ),
            (
                b"\x1b[?1000;1006;2004l".as_slice(),
                b"".as_slice(),
                Box::new(|state: &InnerInputState| {
                    assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
                    assert!(!state.sgr_mouse);
                    assert!(!state.bracketed_paste);
                }),
            ),
        ] {
            let (state, mut tracker) = tracker();
            let mut output = Vec::new();
            for byte in sequence {
                output.extend_from_slice(&tracker.filter(&[*byte]));
            }

            assert_eq!(output, expected_output);
            assert_eq!(tracker.drain_pending(), b"");
            assert_state(&state.lock().unwrap());
        }
    }

    #[test]
    fn malformed_dec_private_mode_is_preserved_when_split_at_every_byte() {
        let (state, mut tracker) = tracker();
        let sequence = b"\x1b[?1000:1h";
        let mut output = Vec::new();

        for byte in sequence {
            output.extend_from_slice(&tracker.filter(&[*byte]));
        }

        assert_eq!(output, sequence);
        assert_eq!(
            state.lock().unwrap().mouse_tracking,
            MouseTrackingMode::None
        );
    }

    #[test]
    fn pending_malformed_csi_before_new_escape_is_preserved() {
        let (state, mut tracker) = tracker();

        assert_eq!(tracker.filter(b"\x1b[?"), b"");
        let (output, responses) =
            tracker.filter_and_get_responses(b"\x1b]0;\x1b[6n\x1b[?9001h\x07tail");

        assert_eq!(output, b"\x1b[?\x1b]0;\x1b[6n\x1b[?9001h\x07tail");
        assert_eq!(responses, b"");
        assert!(!state.lock().unwrap().win32_input);
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
            filter_child_output_for_outer_terminal(&mut tracker, b"9001hB\x1b[", &mut child_input)
                .unwrap(),
            b"B"
        );
        assert_eq!(
            filter_child_output_for_outer_terminal(&mut tracker, b"6nC", &mut child_input).unwrap(),
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
    fn control_strings_preserve_csi_like_payloads_and_st_terminators() {
        for (sequence, split_at) in [
            (b"\x1b]0;osc \x1b[6n \x1b[?9001h\x1b\\after".as_slice(), 9),
            (b"\x1b_apc \x1b[6n \x1b[?1000h\x1b\\after", 12),
            (b"\x1b^pm \x1b[5n \x1b[?2004h\x1b\\after", 15),
            (b"\x1bXsos \x1b[c \x1b[?1006h\x1b\\after", 18),
        ] {
            let (state, mut tracker) = tracker();

            assert_eq!(tracker.filter(&sequence[..split_at]), b"");
            let (output, responses) = tracker.filter_and_get_responses(&sequence[split_at..]);

            assert_eq!(output, sequence);
            assert_eq!(responses, b"");
            let state = state.lock().unwrap();
            assert!(!state.win32_input);
            assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
            assert!(!state.bracketed_paste);
        }
    }

    #[test]
    fn control_strings_are_preserved_when_split_at_every_byte() {
        for sequence in [
            b"\x1b]0;osc \x1b[6n \x1b[?9001h\x07after".as_slice(),
            b"\x1b]0;osc \x1b[6n \x1b[?9001h\x1b\\after",
            b"\x1bPpayload \x1b[c \x1b[?1000h\x1b\\after",
            b"\x1b_payload \x1b[5n \x1b[?1002h\x1b\\after",
            b"\x1b^payload \x1b[6n \x1b[?1003h\x1b\\after",
            b"\x1bXpayload \x1b[>0c \x1b[?2004h\x1b\\after",
        ] {
            for split_at in 1..sequence.len() {
                let (state, mut tracker) = tracker();
                let mut combined_output = tracker.filter(&sequence[..split_at]);

                let (output, responses) = tracker.filter_and_get_responses(&sequence[split_at..]);
                combined_output.extend_from_slice(&output);

                assert_eq!(combined_output, sequence);
                assert_eq!(responses, b"");
                let state = state.lock().unwrap();
                assert!(!state.win32_input);
                assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
                assert!(!state.bracketed_paste);
            }
        }
    }

    #[test]
    fn split_control_string_st_terminator_is_preserved() {
        let (_state, mut tracker) = tracker();

        assert_eq!(tracker.filter(b"\x1b]0;title\x1b"), b"");
        assert_eq!(tracker.filter(b"\\tail"), b"\x1b]0;title\x1b\\tail");
    }

    #[test]
    fn c1_csi_and_st_bytes_are_passed_through_without_state_changes() {
        let (state, mut tracker) = tracker();
        let c1 = b"\x9b?9001htext\x9c";

        let (output, responses) = tracker.filter_and_get_responses(c1);

        assert_eq!(output, c1);
        assert_eq!(responses, b"");
        assert!(!state.lock().unwrap().win32_input);
    }

    #[test]
    fn c1_control_strings_are_passed_through_without_state_or_responses() {
        for sequence in [
            b"\x9d0;osc \x1b[6n \x1b[?9001h\x07after".as_slice(),
            b"\x90dcs \x1b[c \x1b[?1000h\x9cafter",
            b"\x9fapc \x1b[5n \x1b[?1002h\x9cafter",
            b"\x9epm \x1b[6n \x1b[?1003h\x9cafter",
            b"\x98sos \x1b[>0c \x1b[?2004h\x9cafter",
        ] {
            let (state, mut tracker) = tracker();

            let (output, responses) = tracker.filter_and_get_responses(sequence);

            assert_eq!(output, sequence);
            assert_eq!(responses, b"");
            let state = state.lock().unwrap();
            assert!(!state.win32_input);
            assert_eq!(state.mouse_tracking, MouseTrackingMode::None);
            assert!(!state.bracketed_paste);
        }
    }

    #[test]
    fn query_responses_are_written_before_later_user_input() {
        let (_state, mut tracker) = tracker();
        let mut child_input = Vec::new();

        assert_eq!(
            filter_child_output_for_outer_terminal(&mut tracker, b"\x1b[6n", &mut child_input)
                .unwrap(),
            b""
        );
        child_input.extend_from_slice(b"user");
        assert_eq!(
            filter_child_output_for_outer_terminal(&mut tracker, b"\x1b[5n", &mut child_input)
                .unwrap(),
            b""
        );

        assert_eq!(child_input, b"\x1b[1;1Ruser\x1b[0n");
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
    fn record_loop_waits_for_stdout_eof_after_termination() {
        let mut state = RecordLoopState::default();
        let mut io = TestRecordLoopIo::default();
        let now = Instant::now();

        assert_eq!(
            process_record_message(
                &mut state,
                &mut io,
                Message::Terminated(portable_pty::ExitStatus::with_exit_code(0)),
                now,
            )
            .unwrap(),
            RecordLoopControl::Continue
        );
        assert!(state.child_status.is_some());
        assert!(!state.stdout_eof);

        assert_eq!(
            process_record_message(&mut state, &mut io, Message::Stdout(b"late".to_vec()), now,)
                .unwrap(),
            RecordLoopControl::Continue
        );
        assert_eq!(io.stdout_writes, vec![b"late".to_vec()]);

        assert_eq!(
            process_record_message(&mut state, &mut io, Message::StdoutEof, now).unwrap(),
            RecordLoopControl::Break
        );
    }

    #[test]
    fn record_loop_drains_pending_tracker_output_when_stdout_eof_follows_termination() {
        let (_state, tracker) = tracker();
        let mut state = RecordLoopState::default();
        let mut io = TestRecordLoopIo::with_tracker(tracker);
        let now = Instant::now();

        assert_eq!(
            process_record_message(
                &mut state,
                &mut io,
                Message::Stdout(b"\x1b[?".to_vec()),
                now,
            )
            .unwrap(),
            RecordLoopControl::Continue
        );
        assert!(io.stdout_writes.is_empty());

        assert_eq!(
            process_record_message(
                &mut state,
                &mut io,
                Message::Terminated(portable_pty::ExitStatus::with_exit_code(0)),
                now,
            )
            .unwrap(),
            RecordLoopControl::Continue
        );
        assert_eq!(
            process_record_message(&mut state, &mut io, Message::StdoutEof, now).unwrap(),
            RecordLoopControl::Break
        );
        assert_eq!(io.stdout_writes, vec![b"\x1b[?".to_vec()]);
    }

    #[test]
    fn record_loop_delivers_resize_messages_to_master() {
        let mut state = RecordLoopState::default();
        let mut io = TestRecordLoopIo::default();
        let size = PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };

        assert_eq!(
            process_record_message(&mut state, &mut io, Message::Resize(size), Instant::now())
                .unwrap(),
            RecordLoopControl::Continue
        );
        assert_eq!(io.resizes, vec![size]);
    }

    #[test]
    fn record_loop_writes_query_responses_before_later_user_input() {
        let (_state, tracker) = tracker();
        let mut state = RecordLoopState::default();
        let mut io = TestRecordLoopIo::with_tracker(tracker);
        let now = Instant::now();

        assert_eq!(
            process_record_message(
                &mut state,
                &mut io,
                Message::Stdout(b"\x1b[6n".to_vec()),
                now,
            )
            .unwrap(),
            RecordLoopControl::Continue
        );
        assert_eq!(
            process_record_message(&mut state, &mut io, Message::Stdin(b"user".to_vec()), now,)
                .unwrap(),
            RecordLoopControl::Continue
        );

        assert_eq!(io.stdout_writes, Vec::<Vec<u8>>::new());
        assert_eq!(
            io.stdin_writes,
            vec![b"\x1b[1;1R".to_vec(), b"user".to_vec()]
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
    fn win32_input_preserves_key_control_state() {
        let control_state = SHIFT_PRESSED | RIGHT_CTRL_PRESSED | LEFT_ALT_PRESSED | ENHANCED_KEY;
        let a = key_record('A' as u16, 0x1e, 'a', true, 1, control_state);
        let a_up = key_record('A' as u16, 0x1e, 'a', false, 1, control_state);

        assert_eq!(
            encode_records_with_state(&[a, a_up], &win32_state()),
            [
                expected_win32_key(65, 30, 97, true, control_state, 1),
                expected_win32_key(65, 30, 97, false, control_state, 1),
            ]
            .concat()
        );
    }

    #[test]
    fn win32_input_preserves_control_state_matrix() {
        for control_state in [
            LEFT_CTRL_PRESSED,
            RIGHT_CTRL_PRESSED,
            LEFT_ALT_PRESSED,
            RIGHT_ALT_PRESSED,
            LEFT_CTRL_PRESSED | RIGHT_ALT_PRESSED,
            CAPSLOCK_ON | NUMLOCK_ON | SCROLLLOCK_ON,
            ENHANCED_KEY | SHIFT_PRESSED,
        ] {
            let down = key_record(winuser::VK_LEFT as u16, 0x4b, '\0', true, 1, control_state);
            let up = key_record(winuser::VK_LEFT as u16, 0x4b, '\0', false, 1, control_state);

            assert_eq!(
                encode_records_with_state(&[down, up], &win32_state()),
                [
                    expected_win32_key(winuser::VK_LEFT as u16, 0x4b, 0, true, control_state, 1),
                    expected_win32_key(winuser::VK_LEFT as u16, 0x4b, 0, false, control_state, 1),
                ]
                .concat()
            );
        }
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
    fn modified_non_printable_keys_encode_xterm_modifiers() {
        let shifted_tab = key_record(winuser::VK_TAB as u16, 0x0f, '\0', true, 1, SHIFT_PRESSED);
        let ctrl_left = key_record(
            winuser::VK_LEFT as u16,
            0x4b,
            '\0',
            true,
            1,
            LEFT_CTRL_PRESSED,
        );

        assert_eq!(encode_records_default(&[shifted_tab]), b"\x1b[Z");
        assert_eq!(encode_records_default(&[ctrl_left]), b"\x1b[1;5D");
    }

    #[test]
    fn alt_and_enhanced_non_printable_keys_encode_xterm_modifiers() {
        let alt_up = key_record(winuser::VK_UP as u16, 0x48, '\0', true, 1, LEFT_ALT_PRESSED);
        let ctrl_alt_right = key_record(
            winuser::VK_RIGHT as u16,
            0x4d,
            '\0',
            true,
            1,
            LEFT_CTRL_PRESSED | RIGHT_ALT_PRESSED | ENHANCED_KEY,
        );

        assert_eq!(encode_records_default(&[alt_up]), b"\x1b[1;3A");
        assert_eq!(encode_records_default(&[ctrl_alt_right]), b"\x1b[1;7C");
    }

    #[test]
    fn unknown_event_types_are_ignored_without_disturbing_key_order() {
        let mut unknown: INPUT_RECORD = unsafe { std::mem::zeroed() };
        unknown.EventType = 0xffff;
        let records = [
            key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            unknown,
            key_record('B' as u16, 0x30, 'b', true, 1, 0),
        ];

        assert_eq!(encode_records_default(&records), b"ab");
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
    fn sgr_right_and_middle_releases_preserve_button() {
        for (button, expected) in [
            (
                RIGHTMOST_BUTTON_PRESSED,
                b"\x1b[<2;1;1M\x1b[<2;1;1m".as_slice(),
            ),
            (FROM_LEFT_2ND_BUTTON_PRESSED, b"\x1b[<1;1;1M\x1b[<1;1;1m"),
        ] {
            let mut encoder = WinInputEncoder::default();
            let mut parser = termwiz::input::InputParser::new();
            let state = sgr_mouse_state(MouseTrackingMode::Default);
            let records = [
                mouse_input_record(mouse_record(0, 0, button, 0, 0)),
                mouse_input_record(mouse_record(0, 0, 0, 0, 0)),
            ];

            assert_eq!(
                encoder.encode_records(&mut parser, &records, &state),
                expected
            );
        }
    }

    #[test]
    fn sgr_multi_button_transition_uses_lowest_tracked_button() {
        let mut encoder = WinInputEncoder::default();
        let state = sgr_mouse_state(MouseTrackingMode::Default);

        assert_eq!(
            encoder
                .encode_mouse(
                    &mouse_record(
                        0,
                        0,
                        FROM_LEFT_1ST_BUTTON_PRESSED | RIGHTMOST_BUTTON_PRESSED,
                        0,
                        0,
                    ),
                    &state
                )
                .unwrap(),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encoder
                .encode_mouse(&mouse_record(0, 0, RIGHTMOST_BUTTON_PRESSED, 0, 0), &state)
                .unwrap(),
            b"\x1b[<2;1;1M"
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

    #[test]
    fn x_mouse_buttons_do_not_create_tracked_button_state() {
        let mut encoder = WinInputEncoder::default();
        let state = sgr_mouse_state(MouseTrackingMode::Default);

        assert_eq!(
            encoder.encode_mouse(
                &mouse_record(0, 0, FROM_LEFT_3RD_BUTTON_PRESSED, 0, 0),
                &state
            ),
            None
        );
        assert_eq!(
            encoder.encode_mouse(
                &mouse_record(0, 0, FROM_LEFT_4TH_BUTTON_PRESSED, 0, 0),
                &state
            ),
            None
        );
        assert_eq!(
            encoder.encode_mouse(&mouse_record(0, 0, 0, 0, 0), &state),
            None
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

    #[test]
    fn win_input_batch_handles_resize_only_and_adjacent_resizes() {
        let mut encoder = WinInputEncoder::default();
        let mut parser = termwiz::input::InputParser::new();
        let records = [
            resize_input_record(100, 30),
            resize_input_record(120, 40),
            key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            resize_input_record(80, 25),
        ];

        assert_eq!(
            encode_win_input_batch(
                &mut parser,
                &mut encoder,
                &records,
                &InnerInputState::default()
            ),
            vec![
                WinInputBatchMessage::Resize,
                WinInputBatchMessage::Resize,
                WinInputBatchMessage::Stdin(b"a".to_vec()),
                WinInputBatchMessage::Resize,
            ]
        );
    }

    #[test]
    fn win_input_batch_preserves_focus_resize_mouse_order() {
        let mut encoder = WinInputEncoder::default();
        let mut parser = termwiz::input::InputParser::new();
        let state = InnerInputState {
            focus: true,
            mouse_default: true,
            mouse_tracking: MouseTrackingMode::Default,
            sgr_mouse: true,
            ..Default::default()
        };
        let records = [
            focus_input_record(true),
            resize_input_record(120, 40),
            mouse_input_record(mouse_record(0, 0, FROM_LEFT_1ST_BUTTON_PRESSED, 0, 0)),
            resize_input_record(100, 30),
            focus_input_record(false),
        ];

        assert_eq!(
            encode_win_input_batch(&mut parser, &mut encoder, &records, &state),
            vec![
                WinInputBatchMessage::Stdin(b"\x1b[I".to_vec()),
                WinInputBatchMessage::Resize,
                WinInputBatchMessage::Stdin(b"\x1b[<0;1;1M".to_vec()),
                WinInputBatchMessage::Resize,
                WinInputBatchMessage::Stdin(b"\x1b[O".to_vec()),
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
    fn legacy_right_and_middle_releases_preserve_release_marker() {
        for button in [RIGHTMOST_BUTTON_PRESSED, FROM_LEFT_2ND_BUTTON_PRESSED] {
            let mut encoder = WinInputEncoder::default();
            encoder.last_mouse_buttons = button;
            let state = legacy_mouse_state(MouseTrackingMode::Default);

            assert_eq!(
                encoder
                    .encode_mouse(&mouse_record(0, 0, 0, 0, 0), &state)
                    .unwrap(),
                vec![0x1b, b'[', b'M', 35, 33, 33]
            );
        }
    }

    #[test]
    fn legacy_mouse_encodes_wheels() {
        let state = legacy_mouse_state(MouseTrackingMode::Default);

        assert_eq!(
            encode_mouse_with_state(&mouse_record(0, 0, 120u32 << 16, 0, MOUSE_WHEELED), &state)
                .unwrap(),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
        assert_eq!(
            encode_mouse_with_state(
                &mouse_record(0, 0, (-120i32 as u32) << 16, 0, MOUSE_WHEELED),
                &state
            )
            .unwrap(),
            vec![0x1b, b'[', b'M', 97, 33, 33]
        );
        assert_eq!(
            encode_mouse_with_state(&mouse_record(0, 0, 120u32 << 16, 0, MOUSE_HWHEELED), &state)
                .unwrap(),
            vec![0x1b, b'[', b'M', 98, 33, 33]
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
            encode_mouse_with_state(&mouse_record(0, 0, RIGHTMOST_BUTTON_PRESSED, 0, 0), &state)
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

    #[test]
    fn menu_events_are_ignored_without_disturbing_key_order() {
        let records = [
            key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            menu_input_record(),
            key_record('B' as u16, 0x30, 'b', true, 1, 0),
        ];

        assert_eq!(encode_records_default(&records), b"ab");
    }
}

mod windows_console_integration {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{MutexGuard, OnceLock};
    use std::time::Duration;

    fn console_e2e_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("console e2e lock poisoned")
    }

    fn require_wezterm_pane_for_record_e2e() -> anyhow::Result<bool> {
        if std::env::var_os("WEZTERM_PANE").is_some() {
            return Ok(true);
        }

        if std::env::var_os("WEZTERM_RECORD_E2E_REQUIRED").is_some() {
            anyhow::bail!(
                    "wezterm record e2e requires WEZTERM_PANE; unset WEZTERM_RECORD_E2E_REQUIRED to skip outside a real pane"
                );
        }

        eprintln!(
                "skipping: this e2e requires a real WezTerm pane; set WEZTERM_RECORD_E2E_REQUIRED=1 to make missing prerequisites fail"
            );
        Ok(false)
    }

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

    fn current_test_exe() -> PathBuf {
        std::env::current_exe().expect("current test executable path")
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

    fn wait_for_path(path: &std::path::Path) -> anyhow::Result<()> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if path.exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!("timed out waiting for {}", path.display());
    }

    #[test]
    fn synthetic_reply_child_probe() -> anyhow::Result<()> {
        if std::env::var_os("WEZTERM_RECORD_SYNTHETIC_REPLY_CHILD").is_none() {
            return Ok(());
        }

        let stdin = OpenOptions::new().read(true).write(true).open("CONIN$")?;
        let stdin_handle = stdin.as_raw_handle() as *mut _;
        let mut original_mode = 0;
        assert_ne!(
            unsafe { GetConsoleMode(stdin_handle, &mut original_mode) },
            0
        );
        let probe_mode = (original_mode | ENABLE_VIRTUAL_TERMINAL_INPUT)
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT);
        assert_ne!(unsafe { SetConsoleMode(stdin_handle, probe_mode) }, 0);

        let mut stdout = std::io::stdout();
        stdout.write_all(b"SYNTH_REPLY_PROBE_READY\r\n\x1b[c\x1b[>c\x1b[5n\x1b[6n")?;
        stdout.flush()?;

        let expected = b"\x1b[?1;0c\x1b[>0;0;0c\x1b[0n\x1b[1;1R";
        let mut bytes = Vec::new();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = stdin;
            loop {
                let mut byte = [0u8; 1];
                match reader.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(byte[0]).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !bytes.windows(expected.len()).any(|w| w == expected) {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(byte) => bytes.push(byte),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let _ = unsafe { SetConsoleMode(stdin_handle, original_mode) };
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        println!("SYNTH_REPLY_PROBE_HEX={hex}");
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "synthetic replies missing from child stdin bytes: {hex}",
            hex = hex,
        );

        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows console input buffer"]
    fn write_console_input_records_are_read_and_encoded_by_bridge_reader() -> anyhow::Result<()> {
        let _guard = console_e2e_lock();
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
        if !require_wezterm_pane_for_record_e2e()? {
            return Ok(());
        }
        let _guard = console_e2e_lock();

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

    #[test]
    #[ignore = "spawns wezterm record and injects interactive Windows console input"]
    fn wezterm_record_win_input_on_forwards_console_input_to_child() -> anyhow::Result<()> {
        if !require_wezterm_pane_for_record_e2e()? {
            return Ok(());
        }
        let _guard = console_e2e_lock();

        let cast = tempfile::Builder::new()
            .prefix("wezterm-record-win-input-forward-e2e-")
            .suffix(".cast.txt")
            .tempfile()?
            .into_temp_path();
        let cast_path = cast.to_path_buf();
        let _ready_dir = tempfile::Builder::new()
            .prefix("wezterm-record-win-input-ready-")
            .tempdir()?;
        let ready_path = _ready_dir.path().join("ready");
        let ready_literal = ready_path.display().to_string().replace('\'', "''");
        let script = r#"
$e=[char]27
[Console]::Out.Write("${e}[?9001hINPUT_E2E_READY`r`n")
[System.IO.File]::WriteAllText('__READY__', 'ready')
$stdin=[Console]::OpenStandardInput()
$buf=New-Object byte[] 128
$task=$stdin.ReadAsync($buf, 0, $buf.Length)
if (-not $task.Wait(10000)) {
  [Console]::Out.Write("INPUT_E2E_TIMEOUT`r`n")
  [Console]::Out.Write("${e}[?9001l")
  exit 2
}
$n=$task.Result
$hex=($buf[0..($n - 1)] | ForEach-Object { $_.ToString('X2') }) -join ''
[Console]::Out.Write("INPUT_E2E_HEX=$hex`r`n${e}[?9001l")
"#
        .replace("__READY__", &ready_literal);

        let conin = OpenOptions::new().read(true).write(true).open("CONIN$")?;
        let conin_handle = conin.as_raw_handle() as *mut _;
        assert_ne!(unsafe { FlushConsoleInputBuffer(conin_handle) }, 0);

        let mut child = Command::new(debug_wezterm_exe())
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
            .spawn()?;

        wait_for_path(&ready_path)?;
        assert_ne!(unsafe { FlushConsoleInputBuffer(conin_handle) }, 0);

        let input = [
            key_record('A' as u16, 0x1e, 'a', true, 1, 0),
            key_record(winuser::VK_RETURN as u16, 0x1c, '\r', true, 1, 0),
        ];
        let mut written = 0;
        let ok = unsafe {
            WriteConsoleInputW(
                conin_handle,
                input.as_ptr() as *mut _,
                input.len() as u32,
                &mut written,
            )
        };
        assert_ne!(ok, 0);
        assert_eq!(written, input.len() as u32);

        let status = child.wait()?;
        assert!(
            status.success(),
            "wezterm record failed: status={:?}",
            status.code()
        );

        let payload = cast_payload(&cast_path)?;
        assert!(payload.contains("INPUT_E2E_READY"));
        assert!(
            payload.contains("INPUT_E2E_HEX=610D0A"),
            "payload did not include expected input bytes: {:?}",
            payload
        );
        assert!(!payload.contains("\x1b[?9001h"));
        assert!(!payload.contains("\x1b[?9001l"));
        assert_ne!(unsafe { FlushConsoleInputBuffer(conin_handle) }, 0);

        Ok(())
    }

    #[test]
    #[ignore = "spawns wezterm record and requires an interactive Windows console"]
    fn wezterm_record_win_input_on_sends_synthetic_query_replies_to_child() -> anyhow::Result<()> {
        if !require_wezterm_pane_for_record_e2e()? {
            return Ok(());
        }
        let _guard = console_e2e_lock();

        let cast = tempfile::Builder::new()
            .prefix("wezterm-record-win-input-replies-e2e-")
            .suffix(".cast.txt")
            .tempfile()?
            .into_temp_path();
        let cast_path = cast.to_path_buf();
        let test_exe = current_test_exe();

        let status = Command::new(debug_wezterm_exe())
            .arg("record")
            .arg("--win-input=on")
            .arg("-o")
            .arg(&cast_path)
            .arg("--")
            .arg(&test_exe)
            .arg(
                "asciicast::windows_input_bridge_tests::windows_console_integration::synthetic_reply_child_probe",
            )
            .arg("--exact")
            .arg("--nocapture")
            .env("WEZTERM_RECORD_SYNTHETIC_REPLY_CHILD", "1")
            .status()?;

        assert!(
            status.success(),
            "wezterm record failed: status={:?}",
            status.code()
        );

        let payload = cast_payload(&cast_path)?;
        assert!(payload.contains("SYNTH_REPLY_PROBE_READY"));
        assert!(
            payload.contains("1B5B3F313B30631B5B3E303B303B30631B5B306E1B5B313B3152"),
            "payload did not include expected synthetic query replies: {:?}",
            payload
        );
        assert!(!payload.contains("\x1b[c"));
        assert!(!payload.contains("\x1b[>c"));
        assert!(!payload.contains("\x1b[5n"));
        assert!(!payload.contains("\x1b[6n"));

        Ok(())
    }
}
