// SPDX-License-Identifier: AGPL-3.0-only
//! Config-driven evdev control surface.
//!
//! Mirrors `EvdevSurface` in `controller/evdev_surface.py`.
//!
//! Example config (JSON):
//! ```json
//! {
//!   "id": "shuttle",
//!   "device": "ShuttlePRO",
//!   "grab": true,
//!   "buttons": {
//!     "260": { "action": "scenario.next", "value": -1 }
//!   },
//!   "axes": {
//!     "7": { "action": "audio.volume", "target": "@active", "min": 0, "max": 255 }
//!   },
//!   "rel_axes": {
//!     "7": { "action": "scenario.next" }
//!   }
//! }
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use evdev::{AbsoluteAxisType, Device, InputEventKind, RelativeAxisType};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::control_surface::{ControlBinding, ControlEvent, ControlInfo, ControlSurface};

// ── Device discovery ─────────────────────────────────────────────────────────

/// Find the first `/dev/input/event*` device whose name contains `pattern`
/// (case-insensitive).
///
/// Mirrors `find_evdev_device()` in `controller/evdev_surface.py`.
fn find_device(pattern: &str) -> Option<std::path::PathBuf> {
    let pattern_lower = pattern.to_lowercase();
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(dev) = Device::open(&path) {
            if dev
                .name()
                .unwrap_or("")
                .to_lowercase()
                .contains(&pattern_lower)
            {
                return Some(path);
            }
        }
    }
    None
}

// ── Internal mapping types ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ButtonMapping {
    name: String,
    binding: ControlBinding,
}

#[derive(Debug, Clone)]
struct AbsMapping {
    name: String,
    binding: ControlBinding,
    min: f64,
    max: f64,
}

#[derive(Debug, Clone)]
struct RelMapping {
    name: String,
    binding: ControlBinding,
}

// ── EvdevSurface ──────────────────────────────────────────────────────────────

/// Config-driven evdev control surface.
///
/// Mirrors `EvdevSurface` in `controller/evdev_surface.py`.
pub struct EvdevSurface {
    id: String,
    device_pattern: String,
    grab: bool,

    button_map: HashMap<u16, ButtonMapping>,
    abs_map: HashMap<u16, AbsMapping>,
    rel_map: HashMap<u16, RelMapping>,

    controls_meta: HashMap<String, ControlInfo>,

    event_tx: mpsc::Sender<ControlEvent>,
    event_rx: mpsc::Receiver<ControlEvent>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EvdevSurface {
    /// Construct from a surface ID and JSON config object.
    pub fn new(surface_id: impl Into<String>, config: &serde_json::Value) -> Self {
        let id = surface_id.into();
        let device_pattern = config
            .get("device")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let grab = config.get("grab").and_then(Value::as_bool).unwrap_or(false);

        let mut button_map: HashMap<u16, ButtonMapping> = HashMap::new();
        let mut abs_map: HashMap<u16, AbsMapping> = HashMap::new();
        let mut rel_map: HashMap<u16, RelMapping> = HashMap::new();
        let mut controls_meta: HashMap<String, ControlInfo> = HashMap::new();

        // Parse button mappings
        if let Some(buttons) = config.get("buttons").and_then(Value::as_object) {
            for (code_str, binding_cfg) in buttons {
                if let Ok(code) = code_str.parse::<u16>() {
                    let name = format!("btn_{code}");
                    let binding = parse_binding(binding_cfg);
                    button_map.insert(
                        code,
                        ButtonMapping { name: name.clone(), binding: binding.clone() },
                    );
                    controls_meta.insert(
                        name.clone(),
                        ControlInfo { name, surface_id: id.clone(), binding },
                    );
                }
            }
        }

        // Parse absolute axis mappings
        if let Some(axes) = config.get("axes").and_then(Value::as_object) {
            for (code_str, axis_cfg) in axes {
                if let Ok(code) = code_str.parse::<u16>() {
                    let name = format!("abs_{code}");
                    let binding = parse_binding(axis_cfg);
                    let min = axis_cfg.get("min").and_then(Value::as_f64).unwrap_or(0.0);
                    let max = axis_cfg.get("max").and_then(Value::as_f64).unwrap_or(255.0);
                    abs_map.insert(
                        code,
                        AbsMapping { name: name.clone(), binding: binding.clone(), min, max },
                    );
                    controls_meta.insert(
                        name.clone(),
                        ControlInfo { name, surface_id: id.clone(), binding },
                    );
                }
            }
        }

        // Parse relative axis mappings (jog wheels, scroll rings)
        if let Some(rel_axes) = config.get("rel_axes").and_then(Value::as_object) {
            for (code_str, rel_cfg) in rel_axes {
                if let Ok(code) = code_str.parse::<u16>() {
                    let name = format!("rel_{code}");
                    let binding = parse_binding(rel_cfg);
                    rel_map.insert(
                        code,
                        RelMapping { name: name.clone(), binding: binding.clone() },
                    );
                    controls_meta.insert(
                        name.clone(),
                        ControlInfo { name, surface_id: id.clone(), binding },
                    );
                }
            }
        }

        let (event_tx, event_rx) = mpsc::channel(256);

        Self {
            id,
            device_pattern,
            grab,
            button_map,
            abs_map,
            rel_map,
            controls_meta,
            event_tx,
            event_rx,
            task: None,
        }
    }
}

fn parse_binding(cfg: &Value) -> ControlBinding {
    ControlBinding {
        action: cfg.get("action").and_then(Value::as_str).unwrap_or("").to_string(),
        target: cfg.get("target").and_then(Value::as_str).unwrap_or("").to_string(),
        value: cfg.get("value").cloned(),
    }
}

#[async_trait]
impl ControlSurface for EvdevSurface {
    fn id(&self) -> &str {
        &self.id
    }

