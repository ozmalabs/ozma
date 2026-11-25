//! Meeting-detection supervisor and bounded status report.
//!
//! The meeting detector is a separate binary (`ozma-meeting-detect`) that owns
//! process scanning and calendar enrichment. This module is the opt-in bridge:
//! it may spawn that binary, poll its Unix-socket IPC snapshot, and publish a
//! bounded, typed report for the supervisor's `/status` document.
//!
//! Design rules:
//!
//! * A meeting is reported only when an IPC `Snapshot` contains an entry with
//!   `active: true`. An `active` snapshot with no meeting entry is surfaced as
//!   `unknown`, never as a fabricated detection.
//! * Socket paths, binary paths, calendar UIDs, attendee addresses and tokens
//!   never enter the report. Titles/organizers are bounded text, meetings are
//!   capped, and errors are a fixed code plus a bounded message.
//! * Child lifecycle is owned here: opt-in spawn, startup timeout, capped
//!   exponential restart backoff, and a clean kill on shutdown. Attach-only
//!   mode never spawns a binary and reports a missing socket as typed state.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    sync::watch,
};
use tracing::{info, warn};

use crate::ipc::{
    ActiveMeeting, MeetingPlatform, MeetingStatus as IpcMeetingStatus, ServerMessage,
};

/// Maximum number of active meetings copied into the report.
pub const MAX_REPORTED_MEETINGS: usize = 8;
/// Maximum characters kept from a meeting title.
pub const MAX_TITLE_CHARS: usize = 200;
/// Maximum characters kept from an organizer field.
pub const MAX_ORGANIZER_CHARS: usize = 128;
/// Maximum characters kept from a meeting id.
pub const MAX_ID_CHARS: usize = 64;
/// Maximum bytes read from one IPC connection. Bounds a hostile or buggy peer.
const MAX_IPC_FRAME_BYTES: u64 = 256 * 1024;
/// Startup socket-wait poll granularity.
const SOCKET_WAIT_STEP: Duration = Duration::from_millis(50);
/// How long to wait for a detector child to exit after a kill.
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Detector lifecycle state, independent from whether a meeting is active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingDetectState {
    /// `--meeting-detect` was not passed (the historical default).
    #[default]
    Disabled,
    /// The bridge is waiting for a child or a socket.
    Starting,
    /// A snapshot was read successfully from the detector.
    Running,
    /// The binary/socket is missing or the detector cannot be reached.
    Unavailable,
    /// The supervisor shut the bridge down.
    Stopped,
}

/// Whether a real detector snapshot currently reports a call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingState {
    /// Connected to a detector and no meeting entry is active.
    Idle,
    /// At least one real active meeting entry exists.
    Active,
    /// Not connected, or the detector reported an inconsistent snapshot.
    #[default]
    Unknown,
}

/// Typed failure codes. Values are stable so a controller can switch on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingDetectErrorCode {
    /// The configured binary does not exist.
    BinaryMissing,
    /// The configured socket does not exist.
    SocketMissing,
    /// The socket exists but refused/was reset.
    SocketUnavailable,
    /// Connecting, writing, or reading exceeded the timeout.
    Timeout,
    /// The peer sent a frame that is not a valid snapshot.
    Protocol,
    /// The child could not be started for a non-`NotFound` reason.
    SpawnFailed,
    /// The child exited while the bridge expected it to run.
    Exited,
    /// `Snapshot { status: active }` arrived with no active meeting entry.
    InconsistentSnapshot,
    /// A generic local I/O failure.
    Io,
}

/// Typed bridge error. Messages never include a socket or binary path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingDetectError {
    pub code: MeetingDetectErrorCode,
    pub message: String,
}

/// One active meeting, bounded and stripped of calendar identifiers/attendees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingReport {
    pub id: String,
    pub platform: MeetingPlatform,
    pub active: bool,
    pub started_at_unix_ms: u64,
    pub duration_s: Option<f64>,
    pub title: String,
    pub organizer: String,
    /// Number of attendees, never their addresses.
    pub attendee_count: usize,
    /// True when calendar enrichment supplied a UID.
    pub has_calendar_metadata: bool,
}

