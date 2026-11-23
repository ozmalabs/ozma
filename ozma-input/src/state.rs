// SPDX-License-Identifier: AGPL-3.0-only
//! Held-input state and the pure event router.
//!
//! The router owns no I/O. It turns an [`InputEvent`] into zero or more HID
//! reports and updates the held-key/button state. Sinks clone the router
//! before applying an event, write the resulting reports, and only commit the
//! clone when every write succeeds. That keeps a failed write from leaving the
//! in-memory state ahead of the device.

use std::collections::BTreeSet;

use ozma_proto::{HidKeyboardReport, HidMouseReport};
use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton, TouchPhase};
use ozma_protocol::ApiError;
use serde::{Deserialize, Serialize};

use crate::error;
use crate::report::{
    clamp_i8, pointer_button_bit, AbsoluteMapping, AbsolutePoint, AbsolutePointerReport,
    MouseProtocol,
};

/// Maximum number of non-modifier keys in a boot keyboard report.
pub const MAX_KEYS: usize = 6;

const MODIFIER_USAGE_START: u8 = 0xE0;

/// Snapshot of everything currently held by a sink.
///
/// `pointer` is diagnostic state, not held input: it is the last absolute
/// position and does not make the snapshot non-empty. `buttons` is the
/// *effective* button mask, including the left button synthesized for an
/// active touch contact or pen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldInput {
    /// Non-modifier HID usage ids currently held.
    pub keys: BTreeSet<u8>,
    /// Effective modifier bitmask (client modifiers OR pressed modifier keys).
    pub modifiers: u8,
    /// Effective pointer button bitmask.
    pub buttons: u8,
    /// Active touch contact ids.
    pub contacts: BTreeSet<u32>,
    /// Whether a pen contact is active.
    pub pen_down: bool,
    /// Last absolute position, if one has been submitted.
    pub pointer: Option<AbsolutePoint>,
}

impl HeldInput {
    /// True when no key, modifier, button, or contact is held.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
            && self.modifiers == 0
            && self.buttons == 0
            && self.contacts.is_empty()
            && !self.pen_down
    }

    /// True when a non-modifier key is held.
    pub fn has_key(&self, code: u8) -> bool {
        self.keys.contains(&code)
    }

    /// True when the effective button mask contains `button`.
    pub fn has_button(&self, button: PointerButton) -> bool {
        self.buttons & pointer_button_bit(button) != 0
    }
}

/// A report ready to be written by a sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    Keyboard(HidKeyboardReport),
    RelativeMouse(HidMouseReport),
    AbsoluteMouse(AbsolutePointerReport),
}

#[derive(Clone, Debug)]
struct KeyboardState {
    keys: BTreeSet<u8>,
    client_modifiers: u8,
    modifier_keys: u8,
    /// The last report actually written, so duplicate key presses are no-ops.
    last_report: Option<HidKeyboardReport>,
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self {
            keys: BTreeSet::new(),
            client_modifiers: 0,
            modifier_keys: 0,
            // The device starts neutral; treat it as already written so a
            // key-up or duplicate event before any key-down is a no-op.
            last_report: Some(HidKeyboardReport::empty()),
        }
    }
}

impl KeyboardState {
    fn modifiers(&self) -> u8 {
        self.client_modifiers | self.modifier_keys
    }

    fn report(&self) -> HidKeyboardReport {
        let mut keycodes = [0u8; MAX_KEYS];
        for (slot, code) in self.keys.iter().take(MAX_KEYS).enumerate() {
            keycodes[slot] = *code;
        }
        HidKeyboardReport { modifiers: self.modifiers(), reserved: 0, keycodes }
    }

    fn reset(&mut self) {
        self.keys.clear();
        self.client_modifiers = 0;
        self.modifier_keys = 0;
        self.last_report = None;
    }
}

