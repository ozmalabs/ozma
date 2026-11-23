// SPDX-License-Identifier: AGPL-3.0-only
//! Browser input mapping.
//!
//! The wire protocol identifies keys by USB HID usage id (page 0x07), never by
//! a JavaScript `KeyboardEvent.keyCode` or `key` string. This module maps the
//! layout-independent `KeyboardEvent.code` values a browser reports to their
//! HID usages, and builds the protocol's [`InputEvent`]s.
//!
//! Everything here is a pure function so it compiles and is tested on the host
//! target, not only inside wasm.

use ozma_protocol::{InputEvent, KeyModifiers, PointerButton};

/// USB HID keyboard page.
pub const HID_KEYBOARD_PAGE: u8 = 0x07;

/// Modifier bit for a modifier usage id, if it is one.
///
/// The low byte matches the USB HID boot keyboard modifier report, so a browser
/// can also compute the bits from `ctrlKey`/`shiftKey`/`altKey`/`metaKey` via
/// [`modifiers_from_flags`].
pub fn modifier_bit(hid_usage: u8) -> Option<u8> {
    match hid_usage {
        0xE0 => Some(KeyModifiers::LEFT_CTRL),
        0xE1 => Some(KeyModifiers::LEFT_SHIFT),
        0xE2 => Some(KeyModifiers::LEFT_ALT),
        0xE3 => Some(KeyModifiers::LEFT_GUI),
        0xE4 => Some(KeyModifiers::RIGHT_CTRL),
        0xE5 => Some(KeyModifiers::RIGHT_SHIFT),
        0xE6 => Some(KeyModifiers::RIGHT_ALT),
        0xE7 => Some(KeyModifiers::RIGHT_GUI),
        _ => None,
    }
}

/// Build the HID modifier byte from the four boolean flags exposed by browser
/// pointer and keyboard events.
///
/// Browsers do not distinguish left and right modifiers in those flags, so the
/// left-hand bits are used. A modifier key's own HID usage still identifies the
/// physical side.
pub fn modifiers_from_flags(ctrl: bool, shift: bool, alt: bool, meta: bool) -> KeyModifiers {
    let mut bits = 0u8;
    if ctrl {
        bits |= KeyModifiers::LEFT_CTRL;
    }
    if shift {
        bits |= KeyModifiers::LEFT_SHIFT;
    }
    if alt {
        bits |= KeyModifiers::LEFT_ALT;
    }
    if meta {
        bits |= KeyModifiers::LEFT_GUI;
    }
    KeyModifiers::new(bits)
}