/// Bounded meeting-detection report embedded in `GET /status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingDetectStatus {
    /// Whether the operator opted in with `--meeting-detect`.
    pub enabled: bool,
    pub state: MeetingDetectState,
    pub meeting_state: MeetingState,
    /// True after a successful snapshot on the current connection.
    pub connected: bool,
    /// Whether this bridge owns (spawns) the detector child.
    pub manage_child: bool,
    pub detector_interval_secs: u64,
    pub poll_interval_ms: u64,
    pub restart_count: u64,
    /// Wall-clock time (Unix milliseconds) of the most recent successful poll.
    pub last_update_unix_ms: Option<u64>,
    /// Age of the most recent successful poll, derived at read time.
    pub last_update_age_ms: Option<u64>,
    /// Wall-clock time (Unix milliseconds) of the most recent real detection.
    pub last_detection_unix_ms: Option<u64>,
    /// The most recent real detection, retained after the call ends.
    pub last_detection: Option<MeetingReport>,
    /// Currently active meetings, bounded to [`MAX_REPORTED_MEETINGS`].
    pub meetings: Vec<MeetingReport>,
    pub error: Option<MeetingDetectError>,
}

impl Default for MeetingDetectStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            state: MeetingDetectState::Disabled,
            meeting_state: MeetingState::Unknown,
            connected: false,
            manage_child: false,
            detector_interval_secs: 0,
            poll_interval_ms: 0,
            restart_count: 0,
            last_update_unix_ms: None,
            last_update_age_ms: None,
            last_detection_unix_ms: None,
            last_detection: None,
            meetings: Vec::new(),
            error: None,
        }
    }
}

impl MeetingDetectStatus {
    /// Return a copy with `last_update_age_ms` derived from wall-clock now.
    ///
    /// The stored document deliberately does not cache a possibly stale age;
    /// status readers get an honest age for the last successful poll.
    pub fn refreshed(&self) -> Self {
        let mut copy = self.clone();
        copy.last_update_age_ms = copy
            .last_update_unix_ms
            .map(|updated| now_unix_ms().saturating_sub(updated));
        copy
    }
}

/// Default detector socket path, matching `ozma-meeting-detect`'s default.
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("ozma-meeting-detect.sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/ozma-meeting-detect.sock"))
}

/// Lifecycle and polling options for the bridge.
#[derive(Debug, Clone)]
pub struct MeetingDetectOptions {
    /// Global opt-in. False preserves the historical no-detector behavior.
    pub enabled: bool,
    /// Spawn the detector binary. False attaches to an existing socket only.
    pub manage_child: bool,
    pub binary: PathBuf,
    pub socket_path: PathBuf,
    /// Passed to the detector as `--interval`.
    pub detector_interval: Duration,
    /// Extra `--ics-path` arguments for the detector.
    pub ics_paths: Vec<PathBuf>,
    /// How often to request a fresh snapshot after the first success.
    pub poll_interval: Duration,
    /// Connect/write/read timeout for one snapshot attempt.
    pub request_timeout: Duration,
    pub restart_backoff: Duration,
    pub restart_backoff_max: Duration,
}

impl Default for MeetingDetectOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            manage_child: true,
            binary: PathBuf::from("ozma-meeting-detect"),
            socket_path: default_socket_path(),
            detector_interval: Duration::from_secs(5),
            ics_paths: Vec::new(),
            poll_interval: Duration::from_secs(2),
            request_timeout: Duration::from_secs(3),
            restart_backoff: Duration::from_secs(1),
            restart_backoff_max: Duration::from_secs(30),
        }
    }
}