#[derive(Clone, Debug)]
struct PointerState {
    buttons: u8,
    position: AbsolutePoint,
    positioned: bool,
    contacts: BTreeSet<u32>,
    pen_down: bool,
}

impl Default for PointerState {
    fn default() -> Self {
        Self {
            buttons: 0,
            position: AbsolutePoint::CENTER,
            positioned: false,
            contacts: BTreeSet::new(),
            pen_down: false,
        }
    }
}

impl PointerState {
    fn effective_buttons(&self) -> u8 {
        let touch = if self.contacts.is_empty() && !self.pen_down {
            0
        } else {
            pointer_button_bit(PointerButton::Left)
        };
        self.buttons | touch
    }

    fn set_position(&mut self, point: AbsolutePoint) {
        self.position = point;
        self.positioned = true;
    }
}

/// Pure event router shared by the real gadget sink and the fake sink.
#[derive(Clone, Debug)]
pub(crate) struct Router {
    keyboard: KeyboardState,
    pointer: PointerState,
    mapping: AbsoluteMapping,
    protocol: MouseProtocol,
}

impl Router {
    pub(crate) fn new(mapping: AbsoluteMapping, protocol: MouseProtocol) -> Self {
        Self {
            keyboard: KeyboardState::default(),
            pointer: PointerState::default(),
            mapping,
            protocol,
        }
    }

    pub(crate) fn protocol(&self) -> MouseProtocol {
        self.protocol
    }

    pub(crate) fn apply(&mut self, event: &InputEvent) -> Result<Vec<Report>, ApiError> {
        match event {
            InputEvent::Key { code, down, modifiers } => {
                self.apply_key(*code, *down, *modifiers)
            }
            InputEvent::PointerAbsolute { x, y, .. } => self.absolute(*x, *y),
            InputEvent::PointerRelative { dx, dy } => self.relative(*dx, *dy),
            InputEvent::Button { button, down } => self.button(*button, *down),
            InputEvent::Wheel { dx, dy } => self.wheel(*dx, *dy),
            InputEvent::Touch { contact, phase, x, y, .. } => {
                self.touch(*contact, *phase, *x, *y)
            }
            InputEvent::Pen { phase, x, y, .. } => self.pen(*phase, *x, *y),
            InputEvent::Clipboard { .. } => Err(error::unsupported_event(
                "clipboard",
                "The HID input sink does not transport clipboard data",
            )),
            InputEvent::ReleaseAll => Ok(self.release_all()),
        }
    }

    pub(crate) fn release_all(&mut self) -> Vec<Report> {
        self.keyboard.reset();
        self.pointer.buttons = 0;
        self.pointer.contacts.clear();
        self.pointer.pen_down = false;

        let keyboard = HidKeyboardReport::empty();
        // Record the neutral report so the next key event is emitted even if
        // it happens to reconstruct the same state.
        self.keyboard.last_report = Some(keyboard);

        let mouse = match self.protocol {
            MouseProtocol::Absolute => {
                Report::AbsoluteMouse(self.absolute_report(0))
            }
            MouseProtocol::Relative => Report::RelativeMouse(HidMouseReport::empty()),
        };
        vec![Report::Keyboard(keyboard), mouse]
    }

    pub(crate) fn snapshot(&self) -> HeldInput {
        HeldInput {
            keys: self.keyboard.keys.clone(),
            modifiers: self.keyboard.modifiers(),
            buttons: self.pointer.effective_buttons(),
            contacts: self.pointer.contacts.clone(),
            pen_down: self.pointer.pen_down,
            pointer: if self.pointer.positioned {
                Some(self.pointer.position)
            } else {
                None
            },
        }
    }

