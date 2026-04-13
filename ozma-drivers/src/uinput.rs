// SPDX-License-Identifier: AGPL-3.0-only
//! uinput virtual device creation and HID event injection.
//!
//! Creates virtual keyboard and mouse devices via the Linux uinput subsystem.
//! Used on the controller side to inject HID events received from remote nodes.

use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsoluteAxisType, AttributeSet, EventType, InputEvent, Key, RelativeAxisType,
    UinputAbsSetup, AbsInfo,
};
use tracing::{debug, info};

/// A virtual uinput device that can inject keyboard or mouse events.
pub struct UinputDevice {
    inner: VirtualDevice,
    kind: DeviceKind,
}

/// Whether the virtual device behaves as a keyboard or mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Keyboard,
    Mouse,
}

impl UinputDevice {
    /// Create a virtual keyboard device.
    ///
    /// Registers all standard keyboard keys (codes 1–248).
    pub fn new_keyboard(name: &str) -> anyhow::Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        for code in 1u16..=248 {
            if let Ok(key) = Key::new(code) {
                keys.insert(key);
            }
        }

        let dev = VirtualDeviceBuilder::new()?
            .name(name)
            .with_keys(&keys)?
            .build()?;

        info!("Created virtual keyboard: {}", name);
        Ok(Self { inner: dev, kind: DeviceKind::Keyboard })
    }

    /// Create a virtual relative mouse device (standard desktop mouse).
    pub fn new_mouse(name: &str) -> anyhow::Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        keys.insert(Key::BTN_LEFT);
        keys.insert(Key::BTN_RIGHT);
        keys.insert(Key::BTN_MIDDLE);

        let mut rel_axes = AttributeSet::<RelativeAxisType>::new();
        rel_axes.insert(RelativeAxisType::REL_X);
        rel_axes.insert(RelativeAxisType::REL_Y);
        rel_axes.insert(RelativeAxisType::REL_WHEEL);

        let dev = VirtualDeviceBuilder::new()?
            .name(name)
            .with_keys(&keys)?
            .with_relative_axes(&rel_axes)?
            .build()?;

        info!("Created virtual mouse: {}", name);
        Ok(Self { inner: dev, kind: DeviceKind::Mouse })
    }

    /// Create a virtual absolute mouse (touchpad / tablet style).
    pub fn new_abs_mouse(name: &str, max_x: i32, max_y: i32) -> anyhow::Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        keys.insert(Key::BTN_LEFT);
        keys.insert(Key::BTN_RIGHT);
        keys.insert(Key::BTN_MIDDLE);

        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_X,
            AbsInfo::new(0, 0, max_x, 0, 0, 1),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_Y,
            AbsInfo::new(0, 0, max_y, 0, 0, 1),
        );

        let dev = VirtualDeviceBuilder::new()?
            .name(name)
            .with_keys(&keys)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .build()?;

        info!("Created virtual absolute mouse: {}", name);
        Ok(Self { inner: dev, kind: DeviceKind::Mouse })
    }

    /// Inject a HID keyboard boot-protocol report (8 bytes).
    ///
    /// Layout: `[modifiers, reserved, key1..key6]`
    ///
    /// Mirrors the report format produced by `KeyboardState::build_report()`.
    pub fn inject_keyboard_report(&mut self, report: &[u8; 8]) -> anyhow::Result<()> {
        let modifiers = report[0];
        let keys = &report[2..8];

        let mut events: Vec<InputEvent> = Vec::with_capacity(16);

        const MODIFIER_MAP: &[(u8, Key)] = &[
            (0x01, Key::KEY_LEFTCTRL),
            (0x02, Key::KEY_LEFTSHIFT),
            (0x04, Key::KEY_LEFTALT),
            (0x08, Key::KEY_LEFTMETA),
            (0x10, Key::KEY_RIGHTCTRL),
            (0x20, Key::KEY_RIGHTSHIFT),
            (0x40, Key::KEY_RIGHTALT),
            (0x80, Key::KEY_RIGHTMETA),
        ];
        for &(bit, key) in MODIFIER_MAP {
            let value = i32::from(modifiers & bit != 0);
            events.push(InputEvent::new(EventType::KEY, key.code(), value));
        }

        for &hid in keys {
            if hid == 0 {
                continue;
            }
            if let Some(key) = hid_to_evdev_key(hid) {
                events.push(InputEvent::new(EventType::KEY, key.code(), 1));
            }
        }

        events.push(InputEvent::new(EventType::SYNCHRONIZATION, 0, 0));
        self.inner.emit(&events)?;
        debug!("Injected keyboard report: {:02x?}", report);
        Ok(())
    }

    /// Inject a HID absolute mouse report (6 bytes).
    ///
    /// Layout: `[buttons, x_lo, x_hi, y_lo, y_hi, scroll]`
    ///
    /// Mirrors the report format produced by `MouseState::build_report()`.
    pub fn inject_mouse_report(&mut self, report: &[u8; 6]) -> anyhow::Result<()> {
        let buttons = report[0];
        let x = u16::from_le_bytes([report[1], report[2]]) as i32;
        let y = u16::from_le_bytes([report[3], report[4]]) as i32;
        let scroll = report[5] as i8;

        let mut events: Vec<InputEvent> = Vec::with_capacity(8);
        events.push(InputEvent::new(EventType::KEY, Key::BTN_LEFT.code(), i32::from(buttons & 0x01 != 0)));
        events.push(InputEvent::new(EventType::KEY, Key::BTN_RIGHT.code(), i32::from(buttons & 0x02 != 0)));
        events.push(InputEvent::new(EventType::KEY, Key::BTN_MIDDLE.code(), i32::from(buttons & 0x04 != 0)));
        events.push(InputEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, x));
        events.push(InputEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, y));
        if scroll != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE,
                RelativeAxisType::REL_WHEEL.0,
                scroll as i32,
            ));
        }
        events.push(InputEvent::new(EventType::SYNCHRONIZATION, 0, 0));

        self.inner.emit(&events)?;
        debug!("Injected mouse report: {:02x?}", report);
        Ok(())
    }

    /// Inject a relative mouse report for gaming mode (6 bytes).
    ///
    /// Layout: `[buttons, dx_lo, dx_hi, dy_lo, dy_hi, scroll]`
    /// where dx/dy are signed 16-bit little-endian.
    ///
    /// Mirrors the report format produced by `MouseState::build_relative_report()`.
    pub fn inject_relative_mouse_report(&mut self, report: &[u8; 6]) -> anyhow::Result<()> {
        let buttons = report[0];
        let dx = i16::from_le_bytes([report[1], report[2]]) as i32;
        let dy = i16::from_le_bytes([report[3], report[4]]) as i32;
        let scroll = report[5] as i8;

        let mut events: Vec<InputEvent> = Vec::with_capacity(8);
        events.push(InputEvent::new(EventType::KEY, Key::BTN_LEFT.code(), i32::from(buttons & 0x01 != 0)));
        events.push(InputEvent::new(EventType::KEY, Key::BTN_RIGHT.code(), i32::from(buttons & 0x02 != 0)));
        events.push(InputEvent::new(EventType::KEY, Key::BTN_MIDDLE.code(), i32::from(buttons & 0x04 != 0)));
        if dx != 0 {
            events.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_X.0, dx));
        }
        if dy != 0 {
            events.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_Y.0, dy));
        }
        if scroll != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE,
                RelativeAxisType::REL_WHEEL.0,
                scroll as i32,
            ));
        }
        events.push(InputEvent::new(EventType::SYNCHRONIZATION, 0, 0));

        self.inner.emit(&events)?;
        Ok(())
    }

    pub fn kind(&self) -> DeviceKind {
        self.kind
    }
}