/// Start the bridge and return its status receiver plus task handle.
///
/// The task stops when `shutdown` becomes true (or its sender is dropped). It
/// kills any child it spawned before returning.
pub fn start(
    options: MeetingDetectOptions,
    shutdown: watch::Receiver<bool>,
) -> (
    watch::Receiver<MeetingDetectStatus>,
    tokio::task::JoinHandle<()>,
) {
    let initial = MeetingDetectStatus {
        enabled: options.enabled,
        state: if options.enabled {
            MeetingDetectState::Starting
        } else {
            MeetingDetectState::Disabled
        },
        manage_child: options.manage_child,
        detector_interval_secs: options.detector_interval.as_secs(),
        poll_interval_ms: options.poll_interval.as_millis() as u64,
        ..MeetingDetectStatus::default()
    };
    let (tx, rx) = watch::channel(initial.clone());
    let supervisor = MeetingDetectSupervisor {
        options,
        status: initial,
        tx,
    };
    let handle = tokio::spawn(async move { supervisor.run(shutdown).await });
    (rx, handle)
}

struct MeetingDetectSupervisor {
    options: MeetingDetectOptions,
    status: MeetingDetectStatus,
    tx: watch::Sender<MeetingDetectStatus>,
}

impl MeetingDetectSupervisor {
    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        if !self.options.enabled {
            self.status.state = MeetingDetectState::Disabled;
            self.status.meeting_state = MeetingState::Unknown;
            self.publish();
            wait_for_shutdown(&mut shutdown).await;
            return;
        }

        self.status.state = MeetingDetectState::Starting;
        self.publish();
        let mut backoff = self.options.restart_backoff;

        'supervise: loop {
            if shutdown_now(&shutdown) {
                break;
            }

            let mut child = if self.options.manage_child {
                match self.spawn_child() {
                    Ok(child) => Some(child),
                    Err(error) => {
                        self.set_error(error);
                        if self.wait_backoff(&mut shutdown, &mut backoff).await {
                            break;
                        }
                        continue;
                    }
                }
            } else {
                None
            };

            let initial = tokio::select! {
                _ = wait_for_shutdown(&mut shutdown) => {
                    // Clean shutdown while waiting for the first snapshot.
                    stop_child(&mut child).await;
                    break 'supervise;
                }
                snapshot = self.initial_snapshot(&mut child) => snapshot,
            };
            match initial {
                Ok(snapshot) => {
                    self.apply_snapshot(snapshot);
                    backoff = self.options.restart_backoff;
                }
                Err(error) => {
                    self.set_error(error);
                    stop_child(&mut child).await;
                    if self.wait_backoff(&mut shutdown, &mut backoff).await {
                        break;
                    }
                    continue;
                }
            }
            loop {
                if shutdown_now(&shutdown) {
                    break;
                }

                tokio::select! {
                    _ = wait_for_shutdown(&mut shutdown) => break,
                    _ = tokio::time::sleep(self.options.poll_interval) => {}
                }

                if shutdown_now(&shutdown) {
                    break;
                }

                if let Some(exit) = self.child_exit(&mut child) {
                    self.set_error(MeetingDetectError {
                        code: MeetingDetectErrorCode::Exited,
                        message: format!("meeting detector exited ({exit})"),
                    });
                    break;
                }

                match fetch_snapshot(&self.options.socket_path, self.options.request_timeout).await
                {
                    Ok(snapshot) => self.apply_snapshot(snapshot),
                    Err(error) => self.set_error(error),
                }
            }

