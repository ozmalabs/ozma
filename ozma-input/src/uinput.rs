// SPDX-License-Identifier: AGPL-3.0-only
//! Linux `uinput` sink: a virtual keyboard and pointer driven by the kernel.
//!
//! This is the desktop/VM input path that QEMU's `input-linux` object (or the
//! host's own input stack) reads directly from `/dev/input/eventN`. Unlike the
//! HID gadget sink it does not need a USB gadget controller; it only needs the
//! `uinput` module and write access to `/dev/uinput`.
//!
//! # Mapping
//!
//! [`InputEvent::Key`] carries a USB HID usage id (page 0x07).
//! [`hid_usage_to_linux_keycode`] maps the usages the Python node supports
//! (letters, digits, controls, F1–F24, navigation, keypad, media keys, and
//! `0xE0..=0xE7` modifiers) to Linux keycodes. Unknown usages are rejected with
//! an [`ApiError`] instead of being silently dropped: a lost key is much harder
//! to diagnose than a failed RPC, and the caller can fall back to another sink.
//!
//! Client [`KeyModifiers`](ozma_protocol::input::KeyModifiers) bits are held as
//! virtual modifier keys and diffed against the explicitly pressed modifiers.
//! A key-down that carries `LEFT_CTRL` therefore emits `KEY_LEFTCTRL` before
//! the key event, and the modifier is released when the client stops claiming
//! it. Duplicate presses and releases of unheld keys produce no events.
//!
//! # Pointer modes
//!
//! A uinput device is created in exactly one pointer mode:
//!
//! * [`MouseProtocol::Absolute`] creates `ABS_X`/`ABS_Y` (`0..=32767`) and
//!   accepts [`InputEvent::PointerAbsolute`].
//! * [`MouseProtocol::Relative`] creates `REL_X`/`REL_Y` and accepts
//!   [`InputEvent::PointerRelative`].
//!
//! The other absolute/relative event is rejected with a mode error. Both
//! buttons and both wheel axes are always present. The Python
//! `VirtualMouse` registered absolute and relative axes on one device; this
//! sink keeps one explicit mode so a session's pointer semantics are
//! deterministic and testable. Adding both axis families to one device is
//! possible with the `evdev` builder and is left as later work.
//!
//! # Not supported
//!
//! [`InputEvent::Touch`] and [`InputEvent::Pen`] are rejected with
//! [`codes::UNSUPPORTED`](crate::codes::UNSUPPORTED). The HID gadget path
//! represents them with the absolute pointer report; a real multitouch/tablet
//! uinput device is later work. Clipboard events are not part of any input
//! sink.
//!
//! # Permissions
//!
//! `/dev/uinput` is normally root-only. Provision a udev rule (or add the node
//! user to the `input` group) and make sure the `uinput` module is loaded.
//! Creation also needs a udev rule that grants the node user access to the
//! resulting `/dev/input/eventN` node if QEMU will open it as another user.
//! Devices use vendor `0x1209` and products `0x0001` (keyboard) / `0x0002`
//! (pointer), matching the Python node's virtual devices.
//!
//! # Failure semantics
//!
//! [`InputSink::submit_report`] clones the held state, plans the evdev events,
//! writes them, and commits the clone only after every write succeeded. A
//! failed write therefore leaves the held snapshot behind the device, which is
//! the safe direction: a later [`InputSink::release_all`] can recover. Like
//! [`crate::HidGadgetSink`], `release_all` clears local state even if the
//! neutral write fails.

use std::collections::BTreeSet;

use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton};
use ozma_protocol::ApiError;

use crate::error;
use crate::keymap::hid_usage_to_linux_keycode;
use crate::report::{
    pointer_button_bit, AbsoluteMapping, AbsolutePoint, MouseProtocol, HID_ABSOLUTE_MAX,
};
use crate::sink::{InputSink, SubmitReport};
use crate::state::HeldInput;

/// Default name of the virtual keyboard device.
pub const DEFAULT_UINPUT_KEYBOARD_NAME: &str = "ozma-keyboard";
/// Default name of the virtual pointer device.
pub const DEFAULT_UINPUT_POINTER_NAME: &str = "ozma-mouse";

const MODIFIER_USAGE_START: u8 = 0xE0;

/// Longest device name the kernel accepts (`UINPUT_MAX_NAME_SIZE` is 80,
/// including the terminating NUL and the builder's safety margin).
pub(crate) const MAX_UINPUT_NAME_LEN: usize = 78;

// Linux input-event-codes.h values, kept here so the mapping is testable on
// every target (the `evdev` dependency itself is Linux-only).
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

/// Construction options for [`UinputSink`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UinputOptions {
    /// Name of the virtual keyboard, visible in `/dev/input/by-id`.
    pub keyboard_name: String,
    /// Name of the virtual pointer, visible in `/dev/input/by-id`.
    pub pointer_name: String,
    /// Pointer report mode. The default matches the HID gadget sink.
    pub mouse_protocol: MouseProtocol,
    /// Mapping from event pixel coordinates to absolute `0..=32767` space.
    /// The default is [`AbsoluteMapping::hid_space`].
    pub mapping: AbsoluteMapping,
}

