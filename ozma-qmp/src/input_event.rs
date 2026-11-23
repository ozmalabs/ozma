// SPDX-License-Identifier: AGPL-3.0-only
//! QMP `input-send-event` payload types.
//!
//! The on-the-wire shape is exactly what QEMU expects:
//!
//! ```json
//! {"type":"key","data":{"down":true,"key":{"type":"qcode","data":"a"}}}
//! {"type":"btn","data":{"down":true,"button":"left"}}
//! {"type":"abs","data":{"axis":"x","value":16383}}
//! {"type":"rel","data":{"axis":"x","value":-4}}
//! ```

use serde::{Deserialize, Serialize};

/// A QEMU `InputEvent` value for `input-send-event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum QmpInputEvent {
    /// A keyboard key, identified by a QEMU qcode.
    Key {
        /// True for key-down, false for key-up.
        down: bool,
        /// Key identifier.
        key: QmpKeyValue,
    },
    /// A pointer button (including `wheel-up` / `wheel-down`).
    Btn {
        /// True for press, false for release.
        down: bool,
        /// QEMU button name (`left`, `right`, `middle`, `wheel-up`, ...).
        button: String,
    },
    /// An absolute pointer axis update.
    Abs {
        /// Axis name (`x` or `y`).
        axis: String,
        /// Absolute value in the QEMU input range (`0..=32767`).
        value: u32,
    },
    /// A relative pointer axis update.
    Rel {
        /// Axis name (`x` or `y`).
        axis: String,
        /// Signed delta.
        value: i32,
    },
}

/// The tagged value inside a key event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QmpKeyValue {
    /// Key encoding. Ozma always uses QEMU qcodes.
    #[serde(rename = "type")]
    pub kind: String,
    /// The qcode itself, for example `ret` or `ctrl`.
    pub data: String,
}

impl QmpKeyValue {
    /// Build a `{"type":"qcode","data":...}` value.
    pub fn qcode(qcode: impl Into<String>) -> Self {
        Self {
            kind: "qcode".to_string(),
            data: qcode.into(),
        }
    }
}

impl QmpInputEvent {
    /// Build a keyboard key event.
    pub fn key(qcode: impl Into<String>, down: bool) -> Self {
        Self::Key {
            down,
            key: QmpKeyValue::qcode(qcode),
        }
    }

    /// Build a pointer button event.
    pub fn button(button: impl Into<String>, down: bool) -> Self {
        Self::Btn {
            down,
            button: button.into(),
        }
    }

    /// Build an absolute pointer axis event.
    pub fn abs(axis: impl Into<String>, value: u32) -> Self {
        Self::Abs {
            axis: axis.into(),
            value,
        }
    }

    /// Build a relative pointer axis event.
    pub fn rel(axis: impl Into<String>, value: i32) -> Self {
        Self::Rel {
            axis: axis.into(),
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_json_shape() {
        let event = QmpInputEvent::key("a", true);
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({
                "type": "key",
                "data": {
                    "down": true,
                    "key": {"type": "qcode", "data": "a"}
                }
            })
        );
    }

    #[test]
    fn btn_json_shape() {
        let event = QmpInputEvent::button("left", true);
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "btn", "data": {"down": true, "button": "left"}})
        );
    }

    #[test]
    fn abs_json_shape() {
        let event = QmpInputEvent::abs("x", 16383);
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "abs", "data": {"axis": "x", "value": 16383}})
        );
    }

    #[test]
    fn rel_json_shape() {
        let event = QmpInputEvent::rel("y", -4);
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "rel", "data": {"axis": "y", "value": -4}})
        );
    }

    #[test]
    fn round_trips() {
        let events = vec![
            QmpInputEvent::key("f24", false),
            QmpInputEvent::button("wheel-down", true),
            QmpInputEvent::abs("y", 0),
            QmpInputEvent::rel("x", 12),
        ];
        let encoded = serde_json::to_string(&events).unwrap();
        let decoded: Vec<QmpInputEvent> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, events);
    }
}
