// SPDX-License-Identifier: AGPL-3.0-only
//! HID input sinks and report routing for Ozma nodes.
//!
//! This crate sits between the normalized [`ozma_protocol::InputEvent`] stream
//! and the node's physical injection path. It deliberately contains no
//! platform bindings: a sink is a small, testable adapter that owns held
//! keyboard/pointer state and writes reports.
//!
//! # Sinks
//!
//! * [`HidGadgetSink`] writes to ConfigFS USB gadget nodes (`/dev/hidg0`,
//!   `/dev/hidg1` by default) with plain [`std::fs`] and [`std::io`]. It
//!   supports the six-byte absolute pointer report used by the Ozma gadget
//!   descriptor and the portable four-byte relative boot report.
//! * [`FakeInputSink`] records every event and tracks held state without I/O.
//! * [`RecordingSink`] wraps any sink and records the event stream.
//! * [`SharedSink`] is an `Arc<dyn InputSink>` for the transport/session layer.
//!
//! # Report behavior
//!
//! * Key codes are USB HID usage ids (page 0x07), not evdev codes. Modifier
//!   usage ids `0xE0..=0xE7` set bits in the modifier byte; other key events
//!   also copy [`KeyModifiers`](ozma_protocol::input::KeyModifiers) from the
//!   event. The boot report holds at most six non-modifier keys; a seventh
//!   press is rejected with [`codes::INPUT_KEY_LIMIT`].
//! * Duplicate key presses and key-ups of keys that are not held are no-ops.
//! * `PointerAbsolute`, `Touch`, and `Pen` map into `0..=32767` using
//!   [`AbsoluteMapping`] and are supported by the absolute mouse protocol.
//!   The default mapping treats event coordinates as already being in report
//!   space; construct `AbsoluteMapping::new(width, height)` when events carry
//!   surface pixels.
//! * Touch contacts and pen contact synthesize the left button. A `Down`
//!   starts a contact and `Up`/`Cancel` ends it; `Move` only updates the
//!   position, so a move without a preceding down hovers instead of leaving a
//!   stuck click. The button is held while any contact is active and released
//!   when the last contact lifts. Pressure and tilt are carried by the
//!   protocol event but cannot be represented in a boot/absolute mouse report,
//!   so they are ignored here.
//! * `Wheel.dy` becomes the signed scroll byte. Horizontal wheel motion has no
//!   boot-report field and is ignored (logged at debug level).
//! * `PointerRelative` requires [`MouseProtocol::Relative`]; absolute and
//!   relative modes reject events they cannot represent instead of silently
//!   degrading pointer-lock input.
//! * `ReleaseAll` clears every key, modifier, button, and contact and writes
//!   neutral keyboard and mouse reports. It is idempotent.
//!
//! # Clipboard
//!
//! Clipboard is **not** part of the HID input sink. [`ozma_protocol::InputEvent::Clipboard`]
//! is rejected with [`codes::UNSUPPORTED`] so a caller cannot accidentally
//! paste text by sending it down the input channel. A future
//! `ozma-clipboard` transport will expose a separate sink with its own
//! permission checks (`clipboard_read` / `clipboard_write`).
//!
//! # Example
//!
//! ```
//! use ozma_input::{FakeInputSink, InputSink};
//! use ozma_protocol::input::InputEvent;
//!
//! let sink = FakeInputSink::new();
//! sink.submit(&InputEvent::Key {
//!     code: 0x04, // HID usage id for "a"
//!     down: true,
//!     modifiers: Default::default(),
//! })
//! .unwrap();
//! assert!(sink.held().has_key(0x04));
//! sink.release_all().unwrap();
//! assert!(sink.held().is_empty());
//! ```

#![forbid(unsafe_code)]

mod error;
mod gadget;
mod report;
mod sink;
mod state;

pub use error::codes;
pub use gadget::{
    GadgetOptions, HidGadgetSink, DEFAULT_KEYBOARD_PATH, DEFAULT_MOUSE_PATH,
};
pub use report::{
    clamp_i8, pointer_button_bit, AbsoluteMapping, AbsolutePoint, AbsolutePointerReport,
    MouseProtocol, HID_ABSOLUTE_MAX,
};
pub use sink::{InputSink, RecordingSink, SharedSink, SubmitReport};
pub use state::{HeldInput, MAX_KEYS};

mod fake;
pub use fake::FakeInputSink;
