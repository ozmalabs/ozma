// SPDX-License-Identifier: AGPL-3.0-only
//! The Ozma session state machine.
//!
//! A [`SessionManager`] owns the live session registry and the authorization
//! rules around it. It has no HTTP, database, or filesystem dependencies: the
//! API layer wraps these synchronous methods and forwards the broadcast events
//! to SSE/WebSocket clients.
//!
//! The important invariants:
//!
//! * The permissions required by [`SessionSpec::required_permissions`] must be
//!   present before a session can move out of `Created` or `Preparing`.
//! * A session that needs consent enters `PendingConsent` and cannot start
//!   until [`SessionManager::approve_consent`] is called.
//! * At most one participant is the controller. Viewers can join a session but
//!   cannot claim control or inject input.
//! * Disconnect, control release, stop, consent denial, and local lock all emit
//!   [`InputEvent::ReleaseAll`] on the input bus and
//!   [`SessionEvent::InputReleased`] on the session bus.
//! * Illegal transitions return `session.invalid_state` without mutating the
//!   session. Terminal sessions are immutable.
//!
//! Time is injected. [`SessionManager::new`] uses the system clock;
//! [`SessionManager::with_clock`] is used by deterministic tests.
//! [`SessionManager::create`] takes its `now` explicitly so callers can replay
//! requests, and [`SessionManager::expire`] is called by the supervisor to
//! turn idle sessions into `Ended` with `IdleTimeout`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use ozma_auth::{ConsentPolicy, DEFAULT_CONSENT_TIMEOUT_SECONDS};
use ozma_protocol::{
    codes, ApiError, ClientId, ConsentRequest, EndReason, InputEvent, Participant, ParticipantRole,
    Permission, PermissionSet, Result, Session, SessionEvent, SessionId, SessionSpec, SessionState,
};
use serde_json::json;
use tokio::sync::broadcast;

/// Default number of session events buffered for slow subscribers.
pub const DEFAULT_EVENT_CAPACITY: usize = 128;

/// A source of Unix seconds. Injected so tests never depend on wall time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

struct SessionRecord {
    session: Session,
    /// Absolute expiry derived from `created_at + ttl_seconds`.
    expires_at: u64,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<SessionId, SessionRecord>,
    next_sequence: u64,
}

/// Owns all sessions and broadcasts lifecycle/input events.
pub struct SessionManager {
    consent_policy: ConsentPolicy,
    clock: Clock,
    inner: Mutex<Inner>,
    events: broadcast::Sender<SessionEvent>,
    input_events: broadcast::Sender<InputEvent>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new(ConsentPolicy::default())
    }
}

impl SessionManager {
    /// Create a manager with the node's consent policy and the system clock.
    pub fn new(consent_policy: ConsentPolicy) -> Self {
        Self::with_clock(consent_policy, Arc::new(system_now))
    }

    /// Create a manager with an injected clock.
    pub fn with_clock(consent_policy: ConsentPolicy, clock: Clock) -> Self {
        let (events, _) = broadcast::channel(DEFAULT_EVENT_CAPACITY);
        let (input_events, _) = broadcast::channel(DEFAULT_EVENT_CAPACITY);
        Self {
            consent_policy,
            clock,
            inner: Mutex::new(Inner::default()),
            events,
            input_events,
        }
    }

    /// Session lifecycle and audit events.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Input bus. `ReleaseAll` is broadcast whenever held input must be
    /// dropped; other input events are forwarded by the transport adapter.
    pub fn subscribe_input(&self) -> broadcast::Receiver<InputEvent> {
        self.input_events.subscribe()
    }

    pub fn consent_policy(&self) -> &ConsentPolicy {
        &self.consent_policy
    }

    /// Create a session and, when policy requires it, open a consent request.
    ///
    /// The grant is recorded even when it is too narrow for the spec; `start`
    /// and `approve_consent` reject the transition so the session never leaves
    /// `Created`/`PendingConsent`. This keeps the error at the point where a
    /// permission is actually needed and lets a controller report the missing
    /// scopes before any media starts.
    pub fn create(
        &self,
        spec: SessionSpec,
        granted_permissions: PermissionSet,
        now: u64,
    ) -> Result<Session> {
        if spec.client_id.is_empty() {
            return Err(ApiError::invalid(
                "A session spec requires a non-empty client_id",
            ));
        }

        let needs_consent = self.consent_policy.requires_consent(spec.privacy.consent);

        let mut inner = self.state();
        let sequence = inner.next_sequence;
        inner.next_sequence = inner.next_sequence.wrapping_add(1);
        let id = SessionId::new(format!("sess-{now}-{sequence}"));

        let client_id = spec.client_id.clone();
        let ttl_seconds = spec.ttl_seconds;
        let mut session = Session::new(id.clone(), spec, granted_permissions, now);
        session.participants.push(Participant {
            client_id,
            role: ParticipantRole::Viewer,
            connected: true,
            last_seen: now,
        });

        let mut record = SessionRecord {
            session,
            expires_at: now.saturating_add(ttl_seconds),
        };

        if needs_consent {
            self.transition(&mut record, SessionState::Offered, now)?;
            self.transition(&mut record, SessionState::PendingConsent, now)?;

            let request = ConsentRequest {
                session_id: id.clone(),
                client_id: record.session.spec.client_id.clone(),
                principal: record.session.spec.principal.clone(),
                requested: record.session.spec.required_permissions(),
                created_at: now,
                expires_at: now.saturating_add(DEFAULT_CONSENT_TIMEOUT_SECONDS),
            };
            record.session.consent = Some(request.clone());
            self.emit(SessionEvent::ConsentRequested { request });
        }

        let session = record.session.clone();
        inner.sessions.insert(id, record);
        Ok(session)
    }

