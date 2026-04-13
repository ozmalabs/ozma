// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Cross-platform HID injection for keyboard and mouse events.
//!
//! Uses enigo crate for cross-platform support:
//! - Linux: uinput virtual devices
//! - Windows: SendInput Win32 API
//! - macOS: CGEvent Quartz APIs
//!
//! Provides both low-level HID report injection and high-level text typing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use enigo::{Enigo, Key, KeyboardControllable, MouseButton, MouseControllable};
use tokio::sync::Mutex;
use tokio::time::sleep;

/// A single HID keystroke: modifier + key
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStroke {
    pub modifier: u8,
    pub key: u8,
}

/// HID modifier bits
pub const MOD_NONE: u8 = 0x00;
pub const MOD_LSHIFT: u8 = 0x02;
pub const MOD_RALT: u8 = 0x40; // AltGr (used in non-US layouts)

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

/// Keyboard layout maps - character to KeyStroke mapping
pub type LayoutMap = HashMap<char, KeyStroke>;

/// Build US QWERTY layout map
fn build_us_layout() -> LayoutMap {
    let mut layout = HashMap::new();

    // Lowercase letters (no modifier)
    for c in 'a'..='z' {
        layout.insert(c, KeyStroke {
            modifier: MOD_NONE,
            key: HID_KEYS.get(&c.to_string()[..]).copied().unwrap_or(0),
        });
    }

    // Uppercase letters (shift)
    for c in 'a'..='z' {
        layout.insert(c.to_ascii_uppercase(), KeyStroke {
            modifier: MOD_LSHIFT,
            key: HID_KEYS.get(&c.to_string()[..]).copied().unwrap_or(0),
        });
    }

    // Digits
    for c in ['1','2','3','4','5','6','7','8','9','0'] {
        layout.insert(c, KeyStroke {
            modifier: MOD_NONE,
            key: HID_KEYS.get(&c.to_string()[..]).copied().unwrap_or(0),
        });
    }

    // Shift+digits → symbols
    let shift_digits = [
        ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'),
        ('^', '6'), ('&', '7'), ('*', '8'), ('(', '9'), (')', '0'),
    ];
    for (sym, digit) in shift_digits {
        layout.insert(sym, KeyStroke {
            modifier: MOD_LSHIFT,
            key: HID_KEYS.get(&digit.to_string()[..]).copied().unwrap_or(0),
        });
    }

    // Unshifted punctuation
    layout.insert(' ', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["space"] });
    layout.insert('-', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["minus"] });
    layout.insert('=', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["equal"] });
    layout.insert('[', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["lbracket"] });
    layout.insert(']', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["rbracket"] });
    layout.insert('\\', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["backslash"] });
    layout.insert(';', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["semicolon"] });
    layout.insert('\'', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["quote"] });
    layout.insert('`', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["grave"] });
    layout.insert(',', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["comma"] });
    layout.insert('.', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["period"] });
    layout.insert('/', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["slash"] });

    // Shifted punctuation
    layout.insert('_', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["minus"] });
    layout.insert('+', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["equal"] });
    layout.insert('{', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["lbracket"] });
    layout.insert('}', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["rbracket"] });
    layout.insert('|', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["backslash"] });
    layout.insert(':', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["semicolon"] });
    layout.insert('"', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["quote"] });
    layout.insert('~', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["grave"] });
    layout.insert('<', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["comma"] });
    layout.insert('>', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["period"] });
    layout.insert('?', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["slash"] });

    // Special keys
    layout.insert('\n', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["enter"] });
    layout.insert('\t', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["tab"] });

    layout
}

/// Build UK QWERTY layout map
fn build_uk_layout() -> LayoutMap {
    let mut layout = build_us_layout();
    // UK differences
    layout.insert('"', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["2"] });
    layout.insert('@', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["quote"] });
    layout.insert('£', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["3"] });
    layout.insert('#', KeyStroke { modifier: MOD_NONE, key: 0x32 });
    layout.insert('~', KeyStroke { modifier: MOD_LSHIFT, key: 0x32 });
    layout.insert('\\', KeyStroke { modifier: MOD_NONE, key: 0x64 });
    layout.insert('|', KeyStroke { modifier: MOD_LSHIFT, key: 0x64 });
    layout
}

