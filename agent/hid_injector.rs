//! Cross-platform HID injection using enigo crate
//!
//! Provides keyboard and mouse injection capabilities for Linux (uinput),
//! Windows (SendInput), and macOS (CGEvent).

use anyhow::{Context, Result};
use enigo::{
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, MouseCursor, MouseButton,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// HID modifier bits
pub const MOD_NONE: u8 = 0x00;
pub const MOD_LSHIFT: u8 = 0x02;
pub const MOD_RALT: u8 = 0x40; // AltGr (used in non-US layouts)

/// A single HID keystroke: modifier + key usage ID
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyStroke {
    pub modifier: u8,
    pub key: u8,
}

/// Keyboard layout definitions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Layout {
    Us,
    Uk,
    De,
}

impl Layout {
    pub fn from_str(s: &str) -> Layout {
        match s.to_lowercase().as_str() {
            "uk" => Layout::Uk,
            "de" => Layout::De,
            _ => Layout::Us,
        }
    }
}

/// HID usage IDs for keys
pub const HID_KEYS: phf::Map<&'static str, u8> = phf::phf_map! {
    "a" => 0x04, "b" => 0x05, "c" => 0x06, "d" => 0x07, "e" => 0x08, "f" => 0x09,
    "g" => 0x0A, "h" => 0x0B, "i" => 0x0C, "j" => 0x0D, "k" => 0x0E, "l" => 0x0F,
    "m" => 0x10, "n" => 0x11, "o" => 0x12, "p" => 0x13, "q" => 0x14, "r" => 0x15,
    "s" => 0x16, "t" => 0x17, "u" => 0x18, "v" => 0x19, "w" => 0x1A, "x" => 0x1B,
    "y" => 0x1C, "z" => 0x1D,
    "1" => 0x1E, "2" => 0x1F, "3" => 0x20, "4" => 0x21, "5" => 0x22, "6" => 0x23,
    "7" => 0x24, "8" => 0x25, "9" => 0x26, "0" => 0x27,
    "enter" => 0x28, "esc" => 0x29, "backspace" => 0x2A, "tab" => 0x2B,
    "space" => 0x2C, "minus" => 0x2D, "equal" => 0x2E,
    "lbracket" => 0x2F, "rbracket" => 0x30, "backslash" => 0x31,
    "semicolon" => 0x33, "quote" => 0x34, "grave" => 0x35,
    "comma" => 0x36, "period" => 0x37, "slash" => 0x38,
    "f1" => 0x3A, "f2" => 0x3B, "f3" => 0x3C, "f4" => 0x3D,
    "f5" => 0x3E, "f6" => 0x3F, "f7" => 0x40, "f8" => 0x41,
    "f9" => 0x42, "f10" => 0x43, "f11" => 0x44, "f12" => 0x45,
};

/// HID Injector for cross-platform input injection
pub struct HIDInjector {
    enigo: Arc<Mutex<Enigo>>,
}

impl HIDInjector {
    pub fn new() -> Result<Self> {
        let enigo = Enigo::new();
        match enigo {
            Ok(enigo) => Ok(HIDInjector {
                enigo: Arc::new(Mutex::new(enigo)),
            }),
            Err(e) => Err(anyhow::anyhow!("Failed to create Enigo instance: {}", e)),
        }
    }

    /// Inject an 8-byte HID keyboard report
    pub async fn inject_keyboard(&self, report: &[u8]) -> Result<()> {
        if report.len() < 8 {
            return Err(anyhow::anyhow!("Keyboard report too short"));
        }

        let modifier = report[0];
        let keys: Vec<u8> = report[2..8].iter().filter(|&&k| k != 0).copied().collect();

        let mut enigo = self.enigo.lock().await;

        // Handle modifier keys
        self.handle_modifier_keys(&mut enigo, modifier).await?;

        // Handle regular keys
        self.handle_regular_keys(&mut enigo, &keys).await?;

        Ok(())
    }

