//! Display implementations for MIDI surfaces

use std::sync::{Arc, Mutex};
use midir::MidiOutputConnection;
use super::types::{Color, Invert, render_7seg, unidecode};

/// Behringer X-Touch scribble strip (14 chars, color, invert)
pub struct ScribbleStrip {
    midi_out: Arc<Mutex<Option<MidiOutputConnection>>>,
    text: String,
    color: Color,
    invert: Invert,
}

impl ScribbleStrip {
    pub fn new(midi_out: Arc<Mutex<Option<MidiOutputConnection>>>) -> Self {
        Self {
            midi_out,
            text: " ".repeat(14),
            color: Color::White,
            invert: Invert::None,
        }
    }
    
    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(t) = text {
            self.text = format!("{:<14}", &t[..t.len().min(14)]);
        }
        if let Some(c) = color {
            self.color = c;
        }
        if let Some(i) = invert {
            self.invert = i;
        }
        
        self.send_lcd_update()?;
        Ok(())
    }
    
    pub fn update_top(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        let formatted = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", formatted, &self.text[7..]);
        if let Some(c) = color {
            self.color = c;
        }
        
        self.send_lcd_update()?;
        Ok(())
    }
    
    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        let formatted = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", &self.text[..7], formatted);
        if let Some(c) = color {
            self.color = c;
        }
        
        self.send_lcd_update()?;
        Ok(())
    }
    
    fn send_lcd_update(&self) -> Result<(), Box<dyn std::error::Error>> {
        let text = unidecode(&self.text);
        let mut chars: Vec<u8> = text.chars().take(14).map(|c| c as u8).collect();
        while chars.len() < 14 {
            chars.push(0);
        }
        
        let color_code = self.color as u8 | ((self.invert as u8) << 4);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend(chars);
        
        self.send_sysex(&data)
    }
    
    pub fn segment_update(&self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        let text = unidecode(text);
        let mut rendered = render_7seg(&text);
        while rendered.len() < 12 {
            rendered.push(0);
        }
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend(rendered);
        data.extend_from_slice(&[0x00, 0x00]);
        
        self.send_sysex(&data)
    }
    
    fn send_sysex(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(midi_out) = self.midi_out.lock() {
            if let Some(conn) = &*midi_out {
                let mut message = vec![0xF0];
                message.extend_from_slice(data);
                message.push(0xF7);
                conn.send(&message)?;
            }
        }
        Ok(())
    }
}
//! Display handling for MIDI surfaces

use crate::midi::io::MidiIO;
use crate::midi::io::Color as MidiColor;
use crate::midi::io::Invert as MidiInvert;

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

/// Behringer X-Touch scribble strip (14 chars, color, invert)
pub struct ScribbleStrip {
    midi: MidiIO,
    pub text: String,
    pub color: Color,
    pub invert: Invert,
}

impl ScribbleStrip {
    pub fn new(midi: MidiIO) -> Self {
        Self {
            midi,
            text: " ".repeat(14),
            color: Color::White,
            invert: Invert::None,
        }
    }

    /// Update the entire display
    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(t) = text {
            self.text = format!("{:<14}", t).chars().take(14).collect();
        }
        if let Some(c) = color {
            self.color = c;
        }
        if let Some(i) = invert {
            self.invert = i;
        }
        
        // Convert to MIDI color/invert types
        let midi_color = match self.color {
            Color::Black => MidiColor::Black,
            Color::Red => MidiColor::Red,
            Color::Green => MidiColor::Green,
            Color::Yellow => MidiColor::Yellow,
            Color::Blue => MidiColor::Blue,
            Color::Magenta => MidiColor::Magenta,
            Color::Cyan => MidiColor::Cyan,
            Color::White => MidiColor::White,
        };
        
        let midi_invert = match self.invert {
            Invert::None => MidiInvert::None,
            Invert::Top => MidiInvert::Top,
            Invert::Bottom => MidiInvert::Bottom,
            Invert::Both => MidiInvert::Both,
        };
        
