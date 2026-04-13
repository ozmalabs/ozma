// SPDX-License-Identifier: AGPL-3.0-only
//! Gamepad control surface driver — gilrs backend.
//!
//! Mirrors `controller/gamepad.py` but runs in async Rust via gilrs.
//! Supports Xbox, PlayStation, Nintendo, and generic controllers.
//! Hotplug (connect / disconnect) is handled automatically by gilrs.
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────────────────────────────────────────────────┐
//!  │  GamepadDriver  (one per process)                    │
//!  │   ├─ gilrs::Gilrs  (polls OS gamepad events)         │
//!  │   └─ HashMap<GamepadId, GamepadSurface>              │
//!  │        └─ profile + force-feedback handle            │
//!  └──────────────────────────────────────────────────────┘
//!          │  ControlEvent
//!          ▼
//!   caller-supplied async callback  (→ ControlManager / IPC)
//! ```

pub mod mapping;
pub mod profile;

use std::collections::HashMap;
use std::time::Duration;

use gilrs::{
    ev::filter::{axis_dpad_to_button, deadzone, Jitter},
    EventType, Filter, Gilrs, GamepadId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

use mapping::ControlEvent;
use profile::{detect_profile, ControllerProfile};

// ── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum GamepadError {
    #[error("gilrs initialisation failed: {0}")]
    Init(#[from] gilrs::Error),
}

// ── GamepadSurface ────────────────────────────────────────────────────────────

/// Per-gamepad state tracked by the driver.
#[derive(Debug, Serialize, Deserialize)]
pub struct GamepadSurface {
    pub surface_id:  String,
    pub device_name: String,
    pub profile:     ControllerProfile,
}

impl GamepadSurface {
    fn new(name: &str) -> Self {
        let profile = detect_profile(name);
        let surface_id = profile.surface_id();
        Self {
            surface_id,
            device_name: name.to_owned(),
            profile,
        }
    }

    /// Serialise to a JSON-friendly dict (mirrors `GamepadSurface.to_dict()`).
    pub fn to_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "id":     self.surface_id,
            "device": self.device_name,
            "profile": {
                "family":  format!("{:?}", self.profile.family).to_lowercase(),
                "variant": self.profile.variant,
                "labels": {
                    "south": self.profile.south_label,
                    "east":  self.profile.east_label,
                    "north": self.profile.north_label,
                    "west":  self.profile.west_label,
                }
            }
        })
    }
}

// ── GamepadDriver ─────────────────────────────────────────────────────────────

/// Callback type: receives `(surface_id, ControlEvent)`.
pub type EventCallback =
    Box<dyn Fn(String, ControlEvent) + Send + Sync>;

/// Central gamepad driver.  Call [`GamepadDriver::run`] to start the event loop.
pub struct GamepadDriver {
    gilrs:       Gilrs,
    surfaces:    HashMap<GamepadId, GamepadSurface>,
    callback:    Option<EventCallback>,
    /// Rumble duration for force-feedback confirmation pulses.
    ff_duration: Duration,
}

impl GamepadDriver {
    /// Create a new driver.  Enumerates already-connected gamepads immediately.
    pub fn new() -> Result<Self, GamepadError> {
        let gilrs = Gilrs::new()?;
        let mut surfaces = HashMap::new();

        for (id, gamepad) in gilrs.gamepads() {
            let surface = GamepadSurface::new(gamepad.name());
            info!(
                surface_id = %surface.surface_id,
                device     = %surface.device_name,
                "Gamepad connected (initial)"
            );
            surfaces.insert(id, surface);
        }

        Ok(Self {
            gilrs,
            surfaces,
            callback: None,
            ff_duration: Duration::from_millis(80),
        })
    }

    /// Register a callback that receives `(surface_id, ControlEvent)`.
    pub fn set_callback(&mut self, cb: EventCallback) {
        self.callback = Some(cb);
    }

    /// List currently connected surfaces.
    pub fn list(&self) -> Vec<&GamepadSurface> {
        self.surfaces.values().collect()
    }

