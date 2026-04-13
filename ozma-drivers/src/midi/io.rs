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
