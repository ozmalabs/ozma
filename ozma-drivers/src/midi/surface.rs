//! MIDI surface implementation that integrates with ozma ControlSurface

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use midir::{MidiInput, MidiOutput, MidiInputConnection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use futures::Future;
use tokio::sync::mpsc;
use log::{info, warn, debug};

use crate::controls::ControlSurface;
use super::controls::{MidiControl, MidiFader, MidiButton, MidiRotary, MidiJogWheel};
use super::display::ScribbleStrip;
use super::types::{MidiMessage, ControlType, ButtonStyle, LightStyle, Color, Invert};

/// MIDI control surface error
#[derive(Debug, thiserror::Error)]
pub enum MidiError {
    #[error("MIDI I/O error: {0}")]
    Io(#[from] midir::SendError),
    #[error("MIDI connection error: {0}")]
    Connection(#[from] midir::ConnectError<midir::InitError>),
    #[error("MIDI port not found: {0}")]
    PortNotFound(String),
    #[error("Invalid MIDI message")]
    InvalidMessage,
}

/// Configuration for a MIDI control
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiControlConfig {
    #[serde(rename = "type")]
    pub control_type: ControlType,
    #[serde(default)]
    pub control: Option<u8>,
    #[serde(default)]
    pub note: Option<u8>,
    pub style: Option<String>,
    pub light: Option<String>,
    // Note: binding is handled at the ControlSurface level
}

/// Configuration for a MIDI display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiDisplayConfig {
    #[serde(rename = "type")]
    pub display_type: String,
    pub binding: Option<String>,
}

/// Configuration for a MIDI surface
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiSurfaceConfig {
    pub device: String,
    #[serde(default)]
    pub controls: HashMap<String, MidiControlConfig>,
    #[serde(default)]
    pub displays: HashMap<String, MidiDisplayConfig>,
}

/// MIDI control surface implementation
pub struct MidiSurface {
    id: String,
    config: MidiSurfaceConfig,
    midi_in: Option<MidiInputConnection<mpsc::UnboundedSender<Vec<u8>>>>,
    midi_out: Arc<Mutex<Option<midir::MidiOutputConnection>>>,
    controls: HashMap<String, Box<dyn MidiControl>>,
    scribble: Option<ScribbleStrip>,
    msg_map: HashMap<(u8, u8), String>, // (status_byte, key) -> control_name
    rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    on_control_changed: Option<Box<dyn Fn(String, String, Value) -> Box<dyn Future<Output = ()> + Send> + Send + Sync>>,
}

impl MidiSurface {
    pub fn new(id: String, config: MidiSurfaceConfig) -> Self {
        Self {
            id,
            config,
            midi_in: None,
            midi_out: Arc::new(Mutex::new(None)),
            controls: HashMap::new(),
            scribble: None,
            msg_map: HashMap::new(),
            rx: None,
            on_control_changed: None,
        }
    }
    
    /// Set callback for when a control value changes
    pub fn set_on_changed<F>(&mut self, callback: F) 
    where 
        F: Fn(String, String, Value) -> Box<dyn Future<Output = ()> + Send> + Send + Sync + 'static,
    {
        self.on_control_changed = Some(Box::new(callback));
    }
    
    /// Start the MIDI surface
    pub async fn start(&mut self) -> Result<(), MidiError> {
        // Create MIDI input and output
        let mut input = MidiInput::new("ozma-midi-input")?;
        let output = MidiOutput::new("ozma-midi-output")?;
        
        // Find input port
        let in_port = self.find_port(input.port_names()?, &self.config.device)?;
        let out_port = self.find_port(output.port_names()?, &self.config.device)?;
        
        // Create channel for MIDI messages
        let (tx, rx) = mpsc::unbounded_channel();
        self.rx = Some(rx);
        
        // Connect to input port
        let in_conn = input.connect(
            &in_port,
            "ozma-midi-in",
            move |_stamp, message, tx| {
                // Send MIDI message to async context
                let _ = tx.send(message.to_vec());
            },
            tx,
        )?;
        
        self.midi_in = Some(in_conn);
        
        // Connect to output port
        let out_conn = output.connect(&out_port, "ozma-midi-out")?;
        *self.midi_out.lock().unwrap() = Some(out_conn);
        
        // Create controls
        for (name, cfg) in &self.config.controls {
            let control: Box<dyn MidiControl> = match cfg.control_type {
                ControlType::Fader => {
                    Box::new(MidiFader::new(
                        name.clone(), 
                        cfg.control.unwrap_or(70), 
                        cfg.note, 
                        self.midi_out.clone()
                    ))
                }
                ControlType::Button => {
                    let style = cfg.style.as_ref()
                        .and_then(|s| serde_json::from_str(&format!("\"{}\"", s)).ok())
                        .unwrap_or(ButtonStyle::Toggle);
                        
                    let light_style = cfg.light.as_ref()
                        .and_then(|s| serde_json::from_str(&format!("\"{}\"", s)).ok())
                        .unwrap_or(LightStyle::State);
                        
                    Box::new(MidiButton::new(
                        name.clone(), 
                        cfg.note.unwrap_or(0), 
                        style, 
                        light_style, 
                        self.midi_out.clone()
                    ))
                }
                ControlType::Rotary => {
                    Box::new(MidiRotary::new(
                        name.clone(), 
                        cfg.control.unwrap_or(80), 
                        self.midi_out.clone()
                    ))
                }
                ControlType::JogWheel => {
                    Box::new(MidiJogWheel::new(
                        name.clone(), 
                        cfg.control.unwrap_or(60)
                    ))
                }
            };
            
            self.controls.insert(name.clone(), control);
            
            // Build message routing map
            if let Some(control_num) = cfg.control {
                self.msg_map.insert((0xB0, control_num), name.clone()); // Control change
            }
            if let Some(note_num) = cfg.note {
                self.msg_map.insert((0x90, note_num), name.clone()); // Note on
                self.msg_map.insert((0x80, note_num), name.clone()); // Note off
            }
        }
        
        // Create scribble strip
        if !self.config.displays.is_empty() {
            self.scribble = Some(ScribbleStrip::new(self.midi_out.clone()));
        }
        
        info!(
            "MIDI surface '{}' started: {} controls, {} displays",
            self.id,
            self.controls.len(),
            self.config.displays.len()
        );
        
        Ok(())
    }
    
    /// Stop the MIDI surface
    pub async fn stop(&mut self) -> Result<(), MidiError> {
        // MIDI connections will be dropped when MidiIO is dropped
        self.midi_in.take();
        *self.midi_out.lock().unwrap() = None;
        info!("MIDI surface '{}' stopped", self.id);
        Ok(())
    }
    
    /// Process incoming MIDI messages
    pub async fn process_messages(&mut self) -> Result<(), MidiError> {
        if let Some(ref mut rx) = self.rx {
            while let Ok(message) = rx.try_recv() {
                self.process_message(&message)?;
            }
        }
        Ok(())
    }
    
    /// Process a single MIDI message
    fn process_message(&mut self, message: &[u8]) -> Result<(), MidiError> {
        if message.is_empty() {
            return Ok(());
        }
        
        let status = message[0];
        let key = if message.len() >= 2 { message[1] } else { 0 };
        
        if let Some(control_name) = self.msg_map.get(&(status & 0xF0, key)) {
            if let Some(control) = self.controls.get_mut(control_name) {
                // Parse MIDI message
                let msg_type = match status & 0xF0 {
                    0x80 => "note_off",
                    0x90 => "note_on",
                    0xB0 => "control_change",
                    _ => return Ok(()),
                };
                
                let midi_msg = MidiMessage {
                    msg_type: msg_type.to_string(),
                    channel: status & 0x0F,
                    control: if msg_type == "control_change" { Some(key) } else { None },
                    note: if msg_type == "note_on" || msg_type == "note_off" { Some(key) } else { None },
                    value: if message.len() >= 3 { message[2] } else { 0 },
                };
                
                if let Some(delta) = control.on_midi_message(&midi_msg) {
                    if let Some(value) = delta.get("value") {
                        // Process control change
                        debug!("MIDI control '{}' changed to {:?}", control_name, value);
                        
                        // Notify callback if set
                        if let Some(ref callback) = self.on_control_changed {
                            let id = self.id.clone();
                            let name = control_name.clone();
                            let val = value.clone();
                            // In a real implementation, we would spawn the future
                            // tokio::spawn(callback(id, name, val));
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Create display updater function
    pub fn make_display_updater(&self, display_type: &str) -> Box<dyn Fn(&str, Option<&str>) + Send + Sync> {
        let scribble = self.scribble.clone();
        let display_type = display_type.to_string();
        
        Box::new(move |text: &str, color: Option<&str>| {
            if let Some(ref scribble) = scribble {
                let lcd_color = Color::from_hex(color);
                let result = match display_type.as_str() {
                    "scribble_top" => scribble.update_top(text, Some(lcd_color)),
                    "scribble_bottom" => scribble.update_bottom(text, Some(lcd_color)),
                    _ => scribble.update(Some(text), Some(lcd_color), None),
                };
                
                if let Err(e) = result {
                    warn!("Failed to update display: {}", e);
                }
            }
        })
    }
    
    fn find_port(&self, ports: Vec<String>, pattern: &str) -> Result<midir::MidiInputPort, MidiError> {
        let input = MidiInput::new("ozma-midi-input")?;
        let port = input
            .ports()
            .into_iter()
            .find(|p| {
                if let Ok(name) = input.port_name(p) {
                    name.starts_with(pattern)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::PortNotFound(pattern.to_string()))?;
        Ok(port)
    }
    
    fn find_output_port(&self, pattern: &str) -> Result<midir::MidiOutputPort, MidiError> {
        let output = MidiOutput::new("ozma-midi-output")?;
        let port = output
            .ports()
            .into_iter()
            .find(|p| {
                if let Ok(name) = output.port_name(p) {
                    name.starts_with(pattern)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::PortNotFound(pattern.to_string()))?;
        Ok(port)
    }
}

impl ControlSurface for MidiSurface {
    fn id(&self) -> &str {
        &self.id
    }
    
    // Other ControlSurface methods would be implemented here
}
//! MIDI surface implementation

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::midi::{MidiIO, MidiError, Result};
use crate::midi::controls::{MidiControl, MidiFader, MidiButton, MidiRotary, MidiJogWheel, ButtonStyle, LightStyle};
use crate::midi::display::ScribbleStrip;

/// Type of MIDI control
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MidiControlType {
    #[serde(rename = "fader")]
    Fader,
    #[serde(rename = "button")]
    Button,
    #[serde(rename = "rotary")]
    Rotary,
    #[serde(rename = "jogwheel")]
    JogWheel,
}

/// Configuration for a MIDI control
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiControlConfig {
    #[serde(rename = "type")]
    pub control_type: MidiControlType,
    #[serde(default)]
    pub control: Option<u8>,
    #[serde(default)]
    pub note: Option<u8>,
    #[serde(default)]
    pub style: Option<String>, // for buttons: "toggle" | "momentary"
    #[serde(default)]
    pub light: Option<String>, // for buttons: "state" | "always_on" | "momentary"
}

/// Configuration for a MIDI surface
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiSurfaceConfig {
    pub device: String,
    #[serde(default)]
    pub controls: HashMap<String, MidiControlConfig>,
    #[serde(default)]
    pub displays: Option<HashMap<String, String>>, // display_name -> display_type
}

/// Callback for control changes
pub type ControlChangeCallback = Box<dyn Fn(String, String, serde_json::Value) + Send + Sync>;

/// A MIDI device registered as an ozma control surface
pub struct MidiSurface {
    pub id: String,
    config: MidiSurfaceConfig,
    midi: Option<Arc<Mutex<MidiIO>>>,
    controls: HashMap<String, Box<dyn MidiControl + Send>>,
    scribble: Option<ScribbleStrip>,
    on_changed: Option<ControlChangeCallback>,
}

impl MidiSurface {
    /// Create a new MIDI surface
    pub fn new(id: String, config: MidiSurfaceConfig) -> Self {
        Self {
            id,
            config,
            midi: None,
            controls: HashMap::new(),
            scribble: None,
            on_changed: None,
        }
    }

    /// Start the MIDI surface
    pub async fn start(&mut self) -> Result<()> {
        // Initialize MIDI I/O
        let mut midi_io = MidiIO::new()?;
        midi_io.open(&self.config.device)?;
        
        let midi_arc = Arc::new(Mutex::new(midi_io));
        self.midi = Some(midi_arc.clone());

        // Create controls
        for (name, cfg) in &self.config.controls {
            let control: Box<dyn MidiControl + Send> = match cfg.control_type {
                MidiControlType::Fader => {
                    Box::new(MidiFader::new(
                        name.clone(),
                        cfg.control.unwrap_or(70),
                        cfg.note,
                        midi_arc.clone(),
                    ))
                }
                MidiControlType::Button => {
                    let style = match cfg.style.as_deref() {
                        Some("momentary") => ButtonStyle::Momentary,
                        _ => ButtonStyle::Toggle,
                    };
                    
                    let light_style = match cfg.light.as_deref() {
                        Some("always_on") => LightStyle::AlwaysOn,
                        Some("momentary") => LightStyle::Momentary,
                        _ => LightStyle::State,
                    };
                    
                    Box::new(MidiButton::new(
                        name.clone(),
                        cfg.note.unwrap_or(0),
                        style,
                        light_style,
                        midi_arc.clone(),
                    ))
                }
                MidiControlType::Rotary => {
                    Box::new(MidiRotary::new(
                        name.clone(),
                        cfg.control.unwrap_or(80),
                        midi_arc.clone(),
                    ))
                }
                MidiControlType::JogWheel => {
                    Box::new(MidiJogWheel::new(
                        name.clone(),
                        cfg.control.unwrap_or(60),
                        midi_arc.clone(),
                    ))
                }
            };
            
            self.controls.insert(name.clone(), control);
        }

        // Create scribble strip if displays are configured
        if self.config.displays.is_some() {
            // Note: In a real implementation, we'd need to handle the display creation
            // This is a simplified version for now
        }

        Ok(())
    }

    /// Stop the MIDI surface
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(midi) = &self.midi {
            if let Ok(mut midi) = midi.lock() {
                midi.close();
            }
        }
        Ok(())
    }

    /// Set the callback for when a control value changes
    pub fn set_on_changed(&mut self, callback: ControlChangeCallback) {
        self.on_changed = Some(callback);
    }

    /// Process incoming MIDI message
    pub fn process_midi_message(&mut self, msg: &[u8]) -> Result<()> {
        // This would be called from the MIDI input callback
        // For now, we'll just process it directly
        for (name, control) in &mut self.controls {
            if let Some(delta) = control.on_midi_message(msg) {
                if let Some(ref callback) = self.on_changed {
                    if let Some(value) = delta.get("value") {
                        callback(self.id.clone(), name.clone(), value.clone());
                    }
                }
            }
        }
        Ok(())
    }
}