/// Map a USB HID usage ID (page 0x07) back to an evdev `Key`.
///
/// Inverse of `keyboard::key_to_hid`.
pub fn hid_to_evdev_key(hid: u8) -> Option<Key> {
    let key = match hid {
        0x04 => Key::KEY_A,
        0x05 => Key::KEY_B,
        0x06 => Key::KEY_C,
        0x07 => Key::KEY_D,
        0x08 => Key::KEY_E,
        0x09 => Key::KEY_F,
        0x0A => Key::KEY_G,
        0x0B => Key::KEY_H,
        0x0C => Key::KEY_I,
        0x0D => Key::KEY_J,
        0x0E => Key::KEY_K,
        0x0F => Key::KEY_L,
        0x10 => Key::KEY_M,
        0x11 => Key::KEY_N,
        0x12 => Key::KEY_O,
        0x13 => Key::KEY_P,
        0x14 => Key::KEY_Q,
        0x15 => Key::KEY_R,
        0x16 => Key::KEY_S,
        0x17 => Key::KEY_T,
        0x18 => Key::KEY_U,
        0x19 => Key::KEY_V,
        0x1A => Key::KEY_W,
        0x1B => Key::KEY_X,
        0x1C => Key::KEY_Y,
        0x1D => Key::KEY_Z,
        0x1E => Key::KEY_1,
        0x1F => Key::KEY_2,
        0x20 => Key::KEY_3,
        0x21 => Key::KEY_4,
        0x22 => Key::KEY_5,
        0x23 => Key::KEY_6,
        0x24 => Key::KEY_7,
        0x25 => Key::KEY_8,
        0x26 => Key::KEY_9,
        0x27 => Key::KEY_0,
        0x28 => Key::KEY_ENTER,
        0x29 => Key::KEY_ESC,
        0x2A => Key::KEY_BACKSPACE,
        0x2B => Key::KEY_TAB,
        0x2C => Key::KEY_SPACE,
        0x2D => Key::KEY_MINUS,
        0x2E => Key::KEY_EQUAL,
        0x2F => Key::KEY_LEFTBRACE,
        0x30 => Key::KEY_RIGHTBRACE,
        0x31 => Key::KEY_BACKSLASH,
        0x33 => Key::KEY_SEMICOLON,
        0x34 => Key::KEY_APOSTROPHE,
        0x35 => Key::KEY_GRAVE,
        0x36 => Key::KEY_COMMA,
        0x37 => Key::KEY_DOT,
        0x38 => Key::KEY_SLASH,
        0x39 => Key::KEY_CAPSLOCK,
        0x3A => Key::KEY_F1,
        0x3B => Key::KEY_F2,
        0x3C => Key::KEY_F3,
        0x3D => Key::KEY_F4,
        0x3E => Key::KEY_F5,
        0x3F => Key::KEY_F6,
        0x40 => Key::KEY_F7,
        0x41 => Key::KEY_F8,
        0x42 => Key::KEY_F9,
        0x43 => Key::KEY_F10,
        0x44 => Key::KEY_F11,
        0x45 => Key::KEY_F12,
        0x49 => Key::KEY_INSERT,
        0x4A => Key::KEY_HOME,
        0x4B => Key::KEY_PAGEUP,
        0x4C => Key::KEY_DELETE,
        0x4D => Key::KEY_END,
        0x4E => Key::KEY_PAGEDOWN,
        0x4F => Key::KEY_RIGHT,
        0x50 => Key::KEY_LEFT,
        0x51 => Key::KEY_DOWN,
        0x52 => Key::KEY_UP,
        0x53 => Key::KEY_NUMLOCK,
        0x54 => Key::KEY_KPSLASH,
        0x55 => Key::KEY_KPASTERISK,
        0x56 => Key::KEY_KPMINUS,
        0x57 => Key::KEY_KPPLUS,
        0x58 => Key::KEY_KPENTER,
        0x59 => Key::KEY_KP1,
        0x5A => Key::KEY_KP2,
        0x5B => Key::KEY_KP3,
        0x5C => Key::KEY_KP4,
        0x5D => Key::KEY_KP5,
        0x5E => Key::KEY_KP6,
        0x5F => Key::KEY_KP7,
        0x60 => Key::KEY_KP8,
        0x61 => Key::KEY_KP9,
        0x62 => Key::KEY_KP0,
        0x63 => Key::KEY_KPDOT,
        0x7F => Key::KEY_MUTE,
        0x80 => Key::KEY_VOLUMEUP,
        0x81 => Key::KEY_VOLUMEDOWN,
        0xE0 => Key::KEY_LEFTCTRL,
        0xE1 => Key::KEY_LEFTSHIFT,
        0xE2 => Key::KEY_LEFTALT,
        0xE3 => Key::KEY_LEFTMETA,
        0xE4 => Key::KEY_RIGHTCTRL,
        0xE5 => Key::KEY_RIGHTSHIFT,
        0xE6 => Key::KEY_RIGHTALT,
        0xE7 => Key::KEY_RIGHTMETA,
        _ => return None,
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::key_to_hid;

    /// Every key that has a HID code must round-trip back to itself.
    #[test]
    fn test_hid_roundtrip() {
        let keys = [
            Key::KEY_A, Key::KEY_Z, Key::KEY_0, Key::KEY_9,
            Key::KEY_ENTER, Key::KEY_ESC, Key::KEY_F1, Key::KEY_F12,
            Key::KEY_LEFT, Key::KEY_RIGHT, Key::KEY_UP, Key::KEY_DOWN,
            Key::KEY_LEFTCTRL, Key::KEY_RIGHTSHIFT,
            Key::KEY_MUTE, Key::KEY_VOLUMEUP, Key::KEY_VOLUMEDOWN,
        ];
        for key in keys {
            if let Some(hid) = key_to_hid(key) {
                let back = hid_to_evdev_key(hid);
                assert_eq!(
                    back,
                    Some(key),
                    "Round-trip failed for {:?} (hid=0x{:02x})",
                    key,
                    hid
                );
            }
        }
    }
}