impl Default for UinputOptions {
    fn default() -> Self {
        Self {
            keyboard_name: DEFAULT_UINPUT_KEYBOARD_NAME.to_owned(),
            pointer_name: DEFAULT_UINPUT_POINTER_NAME.to_owned(),
            // Same defaults as GadgetOptions: absolute + HID report space.
            mouse_protocol: MouseProtocol::default(),
            mapping: AbsoluteMapping::default(),
        }
    }
}

const fn is_modifier_usage(usage: u8) -> bool {
    usage >= MODIFIER_USAGE_START && usage <= 0xE7
}

/// One planned Linux input event. The sink converts these to `evdev::InputEvent`
/// only when it writes to a real device, so planning stays testable everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxEvent {
    Key { code: u16, pressed: bool },
    Relative { axis: u16, value: i32 },
    Absolute { axis: u16, value: i32 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PlannedEvents {
    keyboard: Vec<LinuxEvent>,
    pointer: Vec<LinuxEvent>,
}

impl PlannedEvents {
    fn keyboard(events: Vec<LinuxEvent>) -> Self {
        Self {
            keyboard: events,
            pointer: Vec::new(),
        }
    }

    fn pointer(events: Vec<LinuxEvent>) -> Self {
        Self {
            keyboard: Vec::new(),
            pointer: events,
        }
    }

    fn is_empty(&self) -> bool {
        self.keyboard.is_empty() && self.pointer.is_empty()
    }

    fn batches(&self) -> usize {
        usize::from(!self.keyboard.is_empty()) + usize::from(!self.pointer.is_empty())
    }
}

/// Pure held-input state and event planner.
///
/// Kept separate from the device handles so mapping, dedup, mode rejection, and
/// release semantics are unit-testable without `/dev/uinput`.
#[derive(Clone, Debug)]
struct InputState {
    /// Non-modifier HID usages currently held.
    keys: BTreeSet<u8>,
    /// Modifier bits set by explicit `0xE0..=0xE7` key events.
    explicit_modifiers: u8,
    /// Modifier bits carried by the last non-modifier key event.
    client_modifiers: u8,
    /// Effective pointer button bitmask.
    buttons: u8,
    /// Last absolute position, diagnostic only.
    position: AbsolutePoint,
    positioned: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            keys: BTreeSet::new(),
            explicit_modifiers: 0,
            client_modifiers: 0,
            buttons: 0,
            position: AbsolutePoint::CENTER,
            positioned: false,
        }
    }
}

impl InputState {
    fn modifiers(&self) -> u8 {
        self.explicit_modifiers | self.client_modifiers
    }

    fn down_usages(&self) -> BTreeSet<u8> {
        let mut usages = self.keys.clone();
        let modifiers = self.modifiers();
        for bit in 0..8u8 {
            if modifiers & (1 << bit) != 0 {
                usages.insert(MODIFIER_USAGE_START + bit);
            }
        }
        usages
    }

    fn snapshot(&self) -> HeldInput {
        HeldInput {
            keys: self.keys.clone(),
            modifiers: self.modifiers(),
            buttons: self.buttons,
            contacts: BTreeSet::new(),
            pen_down: false,
            pointer: if self.positioned {
                Some(self.position)
            } else {
                None
            },
        }
    }

    fn plan(
        &mut self,
        event: &InputEvent,
        protocol: MouseProtocol,
        mapping: AbsoluteMapping,
    ) -> Result<PlannedEvents, ApiError> {
        match event {
            InputEvent::Key {
                code,
                down,
                modifiers,
            } => self.key(*code, *down, *modifiers),
            InputEvent::PointerAbsolute { x, y, .. } => self.absolute(*x, *y, protocol, mapping),
            InputEvent::PointerRelative { dx, dy } => self.relative(*dx, *dy, protocol),
            InputEvent::Button { button, down } => Ok(self.button(*button, *down)),
            InputEvent::Wheel { dx, dy } => Ok(self.wheel(*dx, *dy)),
            InputEvent::Touch { .. } => Err(error::unsupported_event(
                "touch",
                "The uinput sink has no touch device yet; use the HID gadget sink for touch",
            )),
            InputEvent::Pen { .. } => Err(error::unsupported_event(
                "pen",
                "The uinput sink has no tablet device yet; use the HID gadget sink for pen input",
            )),
            InputEvent::Clipboard { .. } => Err(error::unsupported_event(
                "clipboard",
                "The input sink does not transport clipboard data",
            )),
            InputEvent::ReleaseAll => Ok(self.release_all_planned()),
        }
    }

