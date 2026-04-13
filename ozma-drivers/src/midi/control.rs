//! MIDI control implementations

use crate::midi::types::*;

/// Base trait for MIDI controls
pub trait MidiControl: Send + Sync {
    /// Process incoming MIDI message, return state delta
    fn on_midi_message(&mut self, msg_type: &str, data: &[u8]) -> Option<ControlState>;
    
    /// Set value from external source (feedback)
    fn set_value(&mut self, value: f64);
    
    /// Get current value
    fn get_value(&self) -> f64;
    
    /// Get lockout state
    fn get_lockout(&self) -> bool;
    
    /// Get control name
    fn get_name(&self) -> &str;
}

/// Motorised fader with touch detection
pub struct MidiFader {
    name: String,
    value: f64,
    lockout: bool,
    cc: u8,
    touch_note: Option<u8>,
}

impl MidiFader {
    pub fn new(name: String, config: &ControlConfig) -> Self {
        Self {
            name,
            value: 0.0,
            lockout: false,
            cc: config.control.unwrap_or(70),
            touch_note: config.note,
        }
    }
}

impl MidiControl for MidiFader {
    fn on_midi_message(&mut self, msg_type: &str, data: &[u8]) -> Option<ControlState> {
        match msg_type {
            "control_change" if data.len() >= 2 && data[0] == self.cc => {
                self.value = data[1] as f64 / 127.0;
                Some(ControlState {
                    value: self.value,
                    lockout: self.lockout,
                    pressed: false,
                })
            }
            "note_on" if self.touch_note.is_some() && data.len() >= 2 && data[0] == self.touch_note.unwrap() => {
                self.lockout = data[1] >= 64;
                Some(ControlState {
                    value: self.value,
                    lockout: self.lockout,
                    pressed: false,
                })
            }
            _ => None
        }
    }
    
    fn set_value(&mut self, value: f64) {
        if !self.lockout {
            self.value = value.max(0.0).min(1.0);
        }
    }
    
    fn get_value(&self) -> f64 {
        self.value
    }
    
    fn get_lockout(&self) -> bool {
        self.lockout
    }
    
    fn get_name(&self) -> &str {
        &self.name
    }
}

/// Button with LED feedback
pub struct MidiButton {
    name: String,
    value: bool,
    pressed: bool,
    note: u8,
    style: String,      // "toggle" | "momentary"
    light_style: String, // "state" | "always_on" | "momentary" | "false"
}

impl MidiButton {
    pub fn new(name: String, config: &ControlConfig) -> Self {
        Self {
            name,
            value: false,
            pressed: false,
            note: config.note.unwrap_or(0),
            style: config.style.clone().unwrap_or_else(|| "toggle".to_string()),
            light_style: config.light.clone().unwrap_or_else(|| "state".to_string()),
        }
    }
}

impl MidiControl for MidiButton {
    fn on_midi_message(&mut self, msg_type: &str, data: &[u8]) -> Option<ControlState> {
        if msg_type == "note_on" && data.len() >= 2 && data[0] == self.note {
            if data[1] >= 64 {
                // Press
                self.pressed = true;
                if self.style == "toggle" {
                    self.value = !self.value;
                } else {
                    self.value = true;
                }
            } else {
                // Release
                self.pressed = false;
                if self.style == "momentary" {
                    self.value = false;
                }
            }
            
            Some(ControlState {
                value: if self.value { 1.0 } else { 0.0 },
                lockout: false,
                pressed: self.pressed,
            })
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: f64) {
        self.value = value >= 0.5;
    }
    
    fn get_value(&self) -> f64 {
        if self.value { 1.0 } else { 0.0 }
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
    
    fn get_name(&self) -> &str {
        &self.name
    }
}

/// Rotary encoder
pub struct MidiRotary {
    name: String,
    value: f64,
    cc: u8,
}

impl MidiRotary {
    pub fn new(name: String, config: &ControlConfig) -> Self {
        Self {
            name,
            value: 0.0,
            cc: config.control.unwrap_or(80),
        }
    }
}

impl MidiControl for MidiRotary {
    fn on_midi_message(&mut self, msg_type: &str, data: &[u8]) -> Option<ControlState> {
        if msg_type == "control_change" && data.len() >= 2 && data[0] == self.cc {
            self.value = data[1] as f64 / 127.0;
            Some(ControlState {
                value: self.value,
                lockout: false,
                pressed: false,
            })
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: f64) {
        self.value = value.max(0.0).min(1.0);
    }
    
    fn get_value(&self) -> f64 {
        self.value
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
    
    fn get_name(&self) -> &str {
        &self.name
    }
}

/// Jog wheel
pub struct MidiJogWheel {
    name: String,
    cc: u8,
}

impl MidiJogWheel {
    pub fn new(name: String, config: &ControlConfig) -> Self {
        Self {
            name,
            cc: config.control.unwrap_or(60),
        }
    }
}

impl MidiControl for MidiJogWheel {
    fn on_midi_message(&mut self, msg_type: &str, data: &[u8]) -> Option<ControlState> {
        if msg_type == "control_change" && data.len() >= 2 && data[0] == self.cc {
            let direction = if data[1] == 65 { 1.0 } else { -1.0 };
            Some(ControlState {
                value: direction,
                lockout: false,
                pressed: false,
            })
        } else {
            None
        }
    }
    
    fn set_value(&mut self, _value: f64) {
        // Jog wheels don't have feedback
    }
    
    fn get_value(&self) -> f64 {
        0.0 // Jog wheels don't have a persistent value
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
    
    fn get_name(&self) -> &str {
        &self.name
    }
}