/// Build German QWERTZ layout map
fn build_de_layout() -> LayoutMap {
    let mut layout = build_us_layout();
    // Z and Y swapped
    layout.insert('z', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["y"] });
    layout.insert('y', KeyStroke { modifier: MOD_NONE, key: HID_KEYS["z"] });
    layout.insert('Z', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["y"] });
    layout.insert('Y', KeyStroke { modifier: MOD_LSHIFT, key: HID_KEYS["z"] });
    // German-specific via AltGr
    layout.insert('@', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["q"] });
    layout.insert('€', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["e"] });
    layout.insert('{', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["7"] });
    layout.insert('}', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["0"] });
    layout.insert('[', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["8"] });
    layout.insert(']', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["9"] });
    layout.insert('\\', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["minus"] });
    layout.insert('|', KeyStroke { modifier: MOD_RALT, key: 0x64 });
    layout.insert('~', KeyStroke { modifier: MOD_RALT, key: HID_KEYS["rbracket"] });
    layout
}

/// Available keyboard layouts
pub fn available_layouts() -> Vec<&'static str> {
    vec!["us", "uk", "de"]
}

/// Get layout map by name
pub fn get_layout_map(name: &str) -> LayoutMap {
    match name {
        "uk" => build_uk_layout(),
        "de" => build_de_layout(),
        _ => build_us_layout(), // default to US
    }
}

/// HID Injector for cross-platform keyboard/mouse injection
pub struct HIDInjector {
    enigo: Arc<Mutex<Enigo>>,
    is_typing: Arc<std::sync::atomic::AtomicBool>,
}

