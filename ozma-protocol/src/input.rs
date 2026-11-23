// SPDX-License-Identifier: AGPL-3.0-only
//! Input events carried on the input/control channels.

use serde::{Deserialize, Serialize};

use crate::ids::DisplayId;

/// HID-style modifier bits. The low byte matches the USB HID boot keyboard
/// modifier report so adapters can copy it directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyModifiers(pub u8);

impl KeyModifiers {
    pub const LEFT_CTRL: u8 = 0x01;
    pub const LEFT_SHIFT: u8 = 0x02;
    pub const LEFT_ALT: u8 = 0x04;
    pub const LEFT_GUI: u8 = 0x08;
    pub const RIGHT_CTRL: u8 = 0x10;
    pub const RIGHT_SHIFT: u8 = 0x20;
    pub const RIGHT_ALT: u8 = 0x40;
    pub const RIGHT_GUI: u8 = 0x80;

    pub fn new(bits: u8) -> Self {
        Self(bits)
    }

    pub fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// Pointer button identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// Touch contact phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// One normalized input event. Coordinates are in the target display's local
/// space; the node maps them into the host topology.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum InputEvent {
    Key {
        code: u32,
        down: bool,
        #[serde(default)]
        modifiers: KeyModifiers,
    },
    PointerAbsolute {
        #[serde(default)]
        display_id: Option<DisplayId>,
        x: i32,
        y: i32,
    },
    PointerRelative {
        dx: i32,
        dy: i32,
    },
    Button {
        button: PointerButton,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    Touch {
        contact: u32,
        phase: TouchPhase,
        #[serde(default)]
        display_id: Option<DisplayId>,
        x: i32,
        y: i32,
        #[serde(default)]
        pressure: u16,
    },
    Pen {
        phase: TouchPhase,
        #[serde(default)]
        display_id: Option<DisplayId>,
        x: i32,
        y: i32,
        #[serde(default)]
        pressure: u16,
        #[serde(default)]
        tilt_x: i16,
        #[serde(default)]
        tilt_y: i16,
    },
    Clipboard {
        text: String,
    },
    /// Release every held key/button/contact. Sent on focus loss, disconnect,
    /// DPMS, lock, and control handoff. It is idempotent.
    ReleaseAll,
}

impl InputEvent {
    pub fn release_all() -> Self {
        Self::ReleaseAll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_events_roundtrip() {
        let event = InputEvent::PointerAbsolute {
            display_id: Some(DisplayId::new("remote-1")),
            x: 640,
            y: 480,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: InputEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
}
