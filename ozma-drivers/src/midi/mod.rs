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
use std::time::Duration;
use midir::{MidiInput, MidiInputPort, MidiOutput, MidiOutputPort, MidiInputConnection};
use midir::os::unix::VirtualMidiDeviceExt;
use thiserror::Error;
use log::{info, warn, debug};

#[derive(Error, Debug)]
pub enum MidiError {
    #[error("MIDI backend error: {0}")]
    Backend(#[from] midir::InitError),
    #[error("MIDI connection error: {0}")]
    Connection(#[from] midir::ConnectError<midir::InitError>),
    #[error("MIDI send error: {0}")]
    Send(#[from] midir::SendError),
    #[error("Device not found: {0}")]
    DeviceNotFound(String),
    #[error("Invalid message: {0}")]
    InvalidMessage(String),
}

pub type Result<T> = std::result::Result<T, MidiError>;

// ── Enums ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
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
    pub fn from_hex(hex_color: &str) -> Self {
        let hex_color = hex_color.to_lowercase();
        match hex_color.as_str() {
            "#ff0000" => Color::Red,
            "#00ff00" => Color::Green,
            "#0000ff" => Color::Blue,
            "#ffff00" => Color::Yellow,
            "#ff00ff" => Color::Magenta,
            "#00ffff" => Color::Cyan,
            "#ffffff" => Color::White,
            "#000000" => Color::Black,
            _ => {
                // Try name match
                if hex_color.contains("red") {
                    Color::Red
                } else if hex_color.contains("green") {
                    Color::Green
                } else if hex_color.contains("blue") {
                    Color::Blue
                } else if hex_color.contains("yellow") {
                    Color::Yellow
                } else if hex_color.contains("magenta") {
                    Color::Magenta
                } else if hex_color.contains("cyan") {
                    Color::Cyan
                } else {
                    Color::White
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

// ── 7-segment font (for Behringer segment displays) ─────────────────────────

const SEGMENT_FONT: [u8; 128] = {
    let mut font = [0u8; 128];
    // Initialize with default values
    let mut i = 0;
    while i < 128 {
        font[i] = 0;
        i += 1;
    }
    
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

fn render_7seg(text: &str) -> Vec<u8> {
    text.chars()
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

// ── Low-level MIDI I/O ──────────────────────────────────────────────────────

pub struct MidiIO {
    input: Option<MidiInput>,
    output: Option<MidiOutput>,
    input_port: Option<MidiInputPort>,
    output_port: Option<MidiOutputPort>,
    connection: Option<MidiInputConnection<()>>,
    device_name: String,
}

impl MidiIO {
    pub fn new(device_name: String) -> Self {
        Self {
            input: None,
            output: None,
            input_port: None,
            output_port: None,
            connection: None,
            device_name,
        }
    }

    pub fn available() -> bool {
        true // midir is always available in Rust
    }

    pub fn list_devices() -> Result<Vec<String>> {
        let input = MidiInput::new("ozma-midi-input")?;
        let output = MidiOutput::new("ozma-midi-output")?;
        
        let mut devices = Vec::new();
        
        // Get input devices
        for port in input.ports() {
            if let Ok(name) = input.port_name(&port) {
                devices.push(name);
            }
        }
        
        // Get output devices
        for port in output.ports() {
            if let Ok(name) = output.port_name(&port) {
                if !devices.contains(&name) {
                    devices.push(name);
                }
            }
        }
        
        Ok(devices)
    }

    pub fn open(&mut self) -> Result<()> {
        // Create MIDI input
        let mut input = MidiInput::new("ozma-midi-input")?;
        let input_ports = input.ports();
        
        // Find matching input port
        let input_port = input_ports
            .iter()
            .find(|port| {
                if let Ok(name) = input.port_name(port) {
                    name.starts_with(&self.device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::DeviceNotFound(format!("Input port matching '{}'", self.device_name)))?
            .clone();
        
        // Create MIDI output
        let output = MidiOutput::new("ozma-midi-output")?;
        let output_ports = output.ports();
        
        // Find matching output port
        let output_port = output_ports
            .iter()
            .find(|port| {
                if let Ok(name) = output.port_name(port) {
                    name.starts_with(&self.device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::DeviceNotFound(format!("Output port matching '{}'", self.device_name)))?
            .clone();
        
        self.input = Some(input);
        self.output = Some(output);
        self.input_port = Some(input_port);
        self.output_port = Some(output_port);
        
        info!("MIDI opened: device={}", self.device_name);
        Ok(())
    }

    pub fn close(&mut self) {
        self.connection = None;
        self.input = None;
        self.output = None;
        self.input_port = None;
        self.output_port = None;
    }

    pub fn send(&mut self, msg: &[u8]) -> Result<()> {
        if let (Some(output), Some(port)) = (&self.output, &self.output_port) {
            output.connect(port, "ozma-midi-output")?.send(msg)?;
        }
        Ok(())
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) -> Result<()> {
        self.send(&[0x90, note, velocity])
    }

    pub fn control_change(&mut self, control: u8, value: u8) -> Result<()> {
        self.send(&[0xB0, control, value])
    }

    pub fn sysex(&mut self, data: &[u8]) -> Result<()> {
        let mut msg = vec![0xF0];
        msg.extend_from_slice(data);
        msg.push(0xF7);
        self.send(&msg)
    }

    pub fn lcd_update(&mut self, text: &str, color: Color, invert: Invert) -> Result<()> {
        /// Send Behringer X-Touch scribble strip LCD update (14 chars).
        let mut chars = text.chars().take(14).collect::<Vec<_>>();
        while chars.len() < 14 {
            chars.push(' ');
        }
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00];
        let color_code = (color as u8) | ((invert as u8) << 4);
        data.push(color_code);
        
        for ch in chars {
            data.push(ch as u8);
        }
        
        self.sysex(&data)
    }

    pub fn segment_update(&mut self, text: &str) -> Result<()> {
        /// Send Behringer 7-segment display update (12 chars).
        let rendered = render_7seg(&text[..text.len().min(12)]);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        
        for &byte in &rendered {
            data.push(byte);
        }
        
        // Pad with zeros to reach 12 characters + 2 trailing zeros
        while data.len() < 5 + 12 + 2 {
            data.push(0);
        }
        
        self.sysex(&data)
    }

    pub fn set_callback<F>(&mut self, callback: F) -> Result<()>
    where
        F: Fn(&[u8], &mut ()) + Send + 'static,
    {
        if let (Some(input), Some(port)) = (&mut self.input, &self.input_port) {
            let connection = input.connect(port, "ozma-midi-input", move |timestamp, message, _| {
                callback(message, &mut ());
            }, ())?;
            self.connection = Some(connection);
        }
        Ok(())
    }
}

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

    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) -> Result<()> {
        if let Some(text) = text {
            self.text = format!("{:14}", text.chars().take(14).collect::<String>());
        }
        if let Some(color) = color {
            self.color = color;
        }
        if let Some(invert) = invert {
            self.invert = invert;
        }
        
        let mut midi = self.midi.lock().unwrap();
        midi.lcd_update(&self.text, self.color, self.invert)
    }

    pub fn update_top(&mut self, text: &str, color: Option<Color>) -> Result<()> {
        let padded_text = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", padded_text, &self.text[7..14]);
        if let Some(color) = color {
            self.color = color;
        }
        
        let mut midi = self.midi.lock().unwrap();
        midi.lcd_update(&self.text, self.color, self.invert)
    }

    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) -> Result<()> {
        let padded_text = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", &self.text[..7], padded_text);
        if let Some(color) = color {
            self.color = color;
        }
        
        let mut midi = self.midi.lock().unwrap();
        midi.lcd_update(&self.text, self.color, self.invert)
    }
}

// ── MidiSurface: integrates with ozma ControlSurface ─────────────────────────

pub struct MidiSurface {
    id: String,
    device_name: String,
    midi: Arc<Mutex<MidiIO>>,
    controls: HashMap<String, Box<dyn MidiControl>>,
    scribble: Option<ScribbleStrip>,
    msg_map: HashMap<(u8, u8), String>, // (message_type, key) -> control_name
}

impl MidiSurface {
    pub fn new(id: String, device_name: String) -> Self {
        let midi = Arc::new(Mutex::new(MidiIO::new(device_name.clone())));
        
        Self {
            id,
            device_name,
            midi,
            controls: HashMap::new(),
            scribble: None,
            msg_map: HashMap::new(),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        if !MidiIO::available() {
            warn!("MIDI backend not available - surface '{}' disabled", self.id);
            return Ok(());
        }
        
        {
            let mut midi = self.midi.lock().unwrap();
            midi.open()?;
        }
        
        info!("MIDI surface '{}' started", self.id);
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        let mut midi = self.midi.lock().unwrap();
        midi.close();
        info!("MIDI surface '{}' stopped", self.id);
        Ok(())
    }

    pub fn add_control(&mut self, name: String, control: Box<dyn MidiControl>) {
        self.controls.insert(name.clone(), control);
    }

    pub fn get_control(&self, name: &str) -> Option<&dyn MidiControl> {
        self.controls.get(name).map(|c| c.as_ref())
    }

    pub fn get_control_mut(&mut self, name: &str) -> Option<&mut dyn MidiControl> {
        self.controls.get_mut(name).map(|c| c.as_mut())
    }

    pub fn set_scribble(&mut self, scribble: ScribbleStrip) {
        self.scribble = Some(scribble);
    }

    pub fn handle_midi_message(&mut self, message: &[u8]) -> Option<(String, MidiControlState)> {
        if message.is_empty() {
            return None;
        }
        
        let msg_type = message[0] & 0xF0;
        let key = if message.len() > 1 { message[1] } else { 0 };
        
        if let Some(control_name) = self.msg_map.get(&(msg_type, key)) {
            if let Some(control) = self.controls.get_mut(control_name) {
                if let Some(state) = control.on_midi_message(message) {
                    return Some((control_name.clone(), state));
                }
            }
        }
        
        None
    }
}
