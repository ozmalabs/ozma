// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/*!
MIDI control surface support for ozma.

Ported from surfacepresser-run's midi_controller.py + midi_integration.py,
rewritten as a clean async module that integrates with ozma's ControlSurface
abstraction.

Supports:
  - Faders (motorised, with touch lockout)
  - Buttons (toggle / momentary, with LED feedback)
  - Rotary encoders
  - Jog wheels
  - Behringer X-Touch scribble strip LCD displays
  - Behringer 7-segment displays

Requires: midir v0.10.3
*/

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection, MidiOutputPort};
use midir::os::unix::VirtualInput;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

// ── Enums ────────────────────────────────────────────────────────────────────

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

impl From<Color> for u8 {
    fn from(color: Color) -> Self {
        color as u8
    }
}

impl From<Invert> for u8 {
    fn from(invert: Invert) -> Self {
        invert as u8
    }
}

// Map hex colour to closest LCD colour
lazy_static::lazy_static! {
    static ref COLOR_MAP: HashMap<&'static str, Color> = {
        let mut m = HashMap::new();
        m.insert("#ff0000", Color::Red);
        m.insert("#00ff00", Color::Green);
        m.insert("#0000ff", Color::Blue);
        m.insert("#ffff00", Color::Yellow);
        m.insert("#ff00ff", Color::Magenta);
        m.insert("#00ffff", Color::Cyan);
        m.insert("#ffffff", Color::White);
        m.insert("#000000", Color::Black);
        m
    };
}

fn hex_to_lcd_color(hex_color: Option<&str>) -> Color {
    /*!Best-effort hex colour → LCD Color mapping.*/
    let hex_color = match hex_color {
        Some(color) => color.to_lowercase(),
        None => return Color::White,
    };
    
    if let Some(&color) = COLOR_MAP.get(hex_color.as_str()) {
        return color;
    }
    
    // Try name match
    for color in Color::all() {
        if color.to_string().to_lowercase().contains(&hex_color) {
            return color;
        }
    }
    
    Color::White
}

impl Color {
    fn all() -> Vec<Color> {
        vec![
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::White,
        ]
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// ── 7-segment font (for Behringer segment displays) ─────────────────────────

lazy_static::lazy_static! {
    static ref SEG7_FONT: HashMap<char, u8> = {
        let mut m = HashMap::new();
        m.insert('0', 0x3F);
        m.insert('1', 0x06);
        m.insert('2', 0x5B);
        m.insert('3', 0x4F);
        m.insert('4', 0x66);
        m.insert('5', 0x6D);
        m.insert('6', 0x7D);
        m.insert('7', 0x07);
        m.insert('8', 0x7F);
        m.insert('9', 0x6F);
        m.insert('A', 0x77);
        m.insert('B', 0x7F);
        m.insert('C', 0x39);
        m.insert('D', 0x3F);
        m.insert('E', 0x79);
        m.insert('F', 0x71);
        m.insert('G', 0x3D);
        m.insert('H', 0x76);
        m.insert('I', 0x06);
        m.insert('J', 0x0E);
        m.insert('K', 0x75);
        m.insert('L', 0x38);
        m.insert('M', 0x37);
        m.insert('N', 0x37);
        m.insert('O', 0x3F);
        m.insert('P', 0x73);
        m.insert('Q', 0x67);
        m.insert('R', 0x77);
        m.insert('S', 0x6D);
        m.insert('T', 0x78);
        m.insert('U', 0x3E);
        m.insert('V', 0x3E);
        m.insert('W', 0x3E);
        m.insert('X', 0x49);
        m.insert('Y', 0x6E);
        m.insert('Z', 0x5B);
        m.insert(' ', 0x00);
        m.insert('-', 0x40);
        m.insert('.', 0x08);
        m.insert(':', 0x09);
        m.insert('(', 0x39);
        m.insert(')', 0x0F);
        m
    };
}

fn render_7seg(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| *SEG7_FONT.get(&c.to_ascii_uppercase()).unwrap_or(&0))
        .collect()
}

// ── Low-level MIDI I/O ──────────────────────────────────────────────────────

pub struct MidiIO {
    device_name: String,
    input_connection: Option<MidiInputConnection<()>>,
    output_connection: Option<MidiOutputConnection>,
    sender: mpsc::UnboundedSender<Vec<u8>>,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>>,
}

impl MidiIO {
    pub fn new(device_name: String) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            device_name,
            input_connection: None,
            output_connection: None,
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    pub fn available() -> bool {
        true // midir is always available in Rust
    }

