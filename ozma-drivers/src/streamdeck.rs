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

use anyhow::Result;
use elgato_streamdeck::{list_devices, StreamDeck};
use hidapi::HidApi;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Represents a control on the Stream Deck surface
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDeckControl {
    pub name: String,
    pub surface_id: String,
    pub binding: ControlBinding,
}

/// Control binding information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlBinding {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Stream Deck control surface implementation
pub struct StreamDeckSurface {
    deck: Arc<Mutex<StreamDeck>>,
    surface_id: String,
    is_visual: bool,
    key_count: usize,
    controls: HashMap<String, StreamDeckControl>,
    scenarios: Vec<ScenarioInfo>,
    active_scenario_id: Option<String>,
}

/// Information about a scenario for display purposes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioInfo {
    pub id: String,
    pub name: String,
    pub color: String,
}

impl StreamDeckSurface {
    /// Create a new StreamDeckSurface by opening the first available device
    pub async fn new_first_device() -> Result<Self> {
        let hidapi = HidApi::new()?;
        
        // Open first Stream Deck device using 0.5 API
        let deck = StreamDeck::open_first_device(&hidapi)?;
        
        let key_count = deck.key_count();
        
        // Determine surface_id from key count (0.5 API removed DeviceType)
        let surface_id = match key_count {
            6 => "streamdeck-mini".to_string(),
            15 => "streamdeck-original".to_string(),
            32 => "streamdeck-xl".to_string(),
            3 => "streamdeck-pedal".to_string(),
            _ => format!("streamdeck-{}", key_count),
        };

        // Visual devices have displays (Mini, Original, XL), Pedal does not
        let is_visual = key_count != 3;

        let mut surface = Self {
            deck: Arc::new(Mutex::new(deck)),
            surface_id,
            is_visual,
            key_count,
            controls: HashMap::new(),
            scenarios: Vec::new(),
            active_scenario_id: None,
        };

        // Build controls based on device type
        if !is_visual {
            // Pedal mode: 3 keys → prev / mute / next
            surface.build_pedal_controls();
        } else {
            // Visual mode: keys map to scenarios
            for i in 0..key_count {
                let ctrl = StreamDeckControl {
                    name: format!("key_{}", i),
                    surface_id: surface.surface_id.clone(),
                    binding: ControlBinding {
                        action: "scenario.activate".to_string(),
                        value: None,
                        target: None,
                    },
                };
                surface.controls.insert(format!("key_{}", i), ctrl);
            }
        }

        Ok(surface)
    }

    /// Start the Stream Deck surface
    pub async fn start(&mut self) -> Result<()> {
        let deck = self.deck.clone();
        
        // Initialize and reset the device
        tokio::task::spawn_blocking(move || {
            let mut deck = deck.blocking_lock();
            deck.reset()?;
            deck.set_brightness(60)?;
            Ok::<(), anyhow::Error>(())
        }).await??;

        tracing::info!(
            "Stream Deck started: {} ({} keys, visual={})",
            self.surface_id,
            self.key_count,
            self.is_visual
        );

        Ok(())
    }

    /// Stop the Stream Deck surface
    pub async fn stop(&mut self) -> Result<()> {
        let deck = self.deck.clone();
        tokio::task::spawn_blocking(move || {
            let mut deck = deck.blocking_lock();
            deck.reset().ok(); // Ignore errors on reset
            Ok::<(), anyhow::Error>(())
        }).await??;
        
        Ok(())
    }

    /// Build controls for the Stream Deck Pedal
    fn build_pedal_controls(&mut self) {
        self.controls.insert(
            "pedal_left".to_string(),
            StreamDeckControl {
                name: "pedal_left".to_string(),
                surface_id: self.surface_id.clone(),
                binding: ControlBinding {
                    action: "scenario.next".to_string(),
                    value: Some(serde_json::Value::Number((-1).into())),
                    target: None,
                },
            },
        );

        self.controls.insert(
            "pedal_middle".to_string(),
            StreamDeckControl {
                name: "pedal_middle".to_string(),
                surface_id: self.surface_id.clone(),
                binding: ControlBinding {
                    action: "audio.mute".to_string(),
                    value: None,
                    target: Some("@active".to_string()),
                },
            },
        );

        self.controls.insert(
            "pedal_right".to_string(),
            StreamDeckControl {
                name: "pedal_right".to_string(),
                surface_id: self.surface_id.clone(),
                binding: ControlBinding {
                    action: "scenario.next".to_string(),
                    value: Some(serde_json::Value::Number(1.into())),
                    target: None,
                },
            },
        );
    }

    /// Update scenarios and re-render all keys
    pub async fn update_scenarios(&mut self, scenarios: Vec<ScenarioInfo>, active_id: Option<String>) -> Result<()> {
        self.scenarios = scenarios;
        self.active_scenario_id = active_id;
        
        if self.is_visual {
            self.render_all_keys().await?;
        }
        
        Ok(())
    }

    /// Render scenario info onto all keys
    async fn render_all_keys(&self) -> Result<()> {
        // This would be the implementation for rendering key images
        // For now, we'll just log that it would happen
        tracing::debug!("Rendering {} keys for {} scenarios", self.key_count, self.scenarios.len());
        Ok(())
    }

    /// Get the surface ID
    pub fn id(&self) -> &str {
        &self.surface_id
    }

    /// Get controls
    pub fn controls(&self) -> &HashMap<String, StreamDeckControl> {
        &self.controls
    }

    /// Convert to serializable dictionary
    pub fn to_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.surface_id,
            "key_count": self.key_count,
            "visual": self.is_visual,
            "controls": self.controls
        })
    }
}

/// Discover and connect to Stream Deck devices
pub fn discover_streamdecks() -> Result<Vec<StreamDeck>> {
    let mut devices = Vec::new();
    
    let hidapi = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            tracing::debug!("Failed to initialize HID API: {}", e);
            return Ok(devices);
        }
    };
    
    match list_devices(&hidapi) {
        Ok(deck_devices) => {
            for deck in deck_devices {
                devices.push(deck);
            }
        }
        Err(e) => tracing::debug!("Stream Deck enumeration failed: {}", e),
    }
    
    Ok(devices)
}
