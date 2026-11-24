// SPDX-License-Identifier: AGPL-3.0-only
//! Ports of `tests/unit/softnode/test_hid_to_qmp.py` plus JSON-shape and
//! relative-pointer coverage.

use ozma_qmp::{
    hid_to_qcode, KeyboardReportState, MouseReportState, QmpInputEvent, RelativeMouseReportState,
    HID_TO_QCODE,
};

fn keys(events: &[QmpInputEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            QmpInputEvent::Key { down, key } => Some((key.data.clone(), *down)),
            _ => None,
        })
        .collect()
}

fn buttons(events: &[QmpInputEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            QmpInputEvent::Btn { down, button } => Some((button.clone(), *down)),
            _ => None,
        })
        .collect()
}

fn abs(events: &[QmpInputEvent]) -> Vec<(String, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            QmpInputEvent::Abs { axis, value } => Some((axis.clone(), *value)),
            _ => None,
        })
        .collect()
}

fn rel(events: &[QmpInputEvent]) -> Vec<(String, i32)> {
    events
        .iter()
        .filter_map(|event| match event {
            QmpInputEvent::Rel { axis, value } => Some((axis.clone(), *value)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// HID usage → qcode mapping (6 Python tests)
// ---------------------------------------------------------------------------

#[test]
fn letters_mapped() {
    for hid in 0x04..=0x1D {
        let qcode = hid_to_qcode(hid).unwrap_or_else(|| panic!("HID 0x{hid:02X} is not mapped"));
        assert!(!qcode.is_empty());
    }
    assert_eq!(hid_to_qcode(0x04), Some("a"));
    assert_eq!(hid_to_qcode(0x1D), Some("z"));
}

#[test]
fn numbers_mapped() {
    for hid in 0x1E..=0x27 {
        assert!(hid_to_qcode(hid).is_some(), "HID 0x{hid:02X} is not mapped");
    }
    assert_eq!(hid_to_qcode(0x1E), Some("1"));
    assert_eq!(hid_to_qcode(0x27), Some("0"));
}

#[test]
fn specific_control_keys_mapped() {
    assert_eq!(hid_to_qcode(0x28), Some("ret"));
    assert_eq!(hid_to_qcode(0x29), Some("esc"));
    assert_eq!(hid_to_qcode(0x2A), Some("backspace"));
    assert_eq!(hid_to_qcode(0x2B), Some("tab"));
    assert_eq!(hid_to_qcode(0x2C), Some("spc"));
}

#[test]
fn function_keys_mapped() {
    for hid in 0x3A..=0x45 {
        assert!(hid_to_qcode(hid).is_some(), "HID 0x{hid:02X} is not mapped");
    }
    assert_eq!(hid_to_qcode(0x3A), Some("f1"));
    assert_eq!(hid_to_qcode(0x45), Some("f12"));
    for hid in 0x68..=0x73 {
        assert!(hid_to_qcode(hid).is_some(), "HID 0x{hid:02X} is not mapped");
    }
    assert_eq!(hid_to_qcode(0x68), Some("f13"));
    assert_eq!(hid_to_qcode(0x73), Some("f24"));
}

#[test]
fn arrow_keys_mapped() {
    assert_eq!(hid_to_qcode(0x4F), Some("right"));
    assert_eq!(hid_to_qcode(0x50), Some("left"));
    assert_eq!(hid_to_qcode(0x51), Some("down"));
    assert_eq!(hid_to_qcode(0x52), Some("up"));
    // ISO "Non-US # and ~" shares the US backslash physical position.
    assert_eq!(hid_to_qcode(0x32), Some("backslash"));
}

#[test]
fn no_empty_values() {
    for &(code, qcode) in HID_TO_QCODE {
        assert!(!qcode.is_empty(), "HID 0x{code:02X} maps to empty string");
        assert_eq!(hid_to_qcode(code), Some(qcode));
    }
}

#[test]
fn unknown_usages_return_none() {
    for hid in [0x00u8, 0x01, 0x02, 0x74, 0x82, 0xFF] {
        assert_eq!(hid_to_qcode(hid), None, "HID 0x{hid:02X} should be None");
    }
}

// ---------------------------------------------------------------------------
// Keyboard report state machine (9 Python tests + extensions)
// ---------------------------------------------------------------------------

#[test]
fn keyboard_empty_report_has_no_events() {
    let mut state = KeyboardReportState::new();
    assert_eq!(state.diff(&[0u8; 8]), Vec::new());
}

#[test]
fn keyboard_press_a() {
    let mut state = KeyboardReportState::new();
    let events = state.diff(&[0x00, 0x00, 0x04, 0, 0, 0, 0, 0]);
    assert_eq!(keys(&events), vec![("a".to_string(), true)]);
}

#[test]
fn keyboard_release_a() {
    let mut state = KeyboardReportState::new();
    state.diff(&[0x00, 0x00, 0x04, 0, 0, 0, 0, 0]);
    let events = state.diff(&[0u8; 8]);
    assert_eq!(keys(&events), vec![("a".to_string(), false)]);
}

#[test]
fn keyboard_modifier_left_ctrl() {
    let mut state = KeyboardReportState::new();
    let events = state.diff(&[0x01, 0x00, 0, 0, 0, 0, 0, 0]);
    assert_eq!(keys(&events), vec![("ctrl".to_string(), true)]);
}

#[test]
fn keyboard_modifier_left_shift() {
    let mut state = KeyboardReportState::new();
    let events = state.diff(&[0x02, 0x00, 0, 0, 0, 0, 0, 0]);
    assert_eq!(keys(&events), vec![("shift".to_string(), true)]);
}

#[test]
fn keyboard_multiple_keys() {
    let mut state = KeyboardReportState::new();
    let events = state.diff(&[0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0]);
    let pressed: Vec<String> = keys(&events)
        .into_iter()
        .filter_map(|(qcode, down)| down.then_some(qcode))
        .collect();
    assert_eq!(pressed, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn keyboard_short_report_ignored() {
    let mut state = KeyboardReportState::new();
    assert_eq!(state.diff(&[0u8; 3]), Vec::new());
    // A short report must not clobber the previous state.
    let events = state.diff(&[0x00, 0x00, 0x04, 0, 0, 0, 0, 0]);
    assert_eq!(keys(&events), vec![("a".to_string(), true)]);
}

#[test]
fn keyboard_rollover_no_crash() {
    let mut state = KeyboardReportState::new();
    let events = state.diff(&[0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]);
    // 0x01 is the rollover phantom and is intentionally unmapped.
    assert!(events.is_empty());
    // Rollover state is still tracked, so leaving it releases the phantom
    // (which maps to nothing) and the state returns to empty.
    let events = state.diff(&[0u8; 8]);
    assert!(events.is_empty());
    assert!(state.is_empty());
}

#[test]
fn keyboard_key_then_key_only_new_pressed() {
    let mut state = KeyboardReportState::new();
    state.diff(&[0x00, 0x00, 0x04, 0, 0, 0, 0, 0]); // press a
    let events = state.diff(&[0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0]); // add b
    assert_eq!(keys(&events), vec![("b".to_string(), true)]);
}

#[test]
fn keyboard_duplicate_report_is_noop() {
    let mut state = KeyboardReportState::new();
    let report = [0x01, 0x00, 0x04, 0x05, 0, 0, 0, 0];
    assert_eq!(keys(&state.diff(&report)).len(), 3); // ctrl + a + b
    assert!(state.diff(&report).is_empty());
}

#[test]
fn keyboard_release_all_clears_state() {
    let mut state = KeyboardReportState::new();
    state.diff(&[0x03, 0x00, 0x04, 0x05, 0, 0, 0, 0]); // ctrl + shift + a + b
    let events = state.release_all();
    assert_eq!(
        keys(&events),
        vec![
            ("ctrl".to_string(), false),
            ("shift".to_string(), false),
            ("a".to_string(), false),
            ("b".to_string(), false),
        ]
    );
    assert!(state.is_empty());
    assert!(state.release_all().is_empty());
}

#[test]
fn keyboard_releases_before_presses() {
    let mut state = KeyboardReportState::new();
    state.diff(&[0x00, 0x00, 0x04, 0, 0, 0, 0, 0]); // a
    let events = state.diff(&[0x00, 0x00, 0x05, 0, 0, 0, 0, 0]); // b
    assert_eq!(
        keys(&events),
        vec![("a".to_string(), false), ("b".to_string(), true)]
    );
}

#[test]
fn keyboard_unknown_usage_is_silent() {
    let mut state = KeyboardReportState::new();
    assert!(state.diff(&[0x00, 0x00, 0x02, 0, 0, 0, 0, 0]).is_empty());
}

// ---------------------------------------------------------------------------
// Absolute mouse report state machine (6 Python tests + extensions)
// ---------------------------------------------------------------------------

#[test]
fn mouse_position_decoding() {
    let mut state = MouseReportState::new();
    let events = state.decode(&[0, 0xFF, 0x3F, 0xFF, 0x3F, 0]);
    assert_eq!(
        abs(&events),
        vec![("x".to_string(), 16383), ("y".to_string(), 16383)]
    );
}

#[test]
fn mouse_left_button_press() {
    let mut state = MouseReportState::new();
    let events = state.decode(&[1, 100, 0, 100, 0, 0]);
    assert_eq!(buttons(&events), vec![("left".to_string(), true)]);
}

#[test]
fn mouse_button_release() {
    let mut state = MouseReportState::new();
    state.decode(&[1, 100, 0, 100, 0, 0]);
    let events = state.decode(&[0, 100, 0, 100, 0, 0]);
    assert_eq!(buttons(&events), vec![("left".to_string(), false)]);
}

#[test]
fn mouse_scroll_up() {
    let mut state = MouseReportState::new();
    let events = state.decode(&[0, 0, 0, 0, 0, 1]);
    assert_eq!(
        buttons(&events),
        vec![
            ("wheel-up".to_string(), true),
            ("wheel-up".to_string(), false)
        ]
    );
}

#[test]
fn mouse_short_report_ignored() {
    let mut state = MouseReportState::new();
    assert!(state.decode(&[0u8; 2]).is_empty());
}

#[test]
fn mouse_zero_position_still_emits_axes() {
    let mut state = MouseReportState::new();
    let events = state.decode(&[0u8; 6]);
    assert_eq!(
        abs(&events),
        vec![("x".to_string(), 0), ("y".to_string(), 0)]
    );
}

#[test]
fn mouse_right_and_middle_buttons() {
    let mut state = MouseReportState::new();
    let events = state.decode(&[0b110, 0, 0, 0, 0, 0]);
    assert_eq!(
        buttons(&events),
        vec![("right".to_string(), true), ("middle".to_string(), true)]
    );
    let events = state.decode(&[0, 0, 0, 0, 0, 0]);
    assert_eq!(
        buttons(&events),
        vec![("right".to_string(), false), ("middle".to_string(), false)]
    );
}

#[test]
fn mouse_scroll_down_repeats_pairs() {
    let mut state = MouseReportState::new();
    let events = state.decode(&[0, 0, 0, 0, 0, 0xFE]); // -2
    assert_eq!(
        buttons(&events),
        vec![
            ("wheel-down".to_string(), true),
            ("wheel-down".to_string(), false),
            ("wheel-down".to_string(), true),
            ("wheel-down".to_string(), false),
        ]
    );
}

// ---------------------------------------------------------------------------
// Relative mouse report (0x05 wire format)
// ---------------------------------------------------------------------------

#[test]
fn relative_mouse_decodes_signed_deltas() {
    let mut state = RelativeMouseReportState::new();
    // dx = -2 (0xFFFE), dy = 2
    let events = state.decode(&[0, 0xFE, 0xFF, 0x02, 0x00, 0]);
    assert_eq!(
        rel(&events),
        vec![("x".to_string(), -2), ("y".to_string(), 2)]
    );
}

#[test]
fn relative_mouse_buttons_and_scroll() {
    let mut state = RelativeMouseReportState::new();
    let events = state.decode(&[1, 0, 0, 0, 0, 1]);
    assert!(buttons(&events).contains(&("left".to_string(), true)));
    assert!(buttons(&events).contains(&("wheel-up".to_string(), true)));
    let events = state.decode(&[0, 0, 0, 0, 0, 0]);
    assert!(buttons(&events).contains(&("left".to_string(), false)));
    assert!(rel(&events).contains(&("x".to_string(), 0)));
}

#[test]
fn relative_mouse_short_report_ignored() {
    let mut state = RelativeMouseReportState::new();
    assert!(state.decode(&[0u8; 4]).is_empty());
}

#[test]
fn relative_mouse_five_byte_report_has_no_scroll() {
    let mut state = RelativeMouseReportState::new();
    let events = state.decode(&[0, 1, 0, 1, 0]);
    assert_eq!(
        rel(&events),
        vec![("x".to_string(), 1), ("y".to_string(), 1)]
    );
    assert!(buttons(&events).is_empty());
}