    /// Run the event loop.  Blocks the calling thread — run inside
    /// `tokio::task::spawn_blocking` or a dedicated OS thread.
    pub fn run(&mut self) {
        let jitter = Jitter::new();

        loop {
            while let Some(event) = self
                .gilrs
                .next_event()
                .filter_ev(&axis_dpad_to_button, &mut self.gilrs)
                .filter_ev(&jitter, &mut self.gilrs)
                .filter_ev(&deadzone, &mut self.gilrs)
            {
                self.gilrs.update(&event);
                self.handle_event(event);
            }

            std::thread::sleep(Duration::from_millis(4));
        }
    }

    // ── internal ─────────────────────────────────────────────────────────────

    fn handle_event(&mut self, event: gilrs::Event) {
        let id = event.id;

        match event.event {
            // ── Hotplug ───────────────────────────────────────────────────
            EventType::Connected => {
                if let Some(gamepad) = self.gilrs.connected_gamepad(id) {
                    let surface = GamepadSurface::new(gamepad.name());
                    info!(
                        surface_id = %surface.surface_id,
                        device     = %surface.device_name,
                        "Gamepad connected"
                    );
                    self.surfaces.insert(id, surface);
                }
            }
            EventType::Disconnected => {
                if let Some(surface) = self.surfaces.remove(&id) {
                    warn!(
                        surface_id = %surface.surface_id,
                        device     = %surface.device_name,
                        "Gamepad disconnected"
                    );
                }
            }

            // ── Button press ──────────────────────────────────────────────
            EventType::ButtonPressed(button, _code) => {
                if let Some(ev) = mapping::map_button(button) {
                    debug!(button = ?button, control = %ev.control, "Button press");
                    self.dispatch(id, ev);
                }
            }

            // ── Axis change ───────────────────────────────────────────────
            EventType::AxisChanged(axis, value, _code) => {
                // Apply deadzone filtering for all axes
                let filtered_value = match axis {
                    Axis::RightZ | Axis::LeftZ => {
                        // Triggers: 0.0 to 1.0 range
                        if value.abs() < mapping::TRIGGER_DEADZONE {
                            0.0
                        } else {
                            value
                        }
                    }
                    _ => {
                        // Sticks and D-pad: -1.0 to 1.0 range
                        if value.abs() < 0.15 {
                            0.0
                        } else {
                            value
                        }
                    }
                };

                if filtered_value != 0.0 {
                    if let Some(ev) = mapping::map_axis(axis, filtered_value) {
                        debug!(axis = ?axis, value = filtered_value, control = %ev.control, "Axis change");
                        self.dispatch(id, ev);
                    }
                }
            }

            // ButtonReleased, ButtonRepeated, ButtonChanged — ignored
            _ => {}
        }
    }

    fn dispatch(&self, id: GamepadId, ev: ControlEvent) {
        let surface_id = match self.surfaces.get(&id) {
            Some(s) => s.surface_id.clone(),
            None => return,
        };

        if let Some(cb) = &self.callback {
            cb(surface_id, ev);
        }
    }

    // ── Force feedback ────────────────────────────────────────────────────────

    /// Send a short rumble pulse to the gamepad (confirmation feedback).
    ///
    /// Silently ignores gamepads that don't support force feedback.
    pub fn rumble(&mut self, id: GamepadId, strong: f32, weak: f32) {
        use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Replay, Ticks};

        let gamepad = match self.gilrs.connected_gamepad(id) {
            Some(g) => g,
            None => return,
        };

        if !gamepad.is_ff_supported() {
            return;
        }

        let duration_ticks = Ticks::from_ms(self.ff_duration.as_millis() as u32);

        let effect = EffectBuilder::new()
            .add_effect(BaseEffect {
                kind: BaseEffectType::Strong { magnitude: (strong * 65535.0) as u16 },
                scheduling: Replay {
                    play_for: duration_ticks,
                    ..Default::default()
                },
                ..Default::default()
            })
            .add_effect(BaseEffect {
                kind: BaseEffectType::Weak { magnitude: (weak * 65535.0) as u16 },
                scheduling: Replay {
                    play_for: duration_ticks,
                    ..Default::default()
                },
                ..Default::default()
            })
            .gamepads(&[id])
            .finish(&mut self.gilrs);

        match effect {
            Ok(e) => {
                if let Err(err) = e.play() {
                    debug!("FF play error: {err}");
                }
            }
            Err(err) => debug!("FF build error: {err}"),
        }
    }
}

impl Default for GamepadDriver {
    fn default() -> Self {
        Self::new().expect("gilrs init")
    }
}