    fn apply_key(
        &mut self,
        code: u32,
        down: bool,
        modifiers: KeyModifiers,
    ) -> Result<Vec<Report>, ApiError> {
        let code = validate_keycode(code)?;

        if is_modifier_usage(code) {
            let bit = 1u8 << (code - MODIFIER_USAGE_START);
            if down {
                self.keyboard.modifier_keys |= bit;
            } else {
                self.keyboard.modifier_keys &= !bit;
            }
        } else {
            if down
                && !self.keyboard.keys.contains(&code)
                && self.keyboard.keys.len() >= MAX_KEYS
            {
                // Reject before mutating anything so the caller can retry with
                // a different event without a stuck-key shadow state.
                return Err(error::key_limit(code, self.keyboard.keys.len()));
            }
            if down {
                self.keyboard.keys.insert(code);
            } else {
                self.keyboard.keys.remove(&code);
            }
            self.keyboard.client_modifiers = modifiers.0;
        }

        let report = self.keyboard.report();
        if self.keyboard.last_report == Some(report) {
            Ok(Vec::new())
        } else {
            self.keyboard.last_report = Some(report);
            Ok(vec![Report::Keyboard(report)])
        }
    }

    fn absolute(&mut self, x: i32, y: i32) -> Result<Vec<Report>, ApiError> {
        if self.protocol != MouseProtocol::Absolute {
            return Err(error::wrong_mouse_mode("PointerAbsolute", "Absolute"));
        }
        self.pointer.set_position(self.mapping.map(x, y));
        Ok(vec![Report::AbsoluteMouse(self.absolute_report(0))])
    }

    fn relative(&mut self, dx: i32, dy: i32) -> Result<Vec<Report>, ApiError> {
        if self.protocol != MouseProtocol::Relative {
            return Err(error::wrong_mouse_mode("PointerRelative", "Relative"));
        }
        Ok(vec![Report::RelativeMouse(self.relative_report(dx, dy, 0))])
    }

    fn button(&mut self, button: PointerButton, down: bool) -> Result<Vec<Report>, ApiError> {
        let bit = pointer_button_bit(button);
        if down {
            self.pointer.buttons |= bit;
        } else {
            self.pointer.buttons &= !bit;
        }
        Ok(vec![self.pointer_report(0)])
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<Vec<Report>, ApiError> {
        if dx != 0 {
            tracing::debug!(
                dx,
                "horizontal wheel motion is not representable in the HID mouse report; ignoring it"
            );
        }
        Ok(vec![self.pointer_report(clamp_i8(dy))])
    }

    fn touch(
        &mut self,
        contact: u32,
        phase: TouchPhase,
        x: i32,
        y: i32,
    ) -> Result<Vec<Report>, ApiError> {
        if self.protocol != MouseProtocol::Absolute {
            return Err(error::wrong_mouse_mode("Touch", "Absolute"));
        }
        match phase {
            TouchPhase::Down => {
                self.pointer.contacts.insert(contact);
            }
            TouchPhase::Move => {
                // A Move without a preceding Down is treated as a hover: it
                // may update the cursor position but never presses a button,
                // so a lost Down cannot leave the host with a stuck click.
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                self.pointer.contacts.remove(&contact);
            }
        }
        self.pointer.set_position(self.mapping.map(x, y));
        Ok(vec![Report::AbsoluteMouse(self.absolute_report(0))])
    }

    fn pen(&mut self, phase: TouchPhase, x: i32, y: i32) -> Result<Vec<Report>, ApiError> {
        if self.protocol != MouseProtocol::Absolute {
            return Err(error::wrong_mouse_mode("Pen", "Absolute"));
        }
        match phase {
            TouchPhase::Down => {
                self.pointer.pen_down = true;
            }
            // A Move without a Down is a hover; do not synthesize a contact.
            TouchPhase::Move => {}
            TouchPhase::Up | TouchPhase::Cancel => {
                self.pointer.pen_down = false;
            }
        }
        self.pointer.set_position(self.mapping.map(x, y));
        Ok(vec![Report::AbsoluteMouse(self.absolute_report(0))])
    }

    fn absolute_report(&self, scroll: i8) -> AbsolutePointerReport {
        AbsolutePointerReport {
            buttons: self.pointer.effective_buttons(),
            x: self.pointer.position.x,
            y: self.pointer.position.y,
            scroll,
        }
    }

    fn relative_report(&self, dx: i32, dy: i32, scroll: i8) -> HidMouseReport {
        HidMouseReport {
            buttons: self.pointer.effective_buttons(),
            x: clamp_i8(dx),
            y: clamp_i8(dy),
            scroll,
        }
    }

    fn pointer_report(&self, scroll: i8) -> Report {
        match self.protocol {
            MouseProtocol::Absolute => {
                Report::AbsoluteMouse(self.absolute_report(scroll))
            }
            MouseProtocol::Relative => {
                Report::RelativeMouse(self.relative_report(0, 0, scroll))
            }
        }
    }
}

fn validate_keycode(code: u32) -> Result<u8, ApiError> {
    if code == 0 || code > u32::from(u8::MAX) {
        return Err(error::invalid_key(code));
    }
    Ok(code as u8)
}

fn is_modifier_usage(code: u8) -> bool {
    (MODIFIER_USAGE_START..=0xE7).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(protocol: MouseProtocol) -> Router {
        Router::new(AbsoluteMapping::hid_space(), protocol)
    }

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::Key { code, down, modifiers: KeyModifiers::default() }
    }

