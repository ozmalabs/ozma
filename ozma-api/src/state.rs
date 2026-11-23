// SPDX-License-Identifier: AGPL-3.0-only
//! The backend contract used by the HTTP router, plus an in-memory
//! implementation for tests and local bring-up.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ozma_protocol::{
    transport::TransportInfo, ApiError, DiagnosticReport, DisplayId, DisplayMode, DisplayTopology,
    DpmsState, EndReason, NodeCapabilities, NodeId, RuntimeMetrics, Session, SessionEvent,
    SessionId, SessionOffer, SessionSpec, SessionState, VirtualDisplaySpec,
};
use tokio::sync::broadcast;

/// Everything the HTTP layer needs from a node. Implementations may be the
/// real supervisor or a test double.
pub trait NodeBackend: Send + Sync + 'static {
    fn version(&self) -> &'static str;
    fn capabilities(&self) -> NodeCapabilities;
    fn diagnostics(&self) -> DiagnosticReport;
    fn metrics(&self) -> RuntimeMetrics;

    fn list_sessions(&self) -> Vec<Session>;
    fn get_session(&self, id: &SessionId) -> Option<Session>;
    fn create_session(&self, spec: SessionSpec) -> Result<Session, ApiError>;
    fn start_session(&self, id: &SessionId) -> Result<Session, ApiError>;
    fn stop_session(&self, id: &SessionId, reason: EndReason) -> Result<Session, ApiError>;
    fn approve_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError>;
    fn deny_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError>;
    fn resume_session(&self, id: &SessionId, client_id: &ozma_protocol::ClientId)
        -> Result<Session, ApiError>;

    /// Native transport endpoint, certificate fingerprint, and certificate.
    fn transport_info(&self) -> Option<TransportInfo> {
        None
    }

    /// Build the short-lived transport offer for a newly prepared session.
    /// Backends without a media transport return `unsupported`.
    fn session_offer(&self, _session: &Session) -> Result<SessionOffer, ApiError> {
        Err(ApiError::new(
            ozma_protocol::error::codes::TRANSPORT_UNAVAILABLE,
            "This node has no native session transport configured",
        ))
    }

    fn list_displays(&self) -> Vec<VirtualDisplaySpec>;
    fn create_display(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplaySpec, ApiError>;
    fn destroy_display(&self, id: &DisplayId) -> Result<(), ApiError>;
    fn topology(&self) -> DisplayTopology;

    fn set_display_mode(&self, _id: &DisplayId, _mode: DisplayMode) -> Result<(), ApiError> {
        Err(ApiError::new(
            ozma_protocol::error::codes::DISPLAY_UNSUPPORTED,
            "This node cannot change display modes",
        ))
    }

    fn set_display_dpms(&self, _id: &DisplayId, _state: DpmsState) -> Result<(), ApiError> {
        Err(ApiError::new(
            ozma_protocol::error::codes::DISPLAY_UNSUPPORTED,
            "This node cannot control display DPMS",
        ))
    }

    fn set_topology(&self, _topology: DisplayTopology) -> Result<(), ApiError> {
        Err(ApiError::new(
            ozma_protocol::error::codes::DISPLAY_UNSUPPORTED,
            "This node cannot set display topology",
        ))
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionEvent>;
}

/// Current unix time in seconds. Small helper so tests and the real node use
/// one clock convention.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// In-memory backend. It is intentionally simple: it proves the API shape and
/// gives the node a deterministic local harness before the real session
/// manager is wired in.
#[derive(Clone)]
pub struct MemoryBackend {
    inner: Arc<Mutex<MemoryState>>,
    events: broadcast::Sender<SessionEvent>,
    counter: Arc<AtomicU64>,
}

struct MemoryState {
    capabilities: NodeCapabilities,
    sessions: HashMap<SessionId, Session>,
    displays: HashMap<DisplayId, VirtualDisplaySpec>,
    topology: DisplayTopology,
}

impl MemoryBackend {
    pub fn new(capabilities: NodeCapabilities) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(Mutex::new(MemoryState {
                capabilities,
                sessions: HashMap::new(),
                displays: HashMap::new(),
                topology: DisplayTopology::default(),
            })),
            events,
            counter: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn fake() -> Self {
        Self::new(NodeCapabilities::fake("local-node"))
    }

    pub fn node_id(&self) -> NodeId {
        self.inner
            .lock()
            .expect("backend lock")
            .capabilities
            .node_id
            .clone()
    }

    fn next_id(&self, prefix: &str) -> String {
        let value = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{value}")
    }

    fn transition(
        &self,
        session: &mut Session,
        next: SessionState,
        now: u64,
    ) -> Result<(), ApiError> {
        if !session.state.can_transition_to(next) {
            return Err(ApiError::new(
                ozma_protocol::error::codes::SESSION_STATE,
                format!(
                    "Cannot move session {} from {:?} to {:?}",
                    session.id, session.state, next
                ),
            ));
        }
        session.state = next;
        session.updated_at = now;
        let _ = self.events.send(SessionEvent::StateChanged {
            session_id: session.id.clone(),
            state: next,
        });
        Ok(())
    }
}

impl NodeBackend for MemoryBackend {
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn capabilities(&self) -> NodeCapabilities {
        self.inner.lock().expect("backend lock").capabilities.clone()
    }

    fn diagnostics(&self) -> DiagnosticReport {
        DiagnosticReport::new(
            vec![
                ozma_protocol::DiagnosticCheck::ok("api", "API is running"),
                ozma_protocol::DiagnosticCheck::ok("session", "In-memory session backend"),
            ],
            unix_now(),
        )
    }

    fn metrics(&self) -> RuntimeMetrics {
        let sessions = self.inner.lock().expect("backend lock").sessions.len() as u32;
        RuntimeMetrics {
            sessions,
            ..RuntimeMetrics::default()
        }
    }

    fn list_sessions(&self) -> Vec<Session> {
        self.inner
            .lock()
            .expect("backend lock")
            .sessions
            .values()
            .cloned()
            .collect()
    }

    fn get_session(&self, id: &SessionId) -> Option<Session> {
        self.inner
            .lock()
            .expect("backend lock")
            .sessions
            .get(id)
            .cloned()
    }

    fn create_session(&self, spec: SessionSpec) -> Result<Session, ApiError> {
        let now = unix_now();
        let id = SessionId::new(self.next_id("session"));
        let permissions = ozma_protocol::PermissionSet::all();
        let session = Session::new(id.clone(), spec, permissions, now);
        self.inner
            .lock()
            .expect("backend lock")
            .sessions
            .insert(id, session.clone());
        let _ = self.events.send(SessionEvent::StateChanged {
            session_id: session.id.clone(),
            state: session.state,
        });
        Ok(session)
    }

    fn start_session(&self, id: &SessionId) -> Result<Session, ApiError> {
        let now = unix_now();
        let mut state = self.inner.lock().expect("backend lock");
        let session = state.sessions.get_mut(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found")
        })?;
        if session.state == SessionState::Created {
            self.transition(session, SessionState::Preparing, now)?;
        }
        if session.state == SessionState::Preparing {
            self.transition(session, SessionState::Ready, now)?;
        }
        if session.state == SessionState::Ready {
            self.transition(session, SessionState::Streaming, now)?;
            session.started_at = Some(now);
        }
        Ok(session.clone())
    }

    fn stop_session(&self, id: &SessionId, reason: EndReason) -> Result<Session, ApiError> {
        let now = unix_now();
        let mut state = self.inner.lock().expect("backend lock");
        let session = state.sessions.get_mut(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found")
        })?;
        if !session.state.is_terminal() {
            self.transition(session, SessionState::Ended, now)?;
            session.ended_at = Some(now);
            session.end_reason = Some(reason);
            let _ = self.events.send(SessionEvent::Ended {
                session_id: session.id.clone(),
                reason,
            });
        }
        Ok(session.clone())
    }

    fn approve_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        let now = unix_now();
        let mut state = self.inner.lock().expect("backend lock");
        let session = state.sessions.get_mut(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found")
        })?;
        if session.state != SessionState::PendingConsent {
            return Err(ApiError::new(
                ozma_protocol::error::codes::SESSION_STATE,
                "Session is not waiting for consent",
            ));
        }
        session.consent = None;
        self.transition(session, SessionState::Preparing, now)?;
        let _ = self.events.send(SessionEvent::ConsentDecided {
            session_id: session.id.clone(),
            approved: true,
            by: by.to_owned(),
        });
        Ok(session.clone())
    }

    fn deny_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        let now = unix_now();
        let mut state = self.inner.lock().expect("backend lock");
        let session = state.sessions.get_mut(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found")
        })?;
        if session.state != SessionState::PendingConsent {
            return Err(ApiError::new(
                ozma_protocol::error::codes::SESSION_STATE,
                "Session is not waiting for consent",
            ));
        }
        session.consent = None;
        self.transition(session, SessionState::Rejected, now)?;
        session.ended_at = Some(now);
        session.end_reason = Some(EndReason::ConsentDenied);
        let _ = self.events.send(SessionEvent::ConsentDecided {
            session_id: session.id.clone(),
            approved: false,
            by: by.to_owned(),
        });
        Ok(session.clone())
    }

    fn resume_session(
        &self,
        id: &SessionId,
        client_id: &ozma_protocol::ClientId,
    ) -> Result<Session, ApiError> {
        let mut state = self.inner.lock().expect("backend lock");
        let session = state.sessions.get_mut(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found")
        })?;
        if session.state.is_terminal() {
            return Err(ApiError::new(
                ozma_protocol::error::codes::SESSION_STATE,
                "A terminal session cannot be resumed",
            ));
        }
        if !session.participants.iter().any(|p| &p.client_id == client_id) {
            return Err(ApiError::new(
                ozma_protocol::error::codes::NOT_FOUND,
                "The client is not a participant in this session",
            ));
        }
        for participant in &mut session.participants {
            if &participant.client_id == client_id {
                participant.connected = true;
                participant.last_seen = unix_now();
            }
        }
        Ok(session.clone())
    }

    fn list_displays(&self) -> Vec<VirtualDisplaySpec> {
        self.inner
            .lock()
            .expect("backend lock")
            .displays
            .values()
            .cloned()
            .collect()
    }

    fn create_display(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplaySpec, ApiError> {
        spec.validate()?;
        let mut state = self.inner.lock().expect("backend lock");
        if state.displays.contains_key(&spec.id) {
            return Err(ApiError::new(
                ozma_protocol::error::codes::CONFLICT,
                "A display with that id already exists",
            ));
        }
        state.displays.insert(spec.id.clone(), spec.clone());
        Ok(spec)
    }

    fn destroy_display(&self, id: &DisplayId) -> Result<(), ApiError> {
        let mut state = self.inner.lock().expect("backend lock");
        state.displays.remove(id).ok_or_else(|| {
            ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Display not found")
        })?;
        Ok(())
    }

    fn topology(&self) -> DisplayTopology {
        self.inner.lock().expect("backend lock").topology.clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }
}
