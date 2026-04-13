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
//! MIDI control implementations

use crate::midi::io::MidiIO;
use crate::midi::types::{ButtonStyle, LightStyle};
use std::any::Any;

/// Base trait for MIDI controls
pub trait MidiControl: Send {
    fn name(&self) -> &str;
    fn on_midi_message(&mut self, message: &[u8]) -> Option<ControlEvent>;
    fn set_value(&mut self, value: f64);
    fn value(&self) -> f64;
    fn lockout(&self) -> bool;
    fn as_any(&self) -> &dyn Any; // For downcasting
    fn as_any_mut(&mut self) -> &mut dyn Any; // For downcasting
}

/// Event generated by a MIDI control
#[derive(Debug, Clone)]
pub struct ControlEvent {
    pub control_name: String,
    pub value: f64,
    pub pressed: Option<bool>, // For buttons
    pub lockout: Option<bool>, // For faders with touch
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
    pub fn new(name: String, cc: u8, touch_note: Option<u8>) -> Self {
        Self {
            name,
            value: 0.0,
            lockout: false,
            cc,
            touch_note,
        }
    }
}

impl MidiControl for MidiFader {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<ControlEvent> {
        if message.len() < 3 {
            return None;
        }

        match message[0] & 0xF0 {
            // Control change
            0xB0 => {
                if message[1] == self.cc {
                    self.value = message[2] as f64 / 127.0;
                    return Some(ControlEvent {
                        control_name: self.name.clone(),
                        value: self.value,
                        pressed: None,
                        lockout: None,
                    });
                }
            }
            // Note on/off for touch detection
            0x90 | 0x80 => {
                if let Some(touch_note) = self.touch_note {
                    if message[1] == touch_note {
                        // Touch detection: velocity >= 64 means touched
                        self.lockout = message[0] == 0x90 && message[2] >= 64;
                        return Some(ControlEvent {
                            control_name: self.name.clone(),
                            value: self.value,
                            pressed: None,
                            lockout: Some(self.lockout),
                        });
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn set_value(&mut self, value: f64) {
        if !self.lockout {
            self.value = value.clamp(0.0, 1.0);
            // Note: Actual MIDI output would happen in the surface implementation
        }
    }

    fn value(&self) -> f64 {
        self.value
    }

    fn lockout(&self) -> bool {
        self.lockout
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Button with LED feedback
pub struct MidiButton {
    name: String,
    value: bool,
    pressed: bool,
    note: u8,
    style: ButtonStyle,
    light_style: LightStyle,
}

impl MidiButton {
    pub fn new(
        name: String,
        note: u8,
        style: ButtonStyle,
        light_style: LightStyle,
    ) -> Self {
        Self {
            name,
            value: false,
            pressed: false,
            note,
            style,
            light_style,
        }
    }

    pub fn update_light(&self, midi_io: &mut MidiIO) -> Result<(), Box<dyn std::error::Error>> {
        let on = match self.light_style {
            LightStyle::Off => false,
            LightStyle::AlwaysOn => true,
            LightStyle::Momentary => self.pressed,
            LightStyle::State => self.value,
        };
        
        midi_io.note_on(self.note, if on { 127 } else { 0 })
    }
}

impl MidiControl for MidiButton {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<ControlEvent> {
        if message.len() < 3 {
            return None;
        }

        // Check if this is a note message for our button
        if (message[0] & 0xF0 == 0x90 || message[0] & 0xF0 == 0x80) && message[1] == self.note {
            if message[0] & 0xF0 == 0x90 && message[2] >= 64 {
                // Button pressed
                self.pressed = true;
                match self.style {
                    ButtonStyle::Toggle => self.value = !self.value,
                    ButtonStyle::Momentary => self.value = true,
                }
            } else {
                // Button released
                self.pressed = false;
                if let ButtonStyle::Momentary = self.style {
                    self.value = false;
                }
            }

            return Some(ControlEvent {
                control_name: self.name.clone(),
                value: if self.value { 1.0 } else { 0.0 },
                pressed: Some(self.pressed),
                lockout: None,
            });
        }
        None
    }

    fn set_value(&mut self, value: f64) {
        self.value = value >= 0.5;
        // Note: Actual LED update would happen in the surface implementation
    }

    fn value(&self) -> f64 {
        if self.value { 1.0 } else { 0.0 }
    }

    fn lockout(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Rotary encoder (continuous CC)
pub struct MidiRotary {
    name: String,
    value: f64,
    cc: u8,
}

impl MidiRotary {
    pub fn new(name: String, cc: u8) -> Self {
        Self {
            name,
            value: 0.0,
            cc,
        }
    }
}

impl MidiControl for MidiRotary {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, message: &[u8]) -> Option<ControlEvent> {
        if message.len() < 3 {
            return None;
        }

        if message[0] & 0xF0 == 0xB0 && message[1] == self.cc {
            self.value = message[2] as f64 / 127.0;
            return Some(ControlEvent {
                control_name: self.name.clone(),
                value: self.value,
                pressed: None,
                lockout: None,
            });
        }
        None
    }

    fn set_value(&mut self, value: f64) {
        self.value = value.clamp(0.0, 1.0);
        // Note: Actual MIDI output would happen in the surface implementation
    }

    fn value(&self) -> f64 {
        self.value
    }

    fn lockout(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Jog wheel - emits direction +1 or -1
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

    fn on_midi_message(&mut self, message: &[u8]) -> Option<ControlEvent> {
        if message.len() < 3 {
            return None;
        }

        if message[0] & 0xF0 == 0xB0 && message[1] == self.cc {
            // Direction: 65 = clockwise, 63 = counter-clockwise (typically)
            let direction = if message[2] == 65 { 1.0 } else { -1.0 };
            return Some(ControlEvent {
                control_name: self.name.clone(),
                value: direction,
                pressed: None,
                lockout: None,
            });
        }
        None
    }

    fn set_value(&mut self, _value: f64) {
        // Jog wheels don't have settable values
    }

    fn value(&self) -> f64 {
        0.0 // Jog wheels don't maintain a value
    }

    fn lockout(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
