//! `ozma-proto` — shared wire-format types for the ozma KVM stack.
//!
//! # Modules
//! - [`hid`]     — USB HID boot-protocol keyboard and mouse reports
//! - [`keycodes`] — evdev KEY_* ↔ HID Usage ID / X11 keysym lookup tables
//! - [`vban`]    — VBAN V0.3 audio-frame wire format

pub mod hid;
pub mod keycodes;
pub mod vban;

pub use hid::{HidKeyboardReport, HidMouseReport, HidReportError};
pub use vban::{VbanAudioFrame, VbanHeader, VbanHeaderError, VbanCodec, VbanSampleRate};
