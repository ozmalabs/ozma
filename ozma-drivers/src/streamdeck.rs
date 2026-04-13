//! Elgato Stream Deck control surface driver for ozma.
//!
//! Maps each key to a scenario. Key images show the scenario name and colour.
//! The active scenario's key is highlighted. Pressing a key activates that scenario.
//!
//! Supports all Stream Deck models:
//! - Stream Deck Mini (6 keys)
//! - Stream Deck Original / V2 (15 keys)
//! - Stream Deck XL (32 keys)
//! - Stream Deck Pedal (3 foot switches, no display)
//!
//! Uses elgato-streamdeck 0.5 API: `list_devices` + `StreamDeck::connect`.

use anyhow::Result;
use elgato_streamdeck::{info::Kind, list_devices, new_hidapi, StreamDeck};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Represents a control on the Stream Deck surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDeckControl {
    pub name: String,
    pub surface_id: String,
    pub binding: ControlBinding,
}

/// Control binding information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlBinding {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Stream Deck control surface.
pub struct StreamDeckSurface {
    deck: Arc<Mutex<StreamDeck>>,
    surface_id: String,
    is_visual: bool,
    key_count: usize,
    controls: HashMap<String, StreamDeckControl>,
    scenarios: Vec<ScenarioInfo>,
    active_scenario_id: Option<String>,
}

/// Scenario info for display on keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioInfo {
    pub id: String,
    pub name: String,
    pub color: String,
}

impl StreamDeckSurface {
    /// Create a new `StreamDeckSurface` wrapping an already-connected device.
    pub async fn new(deck: StreamDeck) -> Result<Self> {
        let kind = deck.kind();
        let key_count = kind.key_count() as usize;
        // Devices with key images are the visual variants (not Pedal/Plus encoder-only).
        let is_visual = key_count > 0
            && !matches!(kind, Kind::Pedal);

        let surface_id = format!(
            "streamdeck-{}",
            format!("{kind:?}").to_lowercase().replace(' ', "-")
        );

        let mut surface = Self {
            deck: Arc::new(Mutex::new(deck)),
            surface_id,
            is_visual,
            key_count,
            controls: HashMap::new(),
            scenarios: Vec::new(),
            active_scenario_id: None,
        };

        if !is_visual {
            surface.build_pedal_controls();
        } else {
            for i in 0..key_count {
                let ctrl = StreamDeckControl {
                    name: format!("key_{i}"),
                    surface_id: surface.surface_id.clone(),
                    binding: ControlBinding {
                        action: "scenario.activate".to_string(),
                        value: None,
                        target: None,
                    },
                };
                surface.controls.insert(format!("key_{i}"), ctrl);
            }
        }

        Ok(surface)
    }

    /// Initialise the device: reset + set brightness.
    pub async fn start(&mut self) -> Result<()> {
        let deck = self.deck.clone();
        tokio::task::spawn_blocking(move || {
            let deck = deck.blocking_lock();
            deck.reset()?;
            deck.set_brightness(60)?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        tracing::info!(
            surface_id = %self.surface_id,
            key_count  = self.key_count,
            visual     = self.is_visual,
            "Stream Deck started"
        );
        Ok(())
    }

    /// Reset the device cleanly before teardown.
    pub async fn stop(&mut self) -> Result<()> {
        let deck = self.deck.clone();
        tokio::task::spawn_blocking(move || {
            let deck = deck.blocking_lock();
            deck.reset().ok();
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    fn build_pedal_controls(&mut self) {
        for (name, action, value) in [
            ("pedal_left",   "scenario.next",  Some(serde_json::json!(-1))),
            ("pedal_middle", "audio.mute",     None),
            ("pedal_right",  "scenario.next",  Some(serde_json::json!(1))),
        ] {
            self.controls.insert(
                name.to_string(),
                StreamDeckControl {
                    name: name.to_string(),
                    surface_id: self.surface_id.clone(),
                    binding: ControlBinding {
                        action: action.to_string(),
                        value,
                        target: if action == "audio.mute" {
                            Some("@active".to_string())
                        } else {
                            None
                        },
                    },
                },
            );
        }
    }

    /// Push updated scenario list to the device (re-renders all keys).
    pub async fn update_scenarios(
        &mut self,
        scenarios: Vec<ScenarioInfo>,
        active_id: Option<String>,
    ) -> Result<()> {
        self.scenarios = scenarios;
        self.active_scenario_id = active_id;
        if self.is_visual {
            self.render_all_keys().await?;
        }
        Ok(())
    }

    async fn render_all_keys(&self) -> Result<()> {
        tracing::debug!(
            keys = self.key_count,
            scenarios = self.scenarios.len(),
            "Rendering Stream Deck keys"
        );
        // TODO: render scenario names/colours onto key images via set_button_image.
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.surface_id
    }

    pub fn controls(&self) -> &HashMap<String, StreamDeckControl> {
        &self.controls
    }

    pub fn to_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "id":        self.surface_id,
            "key_count": self.key_count,
            "visual":    self.is_visual,
            "controls":  self.controls,
        })
    }
}

/// Enumerate and connect to all attached Stream Deck devices.
pub fn discover_streamdecks() -> Result<Vec<StreamDeck>> {
    let hidapi = new_hidapi().map_err(|e| anyhow::anyhow!("hidapi init: {e}"))?;
    let mut decks = Vec::new();

    for (kind, serial) in list_devices(&hidapi) {
        match StreamDeck::connect(&hidapi, kind, &serial) {
            Ok(deck) => {
                tracing::info!(kind = ?kind, serial = %serial, "Stream Deck connected");
                decks.push(deck);
            }
            Err(e) => {
                tracing::warn!(kind = ?kind, serial = %serial, error = %e, "Failed to connect to Stream Deck");
            }
        }
    }

    Ok(decks)
}
