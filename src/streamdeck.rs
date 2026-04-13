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
use serde_json::Value as JsonValue;

// Assuming we have a controls module with these traits/types
use crate::controls::{ControlSurface, Control, ControlBinding, DisplayControl};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Scenario {
    pub id: String,
    pub name: String,
    pub color: String,
}

impl Scenario {
    pub fn from_map(scenario_map: &HashMap<String, JsonValue>) -> Self {
        Self {
            id: scenario_map.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            name: scenario_map.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            color: scenario_map.get("color").and_then(|v| v.as_str()).unwrap_or("#888888").to_string(),
        }
    }
}

pub struct StreamDeckSurface {
    device: Arc<Mutex<streamdeck::StreamDeck>>,
    is_visual: bool,
    key_count: usize,
    surface_id: String,
    controls: HashMap<String, Control>,
    scenarios: Vec<Scenario>,
    active_scenario_id: Option<String>,
    on_changed: Option<Box<dyn Fn(String, String, String) -> Box<dyn std::future::Future<Output = ()> + Send> + Send + Sync>>,
}

impl StreamDeckSurface {
    pub fn new(device: streamdeck::StreamDeck, surface_id: Option<String>) -> Result<Self> {
        let device_type = device.get_device_type();
        let is_visual = device_type.is_visual();
        let key_count = device.key_count();
        
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
            on_changed: None,
        };
        
        surface.build_controls();
        
