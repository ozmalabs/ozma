// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Input-sink selection for the node profiles.
//!
//! The plan's VM/desktop chain is `uinput → qmp → hid-gadget → recording`.
//! Hardware keeps its existing `hid-gadget → recording` chain unless
//! `--input-provider` explicitly asks for something else. The chain is resolved
//! by [`crate::config::VmProviderConfig`]; this module only tries the candidates
//! in order and keeps the selected QMP task alive.
//!
//! Constructors are behind [`InputSinkFactory`] so the fallback logic can be
//! tested without `/dev/uinput`, a QMP socket, or a USB gadget.

use std::sync::Arc;

use async_trait::async_trait;
use ozma_input::{
    FakeInputSink, HidGadgetSink, InputSink, QmpSink, QmpSinkHandle, RecordingSink, SharedSink,
    SinkKind, UinputSink,
};
use ozma_qmp::QmpConfig;
use tracing::{debug, info, warn};

/// A sink that opened successfully, plus the QMP background task (if any).
///
/// The node must hold `handle` for as long as the sink is in use; dropping it
/// aborts the QMP sender task.
pub struct BuiltInput {
    pub sink: SharedSink,
    pub handle: Option<QmpSinkHandle>,
}

impl BuiltInput {
    fn recorded<S>(sink: S) -> Self
    where
        S: InputSink + 'static,
    {
        Self {
            sink: Arc::new(RecordingSink::new(sink)),
            handle: None,
        }
    }

    fn recording() -> Self {
        Self::recorded(FakeInputSink::new())
    }
}

/// Construct one input sink candidate.
#[async_trait]
pub trait InputSinkFactory: Send + Sync {
    /// Build `kind`, or return a human-readable reason it is unavailable.
    async fn build(&self, kind: SinkKind) -> Result<BuiltInput, String>;
}

/// Production factory backed by the real sinks.
pub struct SystemInputFactory {
    qmp_input: Option<QmpConfig>,
}

impl SystemInputFactory {
    /// Build a factory. `qmp_input` is `Some` only when
    /// `--qmp-input-socket` is configured.
    pub fn new(qmp_input: Option<QmpConfig>) -> Self {
        Self { qmp_input }
    }
}

#[async_trait]
impl InputSinkFactory for SystemInputFactory {
    async fn build(&self, kind: SinkKind) -> Result<BuiltInput, String> {
        match kind {
            SinkKind::Uinput => UinputSink::open()
                .map(BuiltInput::recorded)
                .map_err(|error| format!("uinput unavailable: {error}")),
            SinkKind::Qmp => {
                let config = self.qmp_input.clone().ok_or_else(|| {
                    "qmp unavailable: no --qmp-input-socket configured".to_owned()
                })?;
                let (sink, handle) = QmpSink::spawn(config)
                    .await
                    .map_err(|error| format!("qmp unavailable: {error}"))?;
                Ok(BuiltInput {
                    sink: Arc::new(RecordingSink::new(sink)),
                    handle: Some(handle),
                })
            }
            SinkKind::HidGadget => HidGadgetSink::open()
                .map(BuiltInput::recorded)
                .map_err(|error| format!("hid-gadget unavailable: {error}")),
            SinkKind::Recording => Ok(BuiltInput::recording()),
        }
    }
}

/// One failed candidate, retained for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputAttempt {
    pub kind: SinkKind,
    pub error: String,
}

/// The selected sink and the candidates that failed before it.
pub struct SelectedInput {
    pub sink: SharedSink,
    pub kind: SinkKind,
    pub handle: Option<QmpSinkHandle>,
    pub attempts: Vec<InputAttempt>,
}

