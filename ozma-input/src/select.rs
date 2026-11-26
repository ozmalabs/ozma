// SPDX-License-Identifier: AGPL-3.0-only
//! Sink-selection vocabulary.
//!
//! Nodes pick an input backend at startup. The plan's default chain is
//! `uinput → qmp → hid-gadget → recording`, but the order is configurable and
//! every constructor is supplied by the caller. That keeps selection testable
//! without `/dev/uinput`, a QMP socket, or a USB gadget.
//!
//! All four kinds now have a concrete implementation: [`crate::UinputSink`],
//! [`crate::QmpSink`], [`crate::HidGadgetSink`], and [`crate::RecordingSink`].
//! [`SinkKind::Qmp`] is no longer vocabulary-only. QMP still needs an async
//! [`crate::QmpSink::spawn`] (or a caller-built channel) before a candidate can
//! be constructed, so selection deliberately keeps taking caller-supplied
//! constructors and never hides a connection failure.
//!
//! ```
//! use std::sync::Arc;
//! use ozma_input::{
//!     select_sink, FakeInputSink, InputSink, SinkCandidate, SinkKind, SharedSink,
//! };
//!
//! let candidate = SinkCandidate::new(SinkKind::HidGadget, || {
//!     Ok(Arc::new(FakeInputSink::new()) as SharedSink)
//! });
//! let selected = select_sink(&[candidate]).unwrap();
//! assert_eq!(selected.kind, SinkKind::HidGadget);
//! selected.sink.release_all().unwrap();
//! ```

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use ozma_protocol::ApiError;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::codes;
use crate::fake::FakeInputSink;
use crate::sink::{RecordingSink, SharedSink};

/// A stable name for an input backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SinkKind {
    /// Linux evdev/uinput virtual keyboard and pointer.
    Uinput,
    /// Windows desktop `SendInput` keyboard and pointer.
    SendInput,
    /// QEMU QMP `input-send-event` injection.
    Qmp,
    /// ConfigFS USB HID gadget (`/dev/hidg0`, `/dev/hidg1`).
    HidGadget,
    /// A sink that records events without injecting them (CI and diagnostics).
    Recording,
}

impl SinkKind {
    /// Every sink kind known to this crate.
    pub const ALL: [SinkKind; 5] = [
        SinkKind::Uinput,
        SinkKind::SendInput,
        SinkKind::Qmp,
        SinkKind::HidGadget,
        SinkKind::Recording,
    ];

    /// Stable configuration/API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            SinkKind::Uinput => "uinput",
            SinkKind::SendInput => "send-input",
            SinkKind::Qmp => "qmp",
            SinkKind::HidGadget => "hid-gadget",
            SinkKind::Recording => "recording",
        }
    }
}

impl fmt::Display for SinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a sink name is not recognized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownSinkKind(pub String);

impl fmt::Display for UnknownSinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown input sink kind {:?}; expected uinput, qmp, hid-gadget, or recording",
            self.0
        )
    }
}

impl std::error::Error for UnknownSinkKind {}

impl FromStr for SinkKind {
    type Err = UnknownSinkKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "uinput" => Ok(SinkKind::Uinput),
            "send-input" | "send_input" | "sendinput" => Ok(SinkKind::SendInput),
            "qmp" => Ok(SinkKind::Qmp),
            // Accept the snake-case spelling used by some config files.
            "hid-gadget" | "hid_gadget" | "gadget" => Ok(SinkKind::HidGadget),
            "recording" | "record" => Ok(SinkKind::Recording),
            other => Err(UnknownSinkKind(other.to_owned())),
        }
    }
}

/// An ordered list of sink kinds to try at startup.
///
/// [`SinkPreference::default_chain`] is the plan's
/// `uinput → qmp → hid-gadget → recording` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkPreference {
    order: Vec<SinkKind>,
}

impl SinkPreference {
    /// The plan's default fallback chain.
    pub fn default_chain() -> Self {
        Self {
            order: vec![
                SinkKind::Uinput,
                SinkKind::Qmp,
                SinkKind::HidGadget,
                SinkKind::Recording,
            ],
        }
    }