            stop_child(&mut child).await;
            if shutdown_now(&shutdown) {
                break;
            }
            if self.wait_backoff(&mut shutdown, &mut backoff).await {
                break;
            }
        }

        self.status.state = MeetingDetectState::Stopped;
        self.status.connected = false;
        self.status.meeting_state = MeetingState::Unknown;
        // A stopped bridge must not claim a live call.
        self.status.meetings.clear();
        self.publish();
    }

    fn spawn_child(&self) -> Result<Child, MeetingDetectError> {
        let mut command = Command::new(&self.options.binary);
        command
            .arg("--interval")
            .arg(self.options.detector_interval.as_secs().max(1).to_string())
            .arg("--socket")
            .arg(&self.options.socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for path in &self.options.ics_paths {
            command.arg("--ics-path").arg(path);
        }

        command
            .spawn()
            .map(|child| {
                info!("Meeting detector child started");
                child
            })
            .map_err(|error| {
                use std::io::ErrorKind;
                let (code, message) = match error.kind() {
                    ErrorKind::NotFound => (
                        MeetingDetectErrorCode::BinaryMissing,
                        "meeting detector binary was not found".to_owned(),
                    ),
                    ErrorKind::PermissionDenied => (
                        MeetingDetectErrorCode::SpawnFailed,
                        "meeting detector binary is not executable".to_owned(),
                    ),
                    other => (
                        MeetingDetectErrorCode::SpawnFailed,
                        format!("could not start the meeting detector ({other:?})"),
                    ),
                };
                MeetingDetectError { code, message }
            })
    }

    /// Wait for the socket, then read one snapshot, bounded by the startup
    /// timeout. A child that exits early is reported as `exited`.
    async fn initial_snapshot(
        &mut self,
        child: &mut Option<Child>,
    ) -> Result<ServerMessage, MeetingDetectError> {
        let deadline = tokio::time::Instant::now() + self.options.request_timeout;
        let mut last_error = MeetingDetectError {
            code: MeetingDetectErrorCode::SocketMissing,
            message: "meeting detector socket was not found".to_owned(),
        };

        loop {
            if let Some(exit) = self.child_exit(child) {
                return Err(MeetingDetectError {
                    code: MeetingDetectErrorCode::Exited,
                    message: format!("meeting detector exited before its socket appeared ({exit})"),
                });
            }

            let now = tokio::time::Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                last_error.message =
                    "meeting detector socket was not ready before the startup timeout".to_owned();
                return Err(last_error);
            }

            match fetch_snapshot(
                &self.options.socket_path,
                remaining.min(Duration::from_millis(500)),
            )
            .await
            {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) => {
                    let retry = matches!(
                        error.code,
                        MeetingDetectErrorCode::SocketMissing
                            | MeetingDetectErrorCode::SocketUnavailable
                            | MeetingDetectErrorCode::Timeout
                    );
                    last_error = error;
                    if !retry {
                        return Err(last_error);
                    }
                }
            }

            let sleep_for = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(SOCKET_WAIT_STEP);
            if sleep_for.is_zero() {
                return Err(last_error);
            }
            tokio::time::sleep(sleep_for).await;
        }
    }

    fn child_exit(&self, child: &mut Option<Child>) -> Option<String> {
        let Some(child) = child.as_mut() else {
            return None;
        };
        match child.try_wait() {
            Ok(Some(status)) => Some(exit_description(status)),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    /// Apply one detector snapshot. Only a real `active: true` meeting entry can
    /// move `meeting_state` to `active`.
    fn apply_snapshot(&mut self, message: ServerMessage) {
        let ServerMessage::Snapshot { status, meetings } = message else {
            return;
        };

        let mut active: Vec<MeetingReport> = meetings
            .iter()
            .filter(|meeting| meeting.active)
            .map(MeetingReport::from_ipc)
            .collect();
        active.sort_by(|left, right| right.started_at_unix_ms.cmp(&left.started_at_unix_ms));
        active.truncate(MAX_REPORTED_MEETINGS);

        let inconsistent = status == IpcMeetingStatus::Active && active.is_empty();
        self.status.meeting_state = if !active.is_empty() {
            MeetingState::Active
        } else if inconsistent {
            MeetingState::Unknown
        } else {
            MeetingState::Idle
        };

        if let Some(newest) = active.first() {
            self.status.last_detection_unix_ms = Some(newest.started_at_unix_ms);
            self.status.last_detection = Some(newest.clone());
        }
        self.status.meetings = active;
        self.status.state = MeetingDetectState::Running;
        self.status.connected = true;
        self.status.last_update_unix_ms = Some(now_unix_ms());
        self.status.last_update_age_ms = Some(0);
        self.status.error = inconsistent.then(|| MeetingDetectError {
            code: MeetingDetectErrorCode::InconsistentSnapshot,
            message: "detector reported an active call without a meeting entry".to_owned(),
        });
        self.publish();
    }

    fn set_error(&mut self, error: MeetingDetectError) {
        warn!(code = ?error.code, "Meeting detector unavailable: {}", error.message);
        self.status.state = MeetingDetectState::Unavailable;
        self.status.connected = false;
        self.status.meeting_state = MeetingState::Unknown;
        self.status.meetings.clear();
        self.status.error = Some(error);
        self.publish();
    }

    async fn wait_backoff(
        &mut self,
        shutdown: &mut watch::Receiver<bool>,
        backoff: &mut Duration,
    ) -> bool {
        let delay = *backoff;
        *backoff = backoff
            .saturating_mul(2)
            .min(self.options.restart_backoff_max);
        self.status.restart_count = self.status.restart_count.saturating_add(1);
        self.publish();
        tokio::select! {
            _ = wait_for_shutdown(shutdown) => true,
            _ = tokio::time::sleep(delay) => false,
        }
    }

    fn publish(&self) {
        let _ = self.tx.send(self.status.clone());
    }
}

impl MeetingReport {
    fn from_ipc(meeting: &ActiveMeeting) -> Self {
        Self {
            id: bounded_text(&meeting.id, MAX_ID_CHARS),
            platform: meeting.platform.clone(),
            active: meeting.active,
            started_at_unix_ms: meeting.started_at.timestamp_millis().max(0) as u64,
            duration_s: meeting
                .duration_s
                .filter(|duration| duration.is_finite() && *duration >= 0.0),
            title: bounded_text(&meeting.title, MAX_TITLE_CHARS),
            organizer: bounded_text(&meeting.organizer, MAX_ORGANIZER_CHARS),
            attendee_count: meeting.attendees.len().min(1024),
            has_calendar_metadata: !meeting.calendar_uid.is_empty(),
        }
    }
}

/// Read one snapshot from the detector socket.
///
/// One connection carries one request and at most [`MAX_IPC_FRAME_BYTES`] of
/// response data. Heartbeats are skipped; any other message is a protocol
/// error because the bridge only asks for a snapshot.
pub async fn fetch_snapshot(
    socket_path: &Path,
    timeout: Duration,
) -> Result<ServerMessage, MeetingDetectError> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|_| MeetingDetectError {
            code: MeetingDetectErrorCode::Timeout,
            message: "meeting detector connection timed out".to_owned(),
        })?
        .map_err(map_connect_error)?;

    let (reader, mut writer) = stream.into_split();
    if let Err(error) =
        tokio::time::timeout(timeout, writer.write_all(b"{\"type\":\"get_snapshot\"}\n"))
            .await
            .map_err(|_| MeetingDetectError {
                code: MeetingDetectErrorCode::Timeout,
                message: "meeting detector request timed out".to_owned(),
            })?
    {
        return Err(MeetingDetectError {
            code: MeetingDetectErrorCode::Io,
            message: format!("meeting detector write failed ({})", error.kind()),
        });
    }

    let limited = reader.take(MAX_IPC_FRAME_BYTES);
    let mut lines = BufReader::new(limited).lines();
    let read = async {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let message: ServerMessage =
                        serde_json::from_str(&line).map_err(|_| MeetingDetectError {
                            code: MeetingDetectErrorCode::Protocol,
                            message: "meeting detector sent an invalid frame".to_owned(),
                        })?;
                    match message {
                        snapshot @ ServerMessage::Snapshot { .. } => return Ok(snapshot),
                        ServerMessage::Heartbeat { .. } => continue,
                        _ => {
                            return Err(MeetingDetectError {
                                code: MeetingDetectErrorCode::Protocol,
                                message: "meeting detector sent an unexpected frame".to_owned(),
                            })
                        }
                    }
                }
                Ok(None) => {
                    return Err(MeetingDetectError {
                        code: MeetingDetectErrorCode::Protocol,
                        message: "meeting detector closed before sending a snapshot".to_owned(),
                    })
                }
                Err(error) => {
                    return Err(MeetingDetectError {
                        code: MeetingDetectErrorCode::Io,
                        message: format!("meeting detector read failed ({})", error.kind()),
                    })
                }
            }
        }
    };

    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| MeetingDetectError {
            code: MeetingDetectErrorCode::Timeout,
            message: "meeting detector snapshot timed out".to_owned(),
        })?
}

