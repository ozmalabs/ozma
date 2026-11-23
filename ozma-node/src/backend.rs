// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Adapter from the real session manager to the API backend contract.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ozma_api::NodeBackend;
use ozma_auth::ConsentPolicy;
use ozma_protocol::{
    ApiError, CheckStatus, DiagnosticCheck, DiagnosticReport, DisplayId, DisplayTopology, EndReason,
    NodeCapabilities, PermissionSet, RuntimeMetrics, Session, SessionEvent, SessionId, SessionSpec,
    SessionState, VirtualDisplaySpec,
};
use ozma_session::SessionManager;
use tokio::sync::broadcast;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// The node's production backend: the session manager plus display state and
/// capability/diagnostic reporting.
pub struct SessionBackend {
    manager: SessionManager,
    capabilities: NodeCapabilities,
    displays: Mutex<HashMap<DisplayId, VirtualDisplaySpec>>,
    topology: Mutex<DisplayTopology>,
}

impl SessionBackend {
    pub fn new(capabilities: NodeCapabilities, consent_policy: ConsentPolicy) -> Self {
        Self {
            manager: SessionManager::new(consent_policy),
            capabilities,
            displays: Mutex::new(HashMap::new()),
            topology: Mutex::new(DisplayTopology::default()),
        }
    }

    pub fn manager(&self) -> &SessionManager {
        &self.manager
    }

    /// Expire idle sessions and consent requests. The supervisor should call
    /// this periodically.
    pub fn expire(&self) -> usize {
        self.manager.expire(now())
    }
}

impl NodeBackend for SessionBackend {
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities.clone()
    }

    fn diagnostics(&self) -> DiagnosticReport {
        let mut checks = Vec::new();
        checks.push(DiagnosticCheck::ok("protocol", "Protocol 1.0"));
        if self.capabilities.capture.is_empty() {
            checks.push(
                DiagnosticCheck::warn("capture", "No capture source is available")
                    .with_remediation("Attach an HDMI capture device or enable the video capability."),
            );
        } else {
            checks.push(DiagnosticCheck::ok(
                "capture",
                format!("{} capture source(s)", self.capabilities.capture.len()),
            ));
        }
        if self.capabilities.encode.is_empty() {
            checks.push(
                DiagnosticCheck::fail("encoder", "No encoder is available")
                    .with_remediation("Install a hardware encoder or enable the software fallback."),
            );
        } else {
            checks.push(DiagnosticCheck::ok(
                "encoder",
                format!("{} encoder(s)", self.capabilities.encode.len()),
            ));
        }
        if self.capabilities.display.supported {
            checks.push(DiagnosticCheck::ok(
                "display",
                "Virtual displays are supported on this platform",
            ));
        } else {
            checks.push(DiagnosticCheck::skip(
                "display",
                "Virtual displays are not supported; use a labelled mirror",
            ));
        }
        let sessions = self.manager.list();
        let active = sessions
            .iter()
            .filter(|session| session.state.is_active())
            .count();
        checks.push(DiagnosticCheck {
            id: "sessions".to_owned(),
            status: CheckStatus::Ok,
            message: format!("{} session(s), {active} active", sessions.len()),
            remediation: None,
        });
        DiagnosticReport::new(checks, now())
    }

    fn metrics(&self) -> RuntimeMetrics {
        let sessions = self.manager.list();
        RuntimeMetrics {
            sessions: sessions.len() as u32,
            active_streams: sessions
                .iter()
                .filter(|session| {
                    matches!(session.state, SessionState::Streaming | SessionState::Degraded)
                })
                .count() as u32,
            ..RuntimeMetrics::default()
        }
    }

    fn list_sessions(&self) -> Vec<Session> {
        self.manager.list()
    }

    fn get_session(&self, id: &SessionId) -> Option<Session> {
        self.manager.get(id).ok()
    }

    fn create_session(&self, spec: SessionSpec) -> Result<Session, ApiError> {
        // The HTTP layer will narrow this to a controller-issued grant in the
        // authentication pass. The local API currently runs on a trusted
        // transport (loopback or the authenticated node API).
        let granted = PermissionSet::all();
        self.manager.create(spec, granted, now())
    }

    fn start_session(&self, id: &SessionId) -> Result<Session, ApiError> {
        let session = self.manager.start(id)?;
        // The fake/local pipeline starts immediately. A transport-backed
        // session calls `mark_streaming` when the first media path is ready.
        match self.manager.mark_streaming(id) {
            Ok(session) => Ok(session),
            Err(_) => Ok(session),
        }
    }

    fn stop_session(&self, id: &SessionId, reason: EndReason) -> Result<Session, ApiError> {
        self.manager.stop(id, reason)
    }

    fn approve_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        self.manager.approve_consent(id, by)
    }

    fn deny_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        self.manager.deny_consent(id, by)
    }

    fn list_displays(&self) -> Vec<VirtualDisplaySpec> {
        let displays = self.displays.lock().expect("display lock");
        displays.values().cloned().collect()
    }

    fn create_display(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplaySpec, ApiError> {
        spec.validate()?;
        let mut displays = self.displays.lock().expect("display lock");
        if displays.contains_key(&spec.id) {
            return Err(ApiError::new(
                ozma_protocol::error::codes::CONFLICT,
                "A display with that id already exists",
            ));
        }
        displays.insert(spec.id.clone(), spec.clone());
        Ok(spec)
    }

    fn destroy_display(&self, id: &DisplayId) -> Result<(), ApiError> {
        let mut displays = self.displays.lock().expect("display lock");
        displays.remove(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Display not found")
        })?;
        Ok(())
    }

    fn topology(&self) -> DisplayTopology {
        self.topology.lock().expect("topology lock").clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.manager.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_auth::MachineClass;
    use ozma_protocol::{ClientId, ConsentRequirement, InputSpec, PointerMode, PrivacyPolicy};

    fn backend() -> SessionBackend {
        SessionBackend::new(
            NodeCapabilities::fake("node"),
            ConsentPolicy::for_machine(MachineClass::Server),
        )
    }

    fn spec() -> SessionSpec {
        SessionSpec {
            client_id: ClientId::new("client"),
            principal: "alice".into(),
            tracks: Vec::new(),
            audio: Default::default(),
            input: InputSpec {
                keyboard: true,
                pointer: PointerMode::Absolute,
                ..Default::default()
            },
            privacy: PrivacyPolicy {
                consent: ConsentRequirement::Never,
                ..Default::default()
            },
            transport: Default::default(),
            ttl_seconds: 60,
        }
    }

    #[test]
    fn create_start_stop_roundtrip() {
        let backend = backend();
        let session = backend.create_session(spec()).unwrap();
        let session = backend.start_session(&session.id).unwrap();
        assert_eq!(session.state, SessionState::Streaming);
        let stopped = backend
            .stop_session(&session.id, EndReason::AdminTerminated)
            .unwrap();
        assert_eq!(stopped.state, SessionState::Ended);
    }

    #[test]
    fn diagnostics_are_structured() {
        let report = backend().diagnostics();
        assert!(report.ok);
        assert!(report.checks.iter().any(|check| check.id == "encoder"));
    }
}