/// Try each candidate in order and return the first that opens.
///
/// A failed candidate is logged and skipped. If the whole chain fails (which a
/// chain ending in `recording` should not), the node still starts with a
/// recording sink so the API and UDP listener remain available; the returned
/// `attempts` make that visible.
pub async fn select_input(factory: &dyn InputSinkFactory, chain: &[SinkKind]) -> SelectedInput {
    let mut attempts: Vec<InputAttempt> = Vec::new();

    for &kind in chain {
        match factory.build(kind).await {
            Ok(built) => {
                if !attempts.is_empty() {
                    let failed: Vec<SinkKind> =
                        attempts.iter().map(|attempt| attempt.kind).collect();
                    info!(
                        selected = %kind,
                        failed = ?failed,
                        "Input sink selected after fallback"
                    );
                } else {
                    debug!(selected = %kind, "Input sink selected");
                }
                return SelectedInput {
                    sink: built.sink,
                    kind,
                    handle: built.handle,
                    attempts,
                };
            }
            Err(error) => {
                warn!(kind = %kind, error = %error, "Input sink unavailable; trying next candidate");
                attempts.push(InputAttempt { kind, error });
            }
        }
    }

    let tried: Vec<SinkKind> = attempts.iter().map(|attempt| attempt.kind).collect();
    warn!(
        tried = ?tried,
        "No configured input sink could be opened; recording only \
         (input will be validated but NOT injected)"
    );
    SelectedInput {
        sink: BuiltInput::recording().sink,
        kind: SinkKind::Recording,
        handle: None,
        attempts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use ozma_input::InputSink;

    struct FakeFactory {
        available: Vec<SinkKind>,
        seen: Mutex<Vec<SinkKind>>,
    }

    impl FakeFactory {
        fn new(available: Vec<SinkKind>) -> Self {
            Self {
                available,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl InputSinkFactory for FakeFactory {
        async fn build(&self, kind: SinkKind) -> Result<BuiltInput, String> {
            self.seen.lock().unwrap().push(kind);
            if self.available.contains(&kind) {
                Ok(BuiltInput {
                    sink: Arc::new(RecordingSink::new(FakeInputSink::new())),
                    handle: None,
                })
            } else {
                Err(format!("{kind} not available in test"))
            }
        }
    }

    #[tokio::test]
    async fn selects_the_first_available_candidate() {
        let chain = [
            SinkKind::Uinput,
            SinkKind::Qmp,
            SinkKind::HidGadget,
            SinkKind::Recording,
        ];
        let factory = FakeFactory::new(vec![SinkKind::HidGadget, SinkKind::Recording]);

        let selected = select_input(&factory, &chain).await;
        assert_eq!(selected.kind, SinkKind::HidGadget);
        assert_eq!(
            selected
                .attempts
                .iter()
                .map(|attempt| attempt.kind)
                .collect::<Vec<_>>(),
            vec![SinkKind::Uinput, SinkKind::Qmp]
        );
        assert_eq!(
            *factory.seen.lock().unwrap(),
            vec![SinkKind::Uinput, SinkKind::Qmp, SinkKind::HidGadget]
        );
        assert!(selected.handle.is_none());
        selected.sink.release_all().unwrap();
    }

    #[tokio::test]
    async fn skips_injection_when_only_recording_is_available() {
        let chain = [SinkKind::Uinput, SinkKind::Recording];
        let factory = FakeFactory::new(vec![SinkKind::Recording]);

        let selected = select_input(&factory, &chain).await;
        assert_eq!(selected.kind, SinkKind::Recording);
        assert_eq!(selected.attempts.len(), 1);
    }

    #[tokio::test]
    async fn empty_or_exhausted_chain_still_records() {
        let factory = FakeFactory::new(vec![]);

        let selected = select_input(&factory, &[]).await;
        assert_eq!(selected.kind, SinkKind::Recording);
        assert!(selected.attempts.is_empty());

        let selected = select_input(&factory, &[SinkKind::Uinput]).await;
        assert_eq!(selected.kind, SinkKind::Recording);
        assert_eq!(selected.attempts.len(), 1);
    }
}
