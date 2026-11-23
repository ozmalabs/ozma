// SPDX-License-Identifier: AGPL-3.0-only
//! The [`InputSink`] contract and recording/shared wrappers.

use std::sync::{Arc, Mutex, MutexGuard};

use ozma_protocol::input::InputEvent;
use ozma_protocol::ApiError;
use serde::{Deserialize, Serialize};

use crate::state::HeldInput;

/// Result of accepting one event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReport {
    /// Number of HID reports written (or, for a fake sink, that would have
    /// been written). Zero means the event was a no-op such as a duplicate key
    /// press.
    pub reports_written: usize,
    /// Whether the held-input state or the last report changed.
    pub state_changed: bool,
}

impl SubmitReport {
    /// True when the event did not change any held state.
    pub fn duplicate(&self) -> bool {
        !self.state_changed
    }

    /// A no-op report.
    pub const fn noop() -> Self {
        Self { reports_written: 0, state_changed: false }
    }
}

/// A destination for normalized input events.
///
/// Implementations must be `Send + Sync`; the transport layer shares one sink
/// across control tasks. `release_all` is expected to be safe to call on every
/// disconnect, focus loss, and DPMS transition, including when nothing is held.
pub trait InputSink: Send + Sync {
    /// Submit one event and return a detailed accounting of what was written.
    ///
    /// Errors are stable [`ApiError`]s. A rejected event leaves held state
    /// unchanged.
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError>;

    /// Submit one event, discarding the accounting detail.
    fn submit(&self, event: &InputEvent) -> Result<(), ApiError> {
        self.submit_report(event).map(|_| ())
    }

    /// Release every held key, modifier, button, touch contact, and pen
    /// contact. Implementations write neutral reports and are idempotent.
    fn release_all(&self) -> Result<(), ApiError>;

    /// Snapshot of held input for diagnostics and tests.
    fn held(&self) -> HeldInput;
}

impl<T: InputSink + ?Sized> InputSink for Arc<T> {
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
        (**self).submit_report(event)
    }

    fn release_all(&self) -> Result<(), ApiError> {
        (**self).release_all()
    }

    fn held(&self) -> HeldInput {
        (**self).held()
    }
}

/// A sink shared by the node, transport, and session layers.
pub type SharedSink = Arc<dyn InputSink>;

/// Wraps a sink and records every event submitted to it.
///
/// Recording happens before delegation, so rejected events (clipboard, a mode
/// mismatch, a seventh key) are visible in the log as well.
pub struct RecordingSink<S: InputSink> {
    inner: S,
    events: Mutex<Vec<InputEvent>>,
}

impl<S: InputSink> RecordingSink<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, events: Mutex::new(Vec::new()) }
    }

    /// Every submitted event, in order.
    pub fn events(&self) -> Vec<InputEvent> {
        lock(&self.events).clone()
    }

    /// Drain the recorded events.
    pub fn take_events(&self) -> Vec<InputEvent> {
        std::mem::take(&mut *lock(&self.events))
    }

    pub fn clear(&self) {
        lock(&self.events).clear();
    }

    /// Borrow the wrapped sink.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Consume the wrapper and return the sink.
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: InputSink> InputSink for RecordingSink<S> {
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
        lock(&self.events).push(event.clone());
        self.inner.submit_report(event)
    }

    fn release_all(&self) -> Result<(), ApiError> {
        lock(&self.events).push(InputEvent::ReleaseAll);
        self.inner.release_all()
    }

    fn held(&self) -> HeldInput {
        self.inner.held()
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeInputSink;

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: Default::default(),
        }
    }

    #[test]
    fn recording_sink_records_release_and_rejections() {
        let sink = RecordingSink::new(FakeInputSink::new());
        sink.submit(&key(0x04, true)).unwrap();
        sink.release_all().unwrap();
        let error = sink
            .submit(&InputEvent::Clipboard { text: "nope".into() })
            .unwrap_err();
        assert_eq!(error.code, crate::codes::UNSUPPORTED);

        assert_eq!(
            sink.events(),
            vec![
                key(0x04, true),
                InputEvent::ReleaseAll,
                InputEvent::Clipboard { text: "nope".into() },
            ]
        );
        assert!(sink.held().is_empty());
    }

    #[test]
    fn an_arc_sink_is_usable_through_the_trait() {
        let shared: SharedSink = Arc::new(FakeInputSink::new());
        shared.submit(&key(0x05, true)).unwrap();
        assert!(shared.held().has_key(0x05));
        shared.release_all().unwrap();
        assert!(shared.held().is_empty());
    }
}
