//! Control surface detection and per-surface event loop.
//!
//! Each detected surface runs in its own `tokio::spawn`-ed task and publishes
//! [`ControlEvent`] JSON strings to the shared broadcast channel.
//!
//! New surface backends (MIDI, HID, Stream Deck, …) are added here by
//! implementing [`Surface`] and registering them in [`detect`].

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ── Event types ──────────────────────────────────────────────────────────────

/// A single control-change event emitted by a surface task.
///
/// Serialised as newline-delimited JSON over the IPC socket.
/// The Python controller deserialises this and calls
/// `ControlManager.on_control_changed(surface_id, control_name, value)`.
#[derive(Debug, Clone, Serialize)]
pub struct ControlEvent {
    /// Stable surface identifier (e.g. `"midi:0"`, `"hid:vendor:product"`).
    pub surface_id: String,
    /// Control name within the surface (e.g. `"fader_0"`, `"button_1"`).
    pub control_name: String,
    /// Current value.  Floats for faders (0.0–1.0), bools for buttons.
    pub value: ControlValue,
    /// Monotonic event sequence number (per surface, wraps at u64::MAX).
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ControlValue {
    Float(f64),
    Bool(bool),
    Int(i64),
    Text(String),
}

// ── Surface trait ─────────────────────────────────────────────────────────────

pub struct Surface {
    pub id: String,
    pub kind: SurfaceKind,
}

#[derive(Debug, Clone)]
pub enum SurfaceKind {
    /// Stub / virtual surface used for testing the IPC pipeline.
    Virtual { name: String },
    // Future: Midi { port_index: usize },
    // Future: Hid  { vendor: u16, product: u16 },
    // Future: StreamDeck { serial: String },
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// Detect available control surfaces.
///
/// Returns a list of [`Surface`] descriptors.  Each will be handed to
/// [`run`] in its own task.
pub async fn detect() -> Vec<Surface> {
    let mut surfaces = Vec::new();

    // Virtual surface — always present; useful for smoke-testing the IPC path.
    if std::env::var("OZMA_VIRTUAL_SURFACE").is_ok() {
        let id = format!("virtual:{}", &Uuid::new_v4().to_string()[..8]);
        info!("Detected virtual surface: {id}");
        surfaces.push(Surface {
            id,
            kind: SurfaceKind::Virtual {
                name: "ozma-virtual".into(),
            },
        });
    }

    // TODO: MIDI detection via midir
    // TODO: HID detection via hidapi
    // TODO: Stream Deck detection via elgato-streamdeck

    surfaces
}

// ── Per-surface event loop ────────────────────────────────────────────────────

/// Run a surface's event loop, publishing [`ControlEvent`]s to `tx`.
///
/// This function is `async` and is expected to run for the lifetime of the
/// daemon.  It should handle device reconnection internally.
pub async fn run(surface: Surface, tx: broadcast::Sender<String>) {
    info!("Surface task started: {} ({:?})", surface.id, surface.kind);

    match surface.kind {
        SurfaceKind::Virtual { ref name } => {
            run_virtual(&surface.id, name, tx).await;
        }
    }

    warn!("Surface task exited: {}", surface.id);
}

// ── Virtual surface (smoke-test / demo) ──────────────────────────────────────

async fn run_virtual(surface_id: &str, _name: &str, tx: broadcast::Sender<String>) {
    use std::time::Duration;
    use tokio::time::sleep;

    let mut seq: u64 = 0;

    // Emit a synthetic button-press every 5 seconds so the IPC pipeline can
    // be verified end-to-end without real hardware.
    loop {
        sleep(Duration::from_secs(5)).await;

        let event = ControlEvent {
            surface_id: surface_id.to_owned(),
            control_name: "button_0".to_owned(),
            value: ControlValue::Bool(true),
            seq,
        };
        seq = seq.wrapping_add(1);

        match serde_json::to_string(&event) {
            Ok(json) => {
                debug!("Virtual surface event: {json}");
                // Ignore send errors — no clients connected yet is fine.
                let _ = tx.send(json);
            }
            Err(e) => warn!("Failed to serialise event: {e}"),
        }

        // Emit button-release immediately after.
        let release = ControlEvent {
            surface_id: surface_id.to_owned(),
            control_name: "button_0".to_owned(),
            value: ControlValue::Bool(false),
            seq,
        };
        seq = seq.wrapping_add(1);

        if let Ok(json) = serde_json::to_string(&release) {
            let _ = tx.send(json);
        }
    }
}
