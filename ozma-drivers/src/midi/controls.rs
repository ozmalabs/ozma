//! MIDI control implementations (faders, buttons, etc.)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use midir::MidiOutputConnection;
use serde_json::Value;
use super::types::{MidiMessage, ButtonStyle, LightStyle, Color, Invert};

/// Base trait for MIDI controls
pub trait MidiControl: Send + Sync {
    /// Process incoming MIDI message, return state delta
    fn on_midi_message(&mut self, message: &MidiMessage) -> Option<HashMap<String, Value>>;
    
    /// Set value from external source (feedback)
    fn set_value(&mut self, value: Value);
    
    /// Get current value
    fn get_value(&self) -> &Value;
    
    /// Get lockout state
    fn get_lockout(&self) -> bool;
}

/// Motorised fader with touch detection
pub struct MidiFader {
    name: String,
    value: Value,
    lockout: bool,
    cc: u8,
    touch_note: Option<u8>,
    midi_out: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl MidiFader {
    pub fn new(name: String, cc: u8, touch_note: Option<u8>, midi_out: Arc<Mutex<Option<MidiOutputConnection>>>) -> Self {
        Self {
            name,
            value: Value::Number(serde_json::Number::from(0)),
            lockout: false,
            cc,
            touch_note,
            midi_out,
        }
    }
}

impl MidiControl for MidiFader {
    fn on_midi_message(&mut self, message: &MidiMessage) -> Option<HashMap<String, Value>> {
        if message.msg_type == "control_change" && message.control == Some(self.cc) {
            // Control change message
            self.value = Value::Number(serde_json::Number::from(message.value));
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            Some(delta)
        } else if let Some(touch_note) = self.touch_note {
            if message.msg_type == "note_on" && message.note == Some(touch_note) {
                // Note on/off for touch detection
                self.lockout = message.value >= 64;
                let mut delta = HashMap::new();
                delta.insert("lockout".to_string(), Value::Bool(self.lockout));
                Some(delta)
            } else {
                None
            }
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: Value) {
        if !self.lockout {
            if let Some(v) = value.as_u64() {
                let v = (v as u8).min(127);
                self.value = Value::Number(serde_json::Number::from(v));
                
                if let Ok(mut midi_out) = self.midi_out.lock() {
                    if let Some(ref mut conn) = *midi_out {
                        let _ = conn.send(&[0xB0, self.cc, v]);
                    }
                }
            }
        }
    }
    
    fn get_value(&self) -> &Value {
        &self.value
    }
    
    fn get_lockout(&self) -> bool {
        self.lockout
    }
}

/// Button with LED feedback
pub struct MidiButton {
    name: String,
    value: Value,
    pressed: bool,
    note: u8,
    style: ButtonStyle,
    light_style: LightStyle,
    midi_out: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl MidiButton {
    pub fn new(
        name: String, 
        note: u8, 
        style: ButtonStyle, 
        light_style: LightStyle, 
        midi_out: Arc<Mutex<Option<MidiOutputConnection>>>
    ) -> Self {
        let button = Self {
            name,
            value: Value::Bool(false),
            pressed: false,
            note,
            style,
            light_style,
            midi_out: midi_out.clone(),
        };
        
        // Initialize light state
        button.update_light(midi_out);
        button
    }
    
    fn update_light(&self, midi_out: Arc<Mutex<Option<MidiOutputConnection>>>) {
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
        
        if let Ok(midi_guard) = midi_out.lock() {
            if let Some(conn) = &*midi_guard {
                let _ = conn.send(&[0x90, self.note, if on { 127 } else { 0 }]);
            }
        }
    }
}

impl MidiControl for MidiButton {
    fn on_midi_message(&mut self, message: &MidiMessage) -> Option<HashMap<String, Value>> {
        if message.msg_type == "note_on" && message.note == Some(self.note) {
            if message.value >= 64 {
                // Press
                self.pressed = true;
                if self.style == ButtonStyle::Toggle {
                    if let Some(current) = self.value.as_bool() {
                        self.value = Value::Bool(!current);
                    }
                } else {
                    self.value = Value::Bool(true);
                }
            } else {
                // Release
                self.pressed = false;
                if self.style == ButtonStyle::Momentary {
                    self.value = Value::Bool(false);
                }
            }
            
            // Update light
            self.update_light(self.midi_out.clone());
            
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            delta.insert("pressed".to_string(), Value::Bool(self.pressed));
            Some(delta)
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: Value) {
        self.value = value;
        self.update_light(self.midi_out.clone());
    }
    
    fn get_value(&self) -> &Value {
        &self.value
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
}

/// Rotary encoder
pub struct MidiRotary {
    name: String,
    value: Value,
    lockout: bool,
    cc: u8,
    midi_out: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl MidiRotary {
    pub fn new(name: String, cc: u8, midi_out: Arc<Mutex<Option<MidiOutputConnection>>>) -> Self {
        Self {
            name,
            value: Value::Number(serde_json::Number::from(0)),
            lockout: false,
            cc,
            midi_out,
        }
    }
}

impl MidiControl for MidiRotary {
    fn on_midi_message(&mut self, message: &MidiMessage) -> Option<HashMap<String, Value>> {
        if message.msg_type == "control_change" && message.control == Some(self.cc) {
            self.value = Value::Number(serde_json::Number::from(message.value));
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), self.value.clone());
            Some(delta)
        } else {
            None
        }
    }
    
    fn set_value(&mut self, value: Value) {
        if !self.lockout {
            if let Some(v) = value.as_u64() {
                let v = (v as u8).min(127);
                self.value = Value::Number(serde_json::Number::from(v));
                
                if let Ok(mut midi_out) = self.midi_out.lock() {
                    if let Some(ref mut conn) = *midi_out {
                        let _ = conn.send(&[0xB0, self.cc, v]);
                    }
                }
            }
        }
    }
    
    fn get_value(&self) -> &Value {
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
    fn on_midi_message(&mut self, message: &MidiMessage) -> Option<HashMap<String, Value>> {
        if message.msg_type == "control_change" && message.control == Some(self.cc) {
            let direction = if message.value == 65 { 1 } else { -1 };
            let mut delta = HashMap::new();
            delta.insert("value".to_string(), Value::Number(serde_json::Number::from(direction)));
            Some(delta)
        } else {
            None
        }
    }
    
    fn set_value(&mut self, _value: Value) {
        // Jog wheels don't have feedback
    }
    
    fn get_value(&self) -> &Value {
        &Value::Null
    }
    
    fn get_lockout(&self) -> bool {
        false
    }
}
