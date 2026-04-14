//! MIDI control surface support for ozma.
//!
//! Ported from surfacepresser-run's midi_controller.py + midi_integration.py,
//! rewritten as a clean async module that integrates with ozma's ControlSurface
//! abstraction.
//!
//! Supports:
//!   - Faders (motorised, with touch lockout)
//!   - Buttons (toggle / momentary, with LED feedback)
//!   - Rotary encoders
//!   - Jog wheels
//!   - Behringer X-Touch scribble strip LCD displays
//!   - Behringer 7-segment displays

pub mod surface;
pub mod types;
pub mod controls;
pub mod display;