    fn key(
        &mut self,
        code: u32,
        down: bool,
        modifiers: KeyModifiers,
    ) -> Result<PlannedEvents, ApiError> {
        if code == 0 || code > u32::from(u8::MAX) {
            return Err(error::invalid_key(code));
        }
        let usage = code as u8;
        if hid_usage_to_linux_keycode(usage).is_none() {
            // Reject before mutating so the caller can retry or fall back.
            return Err(error::unmapped_key(usage));
        }

        let before = self.down_usages();
        if is_modifier_usage(usage) {
            let bit = 1u8 << (usage - MODIFIER_USAGE_START);
            if down {
                self.explicit_modifiers |= bit;
            } else {
                self.explicit_modifiers &= !bit;
            }
        } else {
            // Match Router: a non-modifier key event carries the client's
            // current modifier mask, including on key-up.
            self.client_modifiers = modifiers.0;
            if down {
                self.keys.insert(usage);
            } else {
                self.keys.remove(&usage);
            }
        }

        Ok(PlannedEvents::keyboard(key_diff(
            &before,
            &self.down_usages(),
        )))
    }

    fn absolute(
        &mut self,
        x: i32,
        y: i32,
        protocol: MouseProtocol,
        mapping: AbsoluteMapping,
    ) -> Result<PlannedEvents, ApiError> {
        if protocol != MouseProtocol::Absolute {
            return Err(error::wrong_mouse_mode("PointerAbsolute", "Absolute"));
        }
        let point = mapping.map(x, y);
        self.position = point;
        self.positioned = true;
        Ok(PlannedEvents::pointer(vec![
            LinuxEvent::Absolute {
                axis: ABS_X,
                value: i32::from(point.x),
            },
            LinuxEvent::Absolute {
                axis: ABS_Y,
                value: i32::from(point.y),
            },
        ]))
    }

    fn relative(
        &mut self,
        dx: i32,
        dy: i32,
        protocol: MouseProtocol,
    ) -> Result<PlannedEvents, ApiError> {
        if protocol != MouseProtocol::Relative {
            return Err(error::wrong_mouse_mode("PointerRelative", "Relative"));
        }
        let mut events = Vec::with_capacity(2);
        if dx != 0 {
            events.push(LinuxEvent::Relative {
                axis: REL_X,
                value: dx,
            });
        }
        if dy != 0 {
            events.push(LinuxEvent::Relative {
                axis: REL_Y,
                value: dy,
            });
        }
        Ok(PlannedEvents::pointer(events))
    }