impl HIDInjector {
    /// Create a new HID injector
    pub fn new() -> Result<Self> {
        let enigo = Enigo::new();
        match enigo {
            Ok(enigo) => Ok(HIDInjector {
                enigo: Arc::new(Mutex::new(enigo)),
                is_typing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            Err(e) => Err(anyhow!("Failed to create Enigo instance: {}", e)),
        }
    }

    /// Inject an 8-byte HID keyboard report
    pub async fn inject_keyboard(&self, report: &[u8]) -> Result<()> {
        if report.len() < 8 {
            return Err(anyhow!("Keyboard report too short"));
        }

        let modifier = report[0];
        let keys: Vec<u8> = report[2..8].iter().filter(|&&k| k != 0).copied().collect();

        let enigo = self.enigo.clone();
        
        // Move blocking operations to a thread pool to avoid blocking async runtime
        tokio::task::spawn_blocking(move || {
            let mut enigo = enigo.blocking_lock();
            
            // Handle modifier keys
            if modifier & MOD_LSHIFT != 0 {
                enigo.key_down(Key::Shift);
            }

            // Handle key presses/releases
            for &key in &keys {
                // Map HID keycodes to enigo keys
                if let Some(enigo_key) = map_hid_to_enigo(key) {
                    enigo.key_down(enigo_key);
                    // Small delay to ensure key is registered
                    std::thread::sleep(Duration::from_millis(1));
                    enigo.key_up(enigo_key);
                }
            }

            // Release modifier if no longer pressed
            if modifier & MOD_LSHIFT != 0 {
                enigo.key_up(Key::Shift);
            }
        }).await.map_err(|e| anyhow!("Failed to execute keyboard injection: {}", e))?;
        
        Ok(())
    }

    /// Inject a 6-byte HID mouse report
    pub async fn inject_mouse(&self, report: &[u8]) -> Result<()> {
        if report.len() < 6 {
            return Err(anyhow!("Mouse report too short"));
        }

        let buttons = report[0];
        let x = i32::from_le_bytes([report[1], report[2], 0, 0]);
        let y = i32::from_le_bytes([report[3], report[4], 0, 0]);
        let scroll = if report.len() > 5 {
            report[5] as i8 as i32
        } else {
            0
        };

        let enigo = self.enigo.clone();
        
        // Move blocking operations to a thread pool to avoid blocking async runtime
        tokio::task::spawn_blocking(move || {
            let mut enigo = enigo.blocking_lock();

            // Move mouse
            enigo.mouse_move_to(x, y);

            // Handle button presses
            if buttons & 0x01 != 0 {
                enigo.mouse_down(MouseButton::Left);
            } else {
                enigo.mouse_up(MouseButton::Left);
            }

            if buttons & 0x02 != 0 {
                enigo.mouse_down(MouseButton::Right);
            } else {
                enigo.mouse_up(MouseButton::Right);
            }

            if buttons & 0x04 != 0 {
                enigo.mouse_down(MouseButton::Middle);
            } else {
                enigo.mouse_up(MouseButton::Middle);
            }

            // Handle scroll
            if scroll != 0 {
                enigo.mouse_scroll_y(scroll);
            }
        }).await.map_err(|e| anyhow!("Failed to execute mouse injection: {}", e))?;
        
        Ok(())
    }

    /// Type text with specified layout and rate
    pub async fn type_text(
        &mut self,
        text: &str,
        layout: &str,
        rate: f32,
    ) -> Result<(usize, usize)> {
        let keymap = get_layout_map(layout);
        let delay = Duration::from_millis((1000.0 / rate.max(5.0).min(100.0)) as u64);
        
        self.is_typing.store(true, std::sync::atomic::Ordering::Relaxed);
        let is_typing = self.is_typing.clone();
        let enigo = self.enigo.clone();
        
        let text = text.to_string();
        let layout = layout.to_string();
        
        // Move blocking operations to a thread pool to avoid blocking async runtime
        let result = tokio::task::spawn_blocking(move || {
            let mut chars_sent = 0;
            let mut chars_skipped = 0;
            
            for c in text.chars() {
                if !is_typing.load(std::sync::atomic::Ordering::Relaxed) {
                    break; // Allow interruption
                }

                if let Some(stroke) = get_layout_map(&layout).get(&c) {
                    let mut enigo = enigo.blocking_lock();
                    
                    // Handle modifier keys
                    if stroke.modifier & MOD_LSHIFT != 0 {
                        enigo.key_down(Key::Shift);
                    }
                    
                    if stroke.modifier & MOD_RALT != 0 {
                        enigo.key_down(Key::Alt);
                    }

                    // Press the key
                    if let Some(enigo_key) = map_hid_to_enigo(stroke.key) {
                        enigo.key_down(enigo_key);
                        std::thread::sleep(Duration::from_millis((delay.as_millis() as f32 * 0.4) as u64));
                        enigo.key_up(enigo_key);
                    }

                    // Release modifiers
                    if stroke.modifier & MOD_RALT != 0 {
                        enigo.key_up(Key::Alt);
                    }
                    
                    if stroke.modifier & MOD_LSHIFT != 0 {
                        enigo.key_up(Key::Shift);
                    }

                    drop(enigo);
                    chars_sent += 1;
                    std::thread::sleep(Duration::from_millis((delay.as_millis() as f32 * 0.6) as u64));
                } else {
                    chars_skipped += 1;
                }
            }
            
            (chars_sent, chars_skipped)
        }).await;
        
        self.is_typing.store(false, std::sync::atomic::Ordering::Relaxed);
        
        match result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(anyhow!("Type text failed: {}", e)),
            Err(e) => Err(anyhow!("Type text task failed: {}", e)),
        }
    }

    /// Send a single named key
    pub async fn type_key(&self, key: &str) -> Result<bool> {
        if let Some(&hid_key) = HID_KEYS.get(key) {
            let enigo_key = match map_hid_to_enigo(hid_key) {
                Some(key) => key,
                None => return Ok(false),
            };

            let enigo = self.enigo.clone();
            
            // Move blocking operations to a thread pool to avoid blocking async runtime
            tokio::task::spawn_blocking(move || {
                let mut enigo = enigo.blocking_lock();
                enigo.key_down(enigo_key);
                std::thread::sleep(Duration::from_millis(50));
                enigo.key_up(enigo_key);
            }).await.map_err(|e| anyhow!("Failed to execute key press: {}", e))?;
            
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Check if currently typing
    pub fn is_typing(&self) -> bool {
        self.is_typing.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stop typing (interrupt)
    pub fn stop_typing(&self) {
        self.is_typing.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Map HID usage ID to enigo Key
fn map_hid_to_enigo(hid_code: u8) -> Option<Key> {
    match hid_code {
        0x04..=0x1D => { // a-z
            let c = (b'a' + (hid_code - 0x04)) as char;
            Some(Key::Layout(c))
        },
        0x1E..=0x27 => { // 1-0
            let digits = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
            let idx = (hid_code - 0x1E) as usize;
            if idx < digits.len() {
                Some(Key::Layout(digits[idx]))
            } else {
                None
            }
        },
        0x28 => Some(Key::Return),
        0x29 => Some(Key::Escape),
        0x2A => Some(Key::Backspace),
        0x2B => Some(Key::Tab),
        0x2C => Some(Key::Space),
        0x2D => Some(Key::Layout('-')),
        0x2E => Some(Key::Layout('=')),
        0x2F => Some(Key::Layout('[')),
        0x30 => Some(Key::Layout(']')),
        0x31 => Some(Key::Layout('\\')),
        0x33 => Some(Key::Layout(';')),
        0x34 => Some(Key::Layout('\'')),
        0x35 => Some(Key::Layout('`')),
        0x36 => Some(Key::Layout(',')),
        0x37 => Some(Key::Layout('.')),
        0x38 => Some(Key::Layout('/')),
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
        0x4F => Some(Key::RightArrow),
        0x50 => Some(Key::LeftArrow),
        0x51 => Some(Key::DownArrow),
        0x52 => Some(Key::UpArrow),
        _ => None,
    }
}