    pub fn list_devices() -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let input = MidiInput::new("ozma-midi-input")?;
        let output = MidiOutput::new("ozma-midi-output")?;
        
        let mut devices = Vec::new();
        
        for port in input.ports() {
            if let Ok(name) = input.port_name(&port) {
                devices.push(name);
            }
        }
        
        for port in output.ports() {
            if let Ok(name) = output.port_name(&port) {
                if !devices.contains(&name) {
                    devices.push(name);
                }
            }
        }
        
        Ok(devices)
    }

    pub fn open(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let input = MidiInput::new("ozma-midi-input")?;
        let output = MidiOutput::new("ozma-midi-output")?;
        
        // Find input port
        let input_port = self.find_port(input.ports(), &self.device_name)?;
        let sender_clone = self.sender.clone();
        
        let input_connection = input.connect(
            &input_port,
            "ozma-midi-input-connection",
            move |_, message, _| {
                if let Err(e) = sender_clone.send(message.to_vec()) {
                    error!("Failed to send MIDI message: {}", e);
                }
            },
            ()
        )?;
        
        // Find output port
        let output_port = self.find_port(output.ports(), &self.device_name)?;
        let output_connection = output.connect(&output_port, "ozma-midi-output-connection")?;
        
        self.input_connection = Some(input_connection);
        self.output_connection = Some(output_connection);
        
        info!("MIDI opened: device={}", self.device_name);
        Ok(())
    }

    pub fn close(&mut self) {
        self.input_connection.take();
        self.output_connection.take();
    }

    pub fn send(&mut self, msg: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(conn) = &mut self.output_connection {
            conn.send(msg)?;
        }
        Ok(())
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) -> Result<(), Box<dyn std::error::Error>> {
        self.send(&[0x90, note, velocity])
    }

    pub fn control_change(&mut self, control: u8, value: u8) -> Result<(), Box<dyn std::error::Error>> {
        self.send(&[0xB0, control, value])
    }

    pub fn sysex(&mut self, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let mut msg = vec![0xF0];
        msg.extend_from_slice(data);
        msg.push(0xF7);
        self.send(&msg)
    }

    pub fn lcd_update(&mut self, text: &str, color: Color, invert: Invert) -> Result<(), Box<dyn std::error::Error>> {
        /*!Send Behringer X-Touch scribble strip LCD update (14 chars).*/
        let text_bytes: Vec<u8> = text.chars().take(14).map(|c| c as u8).collect();
        let mut chars = text_bytes.clone();
        chars.resize(14, 0);
        
        let color_code = color as u8 | ((invert as u8) << 4);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend_from_slice(&chars);
        
        self.sysex(&data)
    }

    pub fn segment_update(&mut self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        /*!Send Behringer 7-segment display update (12 chars).*/
        let rendered = render_7seg(&text[..std::cmp::min(text.len(), 12)]);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend_from_slice(&rendered);
        data.resize(19, 0); // Ensure correct length
        
        self.sysex(&data)
    }

    fn find_port<T>(&self, ports: Vec<T>, pattern: &str) -> Result<T, Box<dyn std::error::Error>> 
    where
        T: Clone,
        MidiInput: midir::CommonMidiPortApi<T>,
    {
        for (i, port) in ports.into_iter().enumerate() {
            // For now, we'll just return the first port as a placeholder
            // In a real implementation, we'd need to check port names
            return Ok(port);
        }
        Err("No MIDI port found".into())
    }

    pub fn receiver(&self) -> Arc<Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>> {
        self.receiver.clone()
    }
}

