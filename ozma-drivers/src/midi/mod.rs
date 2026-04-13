// ── MIDI Control classes ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MidiControlState {
    pub value: i32,
    pub lockout: bool,
    pub pressed: bool,
}

pub trait MidiControl: Send + Sync {
    fn name(&self) -> &str;
    fn on_midi_message(&mut self, message: &[u8]) -> Option<MidiControlState>;
    fn set_value(&mut self, value: i32);
    fn state(&self) -> MidiControlState;
}

pub struct MidiFader {
    name: String,
    cc: u8,
    touch_note: Option<u8>,
    value: i32,
    lockout: bool,
}

impl MidiFader {
    pub fn new(name: String, cc: u8, touch_note: Option<u8>) -> Self {
        Self {
            name,
            cc,
            touch_note,
            value: 0,
            lockout: false,
        }
    }
}

impl MidiControl for MidiFader {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<MidiControlState> {
        if message.len() >= 3 && message[0] == 0xB0 && message[1] == self.cc {
            self.value = message[2] as i32;
            return Some(MidiControlState {
                value: self.value,
                lockout: self.lockout,
                pressed: false,
            });
        }
        
        if let Some(touch_note) = self.touch_note {
            if message.len() >= 3 && (message[0] == 0x90 || message[0] == 0x80) && message[1] == touch_note {
                self.lockout = message[0] == 0x90 && message[2] >= 64;
                return Some(MidiControlState {
                    value: self.value,
                    lockout: self.lockout,
                    pressed: false,
                });
            }
        }
        
        None
    }

    fn set_value(&mut self, value: i32) {
        if !self.lockout {
            self.value = value.max(0).min(127);
        }
    }

    fn state(&self) -> MidiControlState {
        MidiControlState {
            value: self.value,
            lockout: self.lockout,
            pressed: false,
        }
    }
}

pub struct MidiButton {
    name: String,
    note: u8,
    style: ButtonStyle,
    light_style: LightStyle,
    value: bool,
    pressed: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ButtonStyle {
    Toggle,
    Momentary,
}

#[derive(Debug, Clone, Copy)]
pub enum LightStyle {
    State,
    AlwaysOn,
    Momentary,
    Off,
}

impl MidiButton {
    pub fn new(name: String, note: u8, style: ButtonStyle, light_style: LightStyle) -> Self {
        Self {
            name,
            note,
            style,
            light_style,
            value: false,
            pressed: false,
        }
    }

    fn update_light(&self, midi: &mut MidiIO) -> Result<()> {
        let on = match self.light_style {
            LightStyle::Off => false,
            LightStyle::AlwaysOn => true,
            LightStyle::Momentary => self.pressed,
            LightStyle::State => self.value,
        };
        
        midi.note_on(self.note, if on { 127 } else { 0 })
    }
}

impl MidiControl for MidiButton {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<MidiControlState> {
        if message.len() >= 3 && message[0] == 0x90 && message[1] == self.note {
            if message[2] >= 64 {
                // Press
                self.pressed = true;
                if let ButtonStyle::Toggle = self.style {
                    self.value = !self.value;
                } else {
                    self.value = true;
                }
            } else {
                // Release
                self.pressed = false;
                if let ButtonStyle::Momentary = self.style {
                    self.value = false;
                }
            }
            
            return Some(MidiControlState {
                value: self.value as i32,
                lockout: false,
                pressed: self.pressed,
            });
        }
        
        None
    }

    fn set_value(&mut self, value: i32) {
        self.value = value != 0;
    }

    fn state(&self) -> MidiControlState {
        MidiControlState {
            value: self.value as i32,
            lockout: false,
            pressed: self.pressed,
        }
    }
}

pub struct MidiRotary {
    name: String,
    cc: u8,
    value: i32,
    lockout: bool,
}

impl MidiRotary {
    pub fn new(name: String, cc: u8) -> Self {
        Self {
            name,
            cc,
            value: 0,
            lockout: false,
        }
    }
}

impl MidiControl for MidiRotary {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<MidiControlState> {
        if message.len() >= 3 && message[0] == 0xB0 && message[1] == self.cc {
            self.value = message[2] as i32;
            return Some(MidiControlState {
                value: self.value,
                lockout: self.lockout,
                pressed: false,
            });
        }
        None
    }

