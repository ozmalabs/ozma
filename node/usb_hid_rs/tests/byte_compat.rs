//! Byte-compatibility tests: verify that the Rust encoder produces exactly the
//! same byte sequences as the Python USBHIDGadget helper functions in
//! node/usb_hid.py for the same inputs.
//!
//! Reference (Python):
//!   keyboard_report(modifiers, keys) → bytes([modifiers, 0x00] + slots[:6])
//!   mouse_report(buttons, x, y, scroll) →
//!       bytes([buttons & 0xFF,
//!              x & 0xFF, (x >> 8) & 0xFF,
//!              y & 0xFF, (y >> 8) & 0xFF,
//!              scroll & 0xFF])

use ozma_usb_hid::report::{KeyboardReport, MouseReport};

// ── keyboard_report() parity ──────────────────────────────────────────────────

#[test]
fn kbd_parity_no_keys() {
    // Python: keyboard_report(0) → b'\x00\x00\x00\x00\x00\x00\x00\x00'
    let r = KeyboardReport { modifiers: 0, keys: vec![] };
    assert_eq!(r.to_bytes(), [0x00; 8]);
}

#[test]
fn kbd_parity_left_shift() {
    // Python: keyboard_report(0x02) → b'\x02\x00\x00\x00\x00\x00\x00\x00'
    let r = KeyboardReport { modifiers: 0x02, keys: vec![] };
    assert_eq!(r.to_bytes(), [0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn kbd_parity_ctrl_a() {
    // Python: keyboard_report(0x01, [0x04])
    //   → b'\x01\x00\x04\x00\x00\x00\x00\x00'
    let r = KeyboardReport { modifiers: 0x01, keys: vec![0x04] };
    assert_eq!(r.to_bytes(), [0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn kbd_parity_six_keys() {
    // Python: keyboard_report(0, [4,5,6,7,8,9])
    //   → b'\x00\x00\x04\x05\x06\x07\x08\x09'
    let r = KeyboardReport { modifiers: 0, keys: vec![4, 5, 6, 7, 8, 9] };
    assert_eq!(r.to_bytes(), [0x00, 0x00, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn kbd_parity_overflow_truncated() {
    // Python slots[:6] truncates; key 10 and 11 are dropped.
    let r = KeyboardReport { modifiers: 0, keys: vec![4, 5, 6, 7, 8, 9, 10, 11] };
    assert_eq!(r.to_bytes(), [0x00, 0x00, 4, 5, 6, 7, 8, 9]);
}

// ── mouse_report() parity ─────────────────────────────────────────────────────

#[test]
fn mouse_parity_zero() {
    // Python: mouse_report(0, 0, 0, 0) → b'\x00\x00\x00\x00\x00\x00'
    let r = MouseReport { buttons: 0, x: 0, y: 0, scroll: 0 };
    assert_eq!(r.to_bytes(), [0x00; 6]);
}

#[test]
fn mouse_parity_centre() {
    // Python: mouse_report(0, 16383, 16383, 0)
    //   x=0x3FFF → lo=0xFF hi=0x3F
    let r = MouseReport { buttons: 0, x: 16383, y: 16383, scroll: 0 };
    assert_eq!(r.to_bytes(), [0x00, 0xFF, 0x3F, 0xFF, 0x3F, 0x00]);
}

#[test]
fn mouse_parity_max() {
    // Python: mouse_report(0, 0x7FFF, 0x7FFF, 0)
    //   x=0x7FFF → lo=0xFF hi=0x7F
    let r = MouseReport { buttons: 0, x: 0x7FFF, y: 0x7FFF, scroll: 0 };
    assert_eq!(r.to_bytes(), [0x00, 0xFF, 0x7F, 0xFF, 0x7F, 0x00]);
}

#[test]
fn mouse_parity_clamp_above_max() {
    // Python: x = max(0, min(0x7FFF, 0x8000)) → 0x7FFF
    let r = MouseReport { buttons: 0, x: 0x8000, y: 0x8000, scroll: 0 };
    assert_eq!(r.to_bytes(), [0x00, 0xFF, 0x7F, 0xFF, 0x7F, 0x00]);
}

#[test]
fn mouse_parity_scroll_negative() {
    // Python: scroll & 0xFF for -1 → 0xFF
    let r = MouseReport { buttons: 0, x: 0, y: 0, scroll: -1 };
    assert_eq!(r.to_bytes()[5], 0xFF);
}

#[test]
fn mouse_parity_scroll_min() {
    // Python: -127 & 0xFF → 0x81
    let r = MouseReport { buttons: 0, x: 0, y: 0, scroll: -127 };
    assert_eq!(r.to_bytes()[5], 0x81);
}

#[test]
fn mouse_parity_all_buttons() {
    // Python: buttons & 0xFF = 0x07
    let r = MouseReport { buttons: 0x07, x: 0, y: 0, scroll: 0 };
    assert_eq!(r.to_bytes()[0], 0x07);
}

#[test]
fn mouse_parity_full_frame() {
    // Python: mouse_report(1, 100, 200, 3)
    //   x=100=0x0064 → lo=0x64 hi=0x00
    //   y=200=0x00C8 → lo=0xC8 hi=0x00
    let r = MouseReport { buttons: 1, x: 100, y: 200, scroll: 3 };
    assert_eq!(r.to_bytes(), [0x01, 0x64, 0x00, 0xC8, 0x00, 0x03]);
}
