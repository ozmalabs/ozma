// SPDX-License-Identifier: AGPL-3.0-only
//! Session vocabulary and valid state transitions.

use serde::{Deserialize, Serialize};

use crate::display::{DisplayEvent, DisplayMode, VirtualDisplaySpec};
use crate::error::ApiError;
use crate::ids::{ClientId, SessionId, TrackId};
use crate::media::{AudioFormat, MediaFormat};
use crate::permission::{Permission, PermissionSet};

/// Lifecycle of a session. A connection is disposable; the session is the
/// unit of consent, privacy, resume, and audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Offered,
    PendingConsent,
    Preparing,
    Ready,
    Streaming,
    Degraded,
    Draining,
    Ended,
    Rejected,
    TimedOut,
    Failed,
}

impl SessionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionState::Ended
                | SessionState::Rejected
                | SessionState::TimedOut
                | SessionState::Failed
        )
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            SessionState::Preparing
                | SessionState::Ready
                | SessionState::Streaming
                | SessionState::Degraded
        )
    }

    /// Whether a transition is legal. This is deliberately small and explicit
    /// so that API handlers and tests share one source of truth.
    pub fn can_transition_to(self, next: SessionState) -> bool {
        use SessionState::*;
        match self {
            Created => matches!(next, Offered | Preparing | Rejected | Ended | Failed),
            Offered => matches!(
                next,
                PendingConsent | Preparing | Rejected | TimedOut | Ended | Failed
            ),
            PendingConsent => matches!(next, Preparing | Rejected | TimedOut | Ended | Failed),
            Preparing => matches!(next, Ready | Failed | Ended),
            Ready => matches!(next, Streaming | Ended | Failed),
            Streaming => matches!(next, Degraded | Draining | Ended),
            Degraded => matches!(next, Streaming | Draining | Ended),
            Draining => matches!(next, Ended),
            Ended | Rejected | TimedOut | Failed => false,
        }
    }
}

/// Whether a track is an existing capture source or a newly created display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Capture,
    VirtualDisplay,
}

/// One media track in a session request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackSpec {
    pub track_id: TrackId,
    pub kind: TrackKind,
    /// Capture source id for `Capture` tracks; optional for virtual displays.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Display request for `VirtualDisplay` tracks.
    #[serde(default)]
    pub display: Option<VirtualDisplaySpec>,
    #[serde(default)]
    pub format: Option<MediaFormat>,
    /// A mode preference for a virtual display, or a requested capture mode.
    #[serde(default)]
    pub mode: Option<DisplayMode>,
    #[serde(default)]
    pub hdr: bool,
}

/// Audio request for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSpec {
    pub enabled: bool,
    pub mic: bool,
    #[serde(default)]
    pub format: AudioFormat,
}

/// Pointer behaviour requested by the client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerMode {
    #[default]
    None,
    Absolute,
    Relative,
}

/// Input permissions requested for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSpec {
    pub keyboard: bool,
    #[serde(default)]
    pub pointer: PointerMode,
    pub gamepad: bool,
    pub touch: bool,
    pub pen: bool,
    pub clipboard: bool,
    /// Only the controller may inject input; viewers are read-only.
    #[serde(default = "default_true")]
    pub owner_only: bool,
}

fn default_true() -> bool {
    true
}

/// What happens to the local display while a remote session is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalDisplayAction {
    #[default]
    Unchanged,
    Blank,
    Lock,
    Disable,
}

/// When consent is required before a session becomes active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentRequirement {
    #[default]
    Never,
    Workstation,
    Always,
}

/// Privacy policy for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyPolicy {
    #[serde(default)]
    pub local_display: LocalDisplayAction,
    #[serde(default)]
    pub consent: ConsentRequirement,
    #[serde(default = "default_true")]
    pub indicate: bool,
    #[serde(default)]
    pub disconnect_on_local_unlock: bool,
}

/// Transport preference. `Auto` lets the director measure and choose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportPreference {
    #[default]
    Auto,
    Quic,
    Udp,
    TcpInterleaved,
    Relay,
    Loopback,
}

/// A request to create a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub client_id: ClientId,
    pub principal: String,
    #[serde(default)]
    pub tracks: Vec<TrackSpec>,
    #[serde(default)]
    pub audio: AudioSpec,
    #[serde(default)]
    pub input: InputSpec,
    #[serde(default)]
    pub privacy: PrivacyPolicy,
    #[serde(default)]
    pub transport: TransportPreference,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
}

fn default_ttl() -> u64 {
    3600
}