    #[test]
    fn duplicate_key_press_writes_nothing() {
        let mut router = router(MouseProtocol::Absolute);
        assert_eq!(router.apply(&key(0x04, true)).unwrap().len(), 1);
        assert!(router.apply(&key(0x04, true)).unwrap().is_empty());
        assert_eq!(router.snapshot().keys.len(), 1);
    }

    #[test]
    fn key_up_of_unheld_key_is_quiet() {
        let mut router = router(MouseProtocol::Absolute);
        assert!(router.apply(&key(0x04, false)).unwrap().is_empty());
        assert!(router.snapshot().is_empty());
    }

    #[test]
    fn modifier_keys_set_the_modifier_byte() {
        let mut router = router(MouseProtocol::Absolute);
        router.apply(&key(0xE1, true)).unwrap(); // left shift usage
        let reports = router.apply(&key(0x04, true)).unwrap(); // A
        assert_eq!(reports.len(), 1);
        match reports[0] {
            Report::Keyboard(report) => assert_eq!(report.modifiers, 0x02),
            other => panic!("expected keyboard report, got {other:?}"),
        }
        assert_eq!(router.snapshot().modifiers, 0x02);
    }

    #[test]
    fn client_modifiers_are_applied_without_clobbering_modifier_keys() {
        let mut router = router(MouseProtocol::Absolute);
        router.apply(&key(0xE1, true)).unwrap();
        router
            .apply(&InputEvent::Key {
                code: 0x04,
                down: true,
                modifiers: KeyModifiers::new(KeyModifiers::LEFT_ALT),
            })
            .unwrap();
        assert_eq!(
            router.snapshot().modifiers,
            KeyModifiers::LEFT_SHIFT | KeyModifiers::LEFT_ALT
        );
    }

    #[test]
    fn seventh_key_is_rejected_without_changing_state() {
        let mut router = router(MouseProtocol::Absolute);
        for code in 0x04..0x0A {
            router.apply(&key(code, true)).unwrap();
        }
        let error = router.apply(&key(0x0A, true)).unwrap_err();
        assert_eq!(error.code, crate::error::codes::INPUT_KEY_LIMIT);
        assert_eq!(router.snapshot().keys.len(), 6);
    }

    #[test]
    fn zero_and_out_of_range_keycodes_are_invalid() {
        let mut router = router(MouseProtocol::Absolute);
        assert_eq!(
            router.apply(&key(0, true)).unwrap_err().code,
            crate::error::codes::INVALID_REQUEST
        );
        assert_eq!(
            router.apply(&key(0x1_00, true)).unwrap_err().code,
            crate::error::codes::INVALID_REQUEST
        );
    }