    fn controls(&self) -> HashMap<String, ControlInfo> {
        self.controls_meta.clone()
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        let path = match find_device(&self.device_pattern) {
            Some(p) => p,
            None => {
                warn!(
                    "evdev surface '{}': device matching '{}' not found",
                    self.id, self.device_pattern
                );
                return Ok(());
            }
        };

        let mut dev = Device::open(&path)?;

        if self.grab {
            if let Err(e) = dev.grab() {
                warn!("Could not grab {:?}: {}", path, e);
            }
        }

        // Refine ABS axis ranges from actual device absinfo
        if let Ok(abs_state) = dev.get_absinfo() {
            for (code, mapping) in self.abs_map.iter_mut() {
                if let Some(info) = abs_state.get(*code as usize) {
                    mapping.min = info.minimum() as f64;
                    mapping.max = info.maximum() as f64;
                }
            }
        }

        let surface_id = self.id.clone();
        let button_map = self.button_map.clone();
        let abs_map = self.abs_map.clone();
        let rel_map = self.rel_map.clone();
        let tx = self.event_tx.clone();

        info!(
            "evdev surface '{}' started: {} ({:?}) — {} buttons, {} axes, {} rel",
            self.id,
            dev.name().unwrap_or("?"),
            path,
            button_map.len(),
            abs_map.len(),
            rel_map.len(),
        );

        let task = tokio::spawn(async move {
            let mut stream = match dev.into_event_stream() {
                Ok(s) => s,
                Err(e) => {
                    warn!("evdev surface '{}': failed to open event stream: {}", surface_id, e);
                    return;
                }
            };

            loop {
                let raw = match stream.next_event().await {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("evdev surface '{}' read error: {}", surface_id, e);
                        break;
                    }
                };

                let evt: Option<ControlEvent> = match raw.kind() {
                    // Button press only (value == 1); ignore release and repeat
                    InputEventKind::Key(key) if raw.value() == 1 => {
                        let code = key.code();
                        button_map.get(&code).map(|m| {
                            let val = m.binding.value.clone().unwrap_or(Value::Bool(true));
                            ControlEvent {
                                surface_id: surface_id.clone(),
                                control_name: m.name.clone(),
                                value: val,
                                binding: m.binding.clone(),
                            }
                        })
                    }
                    InputEventKind::AbsAxis(axis) => {
                        let code = axis.0;
                        abs_map.get(&code).map(|m| {
                            let normalized = if (m.max - m.min).abs() > f64::EPSILON {
                                (raw.value() as f64 - m.min) / (m.max - m.min)
                            } else {
                                0.0
                            };
                            ControlEvent {
                                surface_id: surface_id.clone(),
                                control_name: m.name.clone(),
                                value: Value::from(normalized),
                                binding: m.binding.clone(),
                            }
                        })
                    }
                    InputEventKind::RelAxis(axis) => {
                        let code = axis.0;
                        rel_map.get(&code).map(|m| {
                            // Positive = forward/right, negative = backward/left
                            let direction: i64 = if raw.value() > 0 { 1 } else { -1 };
                            ControlEvent {
                                surface_id: surface_id.clone(),
                                control_name: m.name.clone(),
                                value: Value::from(direction),
                                binding: m.binding.clone(),
                            }
                        })
                    }
                    _ => None,
                };

                if let Some(event) = evt {
                    if tx.send(event).await.is_err() {
                        break; // receiver dropped
                    }
                }
            }
        });

        self.task = Some(task);
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Option<ControlEvent> {
        self.event_rx.recv().await
    }

    fn to_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "device_pattern": self.device_pattern,
            "controls": self.controls_meta.keys().collect::<Vec<_>>(),
        })
    }
}
