// SPDX-License-Identifier: AGPL-3.0-only
//! HID usage translation and boot-report state machines.
//!
//! * [`hid_to_qcode`] maps keyboard page `0x07` usage ids to QEMU qcodes.
//! * [`KeyboardReportState`] diffs 8-byte boot keyboard reports.
//! * [`MouseReportState`] decodes the 6-byte absolute pointer report.
//! * [`RelativeMouseReportState`] decodes the 0x05 relative (gaming) report.
//!
//! QMP reference:
//! <https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#qapidoc-2255>

use std::collections::BTreeSet;

use crate::input_event::QmpInputEvent;

/// HID keyboard usage id → QMP qcode table.
///
/// This is the complete Python `HID_TO_QCODE` table, sorted by usage id so
/// [`hid_to_qcode`] can binary-search it. Unknown usages (including the
/// rollover phantom `0x01`) are absent.
pub const HID_TO_QCODE: &[(u8, &str)] = &[
    // Letters
    (0x04, "a"),
    (0x05, "b"),
    (0x06, "c"),
    (0x07, "d"),
    (0x08, "e"),
    (0x09, "f"),
    (0x0A, "g"),
    (0x0B, "h"),
    (0x0C, "i"),
    (0x0D, "j"),
    (0x0E, "k"),
    (0x0F, "l"),
    (0x10, "m"),
    (0x11, "n"),
    (0x12, "o"),
    (0x13, "p"),
    (0x14, "q"),
    (0x15, "r"),
    (0x16, "s"),
    (0x17, "t"),
    (0x18, "u"),
    (0x19, "v"),
    (0x1A, "w"),
    (0x1B, "x"),
    (0x1C, "y"),
    (0x1D, "z"),
    // Numbers
    (0x1E, "1"),
    (0x1F, "2"),
    (0x20, "3"),
    (0x21, "4"),
    (0x22, "5"),
    (0x23, "6"),
    (0x24, "7"),
    (0x25, "8"),
    (0x26, "9"),
    (0x27, "0"),
    // Control keys
    (0x28, "ret"),
    (0x29, "esc"),
    (0x2A, "backspace"),
    (0x2B, "tab"),
    (0x2C, "spc"),
    (0x2D, "minus"),
    (0x2E, "equal"),
    (0x2F, "bracket_left"),
    (0x30, "bracket_right"),
    (0x31, "backslash"),
    (0x33, "semicolon"),
    (0x34, "apostrophe"),
    (0x35, "grave_accent"),
    (0x36, "comma"),
    (0x37, "dot"),
    (0x38, "slash"),
    (0x39, "caps_lock"),
    // Function keys
    (0x3A, "f1"),
    (0x3B, "f2"),
    (0x3C, "f3"),
    (0x3D, "f4"),
    (0x3E, "f5"),
    (0x3F, "f6"),
    (0x40, "f7"),
    (0x41, "f8"),
    (0x42, "f9"),
    (0x43, "f10"),
    (0x44, "f11"),
    (0x45, "f12"),
    // System / navigation
    (0x46, "print"),
    (0x47, "scroll_lock"),
    (0x48, "pause"),
    (0x49, "insert"),
    (0x4A, "home"),
    (0x4B, "pgup"),
    (0x4C, "delete"),
    (0x4D, "end"),
    (0x4E, "pgdn"),
    (0x4F, "right"),
    (0x50, "left"),
    (0x51, "down"),
    (0x52, "up"),
    // Numpad
    (0x53, "num_lock"),
    (0x54, "kp_divide"),
    (0x55, "kp_multiply"),
    (0x56, "kp_subtract"),
    (0x57, "kp_add"),
    (0x58, "kp_enter"),
    (0x59, "kp_1"),
    (0x5A, "kp_2"),
    (0x5B, "kp_3"),
    (0x5C, "kp_4"),
    (0x5D, "kp_5"),
    (0x5E, "kp_6"),
    (0x5F, "kp_7"),
    (0x60, "kp_8"),
    (0x61, "kp_9"),
    (0x62, "kp_0"),
    (0x63, "kp_decimal"),
    // Misc
    (0x64, "less"),
    (0x65, "compose"),
    (0x66, "power"),
    (0x67, "kp_equals"),
    (0x68, "f13"),
    (0x69, "f14"),
    (0x6A, "f15"),
    (0x6B, "f16"),
    (0x6C, "f17"),
    (0x6D, "f18"),
    (0x6E, "f19"),
    (0x6F, "f20"),
    (0x70, "f21"),
    (0x71, "f22"),
    (0x72, "f23"),
    (0x73, "f24"),
    (0x7F, "audmute"),
    (0x80, "volinc"),
    (0x81, "voldec"),
    // Modifier usage ids (also represented as bits in report byte 0)
    (0xE0, "ctrl"),
    (0xE1, "shift"),
    (0xE2, "alt"),
    (0xE3, "meta_l"),
    (0xE4, "ctrl_r"),
    (0xE5, "shift_r"),
    (0xE6, "alt_r"),
    (0xE7, "meta_r"),
];

