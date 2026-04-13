//! IPC types and (de)serialisation for the meeting-detection service.
//!
//! The soft-node exposes a Unix-domain socket at
//! `$XDG_RUNTIME_DIR/ozma-meeting-detect.sock`
//! (falling back to `/tmp/ozma-meeting-detect.sock`).
//!
//! Each connected client receives newline-delimited JSON [`ServerMessage`]
//! frames. Clients may send [`ClientMessage`] frames to query state or force
//! a re-scan.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Meeting platform ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingPlatform {
    Zoom,
    Teams,
    Meet,
    Slack,
    Discord,
    FaceTime,
    Unknown,
}

impl std::fmt::Display for MeetingPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Zoom     => "zoom",
            Self::Teams    => "teams",
            Self::Meet     => "meet",
            Self::Slack    => "slack",
            Self::Discord  => "discord",
            Self::FaceTime => "facetime",
            Self::Unknown  => "unknown",
        };
        f.write_str(s)
    }
}

// ── Active meeting ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveMeeting {
    pub id:         String,
    pub platform:   MeetingPlatform,
    pub active:     bool,
    pub started_at: DateTime<Utc>,
    pub ended_at:   Option<DateTime<Utc>>,
    /// Duration in seconds; `None` while the meeting is still active.
    pub duration_s: Option<f64>,

    // Calendar metadata (populated when an ICS event matches)
    pub title:        String,
    pub organizer:    String,
    pub attendees:    Vec<String>,
    pub calendar_uid: String,
}

impl ActiveMeeting {
    pub fn new(id: String, platform: MeetingPlatform) -> Self {
        Self {
            id,
            platform,
            active:       true,
            started_at:   Utc::now(),
            ended_at:     None,
            duration_s:   None,
            title:        String::new(),
            organizer:    String::new(),
            attendees:    Vec::new(),
            calendar_uid: String::new(),
        }
    }

    /// Mark the meeting as finished and record its duration.
    pub fn finish(&mut self) {
        let now = Utc::now();
        self.active     = false;
        self.ended_at   = Some(now);
        self.duration_s = Some(
            (now - self.started_at).num_milliseconds() as f64 / 1000.0,
        );
    }
}

// ── Overall status ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    /// At least one call is active.
    Active,
    /// No calls detected.
    Idle,
}

// ── IPC messages ─────────────────────────────────────────────────────────────

/// Messages sent from the server to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Full state snapshot — sent on connect and after every change.
    Snapshot {
        status:   MeetingStatus,
        meetings: Vec<ActiveMeeting>,
    },
    /// A new meeting was detected.
    MeetingStarted { meeting: ActiveMeeting },
    /// A previously active meeting ended.
    MeetingEnded   { meeting: ActiveMeeting },
    /// Periodic heartbeat so clients can detect a stale connection.
    Heartbeat      { ts: DateTime<Utc> },
}

/// Messages sent from clients to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Request an immediate snapshot.
    GetSnapshot,
    /// Force an immediate re-scan (useful for testing).
    ForceScan,
}