    fn set_value(&mut self, value: i32) {
        if !self.lockout {
            self.value = value.max(0).min(127);
        }
    }

    fn state(&self) -> MidiControlState {
        MidiControlState {
            value: self.value,
            lockout: self.lockout,
            pressed: false,
        }
    }
}

pub struct MidiJogWheel {
    name: String,
    cc: u8,
}

impl MidiJogWheel {
    pub fn new(name: String, cc: u8) -> Self {
        Self { name, cc }
    }
}

impl MidiControl for MidiJogWheel {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<MidiControlState> {
        if message.len() >= 3 && message[0] == 0xB0 && message[1] == self.cc {
            let direction = if message[2] == 65 { 1 } else { -1 };
            return Some(MidiControlState {
                value: direction,
                lockout: false,
                pressed: false,
            });
        }
        None
    }

    fn set_value(&mut self, _value: i32) {
        // Jog wheels don't have settable values
    }

    fn state(&self) -> MidiControlState {
        MidiControlState {
            value: 0,
            lockout: false,
            pressed: false,
        }
    }
}
//! MIDI control surface support for ozma.
//!
//! Ported from surfacepresser-run's midi_controller.py + midi_integration.py,
//! rewritten as a clean async module that integrates with ozma's ControlSurface
//! abstraction.
//!
//! Supports:
//!   - Faders (motorised, with touch lockout)
//!   - Buttons (toggle / momentary, with LED feedback)
//!   - Rotary encoders
//!   - Jog wheels
//!   - Behringer X-Touch scribble strip LCD displays
//!   - Behringer 7-segment displays

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use midir::{MidiInput, MidiOutput, Ignore};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use log::{info, warn, debug};

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

/// Color for LCD displays
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Black = 0,
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Magenta = 5,
    Cyan = 6,
    White = 7,
}

impl Color {
    /// Convert hex color string to LCD color
    pub fn from_hex(hex_color: Option<&str>) -> Self {
        if let Some(color) = hex_color {
            let color = color.to_lowercase();
            match color.as_str() {
                "#ff0000" => Self::Red,
                "#00ff00" => Self::Green,
                "#0000ff" => Self::Blue,
                "#ffff00" => Self::Yellow,
                "#ff00ff" => Self::Magenta,
                "#00ffff" => Self::Cyan,
                "#ffffff" => Self::White,
                "#000000" => Self::Black,
                _ => {
                    // Try name match
                    if color.contains("red") {
                        Self::Red
                    } else if color.contains("green") {
                        Self::Green
                    } else if color.contains("blue") {
                        Self::Blue
                    } else if color.contains("yellow") {
                        Self::Yellow
                    } else if color.contains("magenta") {
                        Self::Magenta
                    } else if color.contains("cyan") {
                        Self::Cyan
                    } else if color.contains("black") {
                        Self::Black
                    } else {
                        Self::White
                    }
                }
            }
        } else {
            Self::White
        }
    }
}

/// Invert mode for LCD displays
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

