//! MIDI control implementations

use super::types::*;
use midir::MidiOutputConnection;
use std::sync::{Arc, Mutex};

/// Base trait for MIDI controls
pub trait MidiControl {
    fn name(&self) -> &str;
    fn on_midi_message(&mut self, msg_type: &str, data1: u8, data2: u8) -> Option<ControlEvent>;
    fn set_value(&mut self, value: f64);
    fn lockout(&self) -> bool;
}

/// Control event from MIDI input
#[derive(Debug, Clone)]
pub struct ControlEvent {
    pub value: f64,
    pub pressed: Option<bool>,
    pub lockout: Option<bool>,
}

/// Motorised fader with touch detection
pub struct MidiFader {
    name: String,
    cc: u8,
    touch_note: Option<u8>,
    value: f64,
    lockout: bool,
    midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl MidiFader {
    pub fn new(
        name: String,
        config: &ControlConfig,
        midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
    ) -> Self {
        Self {
            name,
            cc: config.control.unwrap_or(70),
            touch_note: config.note,
            value: 0.0,
            lockout: false,
            midi_output,
        }
    }
}

impl MidiControl for MidiFader {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg_type: &str, data1: u8, data2: u8) -> Option<ControlEvent> {
        match msg_type {
            "control_change" if data1 == self.cc => {
                self.value = data2 as f64 / 127.0;
                Some(ControlEvent {
                    value: self.value,
                    pressed: None,
                    lockout: None,
                })
            }
            "note_on" if self.touch_note.map_or(false, |n| n == data1) => {
                self.lockout = data2 >= 64;
                Some(ControlEvent {
                    value: self.value,
                    pressed: None,
                    lockout: Some(self.lockout),
                })
            }
            _ => None,
        }
    }

    fn set_value(&mut self, value: f64) {
        if !self.lockout {
            let midi_value = (value * 127.0).round() as u8;
            self.value = value;
            
            if let Ok(Some(ref mut conn)) = self.midi_output.lock() {
                let _ = conn.send(&[0xB0, self.cc, midi_value]); // Control change message
            }
        }
    }

    fn lockout(&self) -> bool {
        self.lockout
    }
}

/// Button with LED feedback
pub struct MidiButton {
    name: String,
    note: u8,
    style: ButtonStyle,
    light_style: LightStyle,
    value: bool,
    pressed: bool,
    midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl MidiButton {
    pub fn new(
        name: String,
        config: &ControlConfig,
        midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
    ) -> Self {
        Self {
            name,
            note: config.note.unwrap_or(0),
            style: config.style.clone().unwrap_or(ButtonStyle::Toggle),
            light_style: config.light.clone().unwrap_or(LightStyle::State),
            value: false,
            pressed: false,
            midi_output: midi_output,
        }
    }

    fn update_light(&self) {
        let on = match self.light_style {
            LightStyle::Off => false,
            LightStyle::AlwaysOn => true,
            LightStyle::Momentary => self.pressed,
            LightStyle::State => self.value,
        };

        if let Ok(Some(ref mut conn)) = self.midi_output.lock() {
            let velocity = if on { 127 } else { 0 };
            let _ = conn.send(&[0x90, self.note, velocity]); // Note on message
        }
    }
}

impl MidiControl for MidiButton {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg_type: &str, data1: u8, data2: u8) -> Option<ControlEvent> {
        if msg_type != "note_on" || data1 != self.note {
            return None;
        }

        if data2 >= 64 {
            // Press
            self.pressed = true;
            match self.style {
                ButtonStyle::Toggle => self.value = !self.value,
                ButtonStyle::Momentary => self.value = true,
            }
        } else {
            // Release
            self.pressed = false;
            if let ButtonStyle::Momentary = self.style {
                self.value = false;
            }
        }

        self.update_light();

        Some(ControlEvent {
            value: if self.value { 1.0 } else { 0.0 },
            pressed: Some(self.pressed),
            lockout: None,
        })
    }

    fn set_value(&mut self, value: f64) {
        self.value = value >= 0.5;
        self.update_light();
    }

    fn lockout(&self) -> bool {
        false
    }
}

/// Rotary encoder
pub struct MidiRotary {
    name: String,
    cc: u8,
    value: f64,
    lockout: bool,
    midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
}

impl Rotary {
    pub fn new(
        name: String,
        config: &ControlConfig,
        midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
    ) -> Self {
        Self {
            name,
            cc: config.control.unwrap_or(80),
            value: 0.0,
            lockout: false,
            midi_output,
        }
    }
}

impl MidiControl for MidiRotary {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg_type: &str, data1: u8, data2: u8) -> Option<ControlEvent> {
        if msg_type == "control_change" && data1 == self.cc {
            self.value = data2 as f64 / 127.0;
            Some(ControlEvent {
                value: self.value,
                pressed: None,
                lockout: None,
            })
        } else {
            None
        }
    }

    fn set_value(&mut self, value: f64) {
        if !self.lockout {
            let midi_value = (value * 127.0).round() as u8;
            self.value = value;
            
            if let Ok(Some(ref mut conn)) = self.midi_output.lock() {
                let _ = conn.send(&[0xB0, self.cc, midi_value]); // Control change message
            }
        }
    }

    fn lockout(&self) -> bool {
        self.lockout
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
    fn name(&self) -> &str {
        &self.name
    }

    fn on_midi_message(&mut self, msg_type: &str, data1: u8, data2: u8) -> Option<ControlEvent> {
        if msg_type == "control_change" && data1 == self.cc {
            let direction = if data2 == 65 { 1.0 } else { -1.0 };
            Some(ControlEvent {
                value: direction,
                pressed: None,
                lockout: None,
            })
        } else {
            None
        }
    }

    fn set_value(&mut self, _value: f64) {
        // Jog wheels don't have feedback
    }

    fn lockout(&self) -> bool {
        false
    }
}