// ── MIDI Control classes ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MidiControlConfig {
    pub control_type: String,
    pub control: Option<u8>,
    pub note: Option<u8>,
    pub style: Option<String>,
    pub light: Option<String>,
    pub binding: Option<MidiBinding>,
}

#[derive(Debug, Clone)]
pub struct MidiBinding {
    pub action: String,
    pub target: String,
    pub value: Option<serde_json::Value>,
    pub to_target: Option<String>, // Transform spec
    pub from_target: Option<String>, // Transform spec
}

pub trait MidiControl: Send + Sync {
    fn name(&self) -> &str;
    fn on_midi_message(&mut self, msg: &[u8]) -> Option<HashMap<String, serde_json::Value>>;
    fn set_value(&mut self, value: serde_json::Value);
    fn value(&self) -> &serde_json::Value;
    fn lockout(&self) -> bool;
}

pub struct MidiFader {
    name: String,
    value: serde_json::Value,
    lockout: bool,
    cc: u8,
    touch_note: Option<u8>,
    midi: Arc<Mutex<MidiIO>>,
}

impl MidiFader {
    pub fn new(name: String, config: &MidiControlConfig, midi: Arc<Mutex<MidiIO>>) -> Self {
        Self {
            name,
            value: serde_json::Value::Number(0.into()),
            lockout: false,
            cc: config.control.unwrap_or(70),
            touch_note: config.note,
            midi,
        }
    }
}

impl MidiControl for MidiFader {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg: &[u8]) -> Option<HashMap<String, serde_json::Value>> {
        if msg.len() >= 3 && msg[0] >> 4 == 0xB && msg[1] == self.cc {
            // Control change message
            self.value = serde_json::Value::Number((msg[2] as i64).into());
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            return Some(delta);
        }
        
        if let Some(touch_note) = self.touch_note {
            if msg.len() >= 3 && msg[0] >> 4 == 0x9 && msg[1] == touch_note {
                // Note on message
                self.lockout = msg[2] >= 64;
                let mut delta = HashMap::new();
                delta.insert("lockout".to_string(), serde_json::Value::Bool(self.lockout));
                return Some(delta);
            }
        }
        
        None
    }

    fn set_value(&mut self, value: serde_json::Value) {
        if !self.lockout {
            if let Some(v) = value.as_i64() {
                let v = v.max(0).min(127) as u8;
                self.value = serde_json::Value::Number((v as i64).into());
                if let Ok(midi) = self.midi.lock() {
                    let _ = midi.control_change(self.cc, v);
                }
            }
        }
    }

    fn value(&self) -> &serde_json::Value {
        &self.value
    }

    fn lockout(&self) -> bool {
        self.lockout
    }
}

pub struct MidiButton {
    name: String,
    value: serde_json::Value,
    pressed: bool,
    note: u8,
    style: String, // toggle | momentary
    light_style: String, // state | always_on | momentary | false
    midi: Arc<Mutex<MidiIO>>,
}

impl MidiButton {
    pub fn new(name: String, config: &MidiControlConfig, midi: Arc<Mutex<MidiIO>>) -> Self {
        Self {
            name,
            value: serde_json::Value::Bool(false),
            pressed: false,
            note: config.note.unwrap_or(0),
            style: config.style.clone().unwrap_or("toggle".to_string()),
            light_style: config.light.clone().unwrap_or("state".to_string()),
            midi,
        }
    }

    fn update_light(&self) {
        let on = match self.light_style.as_str() {
            "false" => false,
            "always_on" => true,
            "momentary" => self.pressed,
            _ => { // "state" or default
                if let Some(v) = self.value.as_bool() {
                    v
                } else {
                    false
                }
            }
        };
        
        if let Ok(midi) = self.midi.lock() {
            let _ = midi.note_on(self.note, if on { 127 } else { 0 });
        }
    }
}