        self.midi.lcd_update(&self.text, midi_color, midi_invert)?;
        Ok(())
    }

    /// Update top 7 chars only
    pub fn update_top(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        let padded_text = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", padded_text, &self.text[7..]);
        if let Some(c) = color {
            self.color = c;
        }
        
        // Convert to MIDI color/invert types
        let midi_color = match self.color {
            Color::Black => MidiColor::Black,
            Color::Red => MidiColor::Red,
            Color::Green => MidiColor::Green,
            Color::Yellow => MidiColor::Yellow,
            Color::Blue => MidiColor::Blue,
            Color::Magenta => MidiColor::Magenta,
            Color::Cyan => MidiColor::Cyan,
            Color::White => MidiColor::White,
        };
        
        let midi_invert = match self.invert {
            Invert::None => MidiInvert::None,
            Invert::Top => MidiInvert::Top,
            Invert::Bottom => MidiInvert::Bottom,
            Invert::Both => MidiInvert::Both,
        };
        
        self.midi.lcd_update(&self.text, midi_color, midi_invert)?;
        Ok(())
    }

    /// Update bottom 7 chars only
    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        let padded_text = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", &self.text[..7], padded_text);
        if let Some(c) = color {
            self.color = c;
        }
        
        // Convert to MIDI color/invert types
        let midi_color = match self.color {
            Color::Black => MidiColor::Black,
            Color::Red => MidiColor::Red,
            Color::Green => MidiColor::Green,
            Color::Yellow => MidiColor::Yellow,
            Color::Blue => MidiColor::Blue,
            Color::Magenta => MidiColor::Magenta,
            Color::Cyan => MidiColor::Cyan,
            Color::White => MidiColor::White,
        };
        
        let midi_invert = match self.invert {
            Invert::None => MidiInvert::None,
            Invert::Top => MidiInvert::Top,
            Invert::Bottom => MidiInvert::Bottom,
            Invert::Both => MidiInvert::Both,
        };
        
        self.midi.lcd_update(&self.text, midi_color, midi_invert)?;
        Ok(())
    }
}
//! Display implementations for MIDI surfaces

use std::sync::{Arc, Mutex};
use midir::MidiOutputConnection;
use super::types::{Color, Invert, render_7seg, unidecode};

/// Behringer X-Touch scribble strip (14 chars, color, invert)
pub struct ScribbleStrip {
    midi_out: Arc<Mutex<Option<MidiOutputConnection>>>,
    text: String,
    color: Color,
    invert: Invert,
}

impl ScribbleStrip {
    pub fn new(midi_out: Arc<Mutex<Option<MidiOutputConnection>>>) -> Self {
        Self {
            midi_out,
            text: " ".repeat(14),
            color: Color::White,
            invert: Invert::None,
        }
    }
    
    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(t) = text {
            self.text = format!("{:<14}", &t[..t.len().min(14)]);
        }
        if let Some(c) = color {
            self.color = c;
        }
        if let Some(i) = invert {
            self.invert = i;
        }
        
        self.send_lcd_update()?;
        Ok(())
    }
    
    pub fn update_top(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        let formatted = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", formatted, &self.text[7..]);
        if let Some(c) = color {
            self.color = c;
        }
        
        self.send_lcd_update()?;
        Ok(())
    }
    
    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) -> Result<(), Box<dyn std::error::Error>> {
        let formatted = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", &self.text[..7], formatted);
        if let Some(c) = color {
            self.color = c;
        }
        
        self.send_lcd_update()?;
        Ok(())
    }
    
    fn send_lcd_update(&self) -> Result<(), Box<dyn std::error::Error>> {
        let text = unidecode(&self.text);
        let mut chars: Vec<u8> = text.chars().take(14).map(|c| c as u8).collect();
        while chars.len() < 14 {
            chars.push(0);
        }
        
        let color_code = self.color as u8 | ((self.invert as u8) << 4);
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        data.extend(chars);
        
        self.send_sysex(&data)
    }
    
    pub fn segment_update(&self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        let text = unidecode(text);
        let mut rendered = render_7seg(&text);
        while rendered.len() < 12 {
            rendered.push(0);
        }
        
        let mut data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        data.extend(rendered);
        data.extend_from_slice(&[0x00, 0x00]);
        
        self.send_sysex(&data)
    }
    
    fn send_sysex(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(midi_out) = self.midi_out.lock() {
            if let Some(conn) = &*midi_out {
                let mut message = vec![0xF0];
                message.extend_from_slice(data);
                message.push(0xF7);
                conn.send(&message)?;
            }
        }
        Ok(())
    }
}
