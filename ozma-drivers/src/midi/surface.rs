//! MIDI surface implementation

use super::types::*;
use super::controls::*;
use super::display::*;
use midir::{MidiInput, MidiInputPort, MidiOutput, MidiOutputConnection};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

/// MIDI control surface implementation
pub struct MidiSurface {
    id: String,
    config: MidiSurfaceConfig,
    controls: HashMap<String, Box<dyn MidiControl + Send>>,
    displays: HashMap<String, DisplayUpdater>,
    msg_map: HashMap<(String, u8), String>, // (msg_type, key) -> control_name
    midi_input: Option<MidiInput>,
    midi_output: Arc<Mutex<Option<MidiOutputConnection>>>,
    scribble: Option<ScribbleStrip>,
    segment: Option<SegmentDisplay>,
}

/// Display updater function
pub type DisplayUpdater = Box<dyn Fn(&str, Option<&str>) + Send>;

impl MidiSurface {
    pub fn new(id: String, config: MidiSurfaceConfig) -> Self {
        Self {
            id,
            config,
            controls: HashMap::new(),
            displays: HashMap::new(),
            msg_map: HashMap::new(),
            midi_input: None,
            midi_output: Arc::new(Mutex::new(None)),
            scribble: None,
            segment: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Try to initialize MIDI
        match self.init_midi() {
            Ok(_) => {
                log::info!("MIDI surface '{}' started", self.id);
            }
            Err(e) => {
                log::warn!("MIDI surface '{}' failed to start: {}", self.id, e);
                return Ok(());
            }
        }

        // Create controls
        for (name, cfg) in &self.config.controls {
            self.create_control(name.clone(), cfg)?;
        }

        // Create displays
        for (name, dcfg) in &self.config.displays {
            self.create_display(name.clone(), dcfg)?;
        }

        log::info!(
            "MIDI surface '{}' started: {} controls, {} displays",
            self.id,
            self.controls.len(),
            self.displays.len()
        );

        Ok(())
    }

    pub fn stop(&mut self) {
        // MIDI connections are automatically closed when dropped
        log::info!("MIDI surface '{}' stopped", self.id);
    }

    fn init_midi(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let midi_in = MidiInput::new("ozma-midi-in")?;
        let midi_out = MidiOutput::new("ozma-midi-out")?;

        let in_ports = midi_in.ports();
        let out_ports = midi_out.ports();

        let in_port = self.find_port(&in_ports, &self.config.device)?;
        let out_port = self.find_port(&out_ports, &self.config.device)?;

        // Open output port
        let conn_out = midi_out.connect(&out_port, "ozma-midi-out")?;
        *self.midi_output.lock().unwrap() = Some(conn_out);

        // Store input for later use (actual connection would happen in a separate thread)
        self.midi_input = Some(midi_in);

        Ok(())
    }

    fn find_port<T>(
        &self,
        ports: &[T],
        device_name: &str,
    ) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
    where
        T: Clone,
    {
        // In a real implementation, you'd match the port name with device_name
        // For now, we'll just return the first port if any exist
        if ports.is_empty() {
            return Err(format!("No MIDI ports found for device '{}'", device_name).into());
        }
        
        Ok(ports[0].clone())
    }

    fn create_control(
        &mut self,
        name: String,
        config: &ControlConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let control: Box<dyn MidiControl + Send> = match config.control_type {
            ControlType::Fader => Box::new(MidiFader::new(
                name.clone(),
                config,
                self.midi_output.clone(),
            )),
            ControlType::Button => Box::new(MidiButton::new(
                name.clone(),
                config,
                self.midi_output.clone(),
            )),
            ControlType::Rotary => Box::new(MidiRotary::new(
                name.clone(),
                config,
                self.midi_output.clone(),
            )),
            ControlType::JogWheel => Box::new(MidiJogWheel::new(name.clone(), config)),
        };

        // Build message routing map
        if let Some(control_num) = config.control {
            self.msg_map
                .insert(("control_change".to_string(), control_num), name.clone());
        }
        if let Some(note_num) = config.note {
            self.msg_map
                .insert(("note_on".to_string(), note_num), name.clone());
            self.msg_map
                .insert(("note_off".to_string(), note_num), name.clone());
        }

        self.controls.insert(name, control);
        Ok(())
    }

    fn create_display(
        &mut self,
        name: String,
        config: &DisplayConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Initialize scribble strip if not already done
        if self.scribble.is_none() {
            self.scribble = Some(ScribbleStrip::new(self.midi_output.clone()));
        }

        // Initialize segment display if not already done
        if self.segment.is_none() {
            self.segment = Some(SegmentDisplay::new(self.midi_output.clone()));
        }

        let updater: DisplayUpdater = match config.display_type {
            DisplayType::ScribbleTop => {
                let scribble = self.scribble.as_ref().unwrap().clone();
                Box::new(move |text: &str, color: Option<&str>| {
                    scribble.update_top(text, color.map(|c| Color::from_hex(Some(c))));
                })
            }
            DisplayType::ScribbleBottom => {
                let scribble = self.scribble.as_ref().unwrap().clone();
                Box::new(move |text: &str, color: Option<&str>| {
                    scribble.update_bottom(text, color.map(|c| Color::from_hex(Some(c))));
                })
            }
            DisplayType::Scribble => {
                let scribble = self.scribble.as_ref().unwrap().clone();
                Box::new(move |text: &str, color: Option<&str>| {
                    scribble.update(
                        Some(text),
                        color.map(|c| Color::from_hex(Some(c))),
                        None,
                    );
                })
            }
        };

        self.displays.insert(name, updater);
        Ok(())
    }

    pub fn process_midi_message(&mut self, msg_type: &str, data1: u8, data2: u8) {
        let key = (msg_type.to_string(), data1);
        if let Some(control_name) = self.msg_map.get(&key) {
            if let Some(control) = self.controls.get_mut(control_name) {
                if let Some(event) = control.on_midi_message(msg_type, data1, data2) {
                    // This would trigger the control surface event
                    log::debug!(
                        "MIDI control '{}' changed: value={}, pressed={:?}, lockout={:?}",
                        control_name,
                        event.value,
                        event.pressed,
                        event.lockout
                    );
                }
            }
        }
    }

    pub fn update_display(&self, display_name: &str, text: &str, color: Option<&str>) {
        if let Some(updater) = self.displays.get(display_name) {
            updater(text, color);
        }
    }
}

impl Drop for MidiSurface {
    fn drop(&mut self) {
        self.stop();
    }
}