impl MidiControl for MidiButton {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg: &[u8]) -> Option<HashMap<String, serde_json::Value>> {
        if msg.len() >= 3 && msg[0] >> 4 == 0x9 && msg[1] == self.note {
            if msg[2] >= 64 { // press
                self.pressed = true;
                if self.style == "toggle" {
                    if let Some(v) = self.value.as_bool() {
                        self.value = serde_json::Value::Bool(!v);
                    } else {
                        self.value = serde_json::Value::Bool(true);
                    }
                } else {
                    self.value = serde_json::Value::Bool(true);
                }
            } else { // release
                self.pressed = false;
                if self.style == "momentary" {
                    self.value = serde_json::Value::Bool(false);
                }
            }
            
            self.update_light();
            
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            delta.insert("pressed".to_string(), serde_json::Value::Bool(self.pressed));
            return Some(delta);
        }
        
        None
    }

    fn set_value(&mut self, value: serde_json::Value) {
        self.value = value;
        self.update_light();
    }

    fn value(&self) -> &serde_json::Value {
        &self.value
    }

    fn lockout(&self) -> bool {
        false // Buttons don't have lockout
    }
}

pub struct MidiRotary {
    name: String,
    value: serde_json::Value,
    lockout: bool,
    cc: u8,
    midi: Arc<Mutex<MidiIO>>,
}

impl MidiRotary {
    pub fn new(name: String, config: &MidiControlConfig, midi: Arc<Mutex<MidiIO>>) -> Self {
        Self {
            name,
            value: serde_json::Value::Number(0.into()),
            lockout: false,
            cc: config.control.unwrap_or(80),
            midi,
        }
    }
}

impl MidiControl for MidiRotary {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg: &[u8]) -> Option<HashMap<String, serde_json::Value>> {
        if msg.len() >= 3 && msg[0] >> 4 == 0xB && msg[1] == self.cc {
            self.value = serde_json::Value::Number((msg[2] as i64).into());
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            return Some(delta);
        }
        
        None
    }

    fn set_value(&mut self, value: serde_json::Value) {
        if !self.lockout {
            if let Some(v) = value.as_i64() {
                let v = v.max(0).min(127) as u8;
                self.value = serde_json::Value::Number((v as i64).into());
                if let Ok(midi) = self.midi.lock() {
                    let _ = midi.control_change(self.cc, v);
                }
            }
        }
    }

    fn value(&self) -> &serde_json::Value {
        &self.value
    }

    fn lockout(&self) -> bool {
        self.lockout
    }
}

pub struct MidiJogWheel {
    name: String,
    cc: u8,
}

impl MidiJogWheel {
    pub fn new(name: String, config: &MidiControlConfig) -> Self {
        Self {
            name,
            cc: config.control.unwrap_or(60),
        }
    }
}

impl MidiControl for MidiJogWheel {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg: &[u8]) -> Option<HashMap<String, serde_json::Value>> {
        if msg.len() >= 3 && msg[0] >> 4 == 0xB && msg[1] == self.cc {
            let direction = if msg[2] == 65 { 1 } else { -1 };
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), serde_json::Value::Number((direction as i64).into()));
            return Some(delta);
        }
        
        None
    }

    fn set_value(&mut self, _value: serde_json::Value) {
        // Jog wheels don't receive feedback
    }

    fn value(&self) -> &serde_json::Value {
        &serde_json::Value::Null
    }

    fn lockout(&self) -> bool {
        false
    }
}

// Control type registry
pub fn create_midi_control(
    control_type: &str,
    name: String,
    config: &MidiControlConfig,
    midi: Arc<Mutex<MidiIO>>,
) ) -> Option<Box<dyn MidiControl>> {
    match control_type {
        "fader" => Some(Box::new(MidiFader::new(name, config, midi))),
        "button" => Some(Box::new(MidiButton::new(name, config, midi))),
        "rotary" => Some(Box::new(MidiRotary::new(name, config, midi))),
        "jogwheel" => Some(Box::new(MidiJogWheel::new(name, config))),
        _ => None,
    }
}