fn map_connect_error(error: std::io::Error) -> MeetingDetectError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => MeetingDetectError {
            code: MeetingDetectErrorCode::SocketMissing,
            message: "meeting detector socket was not found".to_owned(),
        },
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset => MeetingDetectError {
            code: MeetingDetectErrorCode::SocketUnavailable,
            message: "meeting detector socket is not accepting connections".to_owned(),
        },
        other => MeetingDetectError {
            code: MeetingDetectErrorCode::Io,
            message: format!("meeting detector connection failed ({other:?})"),
        },
    }
}

async fn stop_child(child: &mut Option<Child>) {
    let Some(mut child) = child.take() else {
        return;
    };
    let _ = child.start_kill();
    let _ = tokio::time::timeout(CHILD_STOP_TIMEOUT, child.wait()).await;
}

fn exit_description(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_owned(),
    }
}

fn shutdown_now(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    // A dropped sender means the owner is gone; stopping is safer than running
    // an unsupervised detector forever.
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}

fn bounded_text(value: &str, limit: usize) -> String {
    let mut output = String::new();
    let mut count = 0;
    for character in value.chars() {
        if count >= limit {
            break;
        }
        if character.is_control() {
            output.push(' ');
        } else {
            output.push(character);
        }
        count += 1;
    }
    output.trim().to_owned()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fake_detector(path: PathBuf, responses: Vec<ServerMessage>) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(&path).expect("bind fake detector");
        tokio::spawn(async move {
            let mut remaining = responses.into_iter();
            while let Ok((stream, _)) = listener.accept().await {
                let Some(response) = remaining.next() else {
                    break;
                };
                let (_reader, mut writer) = stream.into_split();
                let mut json = serde_json::to_string(&response).expect("serialize frame");
                json.push('\n');
                let _ = writer.write_all(json.as_bytes()).await;
            }
        })
    }

    fn meeting(id: &str, platform: MeetingPlatform) -> ActiveMeeting {
        let mut meeting = ActiveMeeting::new(id.to_owned(), platform);
        meeting.started_at = Utc::now();
        meeting
    }

    fn snapshot(status: IpcMeetingStatus, meetings: Vec<ActiveMeeting>) -> ServerMessage {
        ServerMessage::Snapshot { status, meetings }
    }

    #[tokio::test]
    async fn missing_socket_is_typed_and_does_not_contain_a_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = fetch_snapshot(&dir.path().join("absent.sock"), Duration::from_millis(200))
            .await
            .expect_err("missing socket must fail");

        assert_eq!(error.code, MeetingDetectErrorCode::SocketMissing);
        assert!(!error
            .message
            .contains(dir.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn missing_binary_is_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (rx, handle) = start(
            MeetingDetectOptions {
                enabled: true,
                manage_child: true,
                binary: dir.path().join("does-not-exist"),
                socket_path: dir.path().join("meeting.sock"),
                restart_backoff: Duration::from_millis(20),
                restart_backoff_max: Duration::from_millis(40),
                ..MeetingDetectOptions::default()
            },
            shutdown_rx,
        );

        let status = wait_for_state(&rx, MeetingDetectState::Unavailable).await;
        assert_eq!(
            status.error.as_ref().map(|error| error.code),
            Some(MeetingDetectErrorCode::BinaryMissing)
        );
        assert!(status.meetings.is_empty());
        assert_eq!(status.meeting_state, MeetingState::Unknown);

        let _ = handle.abort();
    }

    #[tokio::test]
    async fn attach_mode_missing_socket_is_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (status_rx, handle) = start(
            MeetingDetectOptions {
                enabled: true,
                manage_child: false,
                socket_path: dir.path().join("absent.sock"),
                poll_interval: Duration::from_millis(20),
                request_timeout: Duration::from_millis(100),
                restart_backoff: Duration::from_millis(20),
                restart_backoff_max: Duration::from_millis(40),
                ..MeetingDetectOptions::default()
            },
            shutdown_rx,
        );

        let status = wait_for_state(&status_rx, MeetingDetectState::Unavailable).await;
        assert_eq!(
            status.error.as_ref().map(|error| error.code),
            Some(MeetingDetectErrorCode::SocketMissing)
        );
        assert!(!status.connected);
        assert!(status.meetings.is_empty());
        assert!(!status
            .error
            .as_ref()
            .expect("typed error")
            .message
            .contains(dir.path().to_string_lossy().as_ref()));

        shutdown_tx.send(true).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("bridge stops")
            .expect("bridge task");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_child_is_started_and_killed_on_shutdown() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-meeting-detect");
        let pid_file = dir.path().join("detector.pid");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > '{}'\nsleep 60\n", pid_file.display()),
        )
        .expect("write fake detector");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make fake detector executable");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (status_rx, handle) = start(
            MeetingDetectOptions {
                enabled: true,
                manage_child: true,
                binary: script,
                socket_path: dir.path().join("absent.sock"),
                poll_interval: Duration::from_millis(20),
                request_timeout: Duration::from_millis(500),
                restart_backoff: Duration::from_millis(20),
                restart_backoff_max: Duration::from_millis(40),
                ..MeetingDetectOptions::default()
            },
            shutdown_rx,
        );

        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake detector writes its pid");

        // The socket never appears, so the report stays unavailable while the
        // managed child is alive. It must not be mistaken for a detection.
        let status = wait_for_state(&status_rx, MeetingDetectState::Unavailable).await;
        assert!(status.meetings.is_empty());

        shutdown_tx.send(true).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("bridge stops")
            .expect("bridge task");

        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "managed detector child must be killed and reaped"
        );
    }

    #[tokio::test]
    async fn idle_snapshot_never_fabricates_a_meeting() {
        let mut supervisor = unstarted_supervisor();
        supervisor.apply_snapshot(snapshot(IpcMeetingStatus::Idle, Vec::new()));

        assert_eq!(supervisor.status.state, MeetingDetectState::Running);
        assert_eq!(supervisor.status.meeting_state, MeetingState::Idle);
        assert!(supervisor.status.meetings.is_empty());
        assert!(supervisor.status.last_detection.is_none());
        assert!(supervisor.status.error.is_none());
    }

    #[tokio::test]
    async fn active_snapshot_without_a_meeting_is_not_reported_as_active() {
        let mut supervisor = unstarted_supervisor();
        supervisor.apply_snapshot(snapshot(IpcMeetingStatus::Active, Vec::new()));

        assert_eq!(supervisor.status.meeting_state, MeetingState::Unknown);
        assert!(supervisor.status.meetings.is_empty());
        assert_eq!(
            supervisor.status.error.as_ref().map(|error| error.code),
            Some(MeetingDetectErrorCode::InconsistentSnapshot)
        );
    }

    #[tokio::test]
    async fn active_meeting_is_reported_with_bounded_metadata() {
        let mut supervisor = unstarted_supervisor();
        let mut meeting = meeting("zoom-1", MeetingPlatform::Zoom);
        meeting.title = "x".repeat(10_000);
        meeting.organizer = "organizer@example.test".to_owned();
        meeting.attendees = vec![
            "a@example.test".to_owned(),
            "b@example.test".to_owned(),
            "c@example.test".to_owned(),
        ];
        meeting.calendar_uid = "calendar-uid".to_owned();

        supervisor.apply_snapshot(snapshot(IpcMeetingStatus::Active, vec![meeting]));

        assert_eq!(supervisor.status.meeting_state, MeetingState::Active);
        let reported = &supervisor.status.meetings[0];
        assert_eq!(reported.platform, MeetingPlatform::Zoom);
        assert_eq!(reported.title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(reported.attendee_count, 3);
        assert!(reported.has_calendar_metadata);
        assert!(supervisor.status.last_detection.is_some());
        assert!(supervisor.status.last_detection_unix_ms.is_some());
    }

    #[tokio::test]
    async fn report_is_capped_at_the_documented_meeting_count() {
        let mut supervisor = unstarted_supervisor();
        let meetings = (0..(MAX_REPORTED_MEETINGS + 4))
            .map(|index| meeting(&format!("zoom-{index}"), MeetingPlatform::Zoom))
            .collect();
        supervisor.apply_snapshot(snapshot(IpcMeetingStatus::Active, meetings));

        assert_eq!(supervisor.status.meetings.len(), MAX_REPORTED_MEETINGS);
    }

    #[tokio::test]
    async fn bridge_start_and_stop_polls_a_fake_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("meeting.sock");
        let detector = fake_detector(
            socket.clone(),
            vec![
                snapshot(IpcMeetingStatus::Idle, Vec::new()),
                snapshot(
                    IpcMeetingStatus::Active,
                    vec![meeting("teams-1", MeetingPlatform::Teams)],
                ),
            ],
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (status_rx, handle) = start(
            MeetingDetectOptions {
                enabled: true,
                manage_child: false,
                socket_path: socket,
                poll_interval: Duration::from_millis(20),
                request_timeout: Duration::from_millis(500),
                restart_backoff: Duration::from_millis(20),
                restart_backoff_max: Duration::from_millis(40),
                ..MeetingDetectOptions::default()
            },
            shutdown_rx,
        );

        let active = wait_for_meeting_state(&status_rx, MeetingState::Active).await;
        assert!(active.connected);
        assert_eq!(active.meetings[0].platform, MeetingPlatform::Teams);

        shutdown_tx.send(true).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("bridge stops")
            .expect("bridge task");
        detector.abort();

        let stopped = status_rx.borrow().clone();
        assert_eq!(stopped.state, MeetingDetectState::Stopped);
        assert!(stopped.meetings.is_empty());
        assert_eq!(stopped.meeting_state, MeetingState::Unknown);
    }

    #[tokio::test]
    async fn disabled_bridge_reports_disabled_without_touching_the_socket() {
        let (status_rx, handle) = start(MeetingDetectOptions::default(), watch::channel(false).1);
        let status = status_rx.borrow().clone();
        assert_eq!(status.state, MeetingDetectState::Disabled);
        assert!(!status.enabled);
        assert!(status.meetings.is_empty());
        drop(handle);
    }

    fn unstarted_supervisor() -> MeetingDetectSupervisor {
        let (tx, _rx) = watch::channel(MeetingDetectStatus::default());
        MeetingDetectSupervisor {
            options: MeetingDetectOptions::default(),
            status: MeetingDetectStatus::default(),
            tx,
        }
    }

    async fn wait_for_state(
        receiver: &watch::Receiver<MeetingDetectStatus>,
        state: MeetingDetectState,
    ) -> MeetingDetectStatus {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = receiver.borrow().clone();
                if status.state == state {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("state reached")
    }

    async fn wait_for_meeting_state(
        receiver: &watch::Receiver<MeetingDetectStatus>,
        state: MeetingState,
    ) -> MeetingDetectStatus {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = receiver.borrow().clone();
                if status.meeting_state == state {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("meeting state reached")
    }
}
