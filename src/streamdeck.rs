// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//!
//! Elgato Stream Deck control surface driver for ozma.
//!
//! Maps each key to a scenario. Key images show the scenario name and colour.
//! The active scenario's key is highlighted. Pressing a key activates that
//! scenario.
//!
//! Supports all Stream Deck models:
//!   - Stream Deck Mini (6 keys)
//!   - Stream Deck Original / V2 (15 keys)
//!   - Stream Deck XL (32 keys)
//!   - Stream Deck Pedal (3 foot switches, no display)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use anyhow::{Result, anyhow};
use tracing::{info, debug, warn};

// Assuming we have a controls module with these traits/types
use crate::controls::{ControlSurface, Control, ControlBinding};

// We'll need to define these or import them appropriately
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    pub name: String,
    pub color: String,
}

pub struct StreamDeckSurface {
    device: Arc<Mutex<streamdeck::StreamDeck>>,
    is_visual: bool,
    key_count: usize,
    surface_id: String,
    controls: HashMap<String, Control>,
    scenarios: Vec<Scenario>,
    active_scenario_id: Option<String>,
}

impl StreamDeckSurface {
    pub fn new(device: streamdeck::StreamDeck, surface_id: Option<String>) -> Result<Self> {
        let device_type = device.get_device_type();
        let is_visual = device_type.is_visual();
        let key_count = device.get_key_count();
        
        let sid = surface_id.unwrap_or_else(|| {
            format!("streamdeck-{}", device_type.to_string().to_lowercase().replace(" ", "-"))
        });
        
        let mut surface = StreamDeckSurface {
            device: Arc::new(Mutex::new(device)),
            is_visual,
            key_count,
            surface_id: sid,
            controls: HashMap::new(),
            scenarios: Vec::new(),
            active_scenario_id: None,
        };
        
        surface.build_controls()?;
        
        Ok(surface)
    }
    
    fn build_controls(&mut self) -> Result<()> {
        if !self.is_visual {
            // Pedal mode: 3 keys → prev / next / mute
            self.controls.insert(
                "pedal_left".to_string(),
                Control::new(
                    "pedal_left".to_string(),
                    self.surface_id.clone(),
                    ControlBinding::new("scenario.next".to_string(), Some(-1), None),
                ),
            );
            
            self.controls.insert(
                "pedal_middle".to_string(),
                Control::new(
                    "pedal_middle".to_string(),
                    self.surface_id.clone(),
                    ControlBinding::new("audio.mute".to_string(), None, Some("@active".to_string())),
                ),
            );
            
            self.controls.insert(
                "pedal_right".to_string(),
                Control::new(
                    "pedal_right".to_string(),
                    self.surface_id.clone(),
                    ControlBinding::new("scenario.next".to_string(), Some(1), None),
                ),
            );
        } else {
            // Visual mode: keys map to scenarios (built dynamically)
            for i in 0..self.key_count {
                let control_name = format!("key_{}", i);
                self.controls.insert(
                    control_name.clone(),
                    Control::new(
                        control_name,
                        self.surface_id.clone(),
                        ControlBinding::new("scenario.activate".to_string(), None, None),
                    ),
                );
            }
        }
        
        Ok(())
    }
    
    pub async fn start(&mut self) -> Result<()> {
        let mut device = self.device.lock().await;
        
        // Reset and configure device
        device.reset()?;
        device.set_brightness(60)?;
        
        // Set up key callback
        let device_clone = self.device.clone();
        let surface_id = self.surface_id.clone();
        let is_visual = self.is_visual;
        let key_count = self.key_count;
        let scenarios = self.scenarios.clone();
        
        device.set_key_callback(Box::new(move |key, state| {
            // This would need to be handled in the async context
            // For now we'll just log the event
            info!("Key {} {} on StreamDeck {}", key, if state { "pressed" } else { "released" }, surface_id);
        }))?;
        
        info!(
            "Stream Deck started: {:?} ({} keys, visual={})",
            device.get_device_type(),
            self.key_count,
            self.is_visual
        );
        
        // Render initial key images if visual
        if self.is_visual {
            self.render_all_keys().await?;
        }
        
        Ok(())
    }
    
    pub async fn stop(&mut self) -> Result<()> {
        let mut device = self.device.lock().await;
        device.reset()?;
        Ok(())
    }
    
    pub fn update_scenarios(&mut self, scenarios: Vec<Scenario>, active_id: Option<String>) {
        self.scenarios = scenarios;
        self.active_scenario_id = active_id;
    }
    
    async fn render_all_keys(&self) -> Result<()> {
        if !self.is_visual {
            return Ok(());
        }
        
        let device = self.device.lock().await;
        
        for i in 0..self.key_count {
            if i < self.scenarios.len() {
                let scenario = &self.scenarios[i];
                let is_active = self.active_scenario_id.as_ref() == Some(&scenario.id);
                let image = self.render_scenario_key(scenario, is_active)?;
                device.set_key_image(i, &image)?;
            } else {
                let image = self.render_blank_key()?;
                device.set_key_image(i, &image)?;
            }
        }
        
        Ok(())
    }
    
    fn render_scenario_key(&self, scenario: &Scenario, is_active: bool) -> Result<Vec<u8>> {
        // This is a simplified implementation
        // In reality, we would create an image with PIL-like functionality
        // For now, we'll return a placeholder
        Ok(vec![0; 1024]) // Placeholder image data
    }
    
    fn render_blank_key(&self) -> Result<Vec<u8>> {
        // Return blank key image data
        Ok(vec![0; 1024]) // Placeholder image data
    }
}

#[async_trait::async_trait]
impl ControlSurface for StreamDeckSurface {
    fn id(&self) -> &str {
        &self.surface_id
    }
    
    fn controls(&self) -> &HashMap<String, Control> {
        &self.controls
    }
    
    fn to_dict(&self) -> HashMap<String, serde_json::Value> {
        let mut map = HashMap::new();
        map.insert("deck_type".to_string(), serde_json::Value::String("unknown".to_string()));
        map.insert("key_count".to_string(), serde_json::Value::Number(self.key_count.into()));
        map.insert("visual".to_string(), serde_json::Value::Bool(self.is_visual));
        map
    }
}

pub fn discover_streamdecks() -> Result<Vec<streamdeck::StreamDeck>> {
    match streamdeck::StreamDeck::enumerate() {
        Ok(devices) => Ok(devices),
        Err(e) => {
            debug!("Stream Deck enumeration failed: {}", e);
            Ok(vec![])
        }
    }
}
