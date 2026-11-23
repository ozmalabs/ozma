// SPDX-License-Identifier: AGPL-3.0-only
//! In-memory sink used by tests and the node's local harness.

use std::sync::{Mutex, MutexGuard};

use ozma_protocol::input::InputEvent;
use ozma_protocol::ApiError;

use crate::gadget::GadgetOptions;
use crate::report::MouseProtocol;
use crate::sink::{InputSink, SubmitReport};
use crate::state::{HeldInput, Router};

struct FakeState {
    router: Router,
    events: Vec<InputEvent>,
}

/// An [`InputSink`] that records events and tracks held state without I/O.
///
/// It runs the same router as [`crate::HidGadgetSink`], so validation,
/// duplicate handling, the six-key limit, pointer-mode rules, and the
/// clipboard policy behave identically.
pub struct FakeInputSink {
    inner: Mutex<FakeState>,
}

impl FakeInputSink {
    /// Create a fake sink with the default absolute mouse protocol.
    pub fn new() -> Self {
        Self::with_options(GadgetOptions::default())
    }

    /// Create a fake sink with explicit routing options.
    pub fn with_options(options: GadgetOptions) -> Self {
        Self {
            inner: Mutex::new(FakeState {
                router: Router::new(options.mapping, options.mouse_protocol),
                events: Vec::new(),
            }),
        }
    }

    /// Every submitted event, in order, including rejected ones.
    pub fn events(&self) -> Vec<InputEvent> {
        self.lock().events.clone()
    }

    /// Drain the recorded events.
    pub fn take_events(&self) -> Vec<InputEvent> {
        std::mem::take(&mut self.lock().events)
    }

    pub fn clear(&self) {
        self.lock().events.clear();
    }

    /// Number of submitted events (including rejects).
    pub fn len(&self) -> usize {
        self.lock().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().events.is_empty()
    }

    /// The mouse protocol this sink routes with.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.lock().router.protocol()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        crate::sink::lock(&self.inner)
    }
}

impl Default for FakeInputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink for FakeInputSink {
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
        let mut state = self.lock();
        state.events.push(event.clone());

        let mut next = state.router.clone();
        let reports = next.apply(event)?;
        state.router = next;

        Ok(SubmitReport {
            reports_written: reports.len(),
            state_changed: !reports.is_empty(),
        })
    }

    fn release_all(&self) -> Result<(), ApiError> {
        let mut state = self.lock();
        state.events.push(InputEvent::ReleaseAll);
        state.router.release_all();
        Ok(())
    }

    fn held(&self) -> HeldInput {
        self.lock().router.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::input::{KeyModifiers, PointerButton};

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: KeyModifiers::default(),
        }
    }

    #[test]
    fn records_events_and_tracks_held_state() {
        let sink = FakeInputSink::new();
        let first = sink.submit_report(&key(0x04, true)).unwrap();
        assert_eq!(first.reports_written, 1);
        assert!(first.state_changed);

        let duplicate = sink.submit_report(&key(0x04, true)).unwrap();
        assert_eq!(duplicate.reports_written, 0);
        assert!(duplicate.duplicate());

        sink.submit(&InputEvent::Button {
            button: PointerButton::Right,
            down: true,
        })
        .unwrap();
        let held = sink.held();
        assert!(held.has_key(0x04));
        assert!(held.has_button(PointerButton::Right));
        assert_eq!(sink.len(), 3);

        sink.release_all().unwrap();
        assert!(sink.held().is_empty());
        assert_eq!(sink.events().last(), Some(&InputEvent::ReleaseAll));
    }

    #[test]
    fn rejected_events_are_recorded_but_do_not_change_state() {
        let sink = FakeInputSink::new();
        let error = sink
            .submit(&InputEvent::Clipboard { text: "x".into() })
            .unwrap_err();
        assert_eq!(error.code, crate::codes::UNSUPPORTED);
        assert_eq!(sink.len(), 1);
        assert!(sink.held().is_empty());
    }

    #[test]
    fn take_events_drains_the_log() {
        let sink = FakeInputSink::new();
        sink.submit(&key(0x04, true)).unwrap();
        assert_eq!(sink.take_events().len(), 1);
        assert!(sink.is_empty());
    }
}