    /// Inject a 6-byte HID mouse report
    pub async fn inject_mouse(&self, report: &[u8]) -> Result<()> {
        if report.len() < 6 {
            return Err(anyhow::anyhow!("Mouse report too short"));
        }

        let buttons = report[0];
        let x = i32::from_le_bytes([report[1], report[2], 0, 0]);
        let y = i32::from_le_bytes([report[3], report[4], 0, 0]);
        let scroll = if report.len() > 5 {
            report[5] as i8 as i32
        } else {
            0
        };

        let mut enigo = self.enigo.lock().await;

        // Move mouse
        enigo.move_mouse(x, y, MouseCursor::Absolute)
            .map_err(|e| anyhow::anyhow!("Failed to move mouse: {}", e))?;

        // Handle button states
        self.handle_mouse_buttons(&mut enigo, buttons).await?;

        // Handle scroll
        if scroll != 0 {
            enigo.scroll(scroll, enigo::Axis::Vertical)
                .map_err(|e| anyhow::anyhow!("Failed to scroll: {}", e))?;
        }

        Ok(())
    }

    /// Type text using the specified keyboard layout
    pub async fn type_text(
        &self,
        text: &str,
        layout: Layout,
        rate: f32,
    ) -> Result<TypeTextResult> {
        let keymap = self.build_layout_map(&layout);
        let delay = (1000.0 / rate.max(5.0).min(100.0)) as u64; // milliseconds between keystrokes

        let mut chars_sent = 0;
        let mut chars_skipped = 0;

        for char in text.chars() {
            if let Some(stroke) = self.char_to_keystroke(char, &keymap) {
                self.send_key_stroke(&stroke).await?;
                tokio::time::sleep(tokio::time::Duration::from_millis((delay as f64 * 0.4) as u64)).await;
                
                // Release all keys
                self.release_all_keys().await?;
                tokio::time::sleep(tokio::time::Duration::from_millis((delay as f64 * 0.6) as u64)).await;
                
                chars_sent += 1;
            } else {
                chars_skipped += 1;
            }
        }

        Ok(TypeTextResult {
            ok: true,
            chars_sent,
            chars_skipped,
        })
    }

    /// Send a single named key
    pub async fn type_key(&self, key: &str, modifier: u8) -> Result<bool> {
        let hid_key = match HID_KEYS.get(key) {
            Some(&key) => key,
            None => return Ok(false),
        };

        let stroke = KeyStroke { modifier, key: hid_key };
        self.send_key_stroke(&stroke).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        self.release_all_keys().await?;
        
        Ok(true)
    }

