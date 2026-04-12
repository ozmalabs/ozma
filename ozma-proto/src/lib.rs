//! `ozma-proto` — shared wire-format types for the ozma KVM stack.
//!
//! # Modules
//! - [`hid`]  — USB HID boot-protocol keyboard and mouse reports, modifier
//!              bits, and HID Usage ID constants (ported from `controller/keycodes.py`)
//! - [`vban`] — VBAN V0.3 audio-frame wire format

pub mod hid;
pub mod vban;

pub use hid::{HidKeyboardReport, HidMouseReport, ModifierBits};
pub use hid::KeyCode;
pub use vban::{VbanAudioFrame, VbanHeader, VbanSampleRate, VbanSubProtocol, VbanDataFormat};