    /// Approve a pending consent request. The session moves to `Preparing`;
    /// call [`SessionManager::start`] to make it `Ready`.
    pub fn approve_consent(
        &self,
        session_id: &SessionId,
        by: impl Into<String>,
    ) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;
        if record.session.state != SessionState::PendingConsent {
            return Err(invalid_state(&record.session, "approve consent"));
        }
        ensure_can_start(&record.session)?;

        let request = record
            .session
            .consent
            .clone()
            .ok_or_else(|| ApiError::internal("a pending session has no consent request"))?;

        if now >= request.expires_at {
            self.expire_consent_record(record, now)?;
            return Err(ApiError::new(
                codes::CONSENT_REQUIRED,
                "The consent request expired before it was approved",
            )
            .with_details(json!({
                "session_id": session_id,
                "expires_at": request.expires_at,
                "now": now,
            }))
            .retryable());
        }

        self.transition(record, SessionState::Preparing, now)?;
        self.emit(SessionEvent::ConsentDecided {
            session_id: session_id.clone(),
            approved: true,
            by: by.into(),
        });
        Ok(record.session.clone())
    }

    /// Deny consent. The session becomes `Rejected`, a terminal state.
    pub fn deny_consent(&self, session_id: &SessionId, by: impl Into<String>) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;
        if record.session.state != SessionState::PendingConsent {
            return Err(invalid_state(&record.session, "deny consent"));
        }

        self.transition(record, SessionState::Rejected, now)?;
        record.session.ended_at = Some(now);
        record.session.end_reason = Some(EndReason::ConsentDenied);

        self.emit(SessionEvent::ConsentDecided {
            session_id: session_id.clone(),
            approved: false,
            by: by.into(),
        });
        self.emit_input_release(session_id, EndReason::ConsentDenied);
        self.emit(SessionEvent::Ended {
            session_id: session_id.clone(),
            reason: EndReason::ConsentDenied,
        });

        Ok(record.session.clone())
    }

    /// Start a session. `Created` and `Preparing` sessions become `Ready`; a
    /// session still waiting for consent returns `session.not_ready`.
    pub fn start(&self, session_id: &SessionId) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;

        if now >= record.expires_at {
            self.end_record(record, EndReason::IdleTimeout, now)?;
            return Err(ApiError::new(
                codes::SESSION_NOT_READY,
                "The session expired before it started",
            )
            .with_details(json!({ "session_id": session_id })));
        }

        match record.session.state {
            SessionState::Created => {
                ensure_can_start(&record.session)?;
                self.transition(record, SessionState::Preparing, now)?;
                self.transition(record, SessionState::Ready, now)?;
                record.session.started_at = Some(now);
            }
            SessionState::Preparing => {
                ensure_can_start(&record.session)?;
                self.transition(record, SessionState::Ready, now)?;
                if record.session.started_at.is_none() {
                    record.session.started_at = Some(now);
                }
            }
            SessionState::PendingConsent => {
                return Err(ApiError::new(
                    codes::SESSION_NOT_READY,
                    "The session is waiting for local consent",
                )
                .with_details(json!({ "session_id": session_id }))
                .retryable());
            }
            _ => return Err(invalid_state(&record.session, "start")),
        }

        Ok(record.session.clone())
    }

    /// Mark a `Ready` (or recovered `Degraded`) session as streaming.
    pub fn mark_streaming(&self, session_id: &SessionId) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;
        if now >= record.expires_at {
            self.end_record(record, EndReason::IdleTimeout, now)?;
            return Err(session_expired(session_id));
        }

        if !matches!(
            record.session.state,
            SessionState::Ready | SessionState::Degraded
        ) {
            return Err(invalid_state(&record.session, "mark streaming"));
        }

        self.transition(record, SessionState::Streaming, now)?;
        Ok(record.session.clone())
    }

    /// Move a streaming session into `Degraded`. Recovery calls
    /// [`SessionManager::mark_streaming`].
    pub fn degrade(&self, session_id: &SessionId) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;
        if record.session.state != SessionState::Streaming {
            return Err(invalid_state(&record.session, "degrade"));
        }

        self.transition(record, SessionState::Degraded, now)?;
        Ok(record.session.clone())
    }

    /// Stop a session for an explicit reason. This releases held input and
    /// emits `Ended`. Calling it on a terminal session is an invalid
    /// transition.
    pub fn stop(&self, session_id: &SessionId, reason: EndReason) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;
        self.end_record(record, reason, now)?;
        Ok(record.session.clone())
    }

    /// End every non-terminal session whose TTL or consent request has
    /// expired. Returns the number of sessions ended.
    ///
    /// The supervisor should call this periodically; tests call it with an
    /// explicit `now`.
    pub fn expire(&self, now: u64) -> usize {
        let mut ended = 0;

        // Consent requests time out before the session TTL and get the more
        // specific `TimedOut` state.
        let consent_timed_out: Vec<SessionId> = {
            let inner = self.state();
            inner
                .sessions
                .iter()
                .filter(|(_, record)| {
                    record.session.state == SessionState::PendingConsent
                        && record
                            .session
                            .consent
                            .as_ref()
                            .is_some_and(|request| now >= request.expires_at)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        for id in consent_timed_out {
            let mut inner = self.state();
            if let Some(record) = inner.sessions.get_mut(&id) {
                if record.session.state == SessionState::PendingConsent
                    && self.expire_consent_record(record, now).is_ok()
                {
                    ended += 1;
                }
            }
        }

        let idle: Vec<SessionId> = {
            let inner = self.state();
            inner
                .sessions
                .iter()
                .filter(|(_, record)| {
                    !record.session.state.is_terminal() && now >= record.expires_at
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        for id in idle {
            let mut inner = self.state();
            if let Some(record) = inner.sessions.get_mut(&id) {
                if !record.session.state.is_terminal()
                    && self.end_record(record, EndReason::IdleTimeout, now).is_ok()
                {
                    ended += 1;
                }
            }
        }

        ended
    }

    /// Reconnect or first-attach a client. Terminal sessions cannot be
    /// resumed; a disconnected session survives so this can mark the client
    /// connected again.
    pub fn resume(&self, session_id: &SessionId, client_id: &ClientId) -> Result<Session> {
        self.join(session_id, client_id)
    }

    /// Add a client as a viewer, or mark an existing participant connected.
    pub fn join(&self, session_id: &SessionId, client_id: &ClientId) -> Result<Session> {
        if client_id.is_empty() {
            return Err(ApiError::invalid("A participant requires a client_id"));
        }

        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;
        if record.session.state == SessionState::Draining {
            return Err(invalid_state(&record.session, "join"));
        }

        match record
            .session
            .participants
            .iter_mut()
            .find(|participant| participant.client_id == *client_id)
        {
            Some(participant) => {
                participant.connected = true;
                participant.last_seen = now;
            }
            None => record.session.participants.push(Participant {
                client_id: client_id.clone(),
                role: ParticipantRole::Viewer,
                connected: true,
                last_seen: now,
            }),
        }

        record.session.updated_at = now;
        Ok(record.session.clone())
    }

    /// All sessions, oldest first. Clones are returned so callers never hold a
    /// lock while serializing.
    pub fn list(&self) -> Vec<Session> {
        let inner = self.state();
        let mut sessions: Vec<Session> = inner
            .sessions
            .values()
            .map(|record| record.session.clone())
            .collect();
        sessions.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        sessions
    }

    /// Get one session by id.
    pub fn get(&self, session_id: &SessionId) -> Result<Session> {
        let inner = self.state();
        inner
            .sessions
            .get(session_id)
            .map(|record| record.session.clone())
            .ok_or_else(|| not_found(session_id))
    }

    /// Claim input ownership. Requires the session's `control` permission and
    /// refuses to steal control from another connected client.
    pub fn claim_control(&self, session_id: &SessionId, client_id: &ClientId) -> Result<Session> {
        if client_id.is_empty() {
            return Err(ApiError::invalid("A controller requires a client_id"));
        }

        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;

        if !record.session.permissions.contains(Permission::Control) {
            return Err(ApiError::new(
                codes::FORBIDDEN,
                "The session grant does not include the control permission",
            )
            .with_details(json!({ "session_id": session_id, "client_id": client_id })));
        }

        if !matches!(
            record.session.state,
            SessionState::Ready | SessionState::Streaming | SessionState::Degraded
        ) {
            return Err(ApiError::new(
                codes::SESSION_NOT_READY,
                "Control can only be claimed once the session is ready",
            )
            .with_details(json!({
                "session_id": session_id,
                "state": record.session.state,
            }))
            .retryable());
        }

        if let Some(existing) = controller(&record.session) {
            if existing.client_id == *client_id {
                return Ok(record.session.clone());
            }
            return Err(ApiError::new(
                codes::CONFLICT,
                "Another client already controls this session",
            )
            .with_details(json!({
                "session_id": session_id,
                "controller": existing.client_id,
            })));
        }

        let participant = record
            .session
            .participants
            .iter_mut()
            .find(|participant| participant.client_id == *client_id)
            .ok_or_else(|| {
                ApiError::new(
                    codes::NOT_FOUND,
                    "The client is not a participant in this session",
                )
                .with_details(json!({ "session_id": session_id, "client_id": client_id }))
            })?;
        if !participant.connected {
            return Err(ApiError::new(
                codes::FORBIDDEN,
                "The client is disconnected and cannot claim control",
            )
            .with_details(json!({ "session_id": session_id, "client_id": client_id })));
        }

        participant.role = ParticipantRole::Controller;
        participant.last_seen = now;
        record.session.updated_at = now;

        Ok(record.session.clone())
    }

    /// Release control back to viewers. Idempotent when this client is not the
    /// controller.
    pub fn release_control(&self, session_id: &SessionId, client_id: &ClientId) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;

        let Some(existing) = controller(&record.session) else {
            return Ok(record.session.clone());
        };
        if existing.client_id != *client_id {
            return Err(ApiError::new(
                codes::FORBIDDEN,
                "Only the current controller can release control",
            )
            .with_details(json!({
                "session_id": session_id,
                "controller": existing.client_id,
                "client_id": client_id,
            })));
        }

        for participant in &mut record.session.participants {
            if participant.role == ParticipantRole::Controller {
                participant.role = ParticipantRole::Viewer;
                participant.last_seen = now;
            }
        }
        record.session.updated_at = now;

        self.emit_input_release(session_id, EndReason::Superseded);
        Ok(record.session.clone())
    }

    /// Mark a client disconnected. If it held control, control is released and
    /// held input is dropped. The session itself survives so the client can
    /// reconnect; TTL expiry or an explicit stop ends it.
    pub fn on_client_disconnect(
        &self,
        session_id: &SessionId,
        client_id: &ClientId,
    ) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        // Disconnect is idempotent on an already-finished session.
        if record.session.state.is_terminal() {
            return Ok(record.session.clone());
        }

        let Some(participant) = record
            .session
            .participants
            .iter_mut()
            .find(|participant| participant.client_id == *client_id)
        else {
            return Ok(record.session.clone());
        };

        let was_controller =
            participant.role == ParticipantRole::Controller && participant.connected;
        participant.connected = false;
        participant.last_seen = now;

        if was_controller {
            participant.role = ParticipantRole::Viewer;
            self.emit_input_release(session_id, EndReason::ClientDisconnected);
        }

        record.session.updated_at = now;
        Ok(record.session.clone())
    }

    /// Apply a local lock transition. If the privacy policy says so, the
    /// session ends with `local_unlock`; otherwise input is released but the
    /// session stays alive.
    pub fn on_local_lock(&self, session_id: &SessionId) -> Result<Session> {
        let now = self.now();
        let mut inner = self.state();
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;

        if record.session.spec.privacy.disconnect_on_local_unlock {
            self.end_record(record, EndReason::LocalUnlock, now)?;
        } else {
            self.emit_input_release(session_id, EndReason::LocalUnlock);
            record.session.updated_at = now;
        }

        Ok(record.session.clone())
    }

    /// Authorize an input event from a client. This does not forward the event;
    /// the input adapter consumes it only when this returns `Ok`.
    pub fn submit_input(
        &self,
        session_id: &SessionId,
        client_id: &ClientId,
        _event: &InputEvent,
    ) -> Result<()> {
        let inner = self.state();
        let record = inner
            .sessions
            .get(session_id)
            .ok_or_else(|| not_found(session_id))?;

        ensure_open(&record.session)?;

        if !matches!(
            record.session.state,
            SessionState::Ready | SessionState::Streaming | SessionState::Degraded
        ) {
            return Err(ApiError::new(
                codes::SESSION_NOT_READY,
                "Input is not accepted before the session is ready",
            )
            .with_details(json!({
                "session_id": session_id,
                "state": record.session.state,
            }))
            .retryable());
        }

        if !record.session.permissions.contains(Permission::Control) {
            return Err(ApiError::new(
                codes::FORBIDDEN,
                "The session grant does not include the control permission",
            )
            .with_details(json!({ "session_id": session_id, "client_id": client_id })));
        }

        match controller(&record.session) {
            Some(participant) if participant.client_id == *client_id => Ok(()),
            Some(participant) => Err(ApiError::new(
                codes::FORBIDDEN,
                "Only the session controller may inject input",
            )
            .with_details(json!({
                "session_id": session_id,
                "controller": participant.client_id,
                "client_id": client_id,
            }))),
            None => Err(ApiError::new(
                codes::FORBIDDEN,
                "No client currently controls this session",
            )
            .with_details(json!({ "session_id": session_id, "client_id": client_id }))),
        }
    }

    /// Number of registered sessions, including terminal ones retained for
    /// audit.
    pub fn len(&self) -> usize {
        self.state().sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().sessions.is_empty()
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn state(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: SessionEvent) {
        // A missing subscriber is not an error.
        let _ = self.events.send(event);
    }

    fn emit_input_release(&self, session_id: &SessionId, reason: EndReason) {
        self.emit(SessionEvent::InputReleased {
            session_id: session_id.clone(),
            reason,
        });
        let _ = self.input_events.send(InputEvent::ReleaseAll);
    }

    fn transition(&self, record: &mut SessionRecord, next: SessionState, now: u64) -> Result<()> {
        let current = record.session.state;
        if !current.can_transition_to(next) {
            return Err(ApiError::new(
                codes::SESSION_STATE,
                format!("Cannot move a session from {current:?} to {next:?}"),
            )
            .with_details(json!({
                "session_id": record.session.id,
                "from": current,
                "to": next,
            })));
        }

        record.session.state = next;
        record.session.updated_at = now;
        self.emit(SessionEvent::StateChanged {
            session_id: record.session.id.clone(),
            state: next,
        });
        Ok(())
    }

    fn end_record(&self, record: &mut SessionRecord, reason: EndReason, now: u64) -> Result<()> {
        if record.session.state.is_terminal() {
            return Err(invalid_state(&record.session, "end"));
        }

        self.transition(record, SessionState::Ended, now)?;
        record.session.ended_at = Some(now);
        record.session.end_reason = Some(reason);
        self.emit_input_release(&record.session.id, reason);
        self.emit(SessionEvent::Ended {
            session_id: record.session.id.clone(),
            reason,
        });
        Ok(())
    }

    fn expire_consent_record(&self, record: &mut SessionRecord, now: u64) -> Result<()> {
        self.transition(record, SessionState::TimedOut, now)?;
        record.session.ended_at = Some(now);
        record.session.end_reason = Some(EndReason::ConsentTimedOut);
        self.emit_input_release(&record.session.id, EndReason::ConsentTimedOut);
        self.emit(SessionEvent::Ended {
            session_id: record.session.id.clone(),
            reason: EndReason::ConsentTimedOut,
        });
        Ok(())
    }
}

fn controller(session: &Session) -> Option<&Participant> {
    session.participants.iter().find(|participant| {
        participant.role == ParticipantRole::Controller && participant.connected
    })
}

fn not_found(session_id: &SessionId) -> ApiError {
    ApiError::new(codes::NOT_FOUND, "No such session")
        .with_details(json!({ "session_id": session_id }))
}

fn ensure_open(session: &Session) -> Result<()> {
    if session.state.is_terminal() {
        return Err(invalid_state(session, "modify"));
    }
    Ok(())
}

fn ensure_can_start(session: &Session) -> Result<()> {
    let required = session.spec.required_permissions();
    let missing = required.missing_from(&session.permissions);
    if missing.is_empty() {
        return Ok(());
    }

    let missing: Vec<String> = missing
        .iter()
        .map(|permission| permission.to_string())
        .collect();
    Err(ApiError::new(
        codes::FORBIDDEN,
        "The session grant does not cover its required permissions",
    )
    .with_details(json!({
        "session_id": session.id,
        "missing": missing,
    })))
}

fn invalid_state(session: &Session, action: &str) -> ApiError {
    ApiError::new(
        codes::SESSION_STATE,
        format!("Cannot {action} while the session is {:?}", session.state),
    )
    .with_details(json!({
        "session_id": session.id,
        "state": session.state,
    }))
}

fn session_expired(session_id: &SessionId) -> ApiError {
    ApiError::new(codes::SESSION_NOT_READY, "The session has expired")
        .with_details(json!({ "session_id": session_id }))
        .retryable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use ozma_auth::MachineClass;
    use ozma_protocol::{
        AudioSpec, ConsentRequirement, InputSpec, PointerMode, PrivacyPolicy, TransportPreference,
    };
    use tokio::sync::broadcast::error::TryRecvError;

    fn grant(permissions: &[Permission]) -> PermissionSet {
        permissions.iter().copied().collect()
    }

    fn view_and_control() -> PermissionSet {
        grant(&[Permission::View, Permission::Control])
    }

    fn spec(client: &str, control: bool) -> SessionSpec {
        SessionSpec {
            client_id: ClientId::new(client),
            principal: "alice".to_owned(),
            tracks: Vec::new(),
            audio: AudioSpec::default(),
            input: if control {
                InputSpec {
                    keyboard: true,
                    pointer: PointerMode::Absolute,
                    ..InputSpec::default()
                }
            } else {
                InputSpec::default()
            },
            privacy: PrivacyPolicy::default(),
            transport: TransportPreference::default(),
            ttl_seconds: 60,
        }
    }

    fn manager(policy: ConsentPolicy) -> SessionManager {
        SessionManager::with_clock(policy, Arc::new(|| 1_000))
    }

    fn server_manager() -> SessionManager {
        manager(ConsentPolicy::for_machine(MachineClass::Server))
    }

    fn client(id: &str) -> ClientId {
        ClientId::new(id)
    }

    #[test]
    fn happy_path_reaches_streaming() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();
        assert_eq!(created.state, SessionState::Created);
        assert_eq!(created.participants.len(), 1);

        let ready = manager.start(&created.id).unwrap();
        assert_eq!(ready.state, SessionState::Ready);
        assert_eq!(ready.started_at, Some(1_000));

        let claimed = manager
            .claim_control(&created.id, &client("client-1"))
            .unwrap();
        assert_eq!(claimed.participants[0].role, ParticipantRole::Controller);

        let streaming = manager.mark_streaming(&created.id).unwrap();
        assert_eq!(streaming.state, SessionState::Streaming);
        assert_eq!(
            manager.get(&created.id).unwrap().state,
            SessionState::Streaming
        );
    }

    #[test]
    fn denied_consent_blocks_startup() {
        let manager = manager(ConsentPolicy::new(MachineClass::Workstation, true));
        let mut request = spec("client-1", true);
        request.privacy.consent = ConsentRequirement::Workstation;

        let pending = manager.create(request, view_and_control(), 1_000).unwrap();
        assert_eq!(pending.state, SessionState::PendingConsent);
        assert!(pending.consent.is_some());

        let error = manager.start(&pending.id).unwrap_err();
        assert_eq!(error.code, codes::SESSION_NOT_READY);

        let denied = manager.deny_consent(&pending.id, "local user").unwrap();
        assert_eq!(denied.state, SessionState::Rejected);
        assert_eq!(denied.end_reason, Some(EndReason::ConsentDenied));

        let error = manager.start(&pending.id).unwrap_err();
        assert_eq!(error.code, codes::SESSION_STATE);
        assert_eq!(
            manager.get(&pending.id).unwrap().state,
            SessionState::Rejected
        );
    }

    #[test]
    fn approving_consent_allows_start() {
        let manager = manager(ConsentPolicy::new(MachineClass::Workstation, true));
        let mut request = spec("client-1", true);
        request.privacy.consent = ConsentRequirement::Workstation;

        let pending = manager.create(request, view_and_control(), 1_000).unwrap();
        let preparing = manager.approve_consent(&pending.id, "local user").unwrap();
        assert_eq!(preparing.state, SessionState::Preparing);

        let ready = manager.start(&pending.id).unwrap();
        assert_eq!(ready.state, SessionState::Ready);
    }

    #[test]
    fn consent_policy_depends_on_machine_class_and_configuration() {
        // Server with a workstation requirement never prompts.
        let server = manager(ConsentPolicy::new(MachineClass::Server, true));
        let mut request = spec("client-1", true);
        request.privacy.consent = ConsentRequirement::Workstation;
        let created = server.create(request, view_and_control(), 1_000).unwrap();
        assert_eq!(created.state, SessionState::Created);

        // Workstation with consent configured prompts.
        let workstation = manager(ConsentPolicy::new(MachineClass::Workstation, true));
        let mut request = spec("client-2", true);
        request.privacy.consent = ConsentRequirement::Workstation;
        let created = workstation
            .create(request, view_and_control(), 1_000)
            .unwrap();
        assert_eq!(created.state, SessionState::PendingConsent);

        // Kiosk with `Always` still prompts (answered out of band).
        let kiosk = manager(ConsentPolicy::new(MachineClass::Kiosk, true));
        let mut request = spec("client-3", true);
        request.privacy.consent = ConsentRequirement::Always;
        let created = kiosk.create(request, view_and_control(), 1_000).unwrap();
        assert_eq!(created.state, SessionState::PendingConsent);
    }

    #[test]
    fn an_expired_consent_request_times_out() {
        let now = Arc::new(Mutex::new(1_000u64));
        let clock = now.clone();
        let manager = SessionManager::with_clock(
            ConsentPolicy::new(MachineClass::Workstation, true),
            Arc::new(move || *clock.lock().unwrap()),
        );
        let mut request = spec("client-1", true);
        request.privacy.consent = ConsentRequirement::Workstation;

        let pending = manager.create(request, view_and_control(), 1_000).unwrap();
        *now.lock().unwrap() = 1_000 + DEFAULT_CONSENT_TIMEOUT_SECONDS;

        let error = manager
            .approve_consent(&pending.id, "local user")
            .unwrap_err();
        assert_eq!(error.code, codes::CONSENT_REQUIRED);

        let session = manager.get(&pending.id).unwrap();
        assert_eq!(session.state, SessionState::TimedOut);
        assert_eq!(session.end_reason, Some(EndReason::ConsentTimedOut));
    }

    #[test]
    fn insufficient_grant_cannot_leave_created() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), grant(&[Permission::View]), 1_000)
            .unwrap();
        assert_eq!(created.state, SessionState::Created);

        let error = manager.start(&created.id).unwrap_err();
        assert_eq!(error.code, codes::FORBIDDEN);
        assert_eq!(error.details["missing"][0], "control");
        assert_eq!(
            manager.get(&created.id).unwrap().state,
            SessionState::Created
        );
    }

    #[test]
    fn insufficient_grant_cannot_exit_pending_consent() {
        let manager = manager(ConsentPolicy::new(MachineClass::Workstation, true));
        let mut request = spec("client-1", true);
        request.privacy.consent = ConsentRequirement::Workstation;

        let pending = manager
            .create(request, grant(&[Permission::View]), 1_000)
            .unwrap();
        assert_eq!(pending.state, SessionState::PendingConsent);

        let error = manager
            .approve_consent(&pending.id, "local user")
            .unwrap_err();
        assert_eq!(error.code, codes::FORBIDDEN);
        assert_eq!(
            manager.get(&pending.id).unwrap().state,
            SessionState::PendingConsent
        );
    }

    #[test]
    fn view_only_grant_cannot_claim_control() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", false), PermissionSet::view_only(), 1_000)
            .unwrap();
        manager.start(&created.id).unwrap();

        let error = manager
            .claim_control(&created.id, &client("client-1"))
            .unwrap_err();
        assert_eq!(error.code, codes::FORBIDDEN);

        let error = manager
            .submit_input(&created.id, &client("client-1"), &InputEvent::ReleaseAll)
            .unwrap_err();
        assert_eq!(error.code, codes::FORBIDDEN);
    }

    #[test]
    fn only_one_controller_and_control_can_be_released() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();
        manager.start(&created.id).unwrap();
        manager
            .claim_control(&created.id, &client("client-1"))
            .unwrap();
        manager.join(&created.id, &client("client-2")).unwrap();

        let error = manager
            .claim_control(&created.id, &client("client-2"))
            .unwrap_err();
        assert_eq!(error.code, codes::CONFLICT);

        let released = manager
            .release_control(&created.id, &client("client-1"))
            .unwrap();
        assert!(released
            .participants
            .iter()
            .all(|participant| participant.role != ParticipantRole::Controller));

        let claimed = manager
            .claim_control(&created.id, &client("client-2"))
            .unwrap();
        assert!(claimed.participants.iter().any(|participant| {
            participant.client_id == client("client-2")
                && participant.role == ParticipantRole::Controller
        }));
    }

    #[test]
    fn viewer_cannot_inject_input() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();
        manager.start(&created.id).unwrap();
        manager
            .claim_control(&created.id, &client("client-1"))
            .unwrap();
        manager.join(&created.id, &client("client-2")).unwrap();

        let event = InputEvent::Key {
            code: 30,
            down: true,
            modifiers: Default::default(),
        };
        assert!(manager
            .submit_input(&created.id, &client("client-1"), &event)
            .is_ok());
        assert_eq!(
            manager
                .submit_input(&created.id, &client("client-2"), &event)
                .unwrap_err()
                .code,
            codes::FORBIDDEN
        );
    }

    #[test]
    fn disconnect_releases_input_and_allows_resume() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();
        manager.start(&created.id).unwrap();
        manager
            .claim_control(&created.id, &client("client-1"))
            .unwrap();
        manager.mark_streaming(&created.id).unwrap();

        let mut events = manager.subscribe();
        let mut input = manager.subscribe_input();

        let updated = manager
            .on_client_disconnect(&created.id, &client("client-1"))
            .unwrap();
        assert_eq!(updated.state, SessionState::Streaming);
        assert!(updated
            .participants
            .iter()
            .all(|participant| !participant.connected));
        assert!(updated
            .participants
            .iter()
            .all(|participant| participant.role != ParticipantRole::Controller));

        // Drain buffered events to find the release pair.
        let mut session_release = None;
        loop {
            match events.try_recv() {
                Ok(SessionEvent::InputReleased { reason, .. }) => session_release = Some(reason),
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => {}
            }
        }
        assert_eq!(session_release, Some(EndReason::ClientDisconnected));

        let mut input_release = false;
        loop {
            match input.try_recv() {
                Ok(InputEvent::ReleaseAll) => input_release = true,
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => {}
            }
        }
        assert!(input_release);

        let resumed = manager.resume(&created.id, &client("client-1")).unwrap();
        assert!(resumed.participants[0].connected);
    }

    #[test]
    fn expired_session_stops_with_idle_timeout() {
        let manager = server_manager();
        let mut request = spec("client-1", true);
        request.ttl_seconds = 10;
        let created = manager.create(request, view_and_control(), 1_000).unwrap();
        let mut events = manager.subscribe();
        let mut input = manager.subscribe_input();

        assert_eq!(manager.expire(1_009), 0);
        assert_eq!(
            manager.get(&created.id).unwrap().state,
            SessionState::Created
        );

        assert_eq!(manager.expire(1_010), 1);
        let session = manager.get(&created.id).unwrap();
        assert_eq!(session.state, SessionState::Ended);
        assert_eq!(session.end_reason, Some(EndReason::IdleTimeout));
        assert_eq!(session.ended_at, Some(1_010));

        let mut session_release = None;
        loop {
            match events.try_recv() {
                Ok(SessionEvent::InputReleased { reason, .. }) => session_release = Some(reason),
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => {}
            }
        }
        assert_eq!(session_release, Some(EndReason::IdleTimeout));
        assert!(matches!(input.try_recv(), Ok(InputEvent::ReleaseAll)));
    }

    #[test]
    fn terminal_session_cannot_restart_or_mutate() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();
        manager.start(&created.id).unwrap();
        let ended = manager
            .stop(&created.id, EndReason::AdminTerminated)
            .unwrap();
        assert_eq!(ended.state, SessionState::Ended);

        assert_eq!(
            manager.start(&created.id).unwrap_err().code,
            codes::SESSION_STATE
        );
        assert_eq!(
            manager
                .stop(&created.id, EndReason::AdminTerminated)
                .unwrap_err()
                .code,
            codes::SESSION_STATE
        );
        assert_eq!(
            manager
                .resume(&created.id, &client("client-1"))
                .unwrap_err()
                .code,
            codes::SESSION_STATE
        );
        assert_eq!(
            manager
                .approve_consent(&created.id, "local user")
                .unwrap_err()
                .code,
            codes::SESSION_STATE
        );
        assert_eq!(manager.get(&created.id).unwrap().state, SessionState::Ended);
    }

    #[test]
    fn illegal_transitions_do_not_mutate() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();

        for error in [
            manager.mark_streaming(&created.id).unwrap_err(),
            manager.degrade(&created.id).unwrap_err(),
            manager.deny_consent(&created.id, "local user").unwrap_err(),
        ] {
            assert_eq!(error.code, codes::SESSION_STATE, "{error}");
        }
        // Control is a permission/readiness check rather than a transition.
        assert_eq!(
            manager
                .claim_control(&created.id, &client("client-1"))
                .unwrap_err()
                .code,
            codes::SESSION_NOT_READY
        );

        let session = manager.get(&created.id).unwrap();
        assert_eq!(session.state, SessionState::Created);
        assert_eq!(session.started_at, None);
    }

    #[test]
    fn local_lock_releases_input_and_honours_policy() {
        let manager = server_manager();
        let mut request = spec("client-1", true);
        request.privacy.disconnect_on_local_unlock = false;
        let created = manager.create(request, view_and_control(), 1_000).unwrap();
        manager.start(&created.id).unwrap();
        manager
            .claim_control(&created.id, &client("client-1"))
            .unwrap();
        manager.mark_streaming(&created.id).unwrap();

        let mut input = manager.subscribe_input();
        let still_streaming = manager.on_local_lock(&created.id).unwrap();
        assert_eq!(still_streaming.state, SessionState::Streaming);
        assert!(matches!(input.try_recv(), Ok(InputEvent::ReleaseAll)));

        let mut request = spec("client-2", true);
        request.privacy.disconnect_on_local_unlock = true;
        let created = manager.create(request, view_and_control(), 1_000).unwrap();
        manager.start(&created.id).unwrap();
        manager.mark_streaming(&created.id).unwrap();

        let ended = manager.on_local_lock(&created.id).unwrap();
        assert_eq!(ended.state, SessionState::Ended);
        assert_eq!(ended.end_reason, Some(EndReason::LocalUnlock));
    }

    #[test]
    fn state_changes_are_broadcast_in_order() {
        let manager = server_manager();
        let created = manager
            .create(spec("client-1", true), view_and_control(), 1_000)
            .unwrap();
        let mut events = manager.subscribe();
        manager.start(&created.id).unwrap();

        let mut states = Vec::new();
        loop {
            match events.try_recv() {
                Ok(SessionEvent::StateChanged { state, .. }) => states.push(state),
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => {}
            }
        }
        assert_eq!(states, vec![SessionState::Preparing, SessionState::Ready]);
    }

    #[test]
    fn list_and_get_expose_sessions() {
        let manager = server_manager();
        assert!(manager.is_empty());
        let created = manager
            .create(spec("client-1", false), PermissionSet::view_only(), 1_000)
            .unwrap();
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.list()[0].id, created.id);
        assert_eq!(manager.get(&created.id).unwrap(), created);
        assert_eq!(
            manager.get(&SessionId::new("missing")).unwrap_err().code,
            codes::NOT_FOUND
        );
    }
}