    fn button(&mut self, button: PointerButton, down: bool) -> PlannedEvents {
        let bit = pointer_button_bit(button);
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        PlannedEvents::pointer(vec![LinuxEvent::Key {
            code: pointer_button_keycode(button),
            pressed: down,
        }])
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> PlannedEvents {
        let mut events = Vec::with_capacity(2);
        if dy != 0 {
            events.push(LinuxEvent::Relative {
                axis: REL_WHEEL,
                value: dy,
            });
        }
        if dx != 0 {
            events.push(LinuxEvent::Relative {
                axis: REL_HWHEEL,
                value: dx,
            });
        }
        PlannedEvents::pointer(events)
    }

    fn release_all_planned(&mut self) -> PlannedEvents {
        let before = self.down_usages();
        let buttons = self.buttons;

        self.keys.clear();
        self.explicit_modifiers = 0;
        self.client_modifiers = 0;
        self.buttons = 0;

        let keyboard = key_diff(&before, &BTreeSet::new());
        let mut pointer = Vec::new();
        for button in [
            PointerButton::Left,
            PointerButton::Right,
            PointerButton::Middle,
            PointerButton::Back,
            PointerButton::Forward,
        ] {
            if buttons & pointer_button_bit(button) != 0 {
                pointer.push(LinuxEvent::Key {
                    code: pointer_button_keycode(button),
                    pressed: false,
                });
            }
        }

        PlannedEvents { keyboard, pointer }
    }
}

fn key_diff(before: &BTreeSet<u8>, after: &BTreeSet<u8>) -> Vec<LinuxEvent> {
    let mut events = Vec::new();

    // Release non-modifiers before modifiers, so a shortcut's letter is
    // released while the modifier is still logically down.
    for usage in before.difference(after) {
        if !is_modifier_usage(*usage) {
            push_key(&mut events, *usage, false);
        }
    }
    for usage in before.difference(after) {
        if is_modifier_usage(*usage) {
            push_key(&mut events, *usage, false);
        }
    }

    // Press modifiers before non-modifiers.
    for usage in after.difference(before) {
        if is_modifier_usage(*usage) {
            push_key(&mut events, *usage, true);
        }
    }
    for usage in after.difference(before) {
        if !is_modifier_usage(*usage) {
            push_key(&mut events, *usage, true);
        }
    }

    events
}

fn push_key(events: &mut Vec<LinuxEvent>, usage: u8, pressed: bool) {
    if let Some(code) = hid_usage_to_linux_keycode(usage) {
        events.push(LinuxEvent::Key { code, pressed });
    }
}

const fn pointer_button_keycode(button: PointerButton) -> u16 {
    match button {
        PointerButton::Left => BTN_LEFT,
        PointerButton::Right => BTN_RIGHT,
        PointerButton::Middle => BTN_MIDDLE,
        PointerButton::Back => BTN_SIDE,
        PointerButton::Forward => BTN_EXTRA,
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::{Mutex, MutexGuard};

    use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
    use evdev::{
        AbsInfo, AbsoluteAxisType, AttributeSet, BusType, EventType, InputId, Key,
        RelativeAxisType, UinputAbsSetup,
    };

    use super::*;

    // Matches the Python node's virtual devices (pid.codes test vendor).
    const VENDOR: u16 = 0x1209;
    const KEYBOARD_PRODUCT: u16 = 0x0001;
    const POINTER_PRODUCT: u16 = 0x0002;
    const VERSION: u16 = 1;

    struct Inner {
        keyboard: VirtualDevice,
        pointer: VirtualDevice,
        state: InputState,
    }

    /// Linux `uinput`-backed keyboard and pointer sink.
    ///
    /// The sink owns one virtual keyboard and one virtual pointer. Events are
    /// serialized through a single mutex; state is cloned, planned, written,
    /// and only then committed.
    pub struct UinputSink {
        options: UinputOptions,
        inner: Mutex<Inner>,
    }

    impl std::fmt::Debug for UinputSink {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("UinputSink")
                .field("options", &self.options)
                .finish_non_exhaustive()
        }
    }

    impl UinputSink {
        /// Open the default devices with [`UinputOptions::default`].
        pub fn open() -> Result<Self, ApiError> {
            Self::open_with_options(UinputOptions::default())
        }

        /// Open a virtual keyboard and pointer with explicit names, protocol,
        /// and absolute mapping.
        pub fn open_with_options(options: UinputOptions) -> Result<Self, ApiError> {
            validate_name("keyboard", &options.keyboard_name)?;
            validate_name("pointer", &options.pointer_name)?;
            let keyboard = create_keyboard(&options.keyboard_name)?;
            let pointer = create_pointer(&options.pointer_name, options.mouse_protocol)?;
            Ok(Self {
                options,
                inner: Mutex::new(Inner {
                    keyboard,
                    pointer,
                    state: InputState::default(),
                }),
            })
        }

        pub fn options(&self) -> &UinputOptions {
            &self.options
        }

        pub fn mouse_protocol(&self) -> MouseProtocol {
            self.options.mouse_protocol
        }

        pub fn keyboard_name(&self) -> &str {
            &self.options.keyboard_name
        }

        pub fn pointer_name(&self) -> &str {
            &self.options.pointer_name
        }

        fn inner(&self) -> MutexGuard<'_, Inner> {
            crate::sink::lock(&self.inner)
        }
    }

    impl InputSink for UinputSink {
        fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
            let mut inner = self.inner();
            let mut next = inner.state.clone();
            let planned = next.plan(event, self.options.mouse_protocol, self.options.mapping)?;
            let reports_written = write_planned(&mut inner, &planned)?;
            inner.state = next;
            Ok(SubmitReport {
                reports_written,
                state_changed: reports_written > 0,
            })
        }

        fn release_all(&self) -> Result<(), ApiError> {
            let mut inner = self.inner();
            let mut next = inner.state.clone();
            let planned = next.release_all_planned();

            // Clear local state before writing, matching HidGadgetSink: even a
            // failed neutral write must not leave keys logically held.
            inner.state = next;
            write_planned(&mut inner, &planned).map(|_| ())
        }

