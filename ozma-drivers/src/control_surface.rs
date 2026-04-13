// SPDX-License-Identifier: AGPL-3.0-only
//! [`ControlSurface`] trait and associated types.
//!
//! Mirrors the Python `ControlSurface` / `ControlBinding` abstractions in
//! `controller/evdev_surface.py` and `controller/controls.py`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A binding from a physical control to an ozma action.
///
/// Mirrors `ControlBinding` in `controller/controls.py`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlBinding {
    /// Dotted action name, e.g. `"scenario.next"` or `"audio.volume"`.
    pub action: String,
    /// Optional target node/device, e.g. `"@active"`.
    #[serde(default)]
    pub target: String,
    /// Optional fixed value override (for buttons that always send a constant).
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

/// An event emitted by a [`ControlSurface`] when a control changes.
#[derive(Debug, Clone)]
pub struct ControlEvent {
    /// Surface that produced the event.
    pub surface_id: String,
    /// Control name within the surface (e.g. `"btn_260"`, `"abs_7"`).
    pub control_name: String,
    /// Current value of the control.
    pub value: serde_json::Value,
    /// The binding associated with this control.
    pub binding: ControlBinding,
}

/// Metadata for a single control on a surface.
#[derive(Debug, Clone)]
pub struct ControlInfo {
    pub name: String,
    pub surface_id: String,
    pub binding: ControlBinding,
}

/// Trait implemented by every input surface driver.
///
/// Mirrors the Python `ControlSurface` base class in
/// `controller/evdev_surface.py`.
#[async_trait]
pub trait ControlSurface: Send + Sync {
    /// Unique identifier for this surface (from config).
    fn id(&self) -> &str;

    /// Human-readable name (defaults to `id()`).
    fn name(&self) -> &str {
        self.id()
    }

    /// All controls exposed by this surface.
    fn controls(&self) -> HashMap<String, ControlInfo>;

    /// Start the surface (open device, spawn tasks).
    async fn start(&mut self) -> anyhow::Result<()>;

    /// Stop the surface (cancel tasks, release device).
    async fn stop(&mut self) -> anyhow::Result<()>;

    /// Receive the next control event.  Returns `None` when the surface
    /// has shut down.
    async fn next_event(&mut self) -> Option<ControlEvent>;

    /// Serialise surface state for the REST API.
    fn to_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id(),
            "name": self.name(),
            "controls": self.controls().keys().collect::<Vec<_>>(),
        })
    }
}