/// 7-segment font for Behringer displays
const SEGMENT_FONT: [u8; 128] = {
    let mut font = [0u8; 128];
    // Numbers
    font[b'0' as usize] = 0x3F;
    font[b'1' as usize] = 0x06;
    font[b'2' as usize] = 0x5B;
    font[b'3' as usize] = 0x4F;
    font[b'4' as usize] = 0x66;
    font[b'5' as usize] = 0x6D;
    font[b'6' as usize] = 0x7D;
    font[b'7' as usize] = 0x07;
    font[b'8' as usize] = 0x7F;
    font[b'9' as usize] = 0x6F;
    // Letters
    font[b'A' as usize] = 0x77;
    font[b'B' as usize] = 0x7F;
    font[b'C' as usize] = 0x39;
    font[b'D' as usize] = 0x3F;
    font[b'E' as usize] = 0x79;
    font[b'F' as usize] = 0x71;
    font[b'G' as usize] = 0x3D;
    font[b'H' as usize] = 0x76;
    font[b'I' as usize] = 0x06;
    font[b'J' as usize] = 0x0E;
    font[b'K' as usize] = 0x75;
    font[b'L' as usize] = 0x38;
    font[b'M' as usize] = 0x37;
    font[b'N' as usize] = 0x37;
    font[b'O' as usize] = 0x3F;
    font[b'P' as usize] = 0x73;
    font[b'Q' as usize] = 0x67;
    font[b'R' as usize] = 0x77;
    font[b'S' as usize] = 0x6D;
    font[b'T' as usize] = 0x78;
    font[b'U' as usize] = 0x3E;
    font[b'V' as usize] = 0x3E;
    font[b'W' as usize] = 0x3E;
    font[b'X' as usize] = 0x49;
    font[b'Y' as usize] = 0x6E;
    font[b'Z' as usize] = 0x5B;
    // Special characters
    font[b' ' as usize] = 0x00;
    font[b'-' as usize] = 0x40;
    font[b'.' as usize] = 0x08;
    font[b':' as usize] = 0x09;
    font[b'(' as usize] = 0x39;
    font[b')' as usize] = 0x0F;
    font
};

/// Render text for 7-segment display
fn render_7seg(text: &str) -> Vec<u8> {
    text.chars()
        .take(12)
        .map(|c| {
            let idx = c as u8 as usize;
            if idx < SEGMENT_FONT.len() {
                SEGMENT_FONT[idx]
            } else {
                SEGMENT_FONT[b' ' as usize]
            }
        })
        .collect()
}

/// Convert Unicode to ASCII for LCD displays
fn unidecode(text: &str) -> String {
    // Simple ASCII conversion for now
    text.chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}

/// MIDI I/O wrapper
pub struct MidiIO {
    input_connection: Option<midir::MidiInputConnection<mpsc::UnboundedSender<Vec<u8>>>>,
    output_connection: Option<midir::MidiOutputConnection>,
}

impl MidiIO {
    /// Create new MIDI I/O wrapper
    pub fn new() -> Self {
        Self {
            input_connection: None,
            output_connection: None,
        }
    }

    /// List available MIDI devices
    pub fn list_devices() -> Result<(Vec<String>, Vec<String>), MidiError> {
        let input = MidiInput::new("ozma-midi-input")?;
        let output = MidiOutput::new("ozma-midi-output")?;
        
        let input_names: Vec<String> = input
            .ports()
            .iter()
            .filter_map(|p| input.port_name(p).ok())
            .collect();
            
        let output_names: Vec<String> = output
            .ports()
            .iter()
            .filter_map(|p| output.port_name(p).ok())
            .collect();
            
        Ok((input_names, output_names))
    }

