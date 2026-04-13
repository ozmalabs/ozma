//! Cross-platform HID injector.
//!
//! Wraps `enigo` to provide:
//!   - `inject_keyboard_report(&KeyboardReport)` — inject a raw HID boot report
//!   - `inject_mouse_report(&MouseReport)`       — inject a raw HID mouse report
//!   - `inject_text(text, layout, rate)`         — paste-as-typing
//!
//! `build_injector()` returns the right backend for the current platform.

use enigo::{
    Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings,
};
use thiserror::Error;

use crate::hid_report::{KeyboardReport, MouseReport};
use crate::layout::{keystroke_for, Layout};

/// Errors from the injector.
#[derive(Debug, Error)]
pub enum InjectorError {
    #[error("enigo init failed: {0}")]
    Init(String),
    #[error("enigo key event failed: {0}")]
    Key(String),
    #[error("enigo mouse event failed: {0}")]
    Mouse(String),
}

/// Cross-platform HID injector backed by `enigo`.
pub struct HidInjector {
    enigo: Enigo,
    /// Keys currently held down (for proper release on next report).
    held_keys: Vec<Key>,
}

impl HidInjector {
    fn new() -> Result<Self, InjectorError> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| InjectorError::Init(e.to_string()))?;
        Ok(Self { enigo, held_keys: Vec::new() })
    }

    // ── Keyboard ─────────────────────────────────────────────────────────────

    /// Inject a raw 8-byte HID boot keyboard report.
    ///
    /// Releases keys no longer present, presses newly active keys.
    /// Mirrors `HIDInjectorLinux::inject_keyboard` / `HIDInjectorWindows::inject_keyboard`.
    pub fn inject_keyboard_report(&mut self, report: &KeyboardReport) -> Result<(), InjectorError> {
        let active: Vec<Key> = report
            .active_keys()
            .filter_map(|hid| hid_usage_to_enigo_key(hid))
            .collect();

        // Modifier keys from the modifier byte
        let mods = modifier_keys(report.modifier);

        // Release previously held keys that are no longer active
        let to_release: Vec<Key> = self
            .held_keys
            .iter()
            .filter(|k| !active.contains(k) && !mods.contains(k))
            .cloned()
            .collect();
        for key in &to_release {
            self.enigo
                .key(*key, Direction::Release)
                .map_err(|e| InjectorError::Key(e.to_string()))?;
        }
        self.held_keys.retain(|k| !to_release.contains(k));

        // Press modifier keys
        for &m in &mods {
            if !self.held_keys.contains(&m) {
                self.enigo
                    .key(m, Direction::Press)
                    .map_err(|e| InjectorError::Key(e.to_string()))?;
                self.held_keys.push(m);
            }
        }

        // Press newly active keys
        for key in &active {
            if !self.held_keys.contains(key) {
                self.enigo
                    .key(*key, Direction::Press)
                    .map_err(|e| InjectorError::Key(e.to_string()))?;
                self.held_keys.push(*key);
            }
        }

        // If no keys active, release everything
        if active.is_empty() && mods.is_empty() {
            let all: Vec<Key> = self.held_keys.drain(..).collect();
            for key in all {
                let _ = self.enigo.key(key, Direction::Release);
            }
        }

        Ok(())
    }

    /// Press and immediately release a single key (with optional shift/altgr).
    pub fn tap_key(&mut self, key: Key, shift: bool, altgr: bool) -> Result<(), InjectorError> {
        if altgr {
            self.enigo.key(Key::Alt, Direction::Press)
                .map_err(|e| InjectorError::Key(e.to_string()))?;
        }
        if shift {
            self.enigo.key(Key::LShift, Direction::Press)
                .map_err(|e| InjectorError::Key(e.to_string()))?;
        }
        self.enigo.key(key, Direction::Click)
            .map_err(|e| InjectorError::Key(e.to_string()))?;
        if shift {
            self.enigo.key(Key::LShift, Direction::Release)
                .map_err(|e| InjectorError::Key(e.to_string()))?;
        }
        if altgr {
            self.enigo.key(Key::Alt, Direction::Release)
                .map_err(|e| InjectorError::Key(e.to_string()))?;
        }
        Ok(())
    }

    // ── Mouse ─────────────────────────────────────────────────────────────────

    /// Inject a raw 6-byte HID mouse report.
    ///
    /// Mirrors `HIDInjectorLinux::inject_mouse` / `HIDInjectorWindows::inject_mouse`.
    pub fn inject_mouse_report(&mut self, report: &MouseReport) -> Result<(), InjectorError> {
        // Move to absolute position (enigo uses Abs coordinate space)
        self.enigo
            .move_mouse(report.x as i32, report.y as i32, Coordinate::Abs)
            .map_err(|e| InjectorError::Mouse(e.to_string()))?;

        // Buttons
        let btn_map = [
            (0x01, Button::Left),
            (0x02, Button::Right),
            (0x04, Button::Middle),
        ];
        for (bit, btn) in btn_map {
            let dir = if report.buttons & bit != 0 {
                Direction::Press
            } else {
                Direction::Release
            };
            self.enigo
                .button(btn, dir)
                .map_err(|e| InjectorError::Mouse(e.to_string()))?;
        }

        // Scroll
        if report.scroll != 0 {
            self.enigo
                .scroll(report.scroll as i32, enigo::Axis::Vertical)
                .map_err(|e| InjectorError::Mouse(e.to_string()))?;
        }

        Ok(())
    }

    // ── Paste-as-typing ───────────────────────────────────────────────────────

    /// Type `text` character-by-character at `rate` chars/sec using `layout`.
    ///
    /// Mirrors `PasteTyper.type_text()` in `controller/paste_typing.py`.
    /// Rate is clamped to 5-100 chars/sec.
    pub async fn inject_text(
        &mut self,
        text: &str,
        layout: Layout,
        rate: f64,
    ) -> Result<PasteResult, InjectorError> {
        let rate = rate.clamp(5.0, 100.0);
        let delay = std::time::Duration::from_secs_f64(1.0 / rate);

        let mut chars_sent = 0u32;
        let mut chars_skipped = 0u32;

        for ch in text.chars() {
            match keystroke_for(ch, layout) {
                None => {
                    chars_skipped += 1;
                }
                Some(ks) => {
                    self.tap_key(ks.key, ks.shift, ks.altgr)?;
                    chars_sent += 1;
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Ok(PasteResult { chars_sent, chars_skipped })
    }
}

/// Result of a paste-as-typing operation.
#[derive(Debug, Clone)]
pub struct PasteResult {
    pub chars_sent: u32,
    pub chars_skipped: u32,
}

/// Build the platform-appropriate injector.
pub fn build_injector() -> Result<HidInjector, InjectorError> {
    HidInjector::new()
}

// ── HID usage ID → enigo Key ─────────────────────────────────────────────────

/// Map a HID keyboard usage ID to an enigo `Key`.
///
/// Covers the same range as `HID_KEYS` in `paste_typing.py` plus
/// the arrow/navigation keys used in `_build_hid_to_evdev_map()`.
pub fn hid_usage_to_enigo_key(hid: u8) -> Option<Key> {
    use Key::*;
    Some(match hid {
        // a-z: HID 0x04-0x1D
        0x04..=0x1D => Unicode((b'a' + (hid - 0x04)) as char),
        // 1-9: HID 0x1E-0x26
        0x1E..=0x26 => Unicode((b'1' + (hid - 0x1E)) as char),
        // 0: HID 0x27
        0x27 => Unicode('0'),
        // Control keys
        0x28 => Return,
        0x29 => Escape,
        0x2A => Backspace,
        0x2B => Tab,
        0x2C => Space,
        // Punctuation (pass through as Unicode — enigo maps these on all platforms)
        0x2D => Unicode('-'),
        0x2E => Unicode('='),
        0x2F => Unicode('['),
        0x30 => Unicode(']'),
        0x31 => Unicode('\\'),
        0x33 => Unicode(';'),
        0x34 => Unicode('\''),
        0x35 => Unicode('`'),
        0x36 => Unicode(','),
        0x37 => Unicode('.'),
        0x38 => Unicode('/'),
        // F-keys
        0x3A => F1,  0x3B => F2,  0x3C => F3,  0x3D => F4,
        0x3E => F5,  0x3F => F6,  0x40 => F7,  0x41 => F8,
        0x42 => F9,  0x43 => F10, 0x44 => F11, 0x45 => F12,
        // Navigation
        0x49 => Insert,
        0x4A => Home,
        0x4B => PageUp,
        0x4C => Delete,
        0x4D => End,
        0x4E => PageDown,
        0x4F => RightArrow,
        0x50 => LeftArrow,
        0x51 => DownArrow,
        0x52 => UpArrow,
        _ => return None,
    })
}

/// Expand the HID modifier byte into a list of enigo modifier keys.
fn modifier_keys(modifier: u8) -> Vec<Key> {
    let mut keys = Vec::new();
    if modifier & 0x01 != 0 { keys.push(Key::LControl); }
    if modifier & 0x02 != 0 { keys.push(Key::LShift); }
    if modifier & 0x04 != 0 { keys.push(Key::Alt); }
    if modifier & 0x08 != 0 { keys.push(Key::Meta); }
    if modifier & 0x10 != 0 { keys.push(Key::RControl); }
    if modifier & 0x20 != 0 { keys.push(Key::RShift); }
    if modifier & 0x40 != 0 { keys.push(Key::Alt); }  // AltGr
    if modifier & 0x80 != 0 { keys.push(Key::Meta); }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_letter_mapping() {
        assert_eq!(hid_usage_to_enigo_key(0x04), Some(Key::Unicode('a')));
        assert_eq!(hid_usage_to_enigo_key(0x1D), Some(Key::Unicode('z')));
    }

    #[test]
    fn hid_digit_mapping() {
        assert_eq!(hid_usage_to_enigo_key(0x1E), Some(Key::Unicode('1')));
        assert_eq!(hid_usage_to_enigo_key(0x27), Some(Key::Unicode('0')));
    }

    #[test]
    fn hid_fkey_mapping() {
        assert_eq!(hid_usage_to_enigo_key(0x3A), Some(Key::F1));
        assert_eq!(hid_usage_to_enigo_key(0x45), Some(Key::F12));
    }

    #[test]
    fn modifier_byte_expansion() {
        let mods = modifier_keys(0x02 | 0x40); // LShift + AltGr
        assert!(mods.contains(&Key::LShift));
        assert!(mods.contains(&Key::Alt));
        assert!(!mods.contains(&Key::LControl));
    }
}