        Ok(surface)
    }
    
    fn build_controls(&mut self) {
        if !self.is_visual {
            // Pedal mode: 3 keys → prev / next / mute
            self.controls.insert(
                "pedal_left".to_string(),
                Control {
                    name: "pedal_left".to_string(),
                    surface_id: self.surface_id.clone(),
                    binding: ControlBinding {
                        action: "scenario.next".to_string(),
                        value: Some(-1),
                        target: None,
                    },
                    displays: HashMap::new(),
                },
            );
            
            self.controls.insert(
                "pedal_middle".to_string(),
                Control {
                    name: "pedal_middle".to_string(),
                    surface_id: self.surface_id.clone(),
                    binding: ControlBinding {
                        action: "audio.mute".to_string(),
                        value: None,
                        target: Some("@active".to_string()),
                    },
                    displays: HashMap::new(),
                },
            );
            
            self.controls.insert(
                "pedal_right".to_string(),
                Control {
                    name: "pedal_right".to_string(),
                    surface_id: self.surface_id.clone(),
                    binding: ControlBinding {
                        action: "scenario.next".to_string(),
                        value: Some(1),
                        target: None,
                    },
                    displays: HashMap::new(),
                },
            );
        } else {
            // Visual mode: keys map to scenarios (built dynamically)
            for i in 0..self.key_count {
                let control_name = format!("key_{}", i);
                self.controls.insert(
                    control_name.clone(),
                    Control {
                        name: control_name,
                        surface_id: self.surface_id.clone(),
                        binding: ControlBinding {
                            action: "scenario.activate".to_string(),
                            value: None,
                            target: None,
                        },
                        displays: HashMap::new(),
                    },
                );
            }
        }
    }
    
    pub async fn start(&mut self) -> Result<()> {
        let device = self.device.clone();
        
        // Open and configure device
        {
            let mut dev = device.lock().await;
            dev.open()
                .map_err(|e| anyhow!("Failed to open Stream Deck: {}", e))?;
            dev.reset()
                .map_err(|e| anyhow!("Failed to reset Stream Deck: {}", e))?;
            dev.set_brightness(60)
                .map_err(|e| anyhow!("Failed to set brightness: {}", e))?;
        }
        
        // Set up key callback
        let device_clone = self.device.clone();
        let surface_id = self.surface_id.clone();
        let is_visual = self.is_visual;
        let key_count = self.key_count;
        let on_changed = self.on_changed.clone();
        let scenarios = self.scenarios.clone();
        let active_scenario_id = self.active_scenario_id.clone();
        
        let callback = move |key: usize, state: bool| {
            if !state {  // Only handle key press (not release)
                return;
            }
            
            let on_changed = on_changed.clone();
            let scenarios = scenarios.clone();
            let active_scenario_id = active_scenario_id.clone();
            let surface_id = surface_id.clone();
            
            tokio::spawn(async move {
                if let Some(on_changed) = on_changed {
                    if !is_visual {
                        // Pedal mode
                        let names = ["pedal_left", "pedal_middle", "pedal_right"];
                        if key < names.len() {
                            let _ = on_changed(surface_id, names[key].to_string(), "true".to_string()).await;
                        }
                    } else {
                        // Visual mode: key index → scenario
                        if key < scenarios.len() {
                            let scenario_id = &scenarios[key].id;
                            let _ = on_changed(surface_id, format!("key_{}", key), scenario_id.clone()).await;
                        }
                    }
                }
            });
        };
        
        {
            let mut dev = device.lock().await;
            dev.set_key_callback(Box::new(callback))
                .map_err(|e| anyhow!("Failed to set key callback: {}", e))?;
        }
        
        info!(
            "Stream Deck started: {:?} ({} keys, visual={})",
            {
                let dev = device.lock().await;
                dev.get_device_type()
            },
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
        let _ = device.reset();
        let _ = device.close();
        Ok(())
    }
    
    pub fn set_on_changed<F, Fut>(&mut self, callback: F)
    where
        F: Fn(String, String, String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.on_changed = Some(Box::new(move |surface_id, control_name, value| {
            Box::new(callback(surface_id, control_name, value))
        }));
    }
    
    pub fn update_scenarios(&mut self, scenarios: Vec<HashMap<String, JsonValue>>, active_id: Option<String>) {
        self.scenarios = scenarios.iter().map(|s| Scenario::from_map(s)).collect();
        self.active_scenario_id = active_id;
        
        // Re-render all keys in async context
        let surface = self.device.clone();
        let scenarios_clone = self.scenarios.clone();
        let active_id_clone = self.active_scenario_id.clone();
        let is_visual = self.is_visual;
        let key_count = self.key_count;
        
        tokio::spawn(async move {
            if is_visual {
                Self::render_all_keys_internal(surface, scenarios_clone, active_id_clone, key_count).await;
            }
        });
    }
    
    async fn render_all_keys_internal(
        device: Arc<Mutex<streamdeck::StreamDeck>>,
        scenarios: Vec<Scenario>,
        active_scenario_id: Option<String>,
        key_count: usize,
    ) {
        // This would need actual image rendering implementation
        // For now, we'll just log that rendering would happen
        debug!("Would render {} keys for {} scenarios", key_count, scenarios.len());
    }
    
    async fn render_all_keys(&self) -> Result<()> {
        if !self.is_visual {
            return Ok(());
        }
        
        Self::render_all_keys_internal(
            self.device.clone(),
            self.scenarios.clone(),
            self.active_scenario_id.clone(),
            self.key_count,
        ).await;
        
        Ok(())
    }
    
    fn render_scenario_key(&self, scenario: &Scenario, is_active: bool) -> Result<Vec<u8>> {
        // Placeholder implementation - would need actual image generation
        Ok(vec![0; 1024]) // Placeholder image data
    }
    
    fn render_blank_key(&self) -> Result<Vec<u8>> {
        // Placeholder implementation - would need actual image generation
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
    
    fn displays(&self) -> &HashMap<String, DisplayControl> {
        &HashMap::new() // No display controls for StreamDeck
    }
    
    fn to_dict(&self) -> HashMap<String, JsonValue> {
        let mut map = HashMap::new();
        map.insert("deck_type".to_string(), JsonValue::String("unknown".to_string()));
        map.insert("key_count".to_string(), JsonValue::Number(self.key_count.into()));
        map.insert("visual".to_string(), JsonValue::Bool(self.is_visual));
        map
    }
}

pub fn discover_streamdecks() -> Vec<streamdeck::Device> {
    match streamdeck::Device::enumerate() {
        Ok(devices) => devices,
        Err(e) => {
            debug!("Stream Deck enumeration failed: {}", e);
            vec![]
        }
    }
}