    #[test]
    fn touch_holds_left_button_until_last_contact_lifts() {
        let mut router = router(MouseProtocol::Absolute);
        let touch = |contact, phase| InputEvent::Touch {
            contact,
            phase,
            display_id: None,
            x: 10,
            y: 20,
            pressure: 1,
        };
        router.apply(&touch(1, TouchPhase::Down)).unwrap();
        router.apply(&touch(2, TouchPhase::Down)).unwrap();
        assert!(router.snapshot().has_button(PointerButton::Left));
        router.apply(&touch(1, TouchPhase::Up)).unwrap();
        assert!(router.snapshot().has_button(PointerButton::Left));
        router.apply(&touch(2, TouchPhase::Cancel)).unwrap();
        assert!(!router.snapshot().has_button(PointerButton::Left));
        assert!(router.snapshot().contacts.is_empty());
    }

    #[test]
    fn touch_move_without_down_is_a_hover() {
        let mut router = router(MouseProtocol::Absolute);
        router
            .apply(&InputEvent::Touch {
                contact: 9,
                phase: TouchPhase::Move,
                display_id: None,
                x: 12,
                y: 34,
                pressure: 5,
            })
            .unwrap();
        assert!(router.snapshot().contacts.is_empty());
        assert!(!router.snapshot().has_button(PointerButton::Left));
        assert_eq!(
            router.snapshot().pointer,
            Some(AbsolutePoint { x: 12, y: 34 })
        );
    }

    #[test]
    fn pen_contact_holds_left_button() {
        let mut router = router(MouseProtocol::Absolute);
        let pen = |phase| InputEvent::Pen {
            phase,
            display_id: None,
            x: 5,
            y: 6,
            pressure: 10,
            tilt_x: 0,
            tilt_y: 0,
        };
        router.apply(&pen(TouchPhase::Down)).unwrap();
        assert!(router.snapshot().pen_down);
        assert!(router.snapshot().has_button(PointerButton::Left));
        router.apply(&pen(TouchPhase::Up)).unwrap();
        assert!(!router.snapshot().pen_down);
        assert!(!router.snapshot().has_button(PointerButton::Left));
    }

    #[test]
    fn pointer_mode_mismatch_is_rejected() {
        let mut absolute = router(MouseProtocol::Absolute);
        let error = absolute
            .apply(&InputEvent::PointerRelative { dx: 1, dy: 1 })
            .unwrap_err();
        assert_eq!(error.code, crate::error::codes::UNSUPPORTED);

        let mut relative = router(MouseProtocol::Relative);
        let error = relative
            .apply(&InputEvent::PointerAbsolute { display_id: None, x: 1, y: 1 })
            .unwrap_err();
        assert_eq!(error.code, crate::error::codes::UNSUPPORTED);
    }

    #[test]
    fn clipboard_is_explicitly_unsupported() {
        let mut router = router(MouseProtocol::Absolute);
        let error = router
            .apply(&InputEvent::Clipboard { text: "secret".into() })
            .unwrap_err();
        assert_eq!(error.code, crate::error::codes::UNSUPPORTED);
        assert_eq!(error.details["event"], "clipboard");
    }

    #[test]
    fn release_all_is_idempotent_and_returns_neutral_reports() {
        let mut router = router(MouseProtocol::Absolute);
        router.apply(&key(0x04, true)).unwrap();
        router
            .apply(&InputEvent::Button { button: PointerButton::Left, down: true })
            .unwrap();
        assert!(!router.snapshot().is_empty());

        let first = router.release_all();
        assert_eq!(first.len(), 2);
        assert!(router.snapshot().is_empty());
        let second = router.release_all();
        assert_eq!(second.len(), 2);
        assert!(router.snapshot().is_empty());
    }
}