/// Modifier byte bit index → HID usage id.
const MODIFIER_BITS: [u8; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];

/// Mouse button bit index → QMP button name.
const MOUSE_BUTTONS: [(u8, &str); 3] = [(0, "left"), (1, "right"), (2, "middle")];

/// Translate a HID keyboard usage id to a QMP qcode.
///
/// Returns `None` for unmapped usages such as the rollover phantom `0x01`.
pub fn hid_to_qcode(usage: u8) -> Option<&'static str> {
    HID_TO_QCODE
        .binary_search_by_key(&usage, |&(code, _)| code)
        .ok()
        .map(|index| HID_TO_QCODE[index].1)
}

/// Shadows the last HID boot keyboard report and emits only the differences.
///
/// The 8-byte boot report layout is:
///
/// ```text
/// [0]    modifier bitmask
/// [1]    reserved (ignored)
/// [2..8] up to six key usage ids (0x00 = empty slot)
/// ```
///
/// Reports shorter than 8 bytes are ignored. Key slots are treated as a set,
/// so reordering the same keys is a no-op. Releases are emitted before presses
/// and each group is sorted by usage id, matching the Python implementation.
#[derive(Debug, Default, Clone)]
pub struct KeyboardReportState {
    prev_modifiers: u8,
    prev_keys: BTreeSet<u8>,
}

impl KeyboardReportState {
    /// Create an empty state (no keys held).
    pub fn new() -> Self {
        Self::default()
    }

    /// Diff `report` against the previous report and return the changes.
    pub fn diff(&mut self, report: &[u8]) -> Vec<QmpInputEvent> {
        if report.len() < 8 {
            return Vec::new();
        }

        let mut events = Vec::new();
        let modifier_byte = report[0];
        let current_keys: BTreeSet<u8> = report[2..8]
            .iter()
            .copied()
            .filter(|&key| key != 0x00)
            .collect();

        // Modifier bits, low bit first.
        let changed_modifiers = self.prev_modifiers ^ modifier_byte;
        for bit in 0u8..8 {
            if changed_modifiers & (1 << bit) != 0 {
                if let Some(qcode) = hid_to_qcode(MODIFIER_BITS[bit as usize]) {
                    let down = modifier_byte & (1 << bit) != 0;
                    events.push(QmpInputEvent::key(qcode, down));
                }
            }
        }

        // Released keys before pressed keys; BTreeSet iteration is sorted.
        for hid in self.prev_keys.difference(&current_keys) {
            if let Some(qcode) = hid_to_qcode(*hid) {
                events.push(QmpInputEvent::key(qcode, false));
            }
        }
        for hid in current_keys.difference(&self.prev_keys) {
            if let Some(qcode) = hid_to_qcode(*hid) {
                events.push(QmpInputEvent::key(qcode, true));
            }
        }

        self.prev_modifiers = modifier_byte;
        self.prev_keys = current_keys;
        events
    }

    /// Emit key-up events for every modifier and key currently held, then
    /// clear the state. Calling this twice emits nothing the second time.
    pub fn release_all(&mut self) -> Vec<QmpInputEvent> {
        let mut events = Vec::new();

        for bit in 0u8..8 {
            if self.prev_modifiers & (1 << bit) != 0 {
                if let Some(qcode) = hid_to_qcode(MODIFIER_BITS[bit as usize]) {
                    events.push(QmpInputEvent::key(qcode, false));
                }
            }
        }
        for hid in &self.prev_keys {
            if let Some(qcode) = hid_to_qcode(*hid) {
                events.push(QmpInputEvent::key(qcode, false));
            }
        }

        self.prev_modifiers = 0;
        self.prev_keys.clear();
        events
    }

