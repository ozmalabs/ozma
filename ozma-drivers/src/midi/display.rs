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