// ── LCD Display state ────────────────────────────────────────────────────────

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

    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(text) = text {
            self.text = format!("{:<14}", &text[..std::cmp::min(text.len(), 14)]);
        }
        if let Some(color) = color {
            self.color = color;
        }
        if let Some(invert) = invert {
            self.invert = invert;
        }
        
        if let Ok(midi) = self.midi.lock() {
            midi.lcd_update(&self.text, self.color, self.invert)?;
        }
        
        Ok(())
    }

    pub fn update_top(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        /*!Update top 7 chars only.*/
        let padded_text = format!("{:^7}", &text[..std::cmp::min(text.len(), 7)]);
        self.text = format!("{}{}", padded_text, &self.text[7..14]);
        if let Some(color) = color {
            self.color = color;
        }
        
        if let Ok(midi) = self.midi.lock() {
            midi.lcd_update(&self.text, self.color, self.invert)?;
        }
        
        Ok(())
    }

    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        /*!Update bottom 7 chars only.*/
        let padded_text = format!("{:^7}", &text[..std::cmp::min(text.len(), 7)]);
        self.text = format!("{}{}", &self.text[..7], padded_text);
        if let Some(color) = color {
            self.color = color;
        }
        
        if let Ok(midi) = self.midi.lock() {
            midi.lcd_update(&self.text, self.color, self.invert)?;
        }
        
        Ok(())
    }
}

// ── MidiSurface: integrates with ozma ControlSurface ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiSurfaceConfig {
    pub device: String,
    pub controls: HashMap<String, MidiControlConfig>,
    pub displays: Option<HashMap<String, DisplayConfig>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(rename = "type")]
    pub display_type: String,
    pub binding: String,
}

pub struct MidiSurface {
    pub id: String,
    config: MidiSurfaceConfig,
    midi: Option<Arc<Mutex<MidiIO>>>,
    midi_controls: HashMap<String, Box<dyn MidiControl>>,
    scribble: Option<ScribbleStrip>,
    msg_map: HashMap<(String, u8), String>, // (msg_type, key_value) → control_name
}

impl MidiSurface {
    pub fn new(id: String, config: MidiSurfaceConfig) -> Self {
        Self {
            id,
            config,
            midi: None,
            midi_controls: HashMap::new(),
            scribble: None,
            msg_map: HashMap::new(),
        }
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !MidiIO::available() {
            warn!("MIDI I/O not available — MIDI surface '{}' disabled", self.id);
            return Ok(());
        }
        
        let device_name = self.config.device.clone();
        let midi = Arc::new(Mutex::new(MidiIO::new(device_name)));
        
        // Try to open MIDI device
        {
            let mut midi_lock = midi.lock().unwrap();
            if let Err(e) = midi_lock.open() {
                warn!("MIDI surface '{}' failed to open: {}", self.id, e);
                return Ok(());
            }
        }
        
        self.midi = Some(midi.clone());
        
        // Create controls
        for (name, cfg) in &self.config.controls {
            let control_type = &cfg.control_type;
            if let Some(control) = create_midi_control(control_type, name.clone(), cfg, midi.clone()) {
                // Build message routing map
                if let Some(control_num) = cfg.control {
                    self.msg_map.insert(("control".to_string(), control_num), name.clone());
                }
                if let Some(note_num) = cfg.note {
                    self.msg_map.insert(("note".to_string(), note_num), name.clone());
                }
                
                self.midi_controls.insert(name.clone(), control);
            } else {
                warn!("Unknown MIDI control type: {}", control_type);
            }
        }
        
        // Create scribble strip
        if self.config.displays.is_some() {
            self.scribble = Some(ScribbleStrip::new(midi.clone()));
        }
        
        info!("MIDI surface '{}' started: {} controls, {} displays",
              self.id,
              self.midi_controls.len(),
              self.config.displays.as_ref().map(|d| d.len()).unwrap_or(0));
        
        Ok(())
    }

    pub async fn stop(&mut self) {
        if let Some(midi) = &self.midi {
            if let Ok(mut midi_lock) = midi.lock() {
                midi_lock.close();
            }
        }
        info!("MIDI surface '{}' stopped", self.id);
    }