/// Map a `KeyboardEvent.code` value to a USB HID keyboard usage id.
///
/// Returns `None` for keys the protocol does not have a stable usage for. The
/// table covers letters, digits, arrows, modifiers, Enter, Escape, Tab,
/// Backspace, Space, F1–F12, the punctuation/editing block, and the numeric
/// keypad. It is deliberately independent of the user's keyboard layout:
/// `code` names a physical position, which is what the remote compositor
/// expects.
pub fn js_code_to_hid(code: &str) -> Option<u8> {
    Some(match code {
        // Letters: KeyA = 0x04 … KeyZ = 0x1D.
        "KeyA" => 0x04,
        "KeyB" => 0x05,
        "KeyC" => 0x06,
        "KeyD" => 0x07,
        "KeyE" => 0x08,
        "KeyF" => 0x09,
        "KeyG" => 0x0A,
        "KeyH" => 0x0B,
        "KeyI" => 0x0C,
        "KeyJ" => 0x0D,
        "KeyK" => 0x0E,
        "KeyL" => 0x0F,
        "KeyM" => 0x10,
        "KeyN" => 0x11,
        "KeyO" => 0x12,
        "KeyP" => 0x13,
        "KeyQ" => 0x14,
        "KeyR" => 0x15,
        "KeyS" => 0x16,
        "KeyT" => 0x17,
        "KeyU" => 0x18,
        "KeyV" => 0x19,
        "KeyW" => 0x1A,
        "KeyX" => 0x1B,
        "KeyY" => 0x1C,
        "KeyZ" => 0x1D,

        // Digits: Digit1 = 0x1E … Digit9 = 0x26, Digit0 = 0x27.
        "Digit1" => 0x1E,
        "Digit2" => 0x1F,
        "Digit3" => 0x20,
        "Digit4" => 0x21,
        "Digit5" => 0x22,
        "Digit6" => 0x23,
        "Digit7" => 0x24,
        "Digit8" => 0x25,
        "Digit9" => 0x26,
        "Digit0" => 0x27,

        "Enter" => 0x28,
        "Escape" => 0x29,
        "Backspace" => 0x2A,
        "Tab" => 0x2B,
        "Space" => 0x2C,
        "Minus" => 0x2D,
        "Equal" => 0x2E,
        "BracketLeft" => 0x2F,
        "BracketRight" => 0x30,
        "Backslash" => 0x31,
        // 0x32 is the non-US hash key; browsers report it as IntlBackslash or
        // IntlHash depending on platform.
        "IntlBackslash" | "IntlHash" => 0x32,
        "Semicolon" => 0x33,
        "Quote" => 0x34,
        "Backquote" => 0x35,
        "Comma" => 0x36,
        "Period" => 0x37,
        "Slash" => 0x38,
        "CapsLock" => 0x39,

        "F1" => 0x3A,
        "F2" => 0x3B,
        "F3" => 0x3C,
        "F4" => 0x3D,
        "F5" => 0x3E,
        "F6" => 0x3F,
        "F7" => 0x40,
        "F8" => 0x41,
        "F9" => 0x42,
        "F10" => 0x43,
        "F11" => 0x44,
        "F12" => 0x45,

        "PrintScreen" => 0x46,
        "ScrollLock" => 0x47,
        "Pause" => 0x48,
        "Insert" => 0x49,
        "Home" => 0x4A,
        "PageUp" => 0x4B,
        "Delete" => 0x4C,
        "End" => 0x4D,
        "PageDown" => 0x4E,
        "ArrowRight" => 0x4F,
        "ArrowLeft" => 0x50,
        "ArrowDown" => 0x51,
        "ArrowUp" => 0x52,
        "NumLock" => 0x53,
        "NumpadDivide" => 0x54,
        "NumpadMultiply" => 0x55,
        "NumpadSubtract" => 0x56,
        "NumpadAdd" => 0x57,
        "NumpadEnter" => 0x58,
        "Numpad1" => 0x59,
        "Numpad2" => 0x5A,
        "Numpad3" => 0x5B,
        "Numpad4" => 0x5C,
        "Numpad5" => 0x5D,
        "Numpad6" => 0x5E,
        "Numpad7" => 0x5F,
        "Numpad8" => 0x60,
        "Numpad9" => 0x61,
        "Numpad0" => 0x62,
        "NumpadDecimal" => 0x63,
        // 0x64 is the non-US backslash key on ISO keypads.
        "NumpadComma" => 0x85,
        "ContextMenu" => 0x65,

        // Modifiers.
        "ControlLeft" => 0xE0,
        "ShiftLeft" => 0xE1,
        "AltLeft" => 0xE2,
        "MetaLeft" | "OSLeft" => 0xE3,
        "ControlRight" => 0xE4,
        "ShiftRight" => 0xE5,
        "AltRight" => 0xE6,
        "MetaRight" | "OSRight" => 0xE7,

        _ => return None,
    })
}

/// Map a DOM `MouseEvent.button` index to the protocol's pointer button.
///
/// The browser order is left, middle, right, back, forward. Unknown indices
/// (for example a pen eraser) are ignored rather than guessed.
pub fn pointer_button(index: u8) -> Option<PointerButton> {
    Some(match index {
        0 => PointerButton::Left,
        1 => PointerButton::Middle,
        2 => PointerButton::Right,
        3 => PointerButton::Back,
        4 => PointerButton::Forward,
        _ => return None,
    })
}

/// Absolute pointer motion in display-local coordinates.
pub fn pointer_absolute(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerAbsolute {
        display_id: None,
        x,
        y,
    }
}