        fn held(&self) -> HeldInput {
            self.inner().state.snapshot()
        }
    }

    fn validate_name(kind: &'static str, name: &str) -> Result<(), ApiError> {
        if name.len() > MAX_UINPUT_NAME_LEN {
            return Err(error::uinput_name_too_long(kind, name));
        }
        Ok(())
    }

    fn create_keyboard(name: &str) -> Result<VirtualDevice, ApiError> {
        let mut keys = AttributeSet::<Key>::new();
        for usage in 0..=u8::MAX {
            if let Some(code) = hid_usage_to_linux_keycode(usage) {
                keys.insert(Key::new(code));
            }
        }

        VirtualDeviceBuilder::new()
            .map_err(error::uinput_open_error)?
            .name(name)
            .input_id(InputId::new(
                BusType::BUS_USB,
                VENDOR,
                KEYBOARD_PRODUCT,
                VERSION,
            ))
            .with_keys(&keys)
            .map_err(error::uinput_open_error)?
            .build()
            .map_err(error::uinput_open_error)
    }

    fn create_pointer(name: &str, protocol: MouseProtocol) -> Result<VirtualDevice, ApiError> {
        let mut keys = AttributeSet::<Key>::new();
        for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            keys.insert(Key::new(code));
        }

        let mut relative = AttributeSet::<RelativeAxisType>::new();
        relative.insert(RelativeAxisType::REL_WHEEL);
        relative.insert(RelativeAxisType::REL_HWHEEL);

        let builder = VirtualDeviceBuilder::new()
            .map_err(error::uinput_open_error)?
            .name(name)
            .input_id(InputId::new(
                BusType::BUS_USB,
                VENDOR,
                POINTER_PRODUCT,
                VERSION,
            ))
            .with_keys(&keys)
            .map_err(error::uinput_open_error)?;

        match protocol {
            MouseProtocol::Relative => {
                relative.insert(RelativeAxisType::REL_X);
                relative.insert(RelativeAxisType::REL_Y);
                builder
                    .with_relative_axes(&relative)
                    .map_err(error::uinput_open_error)?
                    .build()
                    .map_err(error::uinput_open_error)
            }
            MouseProtocol::Absolute => {
                let x = UinputAbsSetup::new(
                    AbsoluteAxisType::ABS_X,
                    AbsInfo::new(0, 0, i32::from(HID_ABSOLUTE_MAX), 0, 0, 1),
                );
                let y = UinputAbsSetup::new(
                    AbsoluteAxisType::ABS_Y,
                    AbsInfo::new(0, 0, i32::from(HID_ABSOLUTE_MAX), 0, 0, 1),
                );
                builder
                    .with_relative_axes(&relative)
                    .map_err(error::uinput_open_error)?
                    .with_absolute_axis(&x)
                    .map_err(error::uinput_open_error)?
                    .with_absolute_axis(&y)
                    .map_err(error::uinput_open_error)?
                    .build()
                    .map_err(error::uinput_open_error)
            }
        }
    }

    fn write_planned(inner: &mut Inner, planned: &PlannedEvents) -> Result<usize, ApiError> {
        if planned.is_empty() {
            return Ok(0);
        }
        if !planned.keyboard.is_empty() {
            write_events(&mut inner.keyboard, &planned.keyboard)?;
        }
        if !planned.pointer.is_empty() {
            write_events(&mut inner.pointer, &planned.pointer)?;
        }
        Ok(planned.batches())
    }

    fn write_events(device: &mut VirtualDevice, events: &[LinuxEvent]) -> Result<(), ApiError> {
        if events.is_empty() {
            return Ok(());
        }
        let events: Vec<evdev::InputEvent> = events
            .iter()
            .map(|event| match *event {
                LinuxEvent::Key { code, pressed } => {
                    evdev::InputEvent::new(EventType::KEY, code, i32::from(pressed))
                }
                LinuxEvent::Relative { axis, value } => {
                    evdev::InputEvent::new(EventType::RELATIVE, axis, value)
                }
                LinuxEvent::Absolute { axis, value } => {
                    evdev::InputEvent::new(EventType::ABSOLUTE, axis, value)
                }
            })
            .collect();
        // `VirtualDevice::emit` appends the terminating SYN_REPORT.
        device.emit(&events).map_err(error::uinput_write_error)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    /// Stub for non-Linux targets.
    ///
    /// The type exists so callers can compile cross-platform; every open
    /// attempt returns [`codes::UNSUPPORTED`](crate::codes::UNSUPPORTED).
    #[derive(Debug)]
    pub struct UinputSink {
        _private: (),
    }

    impl UinputSink {
        pub fn open() -> Result<Self, ApiError> {
            Err(error::uinput_unsupported())
        }

        pub fn open_with_options(_options: UinputOptions) -> Result<Self, ApiError> {
            Err(error::uinput_unsupported())
        }

        pub fn options(&self) -> &UinputOptions {
            // Unreachable in practice: the type cannot be constructed.
            static DEFAULT: std::sync::OnceLock<UinputOptions> = std::sync::OnceLock::new();
            DEFAULT.get_or_init(UinputOptions::default)
        }

        pub fn mouse_protocol(&self) -> MouseProtocol {
            self.options().mouse_protocol
        }

        pub fn keyboard_name(&self) -> &str {
            &self.options().keyboard_name
        }

        pub fn pointer_name(&self) -> &str {
            &self.options().pointer_name
        }
    }

    impl InputSink for UinputSink {
        fn submit_report(&self, _event: &InputEvent) -> Result<SubmitReport, ApiError> {
            Err(error::uinput_unsupported())
        }

        fn release_all(&self) -> Result<(), ApiError> {
            Err(error::uinput_unsupported())
        }

        fn held(&self) -> HeldInput {
            HeldInput::default()
        }
    }
}

