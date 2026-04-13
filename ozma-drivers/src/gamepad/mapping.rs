// SPDX-License-Identifier: AGPL-3.0-only
//! Button / axis → ozma action mapping.
//!
//! Mirrors the default mapping table in `controller/gamepad.py`.

use gilrs::{Axis, Button};
use serde::{Deserialize, Serialize};

/// An ozma action that a control can trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Cycle to the next (+1) or previous (-1) scenario.
    ScenarioNext { delta: i32 },
    /// Activate / confirm the currently highlighted scenario.
    ScenarioActivate,
    /// Toggle mute on the target node.
    AudioMute { target: String },
    /// Set absolute volume [0.0, 1.0] on the target node.
    AudioVolume { target: String, value: f32 },
    /// Nudge volume by a signed step on the target node.
    AudioVolumeStep { target: String, step: f32 },
}

/// A resolved control event ready to be dispatched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlEvent {
    /// Logical control name (e.g. `"dpad_right"`, `"rt_volume"`).
    pub control: String,
    /// The action to perform.
    pub action:  Action,
}

/// Deadzone for analog triggers (0 → 1 range after normalisation).
pub(crate) const TRIGGER_DEADZONE: f32 = 0.15;

/// Map a gilrs button press to a `ControlEvent`, if any default binding exists.
///
/// Returns `None` for buttons that are reserved / unmapped.
pub fn map_button(button: Button) -> Option<ControlEvent> {
    match button {
        Button::South => Some(ControlEvent {
            control: "south".into(),
            action:  Action::ScenarioActivate,
        }),
        Button::LeftTrigger => Some(ControlEvent {
            control: "lb".into(),
            action:  Action::ScenarioNext { delta: -1 },
        }),
        Button::RightTrigger => Some(ControlEvent {
            control: "rb".into(),
            action:  Action::ScenarioNext { delta: 1 },
        }),
        Button::Mode => Some(ControlEvent {
            control: "guide".into(),
            action:  Action::AudioMute { target: "@active".into() },
        }),
        Button::DPadRight => Some(ControlEvent {
            control: "dpad_right".into(),
            action:  Action::ScenarioNext { delta: 1 },
        }),
        Button::DPadLeft => Some(ControlEvent {
            control: "dpad_left".into(),
            action:  Action::ScenarioNext { delta: -1 },
        }),
        Button::DPadUp => Some(ControlEvent {
            control: "dpad_up".into(),
            action:  Action::AudioVolumeStep {
                target: "@active".into(),
                step:   0.05,
            },
        }),
        Button::DPadDown => Some(ControlEvent {
            control: "dpad_down".into(),
            action:  Action::AudioVolumeStep {
                target: "@active".into(),
                step:   -0.05,
            },
        }),
        // East / North / West / Select / Start / thumb-clicks — reserved
        _ => None,
    }
}

/// Map a gilrs axis event to a `ControlEvent`, if any default binding exists.
///
/// `value` is already in the gilrs normalised range (−1.0 … +1.0 for sticks,
/// 0.0 … 1.0 for triggers).
pub fn map_axis(axis: Axis, value: f32) -> Option<ControlEvent> {
    match axis {
        // D-pad horizontal
        Axis::DPadX => {
            if value > 0.5 {
                Some(ControlEvent {
                    control: "dpad_right".into(),
                    action:  Action::ScenarioNext { delta: 1 },
                })
            } else if value < -0.5 {
                Some(ControlEvent {
                    control: "dpad_left".into(),
                    action:  Action::ScenarioNext { delta: -1 },
                })
            } else {
                None
            }
        }

        // D-pad vertical
        Axis::DPadY => {
            if value < -0.5 {
                Some(ControlEvent {
                    control: "dpad_up".into(),
                    action:  Action::AudioVolumeStep {
                        target: "@active".into(),
                        step:   0.05,
                    },
                })
            } else if value > 0.5 {
                Some(ControlEvent {
                    control: "dpad_down".into(),
                    action:  Action::AudioVolumeStep {
                        target: "@active".into(),
                        step:   -0.05,
                    },
                })
            } else {
                None
            }
        }

        // Right trigger → absolute volume
        Axis::RightZ => {
            // gilrs normalises triggers to 0.0 … 1.0
            if value > TRIGGER_DEADZONE {
                Some(ControlEvent {
                    control: "rt_volume".into(),
                    action:  Action::AudioVolume {
                        target: "@active".into(),
                        value,
                    },
                })
            } else {
                None
            }
        }

        // Left trigger, sticks — reserved
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn south_activates_scenario() {
        let ev = map_button(Button::South).unwrap();
        assert_eq!(ev.control, "south");
        assert_eq!(ev.action, Action::ScenarioActivate);
    }

    #[test]
    fn lb_prev_scenario() {
        let ev = map_button(Button::LeftTrigger).unwrap();
        assert!(matches!(ev.action, Action::ScenarioNext { delta: -1 }));
    }

    #[test]
    fn rb_next_scenario() {
        let ev = map_button(Button::RightTrigger).unwrap();
        assert!(matches!(ev.action, Action::ScenarioNext { delta: 1 }));
    }

    #[test]
    fn guide_mutes() {
        let ev = map_button(Button::Mode).unwrap();
        assert!(matches!(ev.action, Action::AudioMute { .. }));
    }

    #[test]
    fn dpad_right_next() {
        let ev = map_axis(Axis::DPadX, 1.0).unwrap();
        assert!(matches!(ev.action, Action::ScenarioNext { delta: 1 }));
    }

    #[test]
    fn dpad_left_prev() {
        let ev = map_axis(Axis::DPadX, -1.0).unwrap();
        assert!(matches!(ev.action, Action::ScenarioNext { delta: -1 }));
    }

    #[test]
    fn rt_volume_above_deadzone() {
        let ev = map_axis(Axis::RightZ, 0.8).unwrap();
        assert!(matches!(ev.action, Action::AudioVolume { .. }));
    }

    #[test]
    fn rt_volume_below_deadzone_ignored() {
        assert!(map_axis(Axis::RightZ, 0.05).is_none());
    }

    #[test]
    fn dpad_center_ignored() {
        assert!(map_axis(Axis::DPadX, 0.0).is_none());
    }
    
    #[test]
    fn dpad_up_volume_up() {
        let ev = map_button(Button::DPadUp).unwrap();
        assert!(matches!(ev.action, Action::AudioVolumeStep { step: 0.05, .. }));
    }
    
    #[test]
    fn dpad_down_volume_down() {
        let ev = map_button(Button::DPadDown).unwrap();
        assert!(matches!(ev.action, Action::AudioVolumeStep { step: -0.05, .. }));
    }
}