    async fn handle_modifier_keys(&self, enigo: &mut Enigo, modifier: u8) -> Result<()> {
        // Handle left shift
        if modifier & MOD_LSHIFT != 0 {
            enigo.key(Key::Shift, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press shift: {}", e))?;
        }
        
        // Handle right alt (AltGr)
        if modifier & MOD_RALT != 0 {
            enigo.key(Key::AltGr, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press altgr: {}", e))?;
        }

        Ok(())
    }

    async fn handle_regular_keys(&self, enigo: &mut Enigo, keys: &[u8]) -> Result<()> {
        for &key in keys {
            if let Some(enigo_key) = self.hid_to_enigo_key(key) {
                enigo.key(enigo_key, Click)
                    .map_err(|e| anyhow::anyhow!("Failed to press key: {}", e))?;
            }
        }
        Ok(())
    }

    async fn handle_mouse_buttons(&self, enigo: &mut Enigo, buttons: u8) -> Result<()> {
        // Left button
        if buttons & 0x01 != 0 {
            enigo.button(MouseButton::Left, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press left mouse button: {}", e))?;
        } else {
            enigo.button(MouseButton::Left, Release)
                .map_err(|e| anyhow::anyhow!("Failed to release left mouse button: {}", e))?;
        }

        // Right button
        if buttons & 0x02 != 0 {
            enigo.button(MouseButton::Right, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press right mouse button: {}", e))?;
        } else {
            enigo.button(MouseButton::Right, Release)
                .map_err(|e| anyhow::anyhow!("Failed to release right mouse button: {}", e))?;
        }

        // Middle button
        if buttons & 0x04 != 0 {
            enigo.button(MouseButton::Middle, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press middle mouse button: {}", e))?;
        } else {
            enigo.button(MouseButton::Middle, Release)
                .map_err(|e| anyhow::anyhow!("Failed to release middle mouse button: {}", e))?;
        }

        Ok(())
    }

    async fn send_key_stroke(&self, stroke: &KeyStroke) -> Result<()> {
        let mut enigo = self.enigo.lock().await;
        
        // Press modifier keys
        if stroke.modifier & MOD_LSHIFT != 0 {
            enigo.key(Key::Shift, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press shift: {}", e))?;
        }
        if stroke.modifier & MOD_RALT != 0 {
            enigo.key(Key::AltGr, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press altgr: {}", e))?;
        }

        // Press the key
        if let Some(enigo_key) = self.hid_to_enigo_key(stroke.key) {
            enigo.key(enigo_key, Press)
                .map_err(|e| anyhow::anyhow!("Failed to press key: {}", e))?;
        }

        Ok(())
    }

    async fn release_all_keys(&self) -> Result<()> {
        let mut enigo = self.enigo.lock().await;
        
        // Release all possible modifier keys
        enigo.key(Key::Shift, Release)
            .map_err(|e| anyhow::anyhow!("Failed to release shift: {}", e)).ok();
        enigo.key(Key::AltGr, Release)
            .map_err(|e| anyhow::anyhow!("Failed to release altgr: {}", e)).ok();
            
        Ok(())
    }

    fn hid_to_enigo_key(&self, hid_key: u8) -> Option<Key> {
        // This is a simplified mapping - in practice this would need to be more comprehensive
        match hid_key {
            0x04..=0x1D => {
                let c = (b'a' + (hid_key - 0x04)) as char;
                Some(Key::Unicode(c))
            }
            0x1E..=0x27 => {
                let c = (b'1' + (hid_key - 0x1E)) as char;
                Some(Key::Unicode(c))
            }
            0x28 => Some(Key::Return),
            0x29 => Some(Key::Escape),
            0x2A => Some(Key::Backspace),
            0x2B => Some(Key::Tab),
            0x2C => Some(Key::Space),
            0x2D => Some(Key::Unicode('-')),
            0x2E => Some(Key::Unicode('=')),
            0x2F => Some(Key::Unicode('[')),
            0x30 => Some(Key::Unicode(']')),
            0x31 => Some(Key::Unicode('\\')),
            0x33 => Some(Key::Unicode(';')),
            0x34 => Some(Key::Unicode('\'')),
            0x35 => Some(Key::Unicode('`')),
            0x36 => Some(Key::Unicode(',')),
            0x37 => Some(Key::Unicode('.')),
            0x38 => Some(Key::Unicode('/')),
            0x3A => Some(Key::F1),
            0x3B => Some(Key::F2),
            0x3C => Some(Key::F3),
            0x3D => Some(Key::F4),
            0x3E => Some(Key::F5),
            0x3F => Some(Key::F6),
            0x40 => Some(Key::F7),
            0x41 => Some(Key::F8),
            0x42 => Some(Key::F9),
            0x43 => Some(Key::F10),
            0x44 => Some(Key::F11),
            0x45 => Some(Key::F12),
            _ => None,
        }
    }

    fn build_layout_map(&self, layout: &Layout) -> HashMap<char, KeyStroke> {
        let mut layout_map: HashMap<char, KeyStroke> = HashMap::new();

        // Base US layout
        for c in 'a'..='z' {
            let hid_key = HID_KEYS.get(&c.to_string()[..]).copied().unwrap_or(0);
            layout_map.insert(c, KeyStroke { modifier: MOD_NONE, key: hid_key });
        }

        for c in 'A'..='Z' {
            let lower_c = c.to_lowercase().next().unwrap();
            let hid_key = HID_KEYS.get(&lower_c.to_string()[..]).copied().unwrap_or(0);
            layout_map.insert(c, KeyStroke { modifier: MOD_LSHIFT, key: hid_key });
        }

        for c in '0'..='9' {
            let hid_key = HID_KEYS.get(&c.to_string()[..]).copied().unwrap_or(0);
            layout_map.insert(c, KeyStroke { modifier: MOD_NONE, key: hid_key });
        }

        // Add layout-specific mappings
        match layout {
            Layout::Us => {
                // US-specific mappings
                layout_map.insert(' ', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["space"] });
                layout_map.insert('-', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["minus"] });
                layout_map.insert('=', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["equal"] });
                layout_map.insert('[', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["lbracket"] });
                layout_map.insert(']', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["rbracket"] });
                layout_map.insert('\\', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["backslash"] });
                layout_map.insert(';', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["semicolon"] });
                layout_map.insert('\'', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["quote"] });
                layout_map.insert('`', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["grave"] });
                layout_map.insert(',', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["comma"] });
                layout_map.insert('.', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["period"] });
                layout_map.insert('/', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["slash"] });
                
                layout_map.insert('_', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["minus"] });
                layout_map.insert('+', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["equal"] });
                layout_map.insert('{', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["lbracket"] });
                layout_map.insert('}', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["rbracket"] });
                layout_map.insert('|', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["backslash"] });
                layout_map.insert(':', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["semicolon"] });
                layout_map.insert('"', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["quote"] });
                layout_map.insert('~', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["grave"] });
                layout_map.insert('<', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["comma"] });
                layout_map.insert('>', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["period"] });
                layout_map.insert('?', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["slash"] });
                
                layout_map.insert('\n', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["enter"] });
                layout_map.insert('\t', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["tab"] });
            }
            Layout::Uk => {
                // UK-specific mappings (inherit US and override)
                layout_map.insert('"', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["2"] });
                layout_map.insert('@', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["quote"] });
                layout_map.insert('£', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["3"] });
            }
            Layout::De => {
                // German-specific mappings
                // Swap Z and Y
                layout_map.insert('z', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["y"] });
                layout_map.insert('y', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["z"] });
                layout_map.insert('Z', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["y"] });
                layout_map.insert('Y', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["z"] });
                
                // AltGr combinations
                layout_map.insert('@', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["q"] });
                layout_map.insert('€', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["e"] });
            }
        }

        layout_map
    }

    fn char_to_keystroke(&self, ch: char, layout_map: &HashMap<char, KeyStroke>) -> Option<KeyStroke> {
        layout_map.get(&ch).cloned()
    }
}

/// Result of typing text operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTextResult {
    pub ok: bool,
    pub chars_sent: u32,
    pub chars_skipped: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_hid_injector_creation() {
        let injector = HIDInjector::new();
        assert!(injector.is_ok());
    }

    #[tokio::test]
    async fn test_keyboard_injection() {
        let injector = HIDInjector::new().unwrap();
        let report = [0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]; // 'a' key press
        let result = injector.inject_keyboard(&report).await;
        // We can't actually test the injection without a real system, but we can test it doesn't crash
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_mouse_injection() {
        let injector = HIDInjector::new().unwrap();
        let report = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00]; // Left mouse button press
        let result = injector.inject_mouse(&report).await;
        // We can't actually test the injection without a real system, but we can test it doesn't crash
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_type_text() {
        let injector = HIDInjector::new().unwrap();
        let result = injector.type_text("Hello", Layout::Us, 30.0).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_type_key() {
        let injector = HIDInjector::new().unwrap();
        let result = injector.type_key("enter", 0).await;
        assert!(result.is_ok());
    }
}
