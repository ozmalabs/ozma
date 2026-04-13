//! Low-level MIDI I/O handling

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use midir::{MidiInput, MidiOutput, MidiInputConnection, MidiOutputConnection};
use crate::midi::{MidiError, Result};

/// 7-segment font for Behringer segment displays
const SEG7_FONT: [u8; 256] = {
    let mut font = [0u8; 256];
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

/// Color codes for LCD displays
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

impl From<&str> for Color {
    fn from(hex: &str) -> Self {
        match hex.to_lowercase().as_str() {
            "#ff0000" => Color::Red,
            "#00ff00" => Color::Green,
            "#0000ff" => Color::Blue,
            "#ffff00" => Color::Yellow,
            "#ff00ff" => Color::Magenta,
            "#00ffff" => Color::Cyan,
            "#ffffff" => Color::White,
            "#000000" => Color::Black,
            _ => Color::White,
        }
    }
}

/// Invert options for LCD displays
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

/// Low-level MIDI device wrapper
pub struct MidiIO {
    input_connection: Option<MidiInputConnection<()>>,
    output_connection: Option<MidiOutputConnection>,
}

impl MidiIO {
    /// Create a new MIDI I/O instance
    pub fn new() -> Result<Self> {
        Ok(Self {
            input_connection: None,
            output_connection: None,
        })
    }

    /// Open MIDI connections
    pub fn open(&mut self, device_name: &str) -> Result<()> {
        // Find and open input port
        let midi_in = MidiInput::new("ozma-midi-in")?;
        let input_ports = midi_in.ports();
        
        let input_port = input_ports
            .iter()
            .find(|port| {
                if let Ok(port_name) = midi_in.port_name(port) {
                    port_name.starts_with(device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::PortNotFound(device_name.to_string()))?
            .clone();

        // Find and open output port
        let midi_out = MidiOutput::new("ozma-midi-out")?;
        let output_ports = midi_out.ports();
        
        let output_port = output_ports
            .iter()
            .find(|port| {
                if let Ok(port_name) = midi_out.port_name(port) {
                    port_name.starts_with(device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError::PortNotFound(device_name.to_string()))?
            .clone();

        // Actually connect to the ports
        let _input_connection = midi_in.connect(&input_port, "ozma-midi-in", |_timestamp, _message, _| {
            // This callback would handle incoming messages
            // In a real implementation, we'd send them to a channel
        }, ())?;
        
        let output_connection = midi_out.connect(&output_port, "ozma-midi-out")?;
        
        self.output_connection = Some(output_connection);
        // Note: We're not storing the input connection because we need a better way to handle callbacks
        
        Ok(())
    }

    /// Close MIDI connections
    pub fn close(&mut self) {
        self.input_connection = None;
        self.output_connection = None;
    }

    /// Send a raw MIDI message
    pub fn send(&mut self, msg: &[u8]) -> Result<()> {
        if let Some(ref mut conn) = self.output_connection {
            conn.send(msg).map_err(MidiError::from)?;
        }
        Ok(())
    }

    /// Send a note on message
    pub fn note_on(&mut self, note: u8, velocity: u8) -> Result<()> {
        self.send(&[0x90, note, velocity])
    }

    /// Send a control change message
    pub fn control_change(&mut self, control: u8, value: u8) -> Result<()> {
        self.send(&[0xB0, control, value])
    }

    /// Send a SysEx message
    pub fn sysex(&mut self, data: &[u8]) -> Result<()> {
        let mut msg = vec![0xF0];
        msg.extend_from_slice(data);
        msg.push(0xF7);
        self.send(&msg)
    }

    /// Send Behringer X-Touch scribble strip LCD update (14 chars)
    pub fn lcd_update(&mut self, text: &str, color: Color, invert: Invert) -> Result<()> {
        let mut chars = [0u8; 14];
        let text_bytes = text.as_bytes();
        for (i, &byte) in text_bytes.iter().take(14).enumerate() {
            chars[i] = byte;
        }
        
        let color_code = color as u8 | ((invert as u8) << 4);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend_from_slice(&chars);
        
        self.sysex(&data)
    }

    /// Send Behringer 7-segment display update (12 chars)
    pub fn segment_update(&mut self, text: &str) -> Result<()> {
        let mut rendered = [0u8; 12];
        let text_bytes = text.as_bytes();
        for (i, &byte) in text_bytes.iter().take(12).enumerate() {
            let c = byte as char;
            if c.is_ascii() {
                rendered[i] = SEG7_FONT[c as usize];
            }
        }
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend_from_slice(&rendered);
        data.extend_from_slice(&[0x00, 0x00]);
        
        self.sysex(&data)
    }
}

impl Drop for MidiIO {
    fn drop(&mut self) {
        self.close();
    }
}
//! Low-level MIDI I/O using midir crate

use midir::{MidiInput, MidiOutput, MidiInputConnection, MidiOutputConnection, Ignore};
use std::sync::{Arc, Mutex};
use std::error::Error;
use std::fmt;

/// 7-segment font for Behringer displays
const SEG7_FONT: [u8; 91] = {
    let mut font = [0; 91];
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
    font[b' ' as usize] = 0x00;
    font[b'-' as usize] = 0x40;
    font[b'.' as usize] = 0x08;
    font[b':' as usize] = 0x09;
    font[b'(' as usize] = 0x39;
    font[b')' as usize] = 0x0F;
    font
};

/// MIDI I/O error
#[derive(Debug)]
pub struct MidiError(String);

impl fmt::Display for MidiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "MIDI error: {}", self.0)
    }
}

impl Error for MidiError {}

impl From<midir::InitError> for MidiError {
    fn from(err: midir::InitError) -> Self {
        MidiError(format!("Init error: {:?}", err))
    }
}

impl From<midir::ConnectError<midir::MidiInput>> for MidiError {
    fn from(err: midir::ConnectError<midir::MidiInput>) -> Self {
        MidiError(format!("Connect error: {:?}", err))
    }
}

impl From<midir::ConnectError<midir::MidiOutput>> for MidiError {
    fn from(err: midir::ConnectError<midir::MidiOutput>) -> Self {
        MidiError(format!("Connect error: {:?}", err))
    }
}

impl From<midir::SendError> for MidiError {
    fn from(err: midir::SendError) -> Self {
        MidiError(format!("Send error: {:?}", err))
    }
}

/// Low-level MIDI device wrapper using midir
pub struct MidiIO {
    device_name: String,
    input_connection: Option<MidiInputConnection<()>>,
    output_connection: Option<MidiOutputConnection>,
    callback: Arc<Mutex<Option<Box<dyn Fn(&[u8]) + Send>>>>,
}

impl MidiIO {
    /// Create a new MIDI I/O wrapper
    pub fn new(device_name: String) -> Self {
        Self {
            device_name,
            input_connection: None,
            output_connection: None,
            callback: Arc::new(Mutex::new(None)),
        }
    }

    /// List available MIDI devices
    pub fn list_devices() -> Result<Vec<String>, MidiError> {
        let mut devices = std::collections::HashSet::new();
        
        // Get input devices
        let input = MidiInput::new("ozma-midi-input")?;
        for port in input.ports() {
            if let Ok(name) = input.port_name(&port) {
                devices.insert(name);
            }
        }
        
        // Get output devices
        let output = MidiOutput::new("ozma-midi-output")?;
        for port in output.ports() {
            if let Ok(name) = output.port_name(&port) {
                devices.insert(name);
            }
        }
        
        Ok(devices.into_iter().collect())
    }

    /// Open MIDI connections
    pub fn open(&mut self) -> Result<(), MidiError> {
        // Open input
        let mut input = MidiInput::new("ozma-midi-input")?;
        input.ignore(Ignore::None);
        
        let ports = input.ports();
        let port_index = ports.iter()
            .position(|port| {
                if let Ok(name) = input.port_name(port) {
                    name.starts_with(&self.device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError(format!("No MIDI input port matching '{}'", self.device_name)))?;
        
        let callback = self.callback.clone();
        self.input_connection = Some(input.connect(
            &ports[port_index], 
            "ozma-midi-input-handler", 
            move |_stamp, message, _| {
                if let Some(cb) = &*callback.lock().unwrap() {
                    cb(message);
                }
            }, 
            ()
        )?);
        
        // Open output
        let output = MidiOutput::new("ozma-midi-output")?;
        let ports = output.ports();
        let port_index = ports.iter()
            .position(|port| {
                if let Ok(name) = output.port_name(port) {
                    name.starts_with(&self.device_name)
                } else {
                    false
                }
            })
            .ok_or_else(|| MidiError(format!("No MIDI output port matching '{}'", self.device_name)))?;
        
        self.output_connection = Some(output.connect(&ports[port_index], "ozma-midi-output-handler")?);
        
        Ok(())
    }

    /// Close MIDI connections
    pub fn close(&mut self) {
        self.input_connection.take();
        self.output_connection.take();
    }

    /// Send a raw MIDI message
    pub fn send(&mut self, message: &[u8]) -> Result<(), MidiError> {
        if let Some(conn) = &mut self.output_connection {
            conn.send(message)?;
        }
        Ok(())
    }

    /// Send a note on message
    pub fn note_on(&mut self, note: u8, velocity: u8) -> Result<(), MidiError> {
        self.send(&[0x90, note, velocity])
    }

    /// Send a control change message
    pub fn control_change(&mut self, control: u8, value: u8) -> Result<(), MidiError> {
        self.send(&[0xB0, control, value])
    }

    /// Send a SysEx message
    pub fn sysex(&mut self, data: &[u8]) -> Result<(), MidiError> {
        let mut message = vec![0xF0];
        message.extend_from_slice(data);
        message.push(0xF7);
        self.send(&message)
    }

    /// Send Behringer X-Touch scribble strip LCD update (14 chars)
    pub fn lcd_update(&mut self, text: &str, color: super::types::Color, invert: super::types::Invert) -> Result<(), MidiError> {
        // Convert Unicode to ASCII
        let ascii_text: Vec<u8> = text.chars()
            .map(|c| if c as u32 <= 127 { c as u8 } else { b'?' })
            .take(14)
            .collect();
        
        let mut chars = ascii_text;
        while chars.len() < 14 {
            chars.push(0);
        }
        
        let color_code = color as u8 | ((invert as u8) << 4);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend_from_slice(&chars);
        
        self.sysex(&data)
    }

    /// Send Behringer 7-segment display update (12 chars)
    pub fn segment_update(&mut self, text: &str) -> Result<(), MidiError> {
        // Convert Unicode to ASCII and render with 7-segment font
        let rendered: Vec<u8> = text.chars()
            .map(|c| {
                let ascii_byte = if c as u32 <= 127 { c as u8 } else { b'?' };
                let idx = if ascii_byte < 91 { ascii_byte as usize } else { b' ' as usize };
                if idx < SEG7_FONT.len() { SEG7_FONT[idx] } else { 0 }
            })
            .take(12)
            .collect();
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend_from_slice(&rendered);
        
        // Pad with zeros to make sure we have exactly 12 characters + 2 trailing zeros
        while data.len() < 19 {
            data.push(0);
        }
        
        self.sysex(&data)
    }

    /// Set the MIDI message callback
    pub fn set_callback<F>(&self, callback: Option<F>)
    where
        F: Fn(&[u8]) + Send + 'static,
    {
        *self.callback.lock().unwrap() = callback.map(|f| Box::new(f) as Box<dyn Fn(&[u8]) + Send>);
    }
}

impl Drop for MidiIO {
    fn drop(&mut self) {
        self.close();
    }
}
//! Low-level MIDI I/O using midir crate

use midir::{MidiInput, MidiOutput, MidiInputConnection};
use std::sync::{Arc, Mutex};
use crate::midi::types::{Color, Invert, SegmentFont};

/// MIDI I/O wrapper using midir
pub struct MidiIO {
    input_connection: Option<MidiInputConnection<()>>,
    output_connection: Option<midir::MidiOutputConnection>,
    device_name: String,
}

impl MidiIO {
    pub fn new(device_name: String) -> Self {
        Self {
            input_connection: None,
            output_connection: None,
            device_name,
        }
    }

    /// Check if MIDI support is available
    pub fn available() -> bool {
        true // midir is compiled in
    }

    /// List available MIDI devices
    pub fn list_devices() -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut devices = std::collections::HashSet::new();
        
        // Get input devices
        let input = MidiInput::new("ozma-midi-input")?;
        for port in input.ports() {
            if let Ok(name) = input.port_name(&port) {
                devices.insert(name);
            }
        }
        
        // Get output devices
        let output = MidiOutput::new("ozma-midi-output")?;
        for port in output.ports() {
            if let Ok(name) = output.port_name(&port) {
                devices.insert(name);
            }
        }
        
        Ok(devices.into_iter().collect())
    }

    /// Open MIDI ports
    pub fn open<F>(&mut self, callback: F) -> Result<(), Box<dyn std::error::Error>>
    where
        F: Fn(&[u8]) + Send + 'static,
    {
        // Open input
        let mut input = MidiInput::new("ozma-midi-input")?;
        let in_ports = input.ports();
        
        for port in &in_ports {
            if let Ok(name) = input.port_name(port) {
                if name.starts_with(&self.device_name) {
                    let conn = input.connect(
                        port,
                        "ozma-midi-input-connection",
                        move |_stamp, message, _| {
                            callback(message);
                        },
                        ()
                    )?;
                    self.input_connection = Some(conn);
                    break;
                }
            }
        }
        
        // Open output
        let output = MidiOutput::new("ozma-midi-output")?;
        let out_ports = output.ports();
        
        for port in &out_ports {
            if let Ok(name) = output.port_name(port) {
                if name.starts_with(&self.device_name) {
                    let conn = output.connect(port, "ozma-midi-output-connection")?;
                    self.output_connection = Some(conn);
                    break;
                }
            }
        }
        
        Ok(())
    }

    /// Close MIDI ports
    pub fn close(&mut self) {
        self.input_connection = None;
        self.output_connection = None;
    }

    /// Send a raw MIDI message
    pub fn send(&mut self, message: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(conn) = &mut self.output_connection {
            conn.send(message)?;
        }
        Ok(())
    }

    /// Send note on message
    pub fn note_on(&mut self, note: u8, velocity: u8) -> Result<(), Box<dyn std::error::Error>> {
        self.send(&[0x90, note, velocity])
    }

    /// Send control change message
    pub fn control_change(&mut self, control: u8, value: u8) -> Result<(), Box<dyn std::error::Error>> {
        self.send(&[0xB0, control, value])
    }

    /// Send SysEx message
    pub fn sysex(&mut self, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let mut message = vec![0xF0];
        message.extend_from_slice(data);
        message.push(0xF7);
        self.send(&message)
    }

    /// Update LCD display on Behringer X-Touch
    pub fn lcd_update(
        &mut self,
        text: &str,
        color: Color,
        invert: Invert,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Convert text to ASCII
        let ascii_text = text.chars()
            .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
            .take(14)
            .collect::<Vec<u8>>();
        
        // Pad to 14 characters
        let mut chars = ascii_text;
        while chars.len() < 14 {
            chars.push(0);
        }
        
        let color_code = (color as u8) | ((invert as u8) << 4);
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend_from_slice(&chars);
        
        self.sysex(&data)
    }

    /// Update 7-segment display
    pub fn segment_update(&mut self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        let rendered = SegmentFont::render_text(text);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend_from_slice(&rendered);
        
        // Pad to 12 characters + 2 zero bytes
        while data.len() < 19 {
            data.push(0);
        }
        
        self.sysex(&data)
    }
}
