// SPDX-License-Identifier: AGPL-3.0-only
//! HID input sinks and report routing for Ozma nodes.
//!
//! This crate sits between the normalized [`ozma_protocol::InputEvent`] stream
//! and the node's physical injection path. A sink is a small, testable adapter
//! that owns held keyboard/pointer state and writes either HID reports or Linux
//! evdev events.
//!
//! # Sinks
//!
//! * [`HidGadgetSink`] writes to ConfigFS USB gadget nodes (`/dev/hidg0`,
//!   `/dev/hidg1` by default) with plain [`std::fs`] and [`std::io`]. It
//!   supports the six-byte absolute pointer report used by the Ozma gadget
//!   descriptor and the portable four-byte relative boot report.
//! * [`UinputSink`] (Linux) creates a virtual keyboard and a virtual
//!   absolute-or-relative pointer through `/dev/uinput`, for QEMU's
//!   `input-linux` object and desktop seats. Other targets get a stub whose
//!   `open` returns [`codes::UNSUPPORTED`].
//! * [`QmpSink`] translates events to QEMU's `input-send-event` and queues
//!   them for a background task. [`QmpSink::spawn`] connects the QMP client;
//!   [`QmpSink::channel`] is the socket-free test seam.
//! * [`FakeInputSink`] records every event and tracks held state without I/O.
//! * [`RecordingSink`] wraps any sink and records the event stream.
//! * [`SharedSink`] is an `Arc<dyn InputSink>` for the transport/session layer.
//!
//! # Sink selection
//!
//! [`SinkPreference`] and [`select_sink`] try a caller-provided candidate list
//! in order and return the first sink that opens. The plan's default chain is
//! `uinput → qmp → hid-gadget → recording`. All four kinds now have a concrete
//! implementation: [`UinputSink`], [`QmpSink`], [`HidGadgetSink`], and
//! [`RecordingSink`]. Constructors stay caller-supplied because QMP needs an
//! async connection before a candidate can be built. [`SinkCandidate::recording`]
//! always succeeds, so a default chain terminates in CI without hardware.
//!
//! # Report behavior
//!
//! * Key codes are USB HID usage ids (page 0x07), not evdev codes. Modifier
//!   usage ids `0xE0..=0xE7` set bits in the modifier byte; other key events
//!   also copy [`KeyModifiers`](ozma_protocol::input::KeyModifiers) from the
//!   event. The boot report holds at most six non-modifier keys; a seventh
//!   press is rejected with [`codes::INPUT_KEY_LIMIT`]. [`UinputSink`] has no
//!   such limit.
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
//! # uinput specifics
//!
//! [`UinputSink`] translates each HID usage directly to a Linux keycode with
//! [`hid_usage_to_linux_keycode`]. [`linux_keycode_to_hid_usage`] provides the
//! reverse direction, used by the controller compatibility `/input/key`
//! endpoint to turn the evdev keycodes the Python node accepted into protocol
//! HID usages. An unknown usage is rejected with an
//! [`ApiError`](ozma_protocol::ApiError) rather than dropped, so a caller can
//! see the failure and fall back to another sink. Client modifier bits are
//! diffed into real `KEY_LEFTCTRL`/`KEY_LEFTSHIFT`/... events. The pointer is
//! created in one mode: absolute mode accepts `PointerAbsolute` and rejects
//! `PointerRelative`, and relative mode does the reverse. Buttons and both
//! wheel axes work in both modes. `Touch` and `Pen` are rejected with
//! [`codes::UNSUPPORTED`] for now; the gadget path still supports them, and a
//! multitouch/tablet uinput device is later work.
//!
//! # QMP specifics
//!
//! [`QmpSink`] holds the same key/modifier/button state as the other sinks but
//! emits QEMU `input-send-event` values. Keyboard usages go through
//! [`ozma_qmp::hid_to_qcode`]; an unmapped usage is rejected with
//! [`codes::UNSUPPORTED`] and a remediation, not dropped. Client modifier bits
//! become virtual left/right modifier qcodes, so explicit `0xE0..=0xE7` keys
//! and the client mask cannot leave a modifier stuck. Media usages supported by
//! the `ozma-qmp` table (`audmute`, `volinc`, `voldec`) travel as ordinary
//! qcodes.
//!
//! QMP has no wheel axis, so [`ozma_protocol::InputEvent::Wheel`] `dy` becomes
//! `wheel-up`/`wheel-down` button press+release pairs. Horizontal wheel motion
//! is rejected because QEMU cannot represent it. Back/Forward buttons map to
//! QEMU's `side`/`extra`. Touch, pen, and clipboard are rejected: the QMP input
//! event model has no contact, pressure/tilt, or clipboard representation.
//!
//! `QmpSink::submit_report` is non-blocking and only queues a batch. Spawn the
//! sender task with [`QmpSink::spawn`] (or drain the receiver from
//! [`QmpSink::channel`] yourself). A closed channel is a typed
//! [`ApiError`](ozma_protocol::ApiError); `release_all` still clears local
//! state so a reconnect cannot believe keys are held.
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
mod fake;
mod gadget;
mod keymap;
mod qmp;
mod report;
mod select;
mod sink;
mod state;
mod uinput;

pub use error::codes;
pub use fake::FakeInputSink;
pub use gadget::{GadgetOptions, HidGadgetSink, DEFAULT_KEYBOARD_PATH, DEFAULT_MOUSE_PATH};
pub use keymap::{hid_usage_to_linux_keycode, linux_keycode_to_hid_usage};
pub use qmp::{QmpSink, QmpSinkHandle};
pub use report::{
    clamp_i8, pointer_button_bit, AbsoluteMapping, AbsolutePoint, AbsolutePointerReport,
    MouseProtocol, HID_ABSOLUTE_MAX,
};
pub use select::{
    select_sink, SelectedSink, SinkAttempt, SinkCandidate, SinkKind, SinkPreference,
    SinkSelectionError, UnknownSinkKind,
};
pub use sink::{InputSink, RecordingSink, SharedSink, SubmitReport};
pub use state::{HeldInput, MAX_KEYS};
pub use uinput::{
    UinputOptions, UinputSink, DEFAULT_UINPUT_KEYBOARD_NAME, DEFAULT_UINPUT_POINTER_NAME,
};