    /// True when any key or modifier is currently held.
    pub fn is_empty(&self) -> bool {
        self.prev_modifiers == 0 && self.prev_keys.is_empty()
    }
}

/// Decodes the 6-byte absolute pointer report used by the Ozma HID gadget.
///
/// ```text
/// [0]    buttons bitmask (bit 0=left, 1=right, 2=middle)
/// [1..2] X little-endian, 0..=32767
/// [3..4] Y little-endian, 0..=32767
/// [5]    scroll, signed byte
/// ```
///
/// Every report emits absolute X and Y. A non-zero scroll emits one
/// `wheel-up`/`wheel-down` press+release pair per notch. Buttons are diffed
/// against the previous report.
#[derive(Debug, Default, Clone)]
pub struct MouseReportState {
    prev_buttons: u8,
}

impl MouseReportState {
    /// Create an empty state (no buttons held).
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode an absolute mouse report into QMP events.
    pub fn decode(&mut self, report: &[u8]) -> Vec<QmpInputEvent> {
        if report.len() < 6 {
            return Vec::new();
        }

        let mut events = Vec::new();
        let buttons = report[0];
        let x = u32::from(u16::from_le_bytes([report[1], report[2]]));
        let y = u32::from(u16::from_le_bytes([report[3], report[4]]));
        let scroll = i32::from(report[5] as i8);

        events.push(QmpInputEvent::abs("x", x));
        events.push(QmpInputEvent::abs("y", y));
        push_scroll_events(&mut events, scroll);

        let changed = self.prev_buttons ^ buttons;
        for (bit, name) in MOUSE_BUTTONS {
            if changed & (1 << bit) != 0 {
                let down = buttons & (1 << bit) != 0;
                events.push(QmpInputEvent::button(name, down));
            }
        }

        self.prev_buttons = buttons;
        events
    }
}

/// Decodes the 0x05 relative pointer wire format used by the controller's
/// gaming mode.
///
/// ```text
/// [0]    buttons bitmask (bit 0=left, 1=right, 2=middle)
/// [1..3] dx, signed little-endian i16
/// [3..5] dy, signed little-endian i16
/// [5]    scroll, signed byte (optional)
/// ```
///
/// Reports shorter than 5 bytes are ignored. The scroll byte is treated as 0
/// when the report has exactly five bytes.
#[derive(Debug, Default, Clone)]
pub struct RelativeMouseReportState {
    prev_buttons: u8,
}

impl RelativeMouseReportState {
    /// Create an empty state (no buttons held).
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode a relative mouse report into QMP events.
    pub fn decode(&mut self, report: &[u8]) -> Vec<QmpInputEvent> {
        if report.len() < 5 {
            return Vec::new();
        }

        let mut events = Vec::new();
        let buttons = report[0];
        let dx = i32::from(i16::from_le_bytes([report[1], report[2]]));
        let dy = i32::from(i16::from_le_bytes([report[3], report[4]]));
        let scroll = if report.len() > 5 {
            i32::from(report[5] as i8)
        } else {
            0
        };

        events.push(QmpInputEvent::rel("x", dx));
        events.push(QmpInputEvent::rel("y", dy));
        push_scroll_events(&mut events, scroll);

        let changed = self.prev_buttons ^ buttons;
        for (bit, name) in MOUSE_BUTTONS {
            if changed & (1 << bit) != 0 {
                let down = buttons & (1 << bit) != 0;
                events.push(QmpInputEvent::button(name, down));
            }
        }

        self.prev_buttons = buttons;
        events
    }
}

fn push_scroll_events(events: &mut Vec<QmpInputEvent>, scroll: i32) {
    if scroll > 0 {
        for _ in 0..scroll {
            events.push(QmpInputEvent::button("wheel-up", true));
            events.push(QmpInputEvent::button("wheel-up", false));
        }
    } else if scroll < 0 {
        for _ in 0..-scroll {
            events.push(QmpInputEvent::button("wheel-down", true));
            events.push(QmpInputEvent::button("wheel-down", false));
        }
    }
}
