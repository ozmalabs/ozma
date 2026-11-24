// SPDX-License-Identifier: AGPL-3.0-only
//! Bidirectional mapping between USB HID keyboard usages and Linux keycodes.
//!
//! The controller compatibility `/input/key` endpoint receives Linux evdev
//! keycodes (the Python node wrote them straight to an evdev fd), while Ozma's
//! native input path carries USB HID usage ids. [`linux_keycode_to_hid_usage`]
//! is the reverse of [`hid_usage_to_linux_keycode`], so both directions are
//! derived from the single [`KEYMAP`] table below.
//!
//! # Shared Linux keycodes
//!
//! A few HID usages can legitimately share one Linux keycode. For those the
//! table order is the tie-breaker: the first matching entry is the canonical
//! HID usage, so the primary key wins over a secondary one. Two pairs are
//! shared today:
//!
//! * `KEY_PAUSE` (119), used by both `0x48` (Pause) and `0xB1` (the media-page
//!   Pause); `0x48` is listed first and therefore resolves the reverse
//!   direction.
//! * `KEY_BACKSLASH` (43), used by `0x31` (`\` and `|`, the US position) and
//!   `0x32` (Non-US `#` and `~`, the same ISO position used by UK/German/
//!   French layouts). `0x31` is listed first and resolves the reverse
//!   direction; the forward direction still distinguishes the two for typing.
//!
//! Table order otherwise matches ascending HID usage id and is covered by a
//! test, so adding an entry in the wrong place is caught before it can affect
//! the duplicate resolution.

/// Shared HID usage ↔ Linux keycode table.
///
/// Order is significant for duplicate Linux keycodes: the first entry with a
/// given keycode is returned by [`linux_keycode_to_hid_usage`].
const KEYMAP: &[(u8, u16)] = &[
    // Letters.
    (0x04, 30), // a
    (0x05, 48), // b
    (0x06, 46), // c
    (0x07, 32), // d
    (0x08, 18), // e
    (0x09, 33), // f
    (0x0A, 34), // g
    (0x0B, 35), // h
    (0x0C, 23), // i
    (0x0D, 36), // j
    (0x0E, 37), // k
    (0x0F, 38), // l
    (0x10, 50), // m
    (0x11, 49), // n
    (0x12, 24), // o
    (0x13, 25), // p
    (0x14, 16), // q
    (0x15, 19), // r
    (0x16, 31), // s
    (0x17, 20), // t
    (0x18, 22), // u
    (0x19, 47), // v
    (0x1A, 17), // w
    (0x1B, 45), // x
    (0x1C, 21), // y
    (0x1D, 44), // z
    // Digits and punctuation.
    (0x1E, 2),  // 1
    (0x1F, 3),  // 2
    (0x20, 4),  // 3
    (0x21, 5),  // 4
    (0x22, 6),  // 5
    (0x23, 7),  // 6
    (0x24, 8),  // 7
    (0x25, 9),  // 8
    (0x26, 10), // 9
    (0x27, 11), // 0
    (0x28, 28), // Enter
    (0x29, 1),  // Escape
    (0x2A, 14), // Backspace
    (0x2B, 15), // Tab
    (0x2C, 57), // Space
    (0x2D, 12), // -
    (0x2E, 13), // =
    (0x2F, 26), // [
    (0x30, 27), // ]
    (0x31, 43), // backslash
    (0x32, 43), // Non-US # and ~ (ISO position; KEY_BACKSLASH)
    (0x33, 39), // ;
    (0x34, 40), // '
    (0x35, 41), // `
    (0x36, 51), // ,
    (0x37, 52), // .
    (0x38, 53), // /
    (0x39, 58), // CapsLock
    // Function keys F1–F12.
    (0x3A, 59),
    (0x3B, 60),
    (0x3C, 61),
    (0x3D, 62),
    (0x3E, 63),
    (0x3F, 64),
    (0x40, 65),
    (0x41, 66),
    (0x42, 67),
    (0x43, 68),
    (0x44, 87),
    (0x45, 88),
    // System / navigation.
    (0x46, 99),  // PrintScreen (KEY_SYSRQ)
    (0x47, 70),  // ScrollLock
    (0x48, 119), // Pause
    (0x49, 110), // Insert
    (0x4A, 102), // Home
    (0x4B, 104), // PageUp
    (0x4C, 111), // Delete
    (0x4D, 107), // End
    (0x4E, 109), // PageDown
    (0x4F, 106), // Right
    (0x50, 105), // Left
    (0x51, 108), // Down
    (0x52, 103), // Up
    (0x53, 69),  // NumLock
    // Keypad.
    (0x54, 98), // /
    (0x55, 55), // *
    (0x56, 74), // -
    (0x57, 78), // +
    (0x58, 96), // Enter
    (0x59, 79), // 1
    (0x5A, 80), // 2
    (0x5B, 81), // 3
    (0x5C, 75), // 4
    (0x5D, 76), // 5
    (0x5E, 77), // 6
    (0x5F, 71), // 7
    (0x60, 72), // 8
    (0x61, 73), // 9
    (0x62, 82), // 0
    (0x63, 83), // .
    // Non-US backslash, Application/Menu.
    (0x64, 86),  // KEY_102ND
    (0x65, 127), // KEY_COMPOSE
    // Function keys F13–F24.
    (0x68, 183),
    (0x69, 184),
    (0x6A, 185),
    (0x6B, 186),
    (0x6C, 187),
    (0x6D, 188),
    (0x6E, 189),
    (0x6F, 190),
    (0x70, 191),
    (0x71, 192),
    (0x72, 193),
    (0x73, 194),
    // Media keys from the keyboard page.
    (0x7F, 113), // Mute
    (0x80, 115), // Volume up
    (0x81, 114), // Volume down
    (0xB0, 164), // Play/Pause
    (0xB1, 119), // Pause (KEY_PAUSE); 0x48 is canonical in reverse
    (0xB2, 167), // Record
    (0xB3, 208), // Fast forward
    (0xB4, 168), // Rewind
    (0xB5, 163), // Next track
    (0xB6, 165), // Previous track
    (0xB7, 166), // Stop
    (0xB8, 161), // Eject
    // Modifiers.
    (0xE0, 29),  // Left Ctrl
    (0xE1, 42),  // Left Shift
    (0xE2, 56),  // Left Alt
    (0xE3, 125), // Left Meta
    (0xE4, 97),  // Right Ctrl
    (0xE5, 54),  // Right Shift
    (0xE6, 100), // Right Alt
    (0xE7, 126), // Right Meta
];