    /// Build a custom order. Duplicate kinds are kept as given.
    pub fn new(order: impl IntoIterator<Item = SinkKind>) -> Self {
        Self {
            order: order.into_iter().collect(),
        }
    }

    /// The configured order.
    pub fn kinds(&self) -> &[SinkKind] {
        &self.order
    }

    /// True when no candidates are configured.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Build candidates in preference order.
    ///
    /// The caller supplies a constructor for every real backend. Recording is
    /// always the built-in [`RecordingSink`] over [`FakeInputSink`], so it
    /// cannot fail; a default chain therefore always terminates.
    pub fn candidates<F>(&self, build: F) -> Vec<SinkCandidate>
    where
        F: Fn(SinkKind) -> Result<SharedSink, ApiError> + Send + Sync + 'static,
    {
        let build = Arc::new(build);
        self.order
            .iter()
            .copied()
            .map(|kind| {
                if kind == SinkKind::Recording {
                    SinkCandidate::recording()
                } else {
                    let build = Arc::clone(&build);
                    SinkCandidate::new(kind, move || build(kind))
                }
            })
            .collect()
    }

    /// Try this preference with caller-supplied constructors.
    pub fn select<F>(&self, build: F) -> Result<SelectedSink, SinkSelectionError>
    where
        F: Fn(SinkKind) -> Result<SharedSink, ApiError> + Send + Sync + 'static,
    {
        select_sink(&self.candidates(build))
    }
}

impl Default for SinkPreference {
    fn default() -> Self {
        Self::default_chain()
    }
}

/// One candidate sink and its constructor.
#[derive(Clone)]
pub struct SinkCandidate {
    kind: SinkKind,
    build: Arc<dyn Fn() -> Result<SharedSink, ApiError> + Send + Sync>,
}

impl SinkCandidate {
    /// Construct a candidate for `kind` with a fallible constructor.
    pub fn new<F>(kind: SinkKind, build: F) -> Self
    where
        F: Fn() -> Result<SharedSink, ApiError> + Send + Sync + 'static,
    {
        Self {
            kind,
            build: Arc::new(build),
        }
    }

    /// A recording sink over [`FakeInputSink`]. Always succeeds.
    pub fn recording() -> Self {
        Self::new(SinkKind::Recording, || {
            Ok(Arc::new(RecordingSink::new(FakeInputSink::new())) as SharedSink)
        })
    }

    pub fn kind(&self) -> SinkKind {
        self.kind
    }

    /// Run the constructor.
    pub fn build(&self) -> Result<SharedSink, ApiError> {
        (self.build)()
    }
}

impl fmt::Debug for SinkCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SinkCandidate")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// The first sink that opened successfully.
#[derive(Clone)]
pub struct SelectedSink {
    pub kind: SinkKind,
    pub sink: SharedSink,
}

impl SelectedSink {
    /// True when the selected sink records rather than injects.
    pub fn is_recording(&self) -> bool {
        self.kind == SinkKind::Recording
    }
}

impl fmt::Debug for SelectedSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectedSink")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// One failed candidate in a [`SinkSelectionError`].
#[derive(Clone, Debug)]
pub struct SinkAttempt {
    pub kind: SinkKind,
    pub error: ApiError,
}

/// Every candidate failed. Carries the individual errors for diagnostics.
#[derive(Clone, Debug)]
pub struct SinkSelectionError {
    attempts: Vec<SinkAttempt>,
}

impl SinkSelectionError {
    pub fn attempts(&self) -> &[SinkAttempt] {
        &self.attempts
    }

    pub fn tried(&self) -> Vec<SinkKind> {
        self.attempts.iter().map(|attempt| attempt.kind).collect()
    }