impl SessionSpec {
    /// The permissions a session needs before it can become active.
    pub fn required_permissions(&self) -> PermissionSet {
        let mut set = PermissionSet::empty();
        set.insert(Permission::View);
        if self.audio.enabled {
            set.insert(Permission::Audio);
        }
        if self.audio.mic {
            set.insert(Permission::Mic);
        }
        if self.input.keyboard
            || self.input.pointer != PointerMode::None
            || self.input.gamepad
            || self.input.touch
            || self.input.pen
        {
            set.insert(Permission::Control);
        }
        if self.input.clipboard {
            set.insert(Permission::ClipboardRead);
            set.insert(Permission::ClipboardWrite);
        }
        if self
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::VirtualDisplay)
        {
            set.insert(Permission::DisplayAttach);
        }
        set
    }
}

/// A participant in a session. Exactly one controller injects input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub client_id: ClientId,
    pub role: ParticipantRole,
    pub connected: bool,
    pub last_seen: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    Controller,
    Viewer,
    Recorder,
}

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ClientDisconnected,
    IdleTimeout,
    ConsentDenied,
    ConsentTimedOut,
    CaptureLost,
    EncoderFailed,
    TransportLost,
    DisplayRemoved,
    Superseded,
    LocalUnlock,
    AdminTerminated,
    Rejected,
    Failed,
}

/// A pending consent request shown to the local operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRequest {
    pub session_id: SessionId,
    pub client_id: ClientId,
    pub principal: String,
    pub requested: PermissionSet,
    pub created_at: u64,
    pub expires_at: u64,
}

/// A live session record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub state: SessionState,
    pub spec: SessionSpec,
    pub permissions: PermissionSet,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub ended_at: Option<u64>,
    #[serde(default)]
    pub end_reason: Option<EndReason>,
    #[serde(default)]
    pub resume_token: Option<String>,
    #[serde(default)]
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub consent: Option<ConsentRequest>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

impl Session {
    pub fn new(
        id: impl Into<SessionId>,
        spec: SessionSpec,
        permissions: PermissionSet,
        now: u64,
    ) -> Self {
        Self {
            id: id.into(),
            state: SessionState::Created,
            spec,
            permissions,
            created_at: now,
            updated_at: now,
            started_at: None,
            ended_at: None,
            end_reason: None,
            resume_token: None,
            participants: Vec::new(),
            consent: None,
            error: None,
        }
    }

    pub fn can_start(&self) -> bool {
        self.spec
            .required_permissions()
            .is_subset_of(&self.permissions)
    }
}

/// Events emitted by the session manager. Adapters translate these to
/// WebSocket/SSE, audit logs, and controller state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    StateChanged {
        session_id: SessionId,
        state: SessionState,
    },
    ConsentRequested {
        request: ConsentRequest,
    },
    ConsentDecided {
        session_id: SessionId,
        approved: bool,
        by: String,
    },
    TrackStarted {
        session_id: SessionId,
        track_id: TrackId,
    },
    TrackStopped {
        session_id: SessionId,
        track_id: TrackId,
        reason: EndReason,
    },
    InputReleased {
        session_id: SessionId,
        reason: EndReason,
    },
    DisplayChanged {
        session_id: SessionId,
        event: DisplayEvent,
    },
    Error {
        session_id: SessionId,
        error: ApiError,
    },
    Ended {
        session_id: SessionId,
        reason: EndReason,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_state_transitions() {
        assert!(SessionState::Created.can_transition_to(SessionState::Offered));
        assert!(SessionState::Preparing.can_transition_to(SessionState::Ready));
        assert!(SessionState::Streaming.can_transition_to(SessionState::Degraded));
        assert!(!SessionState::Ended.can_transition_to(SessionState::Streaming));
        assert!(!SessionState::Ready.can_transition_to(SessionState::Preparing));
    }

    #[test]
    fn required_permissions_follow_the_spec() {
        let spec = SessionSpec {
            client_id: ClientId::new("client-1"),
            principal: "alice".into(),
            tracks: vec![TrackSpec {
                track_id: TrackId::new("video0"),
                kind: TrackKind::VirtualDisplay,
                source_id: None,
                display: Some(VirtualDisplaySpec::new(
                    "remote-1",
                    "Laptop",
                    (300, 190),
                    DisplayMode::new(1920, 1200, 60),
                )),
                format: None,
                mode: None,
                hdr: false,
            }],
            audio: AudioSpec {
                enabled: true,
                mic: false,
                format: AudioFormat::opus(),
            },
            input: InputSpec {
                keyboard: true,
                pointer: PointerMode::Absolute,
                ..Default::default()
            },
            privacy: PrivacyPolicy::default(),
            transport: TransportPreference::Auto,
            ttl_seconds: 60,
        };
        let required = spec.required_permissions();
        assert!(required.contains(Permission::View));
        assert!(required.contains(Permission::Audio));
        assert!(required.contains(Permission::Control));
        assert!(required.contains(Permission::DisplayAttach));
    }
}