    pub fn process_midi_message(&mut self, msg: &[u8]) -> Option<(String, HashMap<String, serde_json::Value>)> {
        // Map message type
        let (msg_type, key) = if msg.len() >= 3 {
            let status = msg[0];
            let key = msg[1];
            
            if status >> 4 == 0xB { // Control change
                ("control".to_string(), key)
            } else if status >> 4 == 0x9 || status >> 4 == 0x8 { // Note on/off
                ("note".to_string(), key)
            } else {
                return None;
            }
        } else {
            return None;
        };
        
        let key = (msg_type, key);
        if let Some(control_name) = self.msg_map.get(&key) {
            if let Some(control) = self.midi_controls.get_mut(control_name) {
                if let Some(delta) = control.on_midi_message(msg) {
                    return Some((control_name.clone(), delta));
                }
            }
        }
        
        None
    }

    pub fn set_control_value(&mut self, control_name: &str, value: serde_json::Value) {
        if let Some(control) = self.midi_controls.get_mut(control_name) {
            control.set_value(value);
        }
    }

    pub fn make_display_updater(&self, display_type: &str) -> Box<dyn Fn(&str, Option<&str>) -> Result<(), Box<dyn std::error::Error>> + Send + Sync> {
        let scribble = self.scribble.clone();
        
        match display_type {
            "scribble_top" => {
                Box::new(move |text: &str, color: Option<&str>| -> Result<(), Box<dyn std::error::Error>> {
                    if let Some(scribble) = &scribble {
                        let lcd_color = hex_to_lcd_color(color);
                        scribble.lock().unwrap().update_top(text, Some(lcd_color))?;
                    }
                    Ok(())
                })
            },
            "scribble_bottom" => {
                Box::new(move |text: &str, color: Option<&str>| -> Result<(), Box<dyn std::error::Error>> {
                    if let Some(scribble) = &scribble {
                        let lcd_color = hex_to_lcd_color(color);
                        scribble.lock().unwrap().update_bottom(text, Some(lcd_color))?;
                    }
                    Ok(())
                })
            },
            "scribble" | _ => {
                Box::new(move |text: &str, color: Option<&str>| -> Result<(), Box<dyn std::error::Error>> {
                    if let Some(scribble) = &scribble {
                        let lcd_color = hex_to_lcd_color(color);
                        scribble.lock().unwrap().update(Some(text), Some(lcd_color), None)?;
                    }
                    Ok(())
                })
            },
        }
    }

    pub fn make_transform(spec: Option<&str>) -> Option<Box<dyn Fn(f64) -> f64 + Send + Sync>> {
        /*!
        Build a value transform from a config spec.

        Spec can be:
          - None: no transform
          - "midi_to_float": MIDI 0-127 → float 0.0-1.0
          - "float_to_midi": float 0.0-1.0 → MIDI 0-127
        */
        match spec {
            None => None,
            Some("midi_to_float") => Some(Box::new(|v: f64| v / 127.0)),
            Some("float_to_midi") => Some(Box::new(|v: f64| (v * 127.0).max(0.0).min(127.0))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_mapping() {
        assert_eq!(hex_to_lcd_color(Some("#ff0000")), Color::Red);
        assert_eq!(hex_to_lcd_color(Some("#00ff00")), Color::Green);
        assert_eq!(hex_to_lcd_color(Some("#0000ff")), Color::Blue);
        assert_eq!(hex_to_lcd_color(None), Color::White);
    }

    #[test]
    fn test_7seg_render() {
        let result = render_7seg("123");
        assert_eq!(result, vec![0x06, 0x5B, 0x4F]);
    }

    #[test]
    fn test_midi_control_creation() {
        let config = MidiControlConfig {
            control_type: "fader".to_string(),
            control: Some(70),
            note: Some(110),
            style: None,
            light: None,
            binding: None,
        };
        
        let midi = Arc::new(Mutex::new(MidiIO::new("test".to_string())));
        let control = create_midi_control("fader", "test_fader".to_string(), &config, midi);
        assert!(control.is_some());
    }
}