/// One mouse button transition.
pub fn button_event(button: u8, down: bool) -> Option<InputEvent> {
    pointer_button(button).map(|button| InputEvent::Button { button, down })
}

/// Wheel scrolling. Browsers report pixel deltas; the caller supplies the
/// already-scaled wheel units the node expects.
pub fn wheel_event(dx: i32, dy: i32) -> InputEvent {
    InputEvent::Wheel { dx, dy }
}

/// One keyboard transition using a HID usage id (page 0x07).
pub fn key_event(hid_usage: u32, down: bool, modifiers: u8) -> InputEvent {
    InputEvent::Key {
        code: hid_usage,
        down,
        modifiers: KeyModifiers::new(modifiers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::input::TouchPhase;

    #[test]
    fn letters_and_digits_use_the_boot_protocol_usages() {
        assert_eq!(js_code_to_hid("KeyA"), Some(0x04));
        assert_eq!(js_code_to_hid("KeyZ"), Some(0x1D));
        assert_eq!(js_code_to_hid("Digit1"), Some(0x1E));
        assert_eq!(js_code_to_hid("Digit0"), Some(0x27));
    }

    #[test]
    fn editing_and_navigation_keys_are_mapped() {
        assert_eq!(js_code_to_hid("Enter"), Some(0x28));
        assert_eq!(js_code_to_hid("Escape"), Some(0x29));
        assert_eq!(js_code_to_hid("Backspace"), Some(0x2A));
        assert_eq!(js_code_to_hid("Tab"), Some(0x2B));
        assert_eq!(js_code_to_hid("Space"), Some(0x2C));
        assert_eq!(js_code_to_hid("ArrowLeft"), Some(0x50));
        assert_eq!(js_code_to_hid("ArrowUp"), Some(0x52));
    }

    #[test]
    fn function_keys_are_contiguous() {
        for (index, name) in [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                js_code_to_hid(name),
                Some(0x3A + index as u8),
                "unexpected usage for {name}"
            );
        }
    }

    #[test]
    fn modifiers_map_to_hid_bits() {
        assert_eq!(js_code_to_hid("ControlLeft"), Some(0xE0));
        assert_eq!(js_code_to_hid("ShiftRight"), Some(0xE5));
        assert_eq!(js_code_to_hid("MetaLeft"), Some(0xE3));
        assert_eq!(modifier_bit(0xE1), Some(KeyModifiers::LEFT_SHIFT));
        assert_eq!(modifier_bit(0x04), None);

        let modifiers = modifiers_from_flags(true, true, false, false);
        assert_eq!(
            modifiers.0,
            KeyModifiers::LEFT_CTRL | KeyModifiers::LEFT_SHIFT
        );
    }

    #[test]
    fn unknown_codes_are_not_guessed() {
        assert_eq!(js_code_to_hid("Squirrel"), None);
        assert_eq!(js_code_to_hid(""), None);
        assert_eq!(js_code_to_hid("keya"), None);
    }

    #[test]
    fn pointer_and_button_events_use_protocol_types() {
        assert_eq!(
            pointer_absolute(10, 20),
            InputEvent::PointerAbsolute {
                display_id: None,
                x: 10,
                y: 20,
            }
        );
        assert_eq!(
            button_event(0, true),
            Some(InputEvent::Button {
                button: PointerButton::Left,
                down: true,
            })
        );
        assert_eq!(button_event(9, true), None);
        assert_eq!(wheel_event(0, -120), InputEvent::Wheel { dx: 0, dy: -120 });
    }

    #[test]
    fn key_events_carry_the_modifier_byte() {
        let event = key_event(0x04, true, KeyModifiers::LEFT_SHIFT);
        assert_eq!(
            event,
            InputEvent::Key {
                code: 0x04,
                down: true,
                modifiers: KeyModifiers::new(KeyModifiers::LEFT_SHIFT),
            }
        );
        // TouchPhase is re-exported by the protocol; a compile-time use keeps
        // this test honest if the input vocabulary moves.
        let _ = TouchPhase::Down;
    }
}