    /// Convert to the node API's stable error shape.
    pub fn into_api_error(self) -> ApiError {
        let attempts: Vec<_> = self
            .attempts
            .iter()
            .map(|attempt| {
                json!({
                    "kind": attempt.kind.as_str(),
                    "code": attempt.error.code,
                    "message": attempt.error.message,
                })
            })
            .collect();

        ApiError::new(
            codes::INPUT_UNAVAILABLE,
            "No configured input sink could be opened",
        )
        .with_details(json!({ "attempts": attempts }))
        .with_remediation(
            "Check that the selected backend has its device or socket \
             (/dev/uinput, the QMP socket, /dev/hidg0 and /dev/hidg1) and \
             permissions, or configure the recording sink for CI.",
        )
    }
}

impl fmt::Display for SinkSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.attempts.is_empty() {
            return f.write_str("no input sink candidates were configured");
        }
        write!(f, "no input sink could be opened: ")?;
        for (index, attempt) in self.attempts.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{}: {}", attempt.kind, attempt.error.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for SinkSelectionError {}

/// Try candidates in order and return the first successful sink.
pub fn select_sink(candidates: &[SinkCandidate]) -> Result<SelectedSink, SinkSelectionError> {
    let mut attempts = Vec::new();
    for candidate in candidates {
        match candidate.build() {
            Ok(sink) => {
                return Ok(SelectedSink {
                    kind: candidate.kind,
                    sink,
                })
            }
            Err(error) => attempts.push(SinkAttempt {
                kind: candidate.kind,
                error,
            }),
        }
    }
    Err(SinkSelectionError { attempts })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ozma_protocol::input::{InputEvent, KeyModifiers};

    use super::*;

    fn fake() -> Result<SharedSink, ApiError> {
        Ok(Arc::new(FakeInputSink::new()) as SharedSink)
    }

    fn unavailable(kind: SinkKind) -> ApiError {
        ApiError::new(
            codes::INPUT_UNAVAILABLE,
            format!("{kind} backend is unavailable in this test"),
        )
    }

    fn key(code: u32) -> InputEvent {
        InputEvent::Key {
            code,
            down: true,
            modifiers: KeyModifiers::default(),
        }
    }

    #[test]
    fn sink_kind_names_are_stable_and_round_trip() {
        assert_eq!(SinkKind::Uinput.as_str(), "uinput");
        assert_eq!(SinkKind::Qmp.as_str(), "qmp");
        assert_eq!(SinkKind::HidGadget.as_str(), "hid-gadget");
        assert_eq!(SinkKind::Recording.as_str(), "recording");

        for kind in SinkKind::ALL {
            assert_eq!(kind.as_str().parse::<SinkKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), kind.as_str());
        }

        assert_eq!(
            "hid_gadget".parse::<SinkKind>().unwrap(),
            SinkKind::HidGadget
        );
        assert_eq!("gadget".parse::<SinkKind>().unwrap(), SinkKind::HidGadget);
        assert_eq!("record".parse::<SinkKind>().unwrap(), SinkKind::Recording);

        let error = "bogus".parse::<SinkKind>().unwrap_err();
        assert_eq!(error.0, "bogus");
        assert!(error.to_string().contains("bogus"));
    }

    #[test]
    fn preferred_order_wins() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let candidates = vec![
            SinkCandidate::new(SinkKind::Uinput, {
                let order = Arc::clone(&order);
                move || {
                    order.lock().unwrap().push(SinkKind::Uinput);
                    fake()
                }
            }),
            SinkCandidate::new(SinkKind::HidGadget, {
                let order = Arc::clone(&order);
                move || {
                    order.lock().unwrap().push(SinkKind::HidGadget);
                    fake()
                }
            }),
        ];

        let selected = select_sink(&candidates).unwrap();
        assert_eq!(selected.kind, SinkKind::Uinput);
        assert_eq!(*order.lock().unwrap(), vec![SinkKind::Uinput]);
    }

    #[test]
    fn falls_back_to_the_next_candidate_on_error() {
        let tried = Arc::new(Mutex::new(Vec::new()));
        let candidates = vec![
            SinkCandidate::new(SinkKind::Uinput, {
                let tried = Arc::clone(&tried);
                move || {
                    tried.lock().unwrap().push(SinkKind::Uinput);
                    Err(unavailable(SinkKind::Uinput))
                }
            }),
            SinkCandidate::new(SinkKind::HidGadget, {
                let tried = Arc::clone(&tried);
                move || {
                    tried.lock().unwrap().push(SinkKind::HidGadget);
                    fake()
                }
            }),
        ];

        let selected = select_sink(&candidates).unwrap();
        assert_eq!(selected.kind, SinkKind::HidGadget);
        assert_eq!(
            *tried.lock().unwrap(),
            vec![SinkKind::Uinput, SinkKind::HidGadget]
        );
    }

    #[test]
    fn all_failed_returns_a_typed_error() {
        let candidates = vec![
            SinkCandidate::new(SinkKind::Uinput, || Err(unavailable(SinkKind::Uinput))),
            SinkCandidate::new(SinkKind::Qmp, || Err(unavailable(SinkKind::Qmp))),
        ];

        let error = select_sink(&candidates).unwrap_err();
        assert_eq!(error.tried(), vec![SinkKind::Uinput, SinkKind::Qmp]);
        assert_eq!(error.attempts().len(), 2);

        let api = error.clone().into_api_error();
        assert_eq!(api.code, codes::INPUT_UNAVAILABLE);
        assert_eq!(api.details["attempts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_candidate_list_is_an_error() {
        let error = select_sink(&[]).unwrap_err();
        assert!(error.attempts().is_empty());
        assert!(error.to_string().contains("no input sink candidates"));
    }

    #[test]
    fn recording_always_succeeds() {
        let selected = select_sink(&[SinkCandidate::recording()]).unwrap();
        assert_eq!(selected.kind, SinkKind::Recording);
        assert!(selected.is_recording());

        selected.sink.submit(&key(0x04)).unwrap();
        selected.sink.release_all().unwrap();
    }

    #[test]
    fn default_chain_order_and_recording_fallback() {
        let preference = SinkPreference::default_chain();
        assert_eq!(
            preference.kinds(),
            &[
                SinkKind::Uinput,
                SinkKind::Qmp,
                SinkKind::HidGadget,
                SinkKind::Recording,
            ]
        );

        let selected = preference.select(|kind| Err(unavailable(kind))).unwrap();
        assert_eq!(selected.kind, SinkKind::Recording);
    }

    #[test]
    fn default_chain_tries_every_hardware_kind_before_recording() {
        let tried = Arc::new(Mutex::new(Vec::new()));
        let selected = SinkPreference::default_chain()
            .select({
                let tried = Arc::clone(&tried);
                move |kind| {
                    tried.lock().unwrap().push(kind);
                    Err(unavailable(kind))
                }
            })
            .unwrap();

        assert_eq!(selected.kind, SinkKind::Recording);
        assert!(selected.is_recording());
        assert_eq!(
            *tried.lock().unwrap(),
            vec![SinkKind::Uinput, SinkKind::Qmp, SinkKind::HidGadget]
        );
    }

    #[test]
    fn default_chain_accepts_qmp_after_uinput_fails() {
        let selected = SinkPreference::default_chain()
            .select(|kind| match kind {
                SinkKind::Uinput => Err(unavailable(kind)),
                SinkKind::Qmp => fake(),
                _ => Err(unavailable(kind)),
            })
            .unwrap();

        assert_eq!(selected.kind, SinkKind::Qmp);
        assert!(!selected.is_recording());
    }

    #[test]
    fn custom_preference_uses_the_supplied_constructors() {
        let preference = SinkPreference::new([SinkKind::HidGadget, SinkKind::Recording]);
        let tried = Arc::new(Mutex::new(Vec::new()));
        let selected = preference
            .select({
                let tried = Arc::clone(&tried);
                move |kind| {
                    tried.lock().unwrap().push(kind);
                    if kind == SinkKind::HidGadget {
                        fake()
                    } else {
                        Err(unavailable(kind))
                    }
                }
            })
            .unwrap();

        assert_eq!(selected.kind, SinkKind::HidGadget);
        assert_eq!(*tried.lock().unwrap(), vec![SinkKind::HidGadget]);
    }
}