/// Map a USB HID keyboard usage id (page 0x07) to a Linux evdev keycode.
///
/// Returns `None` for reserved usages, consumer/media usages outside the
/// keyboard page, and anything this sink does not register. Modifier usages
/// `0xE0..=0xE7` are included and map to the left/right control, shift, alt,
/// and meta keys.
pub const fn hid_usage_to_linux_keycode(usage: u8) -> Option<u16> {
    let mut i = 0;
    while i < KEYMAP.len() {
        let (candidate, keycode) = KEYMAP[i];
        if candidate == usage {
            return Some(keycode);
        }
        i += 1;
    }
    None
}

/// Map a Linux evdev keycode back to a USB HID keyboard usage id.
///
/// This is the inverse of [`hid_usage_to_linux_keycode`] and uses the same
/// table. If several HID usages share a Linux keycode, the first entry in
/// [`KEYMAP`] is returned: the primary (usually lower-numbered) key wins over
/// a secondary one. `KEY_PAUSE` (119) therefore maps back to `0x48` (Pause),
/// not `0xB1` (media Pause); right-hand modifiers keep distinct keycodes and
/// round-trip individually.
///
/// Returns `None` for keycodes outside the table (including keycodes whose
/// HID usage this crate does not register).
pub const fn linux_keycode_to_hid_usage(keycode: u16) -> Option<u8> {
    let mut i = 0;
    while i < KEYMAP.len() {
        let (usage, candidate) = KEYMAP[i];
        if candidate == keycode {
            return Some(usage);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HID usages whose Linux keycode is intentionally shared with an earlier
    /// table entry, and the canonical usage the reverse map returns for them.
    ///
    /// Keep this in sync with [`KEYMAP`]; `only_known_shared_keycodes_exist`
    /// fails if a new duplicate appears without being documented here.
    const SHARED_LINUX_CODES: &[(u8, u8)] = &[
        // 0x48 (Pause) and 0xB1 (media Pause) both use KEY_PAUSE (119).
        // 0x48 is the primary keyboard Pause and wins the reverse lookup.
        (0xB1, 0x48),
        // 0x31 (US \|) and 0x32 (ISO #~ position) both map to
        // KEY_BACKSLASH (43); 0x31 is the canonical reverse mapping.
        (0x32, 0x31),
    ];

    #[test]
    fn hid_roundtrip_is_identity_except_documented_duplicates() {
        for &(usage, keycode) in KEYMAP {
            let back = linux_keycode_to_hid_usage(keycode).unwrap_or_else(|| {
                panic!("HID usage {usage:#04x} -> keycode {keycode} did not reverse-map")
            });
            let expected = SHARED_LINUX_CODES
                .iter()
                .find(|&&(duplicate, _)| duplicate == usage)
                .map_or(usage, |&(_, canonical)| canonical);
            assert_eq!(
                back, expected,
                "HID usage {usage:#04x} -> keycode {keycode} -> HID usage {back:#04x}"
            );
        }
    }

    #[test]
    fn every_reverse_entry_maps_back_to_the_same_keycode() {
        for &(_, keycode) in KEYMAP {
            let usage = linux_keycode_to_hid_usage(keycode)
                .unwrap_or_else(|| panic!("keycode {keycode} is not reverse-mapped"));
            assert_eq!(
                hid_usage_to_linux_keycode(usage),
                Some(keycode),
                "keycode {keycode} -> HID usage {usage:#04x} did not map back"
            );
        }
    }

    #[test]
    fn only_known_shared_keycodes_exist() {
        let mut shared = Vec::new();
        for (index, &(usage, keycode)) in KEYMAP.iter().enumerate() {
            for &(earlier_usage, earlier_keycode) in &KEYMAP[..index] {
                if earlier_keycode == keycode {
                    shared.push((earlier_keycode, earlier_usage, usage));
                }
            }
        }
        assert_eq!(
            shared,
            vec![(43, 0x31, 0x32), (119, 0x48, 0xB1)],
            "unexpected shared Linux keycode; document it in SHARED_LINUX_CODES"
        );
    }

    #[test]
    fn table_is_sorted_by_hid_usage() {
        for pair in KEYMAP.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "KEYMAP order is significant for shared codes: {:#04x} then {:#04x}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn reverse_maps_letters_and_digits() {
        assert_eq!(linux_keycode_to_hid_usage(30), Some(0x04)); // a
        assert_eq!(linux_keycode_to_hid_usage(44), Some(0x1D)); // z
        assert_eq!(linux_keycode_to_hid_usage(2), Some(0x1E)); // 1
        assert_eq!(linux_keycode_to_hid_usage(11), Some(0x27)); // 0
        assert_eq!(linux_keycode_to_hid_usage(57), Some(0x2C)); // Space
        assert_eq!(linux_keycode_to_hid_usage(28), Some(0x28)); // Enter
        assert_eq!(linux_keycode_to_hid_usage(1), Some(0x29)); // Escape
    }

    #[test]
    fn reverse_maps_left_and_right_modifiers() {
        assert_eq!(linux_keycode_to_hid_usage(29), Some(0xE0)); // Left Ctrl
        assert_eq!(linux_keycode_to_hid_usage(42), Some(0xE1)); // Left Shift
        assert_eq!(linux_keycode_to_hid_usage(56), Some(0xE2)); // Left Alt
        assert_eq!(linux_keycode_to_hid_usage(125), Some(0xE3)); // Left Meta
        assert_eq!(linux_keycode_to_hid_usage(97), Some(0xE4)); // Right Ctrl
        assert_eq!(linux_keycode_to_hid_usage(54), Some(0xE5)); // Right Shift
        assert_eq!(linux_keycode_to_hid_usage(100), Some(0xE6)); // Right Alt
        assert_eq!(linux_keycode_to_hid_usage(126), Some(0xE7)); // Right Meta
    }

    #[test]
    fn reverse_maps_function_keys() {
        assert_eq!(linux_keycode_to_hid_usage(59), Some(0x3A)); // F1
        assert_eq!(linux_keycode_to_hid_usage(88), Some(0x45)); // F12
        assert_eq!(linux_keycode_to_hid_usage(183), Some(0x68)); // F13
        assert_eq!(linux_keycode_to_hid_usage(194), Some(0x73)); // F24
    }

    #[test]
    fn reverse_maps_arrows_keypad_and_media() {
        assert_eq!(linux_keycode_to_hid_usage(103), Some(0x52)); // Up
        assert_eq!(linux_keycode_to_hid_usage(108), Some(0x51)); // Down
        assert_eq!(linux_keycode_to_hid_usage(105), Some(0x50)); // Left
        assert_eq!(linux_keycode_to_hid_usage(106), Some(0x4F)); // Right

        assert_eq!(linux_keycode_to_hid_usage(98), Some(0x54)); // KP /
        assert_eq!(linux_keycode_to_hid_usage(96), Some(0x58)); // KP Enter
        assert_eq!(linux_keycode_to_hid_usage(82), Some(0x62)); // KP 0
        assert_eq!(linux_keycode_to_hid_usage(83), Some(0x63)); // KP .

        assert_eq!(linux_keycode_to_hid_usage(113), Some(0x7F)); // Mute
        assert_eq!(linux_keycode_to_hid_usage(115), Some(0x80)); // Volume up
        assert_eq!(linux_keycode_to_hid_usage(114), Some(0x81)); // Volume down
        assert_eq!(linux_keycode_to_hid_usage(163), Some(0xB5)); // Next track
        assert_eq!(linux_keycode_to_hid_usage(164), Some(0xB0)); // Play/Pause
    }

    #[test]
    fn pause_resolves_to_the_primary_keyboard_usage() {
        assert_eq!(linux_keycode_to_hid_usage(119), Some(0x48));
        assert_eq!(hid_usage_to_linux_keycode(0x48), Some(119));
        assert_eq!(hid_usage_to_linux_keycode(0xB1), Some(119));
    }

    #[test]
    fn unknown_and_out_of_range_keycodes_are_rejected() {
        assert_eq!(linux_keycode_to_hid_usage(0), None); // KEY_RESERVED
        assert_eq!(linux_keycode_to_hid_usage(255), None);
        assert_eq!(linux_keycode_to_hid_usage(256), None);
        assert_eq!(linux_keycode_to_hid_usage(u16::MAX), None);
        // Pointer button codes live in a different range and are not keys.
        assert_eq!(linux_keycode_to_hid_usage(0x110), None); // BTN_LEFT
    }
}