    /// Open MIDI connections
    pub fn open(&mut self, device_name: &str) -> Result<mpsc::UnboundedReceiver<Vec<u8>>, MidiError> {
        let mut input = MidiInput::new("ozma-midi-input")?;
        input.ignore(Ignore::None);
        
        let output = MidiOutput::new("ozma-midi-output")?;
        
        // Find input port
        let in_port = input
            .ports()
            .into_iter()
            .find(|p| {
                if let Ok(name) = input.port_name(p) {
                    name.starts_with(device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::PortNotFound(device_name.to_string()))?;
            
        // Find output port
        let out_port = output
            .ports()
            .into_iter()
            .find(|p| {
                if let Ok(name) = output.port_name(p) {
                    name.starts_with(device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::PortNotFound(device_name.to_string()))?;
            
        let (tx, rx) = mpsc::unbounded_channel();
        let in_conn = input.connect(
            &in_port,
            "ozma-midi-in",
            move |_stamp, message, tx| {
                // Send MIDI message to async context
                let _ = tx.send(message.to_vec());
            },
            tx,
        )?;
        
        let out_conn = output.connect(&out_port, "ozma-midi-out")?;
        
        self.input_connection = Some(in_conn);
        self.output_connection = Some(out_conn);
        
        Ok(rx)
    }

    /// Send MIDI message
    pub fn send(&mut self, message: &[u8]) -> Result<(), MidiError> {
        if let Some(conn) = &mut self.output_connection {
            conn.send(message)?;
        }
        Ok(())
    }

    /// Send note on message
    pub fn note_on(&mut self, note: u8, velocity: u8) -> Result<(), MidiError> {
        self.send(&[0x90, note, velocity])
    }

    /// Send control change message
    pub fn control_change(&mut self, control: u8, value: u8) -> Result<(), MidiError> {
        self.send(&[0xB0, control, value])
    }

    /// Send SysEx message
    pub fn sysex(&mut self, data: &[u8]) -> Result<(), MidiError> {
        let mut message = vec![0xF0];
        message.extend_from_slice(data);
        message.push(0xF7);
        self.send(&message)
    }

    /// Update LCD display (Behringer X-Touch scribble strip)
    pub fn lcd_update(&mut self, text: &str, color: Color, invert: Invert) -> Result<(), MidiError> {
        let text = unidecode(text);
        let mut chars: Vec<u8> = text.chars().take(14).map(|c| c as u8).collect();
        while chars.len() < 14 {
            chars.push(0);
        }
        
        let color_code = color as u8 | ((invert as u8) << 4);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend(chars);
        
        self.sysex(&data)
    }

    /// Update 7-segment display
    pub fn segment_update(&mut self, text: &str) -> Result<(), MidiError> {
        let text = unidecode(text);
        let mut rendered = render_7seg(&text);
        while rendered.len() < 12 {
            rendered.push(0);
        }
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend(rendered);
        data.extend_from_slice(&[0x00, 0x00]);
        
        self.sysex(&data)
    }
}

impl Default for MidiIO {
    fn default() -> Self {
        Self::new()
    }
}

/// Base trait for MIDI controls
pub trait MidiControl: Send + Sync {
    /// Process incoming MIDI message, return state delta
    fn on_midi_message(&mut self, message: &[u8]) -> Option<HashMap<String, JsonValue>>;
    
    /// Set value from external source (feedback)
    fn set_value(&mut self, value: JsonValue);
    
    /// Get current value
    fn get_value(&self) -> &JsonValue;
    
    /// Get lockout state
    fn get_lockout(&self) -> bool;
}

/// Motorised fader with touch detection
pub struct MidiFader {
    name: String,
    value: JsonValue,
    lockout: bool,
    cc: u8,
    touch_note: Option<u8>,
    midi: Arc<Mutex<MidiIO>>,
}

impl MidiFader {
    pub fn new(name: String, cc: u8, touch_note: Option<u8>, midi: Arc<Mutex<MidiIO>>) -> Self {
        Self {
            name,
            value: JsonValue::Number(serde_json::Number::from(0)),
            lockout: false,
            cc,
            touch_note,
            midi,
        }
    }
}

impl MidiControl for MidiFader {
    fn on_midi_message(&mut self, message: &[u8]) -> Option<HashMap<String, JsonValue>> {
        if message.len() >= 3 && message[0] == 0xB0 && message[1] == self.cc {
            // Control change message
            self.value = JsonValue::Number(serde_json::Number::from(message[2]));
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            Some(delta)
        } else if let Some(touch_note) = self.touch_note {
            if message.len() >= 3 && (message[0] == 0x90 || message[0] == 0x80) && message[1] == touch_note {
                // Note on/off for touch detection
                self.lockout = message[0] == 0x90 && message[2] >= 64;
                let mut delta = HashMap::new();
                delta.insert("lockout".to_string(), JsonValue::Bool(self.lockout));
                Some(delta)
            } else {
                None
            }
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: JsonValue) {
        if !self.lockout {
            if let Some(v) = value.as_u64() {
                let v = (v as u8).min(127);
                self.value = JsonValue::Number(serde_json::Number::from(v));
                
                if let Ok(midi) = self.midi.lock() {
                    let _ = midi.control_change(self.cc, v);
                }
            }
        }
    }
    
    fn get_value(&self) -> &JsonValue {
        &self.value
    }
    
    fn get_lockout(&self) -> bool {
        self.lockout
    }
}

/// Button with LED feedback
pub struct MidiButton {
    name: String,
    value: JsonValue,
    pressed: bool,
    note: u8,
    style: ButtonStyle,
    light_style: LightStyle,
    midi: Arc<Mutex<MidiIO>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonStyle {
    #[serde(rename = "toggle")]
    Toggle,
    #[serde(rename = "momentary")]
    Momentary,
}

impl Default for ButtonStyle {
    fn default() -> Self {
        Self::Toggle
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightStyle {
    #[serde(rename = "state")]
    State,
    #[serde(rename = "always_on")]
    AlwaysOn,
    #[serde(rename = "momentary")]
    Momentary,
    #[serde(rename = "false")]
    False,
}

impl Default for LightStyle {
    fn default() -> Self {
        Self::State
    }
}

impl MidiButton {
    pub fn new(name: String, note: u8, style: ButtonStyle, light_style: LightStyle, midi: Arc<Mutex<MidiIO>>) -> Self {
        let button = Self {
            name,
            value: JsonValue::Bool(false),
            pressed: false,
            note,
            style,
            light_style,
            midi: midi.clone(),
        };
        
        // Initialize light state
        button.update_light(midi);
        button
    }
    
    fn update_light(&self, midi: Arc<Mutex<MidiIO>>) {
        let on = match self.light_style {
            LightStyle::False => false,
            LightStyle::AlwaysOn => true,
            LightStyle::Momentary => self.pressed,
            LightStyle::State => {
                if let Some(b) = self.value.as_bool() {
                    b
                } else {
                    false
                }
            }
        };
        
        if let Ok(midi_guard) = midi.lock() {
            let _ = midi_guard.note_on(self.note, if on { 127 } else { 0 });
        }
    }
}

impl MidiControl for MidiButton {
    fn on_midi_message(&mut self, message: &[u8]) -> Option<HashMap<String, JsonValue>> {
        if message.len() >= 3 && message[0] == 0x90 && message[1] == self.note {
            if message[2] >= 64 {
                // Press
                self.pressed = true;
                if self.style == ButtonStyle::Toggle {
                    if let Some(current) = self.value.as_bool() {
                        self.value = JsonValue::Bool(!current);
                    }
                } else {
                    self.value = JsonValue::Bool(true);
                }
            } else {
                // Release
                self.pressed = false;
                if self.style == ButtonStyle::Momentary {
                    self.value = JsonValue::Bool(false);
                }
            }
            
            // Update light
            self.update_light(self.midi.clone());
            
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            delta.insert("pressed".to_string(), JsonValue::Bool(self.pressed));
            Some(delta)
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: JsonValue) {
        self.value = value;
        self.update_light(self.midi.clone());
    }
    
    fn get_value(&self) -> &JsonValue {
        &self.value
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
}

/// Rotary encoder
pub struct MidiRotary {
    name: String,
    value: JsonValue,
    lockout: bool,
    cc: u8,
    midi: Arc<Mutex<MidiIO>>,
}

impl MidiRotary {
    pub fn new(name: String, cc: u8, midi: Arc<Mutex<MidiIO>>) -> Self {
        Self {
            name,
            value: JsonValue::Number(serde_json::Number::from(0)),
            lockout: false,
            cc,
            midi,
        }
    }
}

impl MidiControl for MidiRotary {
    fn on_midi_message(&mut self, message: &[u8]) -> Option<HashMap<String, JsonValue>> {
        if message.len() >= 3 && message[0] == 0xB0 && message[1] == self.cc {
            self.value = JsonValue::Number(serde_json::Number::from(message[2]));
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            Some(delta)
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: JsonValue) {
        if !self.lockout {
            if let Some(v) = value.as_u64() {
                let v = (v as u8).min(127);
                self.value = JsonValue::Number(serde_json::Number::from(v));
                
                if let Ok(midi) = self.midi.lock() {
                    let _ = midi.control_change(self.cc, v);
                }
            }
        }
    }
    
    fn get_value(&self) -> &JsonValue {
        &self.value
    }
    
    fn get_lockout(&self) -> bool {
        self.lockout
    }
}

/// Jog wheel
pub struct MidiJogWheel {
    name: String,
    cc: u8,
}

impl MidiJogWheel {
    pub fn new(name: String, cc: u8) -> Self {
        Self {
            name,
            cc,
        }
    }
}

impl MidiControl for MidiJogWheel {
    fn on_midi_message(&mut self, message: &[u8]) -> Option<HashMap<String, JsonValue>> {
        if message.len() >= 3 && message[0] == 0xB0 && message[1] == self.cc {
            let direction = if message[2] == 65 { 1 } else { -1 };
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), JsonValue::Number(serde_json::Number::from(direction)));
            Some(delta)
        } else {
            None
        }
    }
    
    fn set_value(&mut self, _value: JsonValue) {
        // Jog wheels don't have feedback
    }
    
    fn get_value(&self) -> &JsonValue {
        &JsonValue::Null
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
}

/// Control type registry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlType {
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

/// Behringer X-Touch scribble strip
pub struct ScribbleStrip {
    midi: Arc<Mutex<MidiIO>>,
    text: String,
    color: Color,
    invert: Invert,
}

impl ScribbleStrip {
    pub fn new(midi: Arc<Mutex<MidiIO>>) -> Self {
        Self {
            midi,
            text: " ".repeat(14),
            color: Color::White,
            invert: Invert::None,
        }
    }
    
    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) -> Result<(), MidiError> {
        if let Some(t) = text {
            self.text = format!("{:<14}", &t[..t.len().min(14)]);
        }
        if let Some(c) = color {
            self.color = c;
        }
        if let Some(i) = invert {
            self.invert = i;
        }
        
        if let Ok(midi) = self.midi.lock() {
            midi.lcd_update(&self.text, self.color, self.invert)?;
        }
        
        Ok(())
    }
    
    pub fn update_top(&mut self, text: &str, color: Option<Color>) -> Result<(), MidiError> {
        let formatted = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", formatted, &self.text[7..]);
        if let Some(c) = color {
            self.color = c;
        }
        
        if let Ok(midi) = self.midi.lock() {
            midi.lcd_update(&self.text, self.color, self.invert)?;
        }
        
        Ok(())
    }
    
    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) -> Result<(), MidiError> {
        let formatted = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", &self.text[..7], formatted);
        if let Some(c) = color {
            self.color = c;
        }
        
        if let Ok(midi) = self.midi.lock() {
            midi.lcd_update(&self.text, self.color, self.invert)?;
        }
        
        Ok(())
    }
}

/// MIDI control surface implementation
pub struct MidiSurface {
    id: String,
    config: MidiSurfaceConfig,
    midi: Arc<Mutex<MidiIO>>,
    controls: HashMap<String, Box<dyn MidiControl>>,
    scribble: Option<ScribbleStrip>,
    msg_map: HashMap<(u8, u8), String>, // (status_byte, key) -> control_name
    rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    on_control_changed: Option<Box<dyn Fn(String, String, JsonValue) -> Box<dyn futures::Future<Output = ()> + Send> + Send + Sync>>,
}

impl MidiSurface {
    pub fn new(id: String, config: MidiSurfaceConfig) -> Self {
        Self {
            id,
            config,
            midi: Arc::new(Mutex::new(MidiIO::new())),
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
        F: Fn(String, String, JsonValue) -> Box<dyn futures::Future<Output = ()> + Send> + Send + Sync + 'static,
    {
        self.on_control_changed = Some(Box::new(callback));
    }
    
    /// Start the MIDI surface
    pub async fn start(&mut self) -> Result<(), MidiError> {
        // Open MIDI connections
        let rx = self.midi.lock().unwrap().open(&self.config.device)?;
        self.rx = Some(rx);
        
        // Create controls
        for (name, cfg) in &self.config.controls {
            let control: Box<dyn MidiControl> = match cfg.control_type {
                ControlType::Fader => {
                    Box::new(MidiFader::new(
                        name.clone(), 
                        cfg.control.unwrap_or(70), 
                        cfg.note, 
                        self.midi.clone()
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
                        self.midi.clone()
                    ))
                }
                ControlType::Rotary => {
                    Box::new(MidiRotary::new(
                        name.clone(), 
                        cfg.control.unwrap_or(80), 
                        self.midi.clone()
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
            self.scribble = Some(ScribbleStrip::new(self.midi.clone()));
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
                if let Some(delta) = control.on_midi_message(message) {
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
}
//! MIDI control surface support for ozma.
//!
//! Ported from surfacepresser-run's midi_controller.py + midi_integration.py,
//! rewritten as a clean async module that integrates with ozma's ControlSurface
//! abstraction.
//!
//! Supports:
//!   - Faders (motorised, with touch lockout)
//!   - Buttons (toggle / momentary, with LED feedback)
//!   - Rotary encoders
//!   - Jog wheels
//!   - Behringer X-Touch scribble strip LCD displays
//!   - Behringer 7-segment displays
//!
//! Requires: midir v0.10.3

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use midir::{MidiInput, MidiOutput};
use serde::{Deserialize, Serialize};

/// MIDI control surface support
pub mod surface;
pub mod io;
pub mod controls;
pub mod display;

pub use surface::{MidiSurface, MidiControlType, MidiSurfaceConfig};
pub use controls::{MidiFader, MidiButton, MidiRotary, MidiJogWheel};
pub use display::{ScribbleStrip, Color, Invert};
pub use io::MidiIO;

/// Error types for MIDI operations
#[derive(Debug, thiserror::Error)]
pub enum MidiError {
    #[error("MIDI input error: {0}")]
    InputError(#[from] midir::ConnectError<midir::InitError>),
    #[error("MIDI output error: {0}")]
    OutputError(#[from] midir::ConnectError<midir::InitError>),
    #[error("Port not found: {0}")]
    PortNotFound(String),
    #[error("Invalid message")]
    InvalidMessage,
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Send error: {0}")]
    SendError(#[from] midir::SendError),
}

/// Result type for MIDI operations
pub type Result<T> = std::result::Result<T, MidiError>;

/// List available MIDI devices
pub fn list_devices() -> Result<(Vec<String>, Vec<String>)> {
    let mut input_names = Vec::new();
    let mut output_names = Vec::new();
    
    // Get input devices
    if let Ok(midi_in) = MidiInput::new("ozma-midi-in") {
        for port in midi_in.ports() {
            if let Ok(name) = midi_in.port_name(&port) {
                input_names.push(name);
            }
        }
    }
    
    // Get output devices
    if let Ok(midi_out) = MidiOutput::new("ozma-midi-out") {
        for port in midi_out.ports() {
            if let Ok(name) = midi_out.port_name(&port) {
                output_names.push(name);
            }
        }
    }
    
    Ok((input_names, output_names))
}
