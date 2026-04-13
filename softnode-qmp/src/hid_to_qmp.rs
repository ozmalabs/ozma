// SPDX-License-Identifier: AGPL-3.0-only
//! HID Usage ID (keyboard page 0x07) → QMP qcode translation.
//!
//! Also handles:
//!   - Modifier byte diffing (byte 0 of boot report → individual key events)
//!   - Key slot diffing (bytes 2–7 of boot report → pressed/released sets)
//!   - Mouse report decoding → QMP InputMoveEvent + InputBtnEvent
//!
//! QMP input-send-event reference:
//!   <https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#qapidoc-2255>

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// QMP event types (hand-rolled to avoid pulling in full qapi-qmp for events)
// ---------------------------------------------------------------------------

/// A single QMP input event, serialisable to JSON for `input-send-event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum QmpInputEvent {
    Key(KeyEventData),
    Btn(BtnEventData),
    Abs(AbsEventData),
    Rel(RelEventData),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEventData {
    pub down: bool,
    pub key: QmpKeyValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QmpKeyValue {
    #[serde(rename = "type")]
    pub kind: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtnEventData {
    pub down: bool,
    pub button: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsEventData {
    pub axis: String,
    pub value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelEventData {
    pub axis: String,
    pub value: i32,
}

// ---------------------------------------------------------------------------
// HID Usage ID → QMP qcode table
// ---------------------------------------------------------------------------

/// Returns the QMP qcode string for a HID keyboard Usage ID, or `None` if
/// the usage ID is not mapped.
pub fn hid_to_qcode(usage: u8) -> Option<&'static str> {
    match usage {
        // Letters
        0x04 => Some("a"),
        0x05 => Some("b"),
        0x06 => Some("c"),
        0x07 => Some("d"),
        0x08 => Some("e"),
        0x09 => Some("f"),
        0x0A => Some("g"),
        0x0B => Some("h"),
        0x0C => Some("i"),
        0x0D => Some("j"),
        0x0E => Some("k"),
        0x0F => Some("l"),
        0x10 => Some("m"),
        0x11 => Some("n"),
        0x12 => Some("o"),
        0x13 => Some("p"),
        0x14 => Some("q"),
        0x15 => Some("r"),
        0x16 => Some("s"),
        0x17 => Some("t"),
        0x18 => Some("u"),
        0x19 => Some("v"),
        0x1A => Some("w"),
        0x1B => Some("x"),
        0x1C => Some("y"),
        0x1D => Some("z"),
        // Numbers
        0x1E => Some("1"),
        0x1F => Some("2"),
        0x20 => Some("3"),
        0x21 => Some("4"),
        0x22 => Some("5"),
        0x23 => Some("6"),
        0x24 => Some("7"),
        0x25 => Some("8"),
        0x26 => Some("9"),
        0x27 => Some("0"),
        // Control keys
        0x28 => Some("ret"),
        0x29 => Some("esc"),
        0x2A => Some("backspace"),
        0x2B => Some("tab"),
        0x2C => Some("spc"),
        0x2D => Some("minus"),
        0x2E => Some("equal"),
        0x2F => Some("bracket_left"),
        0x30 => Some("bracket_right"),
        0x31 => Some("backslash"),
        0x33 => Some("semicolon"),
        0x34 => Some("apostrophe"),
        0x35 => Some("grave_accent"),
        0x36 => Some("comma"),
        0x37 => Some("dot"),
        0x38 => Some("slash"),
        0x39 => Some("caps_lock"),
        // Function keys
        0x3A => Some("f1"),
        0x3B => Some("f2"),
        0x3C => Some("f3"),
        0x3D => Some("f4"),
        0x3E => Some("f5"),
        0x3F => Some("f6"),
        0x40 => Some("f7"),
        0x41 => Some("f8"),
        0x42 => Some("f9"),
        0x43 => Some("f10"),
        0x44 => Some("f11"),
        0x45 => Some("f12"),
        // System / nav
        0x46 => Some("print"),
        0x47 => Some("scroll_lock"),
        0x48 => Some("pause"),
        0x49 => Some("insert"),
        0x4A => Some("home"),
        0x4B => Some("pgup"),
        0x4C => Some("delete"),
        0x4D => Some("end"),
        0x4E => Some("pgdn"),
        0x4F => Some("right"),
        0x50 => Some("left"),
        0x51 => Some("down"),
        0x52 => Some("up"),
        // Numpad
        0x53 => Some("num_lock"),
        0x54 => Some("kp_divide"),
        0x55 => Some("kp_multiply"),
        0x56 => Some("kp_subtract"),
        0x57 => Some("kp_add"),
        0x58 => Some("kp_enter"),
        0x59 => Some("kp_1"),
        0x5A => Some("kp_2"),
        0x5B => Some("kp_3"),
        0x5C => Some("kp_4"),
        0x5D => Some("kp_5"),
        0x5E => Some("kp_6"),
        0x5F => Some("kp_7"),
        0x60 => Some("kp_8"),
        0x61 => Some("kp_9"),
        0x62 => Some("kp_0"),
        0x63 => Some("kp_decimal"),
        // Misc
        0x64 => Some("less"),
        0x65 => Some("compose"),
        0x66 => Some("power"),
        0x67 => Some("kp_equals"),
        0x68 => Some("f13"),
        0x69 => Some("f14"),
        0x6A => Some("f15"),
        0x6B => Some("f16"),
        0x6C => Some("f17"),
        0x6D => Some("f18"),
        0x6E => Some("f19"),
        0x6F => Some("f20"),
        0x70 => Some("f21"),
        0x71 => Some("f22"),
        0x72 => Some("f23"),
        0x73 => Some("f24"),
        0x7F => Some("audmute"),
        0x80 => Some("volinc"),
        0x81 => Some("voldec"),
        // Modifiers (byte 0 bits map to these separately)
        0xE0 => Some("ctrl"),
        0xE1 => Some("shift"),
        0xE2 => Some("alt"),
        0xE3 => Some("meta_l"),
        0xE4 => Some("ctrl_r"),
        0xE5 => Some("shift_r"),
        0xE6 => Some("alt_r"),
        0xE7 => Some("meta_r"),
        _ => None,
    }
}

/// Modifier byte bit index → HID Usage ID (matches `_MODIFIER_BITS` in Python).
const MODIFIER_BITS: [u8; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];

/// Mouse button bit → QMP button name.
fn mouse_button_name(bit: u8) -> Option<&'static str> {
    match bit {
        0 => Some("left"),
        1 => Some("right"),
        2 => Some("middle"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// QMP event builders
// ---------------------------------------------------------------------------

fn key_event(qcode: &str, down: bool) -> QmpInputEvent {
    QmpInputEvent::Key(KeyEventData {
        down,
        key: QmpKeyValue {
            kind: "qcode".to_string(),
            data: qcode.to_string(),
        },
    })
}

fn btn_event(button: &str, down: bool) -> QmpInputEvent {
    QmpInputEvent::Btn(BtnEventData {
        down,
        button: button.to_string(),
    })
}

fn abs_event(axis: &str, value: i32) -> QmpInputEvent {
    QmpInputEvent::Abs(AbsEventData {
        axis: axis.to_string(),
        value,
    })
}

// ---------------------------------------------------------------------------
// Boot report state machines
// ---------------------------------------------------------------------------

/// Shadows the last HID boot keyboard report and diffs each new report,
/// producing a list of QMP events representing only the changes.
///
/// The HID boot report is 8 bytes:
///   `[0]` modifier bitmask  
///   `[1]` reserved (ignored)  
///   `[2..7]` up to 6 key usage IDs (0x00 = empty slot)
#[derive(Debug, Default)]
pub struct KeyboardReportState {
    prev_modifiers: u8,
    prev_keys: HashSet<u8>,
}

impl KeyboardReportState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return QMP events for changes between the previous report and this one.
    pub fn diff(&mut self, report: &[u8]) -> Vec<QmpInputEvent> {
        if report.len() < 8 {
            return vec![];
        }

        let mut events = Vec::new();
        let modifier_byte = report[0];
        let current_keys: HashSet<u8> = report[2..8]
            .iter()
            .copied()
            .filter(|&k| k != 0x00)
            .collect();

        // Diff modifier bits
        let changed_mods = self.prev_modifiers ^ modifier_byte;
        for bit in 0u8..8 {
            if changed_mods & (1 << bit) != 0 {
                let hid = MODIFIER_BITS[bit as usize];
                if let Some(qcode) = hid_to_qcode(hid) {
                    let down = modifier_byte & (1 << bit) != 0;
                    events.push(key_event(qcode, down));
                }
            }
        }

        // Diff key slots (treat as sets — order can change between reports)
        let mut released: Vec<u8> = self.prev_keys.difference(&current_keys).copied().collect();
        released.sort_unstable();
        for hid in released {
            if let Some(qcode) = hid_to_qcode(hid) {
                events.push(key_event(qcode, false));
            }
        }

        let mut pressed: Vec<u8> = current_keys.difference(&self.prev_keys).copied().collect();
        pressed.sort_unstable();
        for hid in pressed {
            if let Some(qcode) = hid_to_qcode(hid) {
                events.push(key_event(qcode, true));
            }
        }

        self.prev_modifiers = modifier_byte;
        self.prev_keys = current_keys;
        events
    }

    /// Generate key-up events for everything currently held.
    pub fn release_all(&mut self) -> Vec<QmpInputEvent> {
        let mut events = Vec::new();

        for bit in 0u8..8 {
            if self.prev_modifiers & (1 << bit) != 0 {
                let hid = MODIFIER_BITS[bit as usize];
                if let Some(qcode) = hid_to_qcode(hid) {
                    events.push(key_event(qcode, false));
                }
            }
        }

        let mut keys: Vec<u8> = self.prev_keys.iter().copied().collect();
        keys.sort_unstable();
        for hid in keys {
            if let Some(qcode) = hid_to_qcode(hid) {
                events.push(key_event(qcode, false));
            }
        }

        self.prev_modifiers = 0;
        self.prev_keys.clear();
        events
    }
}

/// Decodes the 6-byte absolute mouse report and produces QMP events.
///
/// ```text
/// [0]    buttons bitmask (bit 0=left, 1=right, 2=middle)
/// [1..2] X little-endian 0–32767
/// [3..4] Y little-endian 0–32767
/// [5]    scroll (signed byte)
/// ```
#[derive(Debug, Default)]
pub struct MouseReportState {
    prev_buttons: u8,
}

impl MouseReportState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode a mouse report into QMP events.
    pub fn decode(&mut self, report: &[u8]) -> Vec<QmpInputEvent> {
        if report.len() < 6 {
            return vec![];
        }

        let mut events = Vec::new();
        let buttons = report[0];
        let x = (report[1] as i32) | ((report[2] as i32) << 8);
        let y = (report[3] as i32) | ((report[4] as i32) << 8);
        // Signed byte
        let scroll = report[5] as i8 as i32;

        // Absolute position
        events.push(abs_event("x", x));
        events.push(abs_event("y", y));

        // Scroll wheel → vertical button events
        if scroll > 0 {
            for _ in 0..scroll {
                events.push(btn_event("wheel-up", true));
                events.push(btn_event("wheel-up", false));
            }
        } else if scroll < 0 {
            for _ in 0..(-scroll) {
                events.push(btn_event("wheel-down", true));
                events.push(btn_event("wheel-down", false));
            }
        }

        // Button diffs
        let changed = self.prev_buttons ^ buttons;
        for bit in 0u8..3 {
            if changed & (1 << bit) != 0 {
                if let Some(name) = mouse_button_name(bit) {
                    let down = buttons & (1 << bit) != 0;
                    events.push(btn_event(name, down));
                }
            }
        }

        self.prev_buttons = buttons;
        events
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hid_to_qcode_letters() {
        assert_eq!(hid_to_qcode(0x04), Some("a"));
        assert_eq!(hid_to_qcode(0x1D), Some("z"));
    }

    #[test]
    fn test_hid_to_qcode_unknown() {
        assert_eq!(hid_to_qcode(0x00), None);
        assert_eq!(hid_to_qcode(0xFF), None);
    }

    #[test]
    fn test_keyboard_diff_press_release() {
        let mut state = KeyboardReportState::new();

        // Press 'a' (0x04)
        let report_press = [0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
        let events = state.diff(&report_press);
        assert_eq!(events.len(), 1);
        if let QmpInputEvent::Key(k) = &events[0] {
            assert_eq!(k.key.data, "a");
            assert!(k.down);
        } else {
            panic!("expected key event");
        }

        // Release 'a'
        let report_release = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let events = state.diff(&report_release);
        assert_eq!(events.len(), 1);
        if let QmpInputEvent::Key(k) = &events[0] {
            assert_eq!(k.key.data, "a");
            assert!(!k.down);
        } else {
            panic!("expected key event");
        }
    }

    #[test]
    fn test_keyboard_modifier_diff() {
        let mut state = KeyboardReportState::new();

        // Press left shift (bit 1 of modifier byte → 0xE1 → "shift")
        let report = [0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let events = state.diff(&report);
        assert_eq!(events.len(), 1);
        if let QmpInputEvent::Key(k) = &events[0] {
            assert_eq!(k.key.data, "shift");
            assert!(k.down);
        } else {
            panic!("expected key event");
        }
    }

    #[test]
    fn test_keyboard_release_all() {
        let mut state = KeyboardReportState::new();
        let report = [0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
        state.diff(&report);

        let events = state.release_all();
        // shift + a released
        assert_eq!(events.len(), 2);
        assert_eq!(state.prev_modifiers, 0);
        assert!(state.prev_keys.is_empty());
    }

    #[test]
    fn test_mouse_abs_position() {
        let mut state = MouseReportState::new();
        // x=0x0100 (256), y=0x0200 (512), no buttons, no scroll
        let report = [0x00, 0x00, 0x01, 0x00, 0x02, 0x00];
        let events = state.decode(&report);
        assert!(events.len() >= 2);
        if let QmpInputEvent::Abs(a) = &events[0] {
            assert_eq!(a.axis, "x");
            assert_eq!(a.value, 256);
        }
        if let QmpInputEvent::Abs(a) = &events[1] {
            assert_eq!(a.axis, "y");
            assert_eq!(a.value, 512);
        }
    }

    #[test]
    fn test_mouse_button_press() {
        let mut state = MouseReportState::new();
        let report = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00]; // left button down
        let events = state.decode(&report);
        let btn_events: Vec<_> = events
            .iter()
            .filter_map(|e| if let QmpInputEvent::Btn(b) = e { Some(b) } else { None })
            .collect();
        assert_eq!(btn_events.len(), 1);
        assert_eq!(btn_events[0].button, "left");
        assert!(btn_events[0].down);
    }

    #[test]
    fn test_mouse_scroll_up() {
        let mut state = MouseReportState::new();
        let report = [0x00, 0x00, 0x00, 0x00, 0x00, 0x02]; // scroll +2
        let events = state.decode(&report);
        let wheel_events: Vec<_> = events
            .iter()
            .filter_map(|e| if let QmpInputEvent::Btn(b) = e { Some(b) } else { None })
            .collect();
        // 2 × (down + up) = 4 events
        assert_eq!(wheel_events.len(), 4);
        assert_eq!(wheel_events[0].button, "wheel-up");
    }
}