pub use imp::UinputSink;

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::input::PointerButton;
    use ozma_protocol::input::TouchPhase;

    use crate::codes;

    fn key_event(code: u32, down: bool, modifiers: u8) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: KeyModifiers(modifiers),
        }
    }

    fn key(code: u16, pressed: bool) -> LinuxEvent {
        LinuxEvent::Key { code, pressed }
    }

    fn relative(axis: u16, value: i32) -> LinuxEvent {
        LinuxEvent::Relative { axis, value }
    }

    fn absolute(axis: u16, value: i32) -> LinuxEvent {
        LinuxEvent::Absolute { axis, value }
    }

    fn plan(state: &mut InputState, event: &InputEvent) -> Result<PlannedEvents, ApiError> {
        state.plan(event, MouseProtocol::Absolute, AbsoluteMapping::hid_space())
    }

    #[test]
    fn options_default_to_the_gadget_sink_behavior() {
        let options = UinputOptions::default();
        assert_eq!(options.keyboard_name, DEFAULT_UINPUT_KEYBOARD_NAME);
        assert_eq!(options.pointer_name, DEFAULT_UINPUT_POINTER_NAME);
        assert_eq!(options.mouse_protocol, MouseProtocol::Absolute);
        assert_eq!(options.mapping, AbsoluteMapping::hid_space());
    }

    #[test]
    fn hid_mapping_covers_the_python_table_and_more() {
        // Letters, digits, punctuation.
        assert_eq!(hid_usage_to_linux_keycode(0x04), Some(30)); // a
        assert_eq!(hid_usage_to_linux_keycode(0x1D), Some(44)); // z
        assert_eq!(hid_usage_to_linux_keycode(0x1E), Some(2)); // 1
        assert_eq!(hid_usage_to_linux_keycode(0x27), Some(11)); // 0
        assert_eq!(hid_usage_to_linux_keycode(0x28), Some(28)); // Enter
        assert_eq!(hid_usage_to_linux_keycode(0x29), Some(1)); // Escape
        assert_eq!(hid_usage_to_linux_keycode(0x2C), Some(57)); // Space
        assert_eq!(hid_usage_to_linux_keycode(0x39), Some(58)); // CapsLock

        // Modifiers.
        assert_eq!(hid_usage_to_linux_keycode(0xE0), Some(29));
        assert_eq!(hid_usage_to_linux_keycode(0xE1), Some(42));
        assert_eq!(hid_usage_to_linux_keycode(0xE2), Some(56));
        assert_eq!(hid_usage_to_linux_keycode(0xE3), Some(125));
        assert_eq!(hid_usage_to_linux_keycode(0xE4), Some(97));
        assert_eq!(hid_usage_to_linux_keycode(0xE5), Some(54));
        assert_eq!(hid_usage_to_linux_keycode(0xE6), Some(100));
        assert_eq!(hid_usage_to_linux_keycode(0xE7), Some(126));

        // Function keys F1–F24.
        assert_eq!(hid_usage_to_linux_keycode(0x3A), Some(59)); // F1
        assert_eq!(hid_usage_to_linux_keycode(0x45), Some(88)); // F12
        assert_eq!(hid_usage_to_linux_keycode(0x68), Some(183)); // F13
        assert_eq!(hid_usage_to_linux_keycode(0x73), Some(194)); // F24

        // Navigation, keypad, system, media.
        assert_eq!(hid_usage_to_linux_keycode(0x4F), Some(106)); // Right
        assert_eq!(hid_usage_to_linux_keycode(0x52), Some(103)); // Up
        assert_eq!(hid_usage_to_linux_keycode(0x54), Some(98)); // KP /
        assert_eq!(hid_usage_to_linux_keycode(0x63), Some(83)); // KP .
        assert_eq!(hid_usage_to_linux_keycode(0x46), Some(99)); // PrintScreen
        assert_eq!(hid_usage_to_linux_keycode(0x65), Some(127)); // Menu
        assert_eq!(hid_usage_to_linux_keycode(0x7F), Some(113)); // Mute
        assert_eq!(hid_usage_to_linux_keycode(0x80), Some(115)); // Volume up
        assert_eq!(hid_usage_to_linux_keycode(0xB5), Some(163)); // Next track

        // Reserved / unmapped usages.
        assert_eq!(hid_usage_to_linux_keycode(0x00), None);
        assert_eq!(hid_usage_to_linux_keycode(0x02), None);
        assert_eq!(hid_usage_to_linux_keycode(0x32), None);
        assert_eq!(hid_usage_to_linux_keycode(0xFF), None);
    }

    #[test]
    fn key_press_release_and_duplicate() {
        let mut state = InputState::default();

        let down = plan(&mut state, &key_event(0x04, true, 0)).unwrap();
        assert_eq!(down.keyboard, vec![key(30, true)]);
        assert!(down.pointer.is_empty());
        assert!(state.snapshot().has_key(0x04));

        let duplicate = plan(&mut state, &key_event(0x04, true, 0)).unwrap();
        assert!(duplicate.is_empty());
        assert_eq!(duplicate.batches(), 0);
        assert!(state.snapshot().has_key(0x04));

        let up = plan(&mut state, &key_event(0x04, false, 0)).unwrap();
        assert_eq!(up.keyboard, vec![key(30, false)]);

        let duplicate_up = plan(&mut state, &key_event(0x04, false, 0)).unwrap();
        assert!(duplicate_up.is_empty());
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn client_modifiers_are_pressed_and_released_with_the_key() {
        let mut state = InputState::default();

        let down = plan(&mut state, &key_event(0x04, true, 0x02)).unwrap();
        assert_eq!(
            down.keyboard,
            vec![key(42, true), key(30, true)],
            "modifier must be emitted before the key"
        );
        assert_eq!(state.snapshot().modifiers, 0x02);

        let up = plan(&mut state, &key_event(0x04, false, 0)).unwrap();
        assert_eq!(
            up.keyboard,
            vec![key(30, false), key(42, false)],
            "key must be released before the modifier"
        );
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn explicit_modifier_key_tracks_held_state() {
        let mut state = InputState::default();

        let down = plan(&mut state, &key_event(0xE1, true, 0)).unwrap();
        assert_eq!(down.keyboard, vec![key(42, true)]);
        assert_eq!(state.snapshot().modifiers, 0x02);

        let duplicate = plan(&mut state, &key_event(0xE1, true, 0)).unwrap();
        assert!(duplicate.is_empty());

        let a = plan(&mut state, &key_event(0x04, true, 0)).unwrap();
        assert_eq!(a.keyboard, vec![key(30, true)]);
        assert!(state.snapshot().has_key(0x04));
        assert_eq!(state.snapshot().modifiers, 0x02);

        let a_up = plan(&mut state, &key_event(0x04, false, 0)).unwrap();
        assert_eq!(a_up.keyboard, vec![key(30, false)]);

        let up = plan(&mut state, &key_event(0xE1, false, 0)).unwrap();
        assert_eq!(up.keyboard, vec![key(42, false)]);
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected_before_state_changes() {
        let mut state = InputState::default();

        let error = plan(&mut state, &key_event(0x02, true, 0)).unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);
        assert!(state.snapshot().is_empty());

        let error = plan(&mut state, &key_event(0, true, 0)).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);

        let error = plan(&mut state, &key_event(300, true, 0)).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);

        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn absolute_mode_moves_the_pointer_and_rejects_relative_events() {
        let mut state = InputState::default();
        let event = InputEvent::PointerAbsolute {
            display_id: None,
            x: 100,
            y: 200,
        };
        let planned = plan(&mut state, &event).unwrap();
        assert_eq!(
            planned.pointer,
            vec![absolute(ABS_X, 100), absolute(ABS_Y, 200)]
        );
        assert_eq!(
            state.snapshot().pointer,
            Some(AbsolutePoint { x: 100, y: 200 })
        );

        let mapped = state
            .plan(
                &InputEvent::PointerAbsolute {
                    display_id: None,
                    x: 1919,
                    y: 1079,
                },
                MouseProtocol::Absolute,
                AbsoluteMapping::new(1920, 1080),
            )
            .unwrap();
        assert_eq!(
            mapped.pointer,
            vec![absolute(ABS_X, 32_767), absolute(ABS_Y, 32_767)]
        );

        let error = plan(&mut state, &InputEvent::PointerRelative { dx: 5, dy: -5 }).unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);
    }

    #[test]
    fn relative_mode_moves_the_pointer_and_rejects_absolute_events() {
        let mut state = InputState::default();
        let protocol = MouseProtocol::Relative;
        let mapping = AbsoluteMapping::hid_space();

        let planned = state
            .plan(
                &InputEvent::PointerRelative { dx: 4, dy: -3 },
                protocol,
                mapping,
            )
            .unwrap();
        assert_eq!(
            planned.pointer,
            vec![relative(REL_X, 4), relative(REL_Y, -3)]
        );

        let error = state
            .plan(
                &InputEvent::PointerAbsolute {
                    display_id: None,
                    x: 10,
                    y: 20,
                },
                protocol,
                mapping,
            )
            .unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);

        let zero = state
            .plan(
                &InputEvent::PointerRelative { dx: 0, dy: 0 },
                protocol,
                mapping,
            )
            .unwrap();
        assert!(zero.is_empty());
    }

    #[test]
    fn buttons_and_wheel_are_pointer_events() {
        let mut state = InputState::default();

        let down = plan(
            &mut state,
            &InputEvent::Button {
                button: PointerButton::Left,
                down: true,
            },
        )
        .unwrap();
        assert_eq!(down.pointer, vec![key(BTN_LEFT, true)]);
        assert!(state.snapshot().has_button(PointerButton::Left));

        let wheel = plan(&mut state, &InputEvent::Wheel { dx: -1, dy: 3 }).unwrap();
        assert_eq!(
            wheel.pointer,
            vec![relative(REL_WHEEL, 3), relative(REL_HWHEEL, -1)]
        );

        let empty = plan(&mut state, &InputEvent::Wheel { dx: 0, dy: 0 }).unwrap();
        assert!(empty.is_empty());

        let up = plan(
            &mut state,
            &InputEvent::Button {
                button: PointerButton::Left,
                down: false,
            },
        )
        .unwrap();
        assert_eq!(up.pointer, vec![key(BTN_LEFT, false)]);
        assert!(state.snapshot().buttons == 0);
    }

    #[test]
    fn release_all_releases_keys_modifiers_and_buttons_idempotently() {
        let mut state = InputState::default();
        plan(&mut state, &key_event(0x04, true, 0)).unwrap();
        plan(&mut state, &key_event(0xE1, true, 0)).unwrap();
        plan(
            &mut state,
            &InputEvent::Button {
                button: PointerButton::Right,
                down: true,
            },
        )
        .unwrap();
        assert!(!state.snapshot().is_empty());

        let planned = plan(&mut state, &InputEvent::ReleaseAll).unwrap();
        assert_eq!(planned.keyboard, vec![key(30, false), key(42, false)]);
        assert_eq!(planned.pointer, vec![key(BTN_RIGHT, false)]);
        assert!(state.snapshot().is_empty());

        let again = plan(&mut state, &InputEvent::ReleaseAll).unwrap();
        assert!(again.is_empty());
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn touch_pen_and_clipboard_are_unsupported() {
        let mut state = InputState::default();

        let touch = InputEvent::Touch {
            contact: 0,
            phase: TouchPhase::Down,
            display_id: None,
            x: 1,
            y: 2,
            pressure: 0,
        };
        assert_eq!(
            plan(&mut state, &touch).unwrap_err().code,
            codes::UNSUPPORTED
        );

        let pen = InputEvent::Pen {
            phase: TouchPhase::Down,
            display_id: None,
            x: 1,
            y: 2,
            pressure: 0,
            tilt_x: 0,
            tilt_y: 0,
        };
        assert_eq!(plan(&mut state, &pen).unwrap_err().code, codes::UNSUPPORTED);

        let clipboard = InputEvent::Clipboard {
            text: "not here".into(),
        };
        assert_eq!(
            plan(&mut state, &clipboard).unwrap_err().code,
            codes::UNSUPPORTED
        );
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn short_state_machine_keeps_device_and_snapshot_in_step() {
        let mut state = InputState::default();

        // Ctrl down, A down, B down, B up, A up, Ctrl up.
        // The controller sends explicit modifier key events, so the per-event
        // modifier mask stays zero (see the compat decoder).
        plan(&mut state, &key_event(0xE0, true, 0)).unwrap();
        plan(&mut state, &key_event(0x04, true, 0)).unwrap();
        plan(&mut state, &key_event(0x05, true, 0)).unwrap();
        assert_eq!(state.snapshot().keys.len(), 2);
        assert_eq!(state.snapshot().modifiers, 0x01);

        plan(&mut state, &key_event(0x05, false, 0)).unwrap();
        assert_eq!(state.snapshot().keys.len(), 1);

        plan(&mut state, &key_event(0x04, false, 0)).unwrap();
        assert_eq!(state.snapshot().keys.len(), 0);
        assert_eq!(state.snapshot().modifiers, 0x01);

        plan(&mut state, &key_event(0xE0, false, 0)).unwrap();
        assert!(state.snapshot().is_empty());
    }

    /// A name longer than the kernel limit is rejected before `/dev/uinput` is
    /// touched, so this test does not need the device.
    #[cfg(target_os = "linux")]
    #[test]
    fn long_device_names_are_rejected_before_open() {
        let options = UinputOptions {
            keyboard_name: "k".repeat(MAX_UINPUT_NAME_LEN + 1),
            ..UinputOptions::default()
        };
        let error = UinputSink::open_with_options(options).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    /// Opt-in smoke test. Requires `/dev/uinput` and the `uinput` module.
    ///
    /// Run with `OZMA_UINPUT_TEST=1 cargo test -p ozma-input -- --ignored`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires /dev/uinput; set OZMA_UINPUT_TEST=1 and run with --ignored"]
    fn real_uinput_device_accepts_events() {
        if std::env::var("OZMA_UINPUT_TEST").as_deref() != Ok("1") {
            eprintln!("skipping: OZMA_UINPUT_TEST is not 1");
            return;
        }

        let sink = UinputSink::open().expect("open uinput keyboard and pointer");
        sink.submit(&key_event(0x04, true, 0)).unwrap();
        assert!(sink.held().has_key(0x04));

        sink.submit(&InputEvent::Button {
            button: PointerButton::Left,
            down: true,
        })
        .unwrap();
        sink.submit(&InputEvent::Wheel { dx: 0, dy: 1 }).unwrap();
        sink.submit(&InputEvent::PointerAbsolute {
            display_id: None,
            x: 100,
            y: 100,
        })
        .unwrap();

        sink.release_all().unwrap();
        assert!(sink.held().is_empty());
    }
}
