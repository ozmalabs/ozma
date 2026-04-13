// SPDX-License-Identifier: AGPL-3.0-only
//! ozma-drivers — evdev capture and uinput virtual device drivers.
//!
//! Provides:
//! - [`ControlSurface`] trait for generic input surface abstraction
//! - [`EvdevCapture`] for reading keyboard/mouse events from `/dev/input/eventX`
//! - [`EvdevSurface`] for config-driven button/axis → action mapping
//! - [`KeyboardState`] / [`MouseState`] for HID report building
//! - [`UinputDevice`] for injecting HID events via uinput

pub mod control_surface;
pub mod evdev_capture;
pub mod evdev_surface;
pub mod keyboard;
pub mod mouse;
pub mod uinput;

pub use control_surface::{ControlBinding, ControlEvent, ControlSurface};
pub use evdev_capture::{EvdevCapture, HotplugScanner, InputEvent};
pub use evdev_surface::EvdevSurface;
pub use keyboard::KeyboardState;
pub use mouse::MouseState;
pub use uinput::{DeviceKind, UinputDevice};
