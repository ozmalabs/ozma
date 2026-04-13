//! Ozma hardware drivers
//!
//! This crate contains implementations for various hardware devices used in the ozma ecosystem.

pub mod gamepad;
pub mod streamdeck;

pub use streamdeck::{StreamDeckSurface, ScenarioInfo, discover_streamdecks};
