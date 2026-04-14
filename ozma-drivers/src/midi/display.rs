//! MIDI display implementations

use super::types::*;
use midir::MidiOutputConnection;
use std::sync::{Arc, Mutex};

/// Behringer X-Touch scribble strip
pub struct ScribbleStrip {
    midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
    text: String,
    color: Color,
    invert: Invert,
}

impl ScribbleStrip {
    pub fn new(midi_output: Arc<Mutex<Option<MidiOutputConnection>>>) -> Self {
        Self {
            midi_output,
            text: " ".repeat(14),
            color: Color::White,
            invert: Invert::None,
        }
    }

    pub fn update(&mut self, text: Option<&str>, color: Option<Color>, invert: Option<Invert>) {
        if let Some(t) = text {
            self.text = format!("{:<14}", t).chars().take(14).collect();
        }
        if let Some(c) = color {
            self.color = c;
        }
        if let Some(i) = invert {
            self.invert = i;
        }
        
        self.send_update();
    }

    pub fn update_top(&mut self, text: &str, color: Option<Color>) {
        let padded_text = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", padded_text, &self.text[7..]);
        if let Some(c) = color {
            self.color = c;
        }
        self.send_update();
    }

    pub fn update_bottom(&mut self, text: &str, color: Option<Color>) {
        let padded_text = format!("{:^7}", &text[..text.len().min(7)]);
        self.text = format!("{}{}", &self.text[..7], padded_text);
        if let Some(c) = color {
            self.color = c;
        }
        self.send_update();
    }

    fn send_update(&self) {
        let color_code = self.color.to_u8() | (self.invert.to_u8() << 4);
        let chars: Vec<u8> = self.text
            .chars()
            .map(|c| c as u8)
            .chain(std::iter::repeat(0))
            .take(14)
            .collect();

        let mut sysex_data = vec![0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code];
        sysex_data.extend(chars);

        if let Ok(Some(ref mut conn)) = self.midi_output.lock() {
            // Send SysEx message
            let mut msg = vec![0xF0]; // Start SysEx
            msg.extend(sysex_data);
            msg.push(0xF7); // End SysEx
            let _ = conn.send(&msg);
        }
    }
}

/// 7-segment display updater
pub struct SegmentDisplay {
    midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl SegmentDisplay {
    pub fn new(midi_output: Arc<Mutex<Option<MidiOutputConnection>>>) -> Self {
        Self { midi_output }
    }

    pub fn update(&self, text: &str) {
        let rendered = render_7seg(text);
        let display_data: Vec<u8> = rendered
            .into_iter()
            .chain(std::iter::repeat(0))
            .take(12)
            .collect();

        let mut sysex_data = vec![0x00, 0x20, 0x32, 0x41, 0x37];
        sysex_data.extend(display_data);
        sysex_data.extend([0x00, 0x00]);

        if let Ok(Some(ref mut conn)) = self.midi_output.lock() {
            // Send SysEx message
            let mut msg = vec![0xF0]; // Start SysEx
            msg.extend(sysex_data);
            msg.push(0xF7); // End SysEx
            let _ = conn.send(&msg);
        }
    }
}
