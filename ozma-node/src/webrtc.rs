// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Controller-compatibility WebRTC through the existing external GStreamer
//! helper.
//!
//! This is the transition path from `docs/rust-reconciliation-plan.md` §3
//! Phase 2: the Python soft node already speaks to a GStreamer worker over a
//! Unix socket and a shared-memory framebuffer, so `ozma-node` supervises the
//! same helper first and replaces it behind the same endpoints later with a
//! native `webrtc-rs`/`str0m` implementation.
//!
//! The wire contract mirrors `softnode/gst_webrtc_worker.py`:
//!
//! * the helper is spawned with
//!   `[program] [script] --sock <path> --shm <path> --width <w> --height <h> --fps <fps>`;
//! * the node writes packed BGRA frames to `shm_path` (whole-file replacement
//!   via a temporary file and `rename`, so the worker never reads a torn frame);
//! * signaling is newline-delimited JSON over `socket_path`:
//!   `{"type":"offer","sdp":"..."}` → `{"type":"answer","sdp":"...","session":"..."}`;
//!   `{"type":"add-ice-candidate","session":"...","candidate":"...",
//!   "sdpMLineIndex":0}` → `{"ok":true}`. The `session`/candidate messages are
//!   optional: a worker that omits them keeps the original embedded-candidate
//!   behavior, and the node's browser route only trickles when a session id is
//!   returned.
//!
//! STUN/TURN are opt-in. When [`WebRtcConfig::stun_server`] or
//! [`WebRtcConfig::turn_server`] is set, the helper also receives
//! `--stun-server` and/or `--turn-server`/`--turn-user`/`--turn-password`;
//! without them the helper argv and host-candidate-only pipeline are unchanged.
//!
//! WebRTC is **disabled unless a helper program is configured**. There is no
//! implicit Python default: a node without `--webrtc-helper` reports
//! `webrtc: "not-configured"` and answers the compatibility routes with a typed
//! 503. A configured helper that cannot be spawned or connected is reported as
//! `failed`, never `running`.
//!
//! Bitrate changes are sent as `{"type":"bitrate","bitrate":<bps>}`. The
//! worker clamps the value to 500 kbps–50 Mbps, applies it to every live
//! session's video encoder, and answers `{"ok":true,"bitrate":<bps>}`. Only
//! that acknowledgement is success; a worker that ignores the message, answers
//! `{"ok":false,...}`, or returns an unknown shape is a typed error.
//!
//! Audio is opt-in. When [`WebRtcConfig::audio_source`] is set, the helper is
//! also given `--audio-source <monitor>` and `--audio-backend <auto|pulse|
//! pipewire>`; the worker adds an Opus/`rtpopuspay` branch from that monitor
//! only if the required GStreamer elements exist. Without it the helper argv
//! and pipeline are unchanged, and video still runs if audio is unavailable.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::debug;

/// Default WebRTC helper width.
pub const DEFAULT_WIDTH: u32 = 640;
/// Default WebRTC helper height.
pub const DEFAULT_HEIGHT: u32 = 480;
/// Default WebRTC helper frame rate.
pub const DEFAULT_FPS: u32 = 30;
/// Default WebRTC H.264 bitrate in kbps.
pub const DEFAULT_BITRATE_KBPS: u32 = 4_000;
/// Default bounded wait for the helper's signaling socket.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Default bounded wait for one signaling response.
pub const DEFAULT_OFFER_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest error string retained for `/api/v1/connection`.
const MAX_STATUS_ERROR_CHARS: usize = 512;
/// How long `stop` waits for the frame feeder before aborting it.
const STOP_TASK_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval while waiting for the helper's Unix socket.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Audio capture backend requested from the external helper.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum WebRtcAudioBackend {
    /// Prefer `pulsesrc`, falling back to `pipewiresrc` when only that exists.
    #[default]
    Auto,
    /// Use PulseAudio's `pulsesrc` only.
    Pulse,
    /// Use PipeWire's `pipewiresrc` only.
    #[value(name = "pipewire")]
    PipeWire,
}

impl WebRtcAudioBackend {
    /// Helper CLI token for this backend.
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pulse => "pulse",
            Self::PipeWire => "pipewire",
        }
    }
}

/// Configuration for the external WebRTC helper.
///
/// `helper_program` is required and has no default: WebRTC stays disabled
/// unless the operator points the node at a helper. `helper_script` is passed
/// as the first argument (for example the Python worker), which lets the
/// program be an interpreter.
///
/// ICE server configuration is optional. With no STUN/TURN values the helper
/// argv is byte-for-byte the historical video/audio argv and `webrtcbin` keeps
/// its default host-candidate gathering. TURN credentials are held separately
/// from the server address so the address can be logged while the credentials
/// are redacted from `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct WebRtcConfig {
    /// Helper executable (for example `/usr/bin/python3`).
    pub helper_program: PathBuf,
    /// Optional first argument (for example `softnode/gst_webrtc_worker.py`).
    pub helper_script: Option<PathBuf>,
    /// Unix socket receiving newline-delimited signaling JSON.
    pub socket_path: PathBuf,
    /// Shared-memory file receiving packed BGRA frames.
    pub shm_path: PathBuf,
    /// Frame width advertised to the worker and written to shared memory.
    pub width: u32,
    /// Frame height advertised to the worker and written to shared memory.
    pub height: u32,
    /// Frame rate for the shared-memory feeder.
    pub fps: u32,
    /// Initial/target H.264 bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Optional PulseAudio/PipeWire monitor source, for example
    /// `ozma-vm1.monitor` (the `<sink>.monitor` convention).
    ///
    /// `None` keeps the helper video-only; audio is never implied.
    pub audio_source: Option<String>,
    /// Capture backend for [`WebRtcConfig::audio_source`].
    pub audio_backend: WebRtcAudioBackend,
    /// Optional STUN server (`stun:host:port`, `stun://host:port`, or bare
    /// `host:port`). The worker normalises it to webrtcbin's
    /// `stun://host:port` form.
    pub stun_server: Option<String>,
    /// Optional TURN server address without credentials
    /// (`turn:host:port`, `turn://host:port`, or bare `host:port`).
    pub turn_server: Option<String>,
    /// TURN username paired with [`WebRtcConfig::turn_server`].
    pub turn_user: Option<String>,
    /// TURN password paired with [`WebRtcConfig::turn_server`].
    ///
    /// Redacted by the manual [`fmt::Debug`] implementation. The worker receives
    /// it as a separate argv value so it never has to be embedded in a logged
    /// server URI.
    pub turn_password: Option<String>,
    /// Bounded wait for the helper to create and accept its socket.
    pub startup_timeout: Duration,
    /// Bounded wait for one signaling response.
    pub offer_timeout: Duration,
}

impl fmt::Debug for WebRtcConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebRtcConfig")
            .field("helper_program", &self.helper_program)
            .field("helper_script", &self.helper_script)
            .field("socket_path", &self.socket_path)
            .field("shm_path", &self.shm_path)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("bitrate_kbps", &self.bitrate_kbps)
            .field("audio_source", &self.audio_source)
            .field("audio_backend", &self.audio_backend)
            .field("stun_server", &self.stun_server)
            .field("turn_server", &self.turn_server)
            .field("turn_user", &self.turn_user.as_ref().map(|_| "<redacted>"))
            .field(
                "turn_password",
                &self.turn_password.as_ref().map(|_| "<redacted>"),
            )
            .field("startup_timeout", &self.startup_timeout)
            .field("offer_timeout", &self.offer_timeout)
            .finish()
    }
}

impl WebRtcConfig {
    /// True when any ICE server was configured.
    ///
    /// This is configuration intent, not proof that a candidate was gathered.
    pub fn ice_servers_configured(&self) -> bool {
        self.stun_server.is_some() || self.turn_server.is_some()
    }

    /// Reject values the helper cannot use and make the parent directories.
    ///
    /// The socket and shared-memory parents are created here (idempotently);
    /// a path the node cannot create is a configuration error rather than a
    /// later, less obvious spawn failure.
    pub fn validate(&self) -> Result<(), WebRtcError> {
        if is_empty_path(&self.helper_program) {
            return Err(WebRtcError::InvalidConfig(
                "a WebRTC helper program is required".to_owned(),
            ));
        }
        if self.helper_script.as_deref().is_some_and(is_empty_path) {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC helper script path must not be empty".to_owned(),
            ));
        }
        if self
            .audio_source
            .as_deref()
            .is_some_and(|source| source.trim().is_empty())
        {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC audio source must not be empty when configured".to_owned(),
            ));
        }
        validate_ice_server("--webrtc-stun", self.stun_server.as_deref(), "stun")?;
        validate_ice_server("--webrtc-turn", self.turn_server.as_deref(), "turn")?;
        if self.turn_server.is_some() {
            if self.turn_user.as_deref().map_or(true, str::is_empty)
                || self.turn_password.as_deref().map_or(true, str::is_empty)
            {
                return Err(WebRtcError::InvalidConfig(
                    "--webrtc-turn requires non-empty --webrtc-turn-user and \
                     --webrtc-turn-password"
                        .to_owned(),
                ));
            }
        } else if self.turn_user.is_some() || self.turn_password.is_some() {
            return Err(WebRtcError::InvalidConfig(
                "--webrtc-turn-user/--webrtc-turn-password require --webrtc-turn".to_owned(),
            ));
        }
        for (flag, value) in [
            ("--webrtc-turn-user", self.turn_user.as_deref()),
            ("--webrtc-turn-password", self.turn_password.as_deref()),
        ] {
            if let Some(value) = value {
                if value.chars().any(char::is_control) {
                    return Err(WebRtcError::InvalidConfig(format!(
                        "{flag} must not contain control characters"
                    )));
                }
            }
        }
        if is_empty_path(&self.socket_path) {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC signaling socket path must not be empty".to_owned(),
            ));
        }
        if is_empty_path(&self.shm_path) {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC shared-memory path must not be empty".to_owned(),
            ));
        }
        if self.width == 0 || self.height == 0 {
            return Err(WebRtcError::InvalidConfig(
                "WebRTC width and height must be greater than zero".to_owned(),
            ));
        }
        if self.fps == 0 {
            return Err(WebRtcError::InvalidConfig(
                "WebRTC fps must be greater than zero".to_owned(),
            ));
        }
        if self.bitrate_kbps == 0 {
            return Err(WebRtcError::InvalidConfig(
                "WebRTC bitrate must be greater than zero kbps".to_owned(),
            ));
        }
        if self.startup_timeout.is_zero() {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC startup timeout must be greater than zero".to_owned(),
            ));
        }
        if self.offer_timeout.is_zero() {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC offer timeout must be greater than zero".to_owned(),
            ));
        }
        create_parent("WebRTC socket", &self.socket_path)?;
        create_parent("WebRTC shared memory", &self.shm_path)?;
        Ok(())
    }

    /// Shared-memory temporary path used for atomic whole-frame writes.
    fn shm_temp_path(&self) -> PathBuf {
        temp_path(&self.shm_path)
    }
}

/// Errors from configuration, helper supervision, signaling, or frame feeding.
#[derive(Debug, thiserror::Error)]
pub enum WebRtcError {
    /// The configuration cannot be used.
    #[error("invalid WebRTC configuration: {0}")]
    InvalidConfig(String),
    /// A capture format cannot be converted to the worker's packed BGRA.
    #[error("WebRTC cannot feed `{0}` frames to the helper; only rgb24 and bgra are supported")]
    UnsupportedFormat(PixelFormat),
    /// A frame's pixel layout differs from the source's declared format.
    #[error("frame format `{actual}` does not match the source's declared `{expected}` format")]
    FormatMismatch {
        /// Format the source declared.
        expected: PixelFormat,
        /// Format the frame actually carried.
        actual: PixelFormat,
    },
    /// A frame's byte length does not match its geometry.
    #[error(
        "frame buffer is {actual} bytes, expected {expected} for {format} at {width}x{height}"
    )]
    FrameSize {
        /// Frame layout.
        format: PixelFormat,
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Required byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
    /// A frame does not match the size the worker was configured with.
    #[error(
        "frame dimensions {width}x{height} do not match the WebRTC helper's {expected_width}x{expected_height}"
    )]
    FrameDimensions {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Width advertised to the helper.
        expected_width: u32,
        /// Height advertised to the helper.
        expected_height: u32,
    },
    /// A directory needed by the helper could not be created.
    #[error("could not create {label} directory `{path}`: {message}")]
    Directory {
        /// Human-readable purpose.
        label: &'static str,
        /// Directory path.
        path: PathBuf,
        /// Underlying failure.
        message: String,
    },
    /// The helper process could not be spawned.
    #[error("could not start WebRTC helper `{program}`: {message}")]
    Spawn {
        /// Executable that failed to start.
        program: PathBuf,
        /// Underlying failure.
        message: String,
    },
    /// The helper exited before exposing its socket.
    #[error("WebRTC helper exited before creating `{path}` ({status})")]
    HelperExited {
        /// Socket that was expected.
        path: PathBuf,
        /// Exit status description.
        status: String,
    },
    /// The helper did not create a connectable socket in time.
    #[error("WebRTC helper did not create `{path}` within {timeout_ms} ms")]
    SocketTimeout {
        /// Socket that was expected.
        path: PathBuf,
        /// Configured bound.
        timeout_ms: u64,
    },
    /// The signaling socket could not be connected.
    #[error("could not connect to the WebRTC helper socket `{path}`: {message}")]
    Connect {
        /// Socket path.
        path: PathBuf,
        /// Underlying failure.
        message: String,
    },
    /// Signaling I/O failed after the socket was connected.
    #[error("WebRTC signaling I/O failed: {0}")]
    Io(String),
    /// The helper's response was not a JSON object with the expected fields.
    #[error("WebRTC helper returned a malformed response: {0}")]
    MalformedResponse(String),
    /// The helper explicitly reported an error.
    #[error("WebRTC helper reported an error: {0}")]
    HelperError(String),
    /// A bounded signaling operation timed out.
    #[error("WebRTC {operation} timed out after {timeout_ms} ms")]
    Timeout {
        /// `offer` or `bitrate`.
        operation: &'static str,
        /// Configured bound.
        timeout_ms: u64,
    },
    /// The helper does not support the requested operation.
    #[error("the WebRTC helper does not support {0}")]
    Unsupported(String),
    /// No helper was configured.
    #[error("WebRTC is not configured")]
    NotConfigured,
    /// A helper was configured but is not running.
    #[error("WebRTC helper is not running")]
    NotRunning,
    /// The frame feeder failed or was aborted unexpectedly.
    #[error("the WebRTC frame task failed: {0}")]
    Task(String),
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Lifecycle state of the WebRTC helper.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebRtcState {
    /// No `--webrtc-helper` was configured.
    #[default]
    NotConfigured,
    /// The helper is being spawned and connected.
    Starting,
    /// The helper socket is connected and the feeder is running.
    Running,
    /// The helper could not start or stopped on an error.
    Failed,
    /// The helper was stopped, including a clean source end-of-stream.
    Stopped,
}

#[derive(Debug, Default)]
struct WebRtcStatusInner {
    state: WebRtcState,
    error: Option<String>,
    helper_pid: Option<u32>,
    audio_configured: bool,
}

/// Cloneable, bounded helper status for `/api/v1/connection`.
///
/// `running` is only true after the helper socket has been connected; a
/// configured helper that fails is `failed` with an error. The compatibility
/// endpoint maps [`WebRtcStatus::status_name`] onto the controller's
/// `running`/`not-configured`/`failed` tokens.
#[derive(Clone, Debug, Default)]
pub struct WebRtcStatus {
    inner: Arc<Mutex<WebRtcStatusInner>>,
}

impl WebRtcStatus {
    /// No helper was configured.
    pub fn not_configured() -> Self {
        Self::default()
    }

    /// A configured helper could not be used.
    pub fn failed(message: impl Into<String>) -> Self {
        let status = Self::starting();
        status.set_failed(message);
        status
    }

    fn starting() -> Self {
        Self {
            inner: Arc::new(Mutex::new(WebRtcStatusInner {
                state: WebRtcState::Starting,
                error: None,
                helper_pid: None,
                audio_configured: false,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, WebRtcStatusInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_running(&self, helper_pid: Option<u32>) {
        let mut inner = self.lock();
        inner.state = WebRtcState::Running;
        inner.error = None;
        inner.helper_pid = helper_pid;
    }

    fn set_audio_configured(&self, configured: bool) {
        self.lock().audio_configured = configured;
    }

    fn set_failed(&self, message: impl Into<String>) {
        let mut inner = self.lock();
        inner.state = WebRtcState::Failed;
        inner.error = Some(bounded(message.into()));
    }

    fn set_stopped(&self, message: impl Into<String>) {
        let mut inner = self.lock();
        if inner.state != WebRtcState::Failed {
            inner.state = WebRtcState::Stopped;
            inner.error = Some(bounded(message.into()));
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> WebRtcState {
        self.lock().state
    }

    /// True only while the helper socket is connected and the feeder runs.
    pub fn running(&self) -> bool {
        self.state() == WebRtcState::Running
    }

    /// Helper process id, when one was spawned.
    pub fn helper_pid(&self) -> Option<u32> {
        self.lock().helper_pid
    }

    /// Whether an audio source was configured and handed to the helper.
    ///
    /// This is configuration intent, not proof that the worker found the
    /// monitor or built the audio branch; the worker logs that separately.
    pub fn audio_configured(&self) -> bool {
        self.lock().audio_configured
    }

    /// Controller-facing status token.
    ///
    /// `running` requires a connected helper socket. `Starting` is reported as
    /// `failed` because it means the offer path is not ready yet; `Failed` and
    /// `Stopped` are `failed` with [`WebRtcStatus::error`].
    pub fn status_name(&self) -> &'static str {
        match self.state() {
            WebRtcState::NotConfigured => "not-configured",
            WebRtcState::Running => "running",
            WebRtcState::Starting | WebRtcState::Failed | WebRtcState::Stopped => "failed",
        }
    }

    /// Bounded error/status string for `/api/v1/connection`.
    pub fn error(&self) -> Option<String> {
        let inner = self.lock();
        match inner.state {
            WebRtcState::NotConfigured | WebRtcState::Running => None,
            WebRtcState::Starting => Some("the WebRTC helper has not connected yet".to_owned()),
            WebRtcState::Failed | WebRtcState::Stopped => Some(
                inner
                    .error
                    .clone()
                    .unwrap_or_else(|| "the WebRTC helper is not running".to_owned()),
            ),
        }
    }
}

fn bounded(message: String) -> String {
    if message.chars().count() <= MAX_STATUS_ERROR_CHARS {
        return message;
    }
    let mut truncated: String = message.chars().take(MAX_STATUS_ERROR_CHARS).collect();
    truncated.push('…');
    truncated
}

// ---------------------------------------------------------------------------
// Spawning and signaling
// ---------------------------------------------------------------------------

/// One newline-delimited JSON signaling channel.
///
/// Implementations are injected in tests so the default suite never needs a
/// GStreamer worker or a real Unix socket. Production uses [`SystemSignaler`].
#[async_trait]
pub trait Signaler: Send {
    /// Send one complete JSON line (the implementation appends `\n`).
    async fn write_line(&mut self, line: &str) -> Result<(), WebRtcError>;
    /// Read one line, or `None` at end of stream.
    async fn read_line(&mut self) -> Result<Option<String>, WebRtcError>;
}

struct SystemSignaler {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

#[async_trait]
impl Signaler for SystemSignaler {
    async fn write_line(&mut self, line: &str) -> Result<(), WebRtcError> {
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))?;
        self.writer
            .flush()
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))
    }

    async fn read_line(&mut self) -> Result<Option<String>, WebRtcError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))?;
        if read == 0 {
            Ok(None)
        } else {
            Ok(Some(line))
        }
    }
}

/// A helper process plus its connected signaling channel.
pub struct SpawnedHelper {
    /// Child process, when a real one was spawned.
    pub child: Option<Child>,
    /// Process id reported in status.
    pub pid: Option<u32>,
    /// Connected signaling channel.
    pub signaler: Box<dyn Signaler>,
}

/// Injectable helper process factory.
///
/// The default implementation spawns the configured program and waits for its
/// socket; tests substitute a fake that returns an in-memory or socket-pair
/// [`Signaler`].
pub trait HelperSpawner: Send + Sync {
    /// Spawn and connect one helper.
    fn spawn(&self, config: &WebRtcConfig) -> Result<SpawnedHelper, WebRtcError>;
}

struct SystemHelperSpawner;

impl HelperSpawner for SystemHelperSpawner {
    fn spawn(&self, config: &WebRtcConfig) -> Result<SpawnedHelper, WebRtcError> {
        // A stale socket would make `connect` fail before the new worker has
        // bound it, and a stale frame would be shown to the controller.
        best_effort_remove(&config.socket_path);
        best_effort_remove(&config.shm_path);
        best_effort_remove(&config.shm_temp_path());

        let argv = helper_argv(config);
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| WebRtcError::Spawn {
            program: config.helper_program.clone(),
            message: error.to_string(),
        })?;
        let pid = child.id();

        let stream = connect_with_timeout(&mut child, config)?;
        stream
            .set_nonblocking(true)
            .map_err(|error| WebRtcError::Connect {
                path: config.socket_path.clone(),
                message: error.to_string(),
            })?;
        let stream = UnixStream::from_std(stream).map_err(|error| WebRtcError::Connect {
            path: config.socket_path.clone(),
            message: error.to_string(),
        })?;
        let (reader, writer) = stream.into_split();
        Ok(SpawnedHelper {
            child: Some(child),
            pid,
            signaler: Box::new(SystemSignaler {
                reader: BufReader::new(reader),
                writer,
            }),
        })
    }
}

fn connect_with_timeout(
    child: &mut Child,
    config: &WebRtcConfig,
) -> Result<std::os::unix::net::UnixStream, WebRtcError> {
    use std::os::unix::net::UnixStream as StdUnixStream;

    let deadline = std::time::Instant::now() + config.startup_timeout;
    loop {
        if config.socket_path.exists() {
            match StdUnixStream::connect(&config.socket_path) {
                Ok(stream) => return Ok(stream),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) => {}
                Err(error) => {
                    return Err(WebRtcError::Connect {
                        path: config.socket_path.clone(),
                        message: error.to_string(),
                    });
                }
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.start_kill();
                let _ = child.wait();
                return Err(WebRtcError::HelperExited {
                    path: config.socket_path.clone(),
                    status: status.to_string(),
                });
            }
            Ok(None) => {}
            Err(error) => {
                return Err(WebRtcError::Io(format!(
                    "could not poll the WebRTC helper process: {error}"
                )));
            }
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            let _ = child.start_kill();
            return Err(WebRtcError::SocketTimeout {
                path: config.socket_path.clone(),
                timeout_ms: duration_millis(config.startup_timeout),
            });
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL.min(deadline - now));
    }
}

/// Build the exact helper argument vector.
///
/// The program is first, then the optional script, then the worker's flags.
/// Bitrate is intentionally not an argument: `gst_webrtc_worker.py` rejects
/// unknown flags, so bitrate changes go through the signaling socket instead.
/// Audio flags are appended only when [`WebRtcConfig::audio_source`] is set, so
/// a video-only node produces exactly the historical argv.
pub fn helper_argv(config: &WebRtcConfig) -> Vec<String> {
    let mut argv = Vec::with_capacity(15);
    argv.push(config.helper_program.display().to_string());
    if let Some(script) = &config.helper_script {
        argv.push(script.display().to_string());
    }
    argv.push("--sock".to_owned());
    argv.push(config.socket_path.display().to_string());
    argv.push("--shm".to_owned());
    argv.push(config.shm_path.display().to_string());
    argv.push("--width".to_owned());
    argv.push(config.width.to_string());
    argv.push("--height".to_owned());
    argv.push(config.height.to_string());
    argv.push("--fps".to_owned());
    argv.push(config.fps.to_string());
    if let Some(source) = &config.audio_source {
        argv.push("--audio-source".to_owned());
        argv.push(source.clone());
        argv.push("--audio-backend".to_owned());
        argv.push(config.audio_backend.as_arg().to_owned());
    }
    if let Some(stun) = &config.stun_server {
        argv.push("--stun-server".to_owned());
        argv.push(stun.clone());
    }
    if let Some(turn) = &config.turn_server {
        argv.push("--turn-server".to_owned());
        argv.push(turn.clone());
        argv.push("--turn-user".to_owned());
        argv.push(config.turn_user.clone().unwrap_or_default());
        argv.push("--turn-password".to_owned());
        argv.push(config.turn_password.clone().unwrap_or_default());
    }
    argv
}

// ---------------------------------------------------------------------------
// Helper supervision
// ---------------------------------------------------------------------------

/// One helper answer plus the session id needed for trickle candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebRtcAnswer {
    /// Answer SDP returned to the browser or peer.
    pub sdp: String,
    /// Helper session id, when the worker supports candidate signaling.
    pub session: Option<String>,
}

/// A running external WebRTC helper plus the shared-memory frame feeder.
pub struct WebRtcHelper {
    config: WebRtcConfig,
    status: WebRtcStatus,
    signaler: Option<Box<dyn Signaler>>,
    child: Arc<Mutex<Option<Child>>>,
    task: Option<JoinHandle<()>>,
    cancel: Arc<Notify>,
    stopped: bool,
}

impl WebRtcHelper {
    /// Spawn the configured helper, connect its socket, and start the feeder.
    ///
    /// The call is synchronous because the startup wait is bounded by
    /// [`WebRtcConfig::startup_timeout`]; it returns only once the signaling
    /// socket is connected, so a returned helper is honestly `running`.
    pub fn start(
        source: Box<dyn CaptureSource>,
        config: WebRtcConfig,
    ) -> Result<Self, WebRtcError> {
        Self::start_with_spawner(source, config, Arc::new(SystemHelperSpawner))
    }

    /// Start with an injected process factory, for tests and embedding.
    pub fn start_with_spawner(
        source: Box<dyn CaptureSource>,
        config: WebRtcConfig,
        spawner: Arc<dyn HelperSpawner>,
    ) -> Result<Self, WebRtcError> {
        config.validate()?;

        let declared = source.format();
        ensure_supported(declared)?;
        if declared.width > 0
            && declared.height > 0
            && (declared.width != config.width || declared.height != config.height)
        {
            return Err(WebRtcError::FrameDimensions {
                width: declared.width,
                height: declared.height,
                expected_width: config.width,
                expected_height: config.height,
            });
        }

        let status = WebRtcStatus::starting();
        status.set_audio_configured(config.audio_source.is_some());
        let spawned = match spawner.spawn(&config) {
            Ok(spawned) => spawned,
            Err(error) => {
                status.set_failed(error.to_string());
                return Err(error);
            }
        };
        status.set_running(spawned.pid);

        let cancel = Arc::new(Notify::new());
        let child = Arc::new(Mutex::new(spawned.child));
        let task = tokio::spawn(run_feeder(
            source,
            config.clone(),
            declared.pixel_format,
            status.clone(),
            cancel.clone(),
            child.clone(),
        ));

        Ok(Self {
            config,
            status,
            signaler: Some(spawned.signaler),
            child,
            task: Some(task),
            cancel,
            stopped: false,
        })
    }

    /// Cloneable status for `/api/v1/connection`.
    pub fn status(&self) -> WebRtcStatus {
        self.status.clone()
    }

    /// Configuration this helper runs with.
    pub fn config(&self) -> &WebRtcConfig {
        &self.config
    }

    /// True while the helper socket is connected and the feeder has not
    /// stopped.
    pub fn is_running(&self) -> bool {
        !self.stopped && self.status.running()
    }

    /// Send one SDP offer and return the helper's answer SDP.
    pub async fn offer(&mut self, sdp: &str) -> Result<String, WebRtcError> {
        Ok(self.offer_answer(sdp).await?.sdp)
    }

    /// Send one SDP offer and return the answer plus the optional helper session
    /// id used by trickle-candidate messages.
    pub async fn offer_answer(&mut self, sdp: &str) -> Result<WebRtcAnswer, WebRtcError> {
        if sdp.trim().is_empty() {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC offer SDP must not be empty".to_owned(),
            ));
        }
        if self.stopped {
            return Err(WebRtcError::NotRunning);
        }
        let message = json!({"type": "offer", "sdp": sdp}).to_string();
        self.signal(&message, "offer").await?;
        let line = self.read_response("offer").await?;
        parse_answer_details(&line)
    }

    /// Forward one trickled remote ICE candidate to the helper session.
    ///
    /// The helper acknowledges with `{"ok":true}`. A worker that does not
    /// implement the message is a typed [`WebRtcError::Unsupported`] rather than
    /// a fake success. `session` may be omitted for compatibility with a worker
    /// that only has one live session; the current worker applies such a
    /// candidate to every live session.
    pub async fn add_ice_candidate(
        &mut self,
        session: Option<&str>,
        candidate: &str,
        sdp_mline_index: u32,
    ) -> Result<(), WebRtcError> {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            return Err(WebRtcError::InvalidConfig(
                "the ICE candidate must not be empty".to_owned(),
            ));
        }
        if candidate.chars().any(char::is_control) {
            return Err(WebRtcError::InvalidConfig(
                "the ICE candidate must not contain control characters".to_owned(),
            ));
        }
        if self.stopped {
            return Err(WebRtcError::NotRunning);
        }
        let message = json!({
            "type": "add-ice-candidate",
            "session": session,
            "candidate": candidate,
            "sdpMLineIndex": sdp_mline_index,
        })
        .to_string();
        self.signal(&message, "candidate").await?;
        let line = self.read_response("candidate").await?;
        parse_ack(&line, "candidate")
    }

    /// Ask the helper to change its video bitrate.
    ///
    /// The outgoing message is `{"type":"bitrate","bitrate":<bps>}`. The
    /// helper clamps it to 500 kbps–50 Mbps, applies it to every live video
    /// encoder, and acknowledges with `{"ok":true,"bitrate":<bps>}`. A helper
    /// that ignores the message is a typed timeout; `{"ok":false,...}` or an
    /// unknown shape is a typed error, so success always means an ack.
    pub async fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), WebRtcError> {
        if bitrate_kbps == 0 {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC bitrate must be greater than zero".to_owned(),
            ));
        }
        if self.stopped {
            return Err(WebRtcError::NotRunning);
        }
        let bps = u64::from(bitrate_kbps) * 1_000;
        let message = json!({"type": "bitrate", "bitrate": bps}).to_string();
        self.signal(&message, "bitrate").await?;
        let line = self.read_response("bitrate").await?;
        parse_bitrate_ack(&line)
    }

    async fn signal(&mut self, line: &str, operation: &'static str) -> Result<(), WebRtcError> {
        let Some(signaler) = self.signaler.as_mut() else {
            return Err(WebRtcError::NotRunning);
        };
        tokio::time::timeout(self.config.offer_timeout, signaler.write_line(line))
            .await
            .map_err(|_| WebRtcError::Timeout {
                operation,
                timeout_ms: duration_millis(self.config.offer_timeout),
            })?
    }

    async fn read_response(&mut self, operation: &'static str) -> Result<String, WebRtcError> {
        let Some(signaler) = self.signaler.as_mut() else {
            return Err(WebRtcError::NotRunning);
        };
        let response = tokio::time::timeout(self.config.offer_timeout, signaler.read_line())
            .await
            .map_err(|_| WebRtcError::Timeout {
                operation,
                timeout_ms: duration_millis(self.config.offer_timeout),
            })??;
        response.ok_or_else(|| {
            WebRtcError::MalformedResponse(
                "the helper closed the signaling connection without a response".to_owned(),
            )
        })
    }

    /// Kill the helper, abort the feeder, remove socket/shm, and reap.
    ///
    /// Idempotent: later calls are no-ops.
    pub async fn stop(&mut self) -> Result<(), WebRtcError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.status.set_stopped("the WebRTC helper was stopped");

        // Ask the feeder to leave its frame wait so the source's `stop()` runs.
        self.cancel.notify_one();
        let mut task_error = None;
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(STOP_TASK_TIMEOUT, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_cancelled() => {}
                Ok(Err(error)) => task_error = Some(error.to_string()),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }

        // Drop the signaling channel before killing the worker so it sees EOF.
        self.signaler.take();
        kill_child(&self.child).await;

        best_effort_remove(&self.config.socket_path);
        best_effort_remove(&self.config.shm_path);
        best_effort_remove(&self.config.shm_temp_path());

        if let Some(message) = task_error {
            self.status
                .set_failed(format!("WebRTC frame task failed: {message}"));
            return Err(WebRtcError::Task(message));
        }
        Ok(())
    }
}

impl Drop for WebRtcHelper {
    fn drop(&mut self) {
        self.cancel.notify_one();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        // `kill_on_drop` reaps a child created by the default spawner; a test
        // spawner may carry no child at all.
        if let Ok(mut slot) = self.child.lock() {
            slot.take();
        }
        best_effort_remove(&self.config.socket_path);
        best_effort_remove(&self.config.shm_path);
        best_effort_remove(&self.config.shm_temp_path());
    }
}

// ---------------------------------------------------------------------------
// Shared control surface for the compatibility routes
// ---------------------------------------------------------------------------

/// Cloneable handle shared by `main`, `/webrtc/*`, and the connection endpoint.
///
/// A control with no helper exists only for a configured-but-failed helper; a
/// node that never configured WebRTC omits the control entirely.
#[derive(Clone)]
pub struct WebRtcControl {
    status: WebRtcStatus,
    helper: Option<Arc<tokio::sync::Mutex<WebRtcHelper>>>,
    /// Configured monitor source, retained even when the helper failed so the
    /// connection endpoint can report configuration intent without claiming
    /// that samples were produced.
    audio_source: Option<String>,
}

impl WebRtcControl {
    /// Wrap a started helper.
    pub fn from_helper(helper: WebRtcHelper) -> Self {
        let status = helper.status();
        let audio_source = helper.config().audio_source.clone();
        Self {
            status,
            helper: Some(Arc::new(tokio::sync::Mutex::new(helper))),
            audio_source,
        }
    }

    /// A configured helper that could not be started.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::failed_with_audio(message, None)
    }

    /// A configured helper that could not be started, retaining the configured
    /// audio monitor for `/api/v1/connection`.
    pub fn failed_with_audio(message: impl Into<String>, audio_source: Option<String>) -> Self {
        Self {
            status: WebRtcStatus::failed(message),
            helper: None,
            audio_source,
        }
    }

    /// Status shared with `/api/v1/connection`.
    pub fn status(&self) -> WebRtcStatus {
        self.status.clone()
    }

    /// Controller-facing status token.
    pub fn status_name(&self) -> &'static str {
        self.status.status_name()
    }

    /// Why WebRTC is not running, when it is configured and failing.
    pub fn error(&self) -> Option<String> {
        self.status.error()
    }

    /// Whether the helper was configured with an audio monitor source.
    pub fn audio_configured(&self) -> bool {
        self.status.audio_configured() || self.audio_source.is_some()
    }

    /// The configured monitor source, when one was supplied.
    ///
    /// This is only the PulseAudio/PipeWire monitor name; it is not proof that
    /// the worker opened the source or produced samples.
    pub fn audio_source(&self) -> Option<&str> {
        self.audio_source.as_deref()
    }

    /// True only when a helper is connected and the feeder is running.
    pub fn available(&self) -> bool {
        self.helper.is_some() && self.status.running()
    }

    /// Reason the routes must answer 503, when they must.
    pub fn unavailable_reason(&self) -> Option<String> {
        if self.available() {
            None
        } else {
            Some(
                self.error()
                    .unwrap_or_else(|| "the WebRTC helper is not running".to_owned()),
            )
        }
    }

    /// Proxy one SDP offer to the helper.
    pub async fn offer(&self, sdp: &str) -> Result<String, WebRtcError> {
        let helper = self.require_helper()?;
        helper.lock().await.offer(sdp).await
    }

    /// Proxy one SDP offer and keep the helper session id for trickle.
    pub async fn offer_answer(&self, sdp: &str) -> Result<WebRtcAnswer, WebRtcError> {
        let helper = self.require_helper()?;
        helper.lock().await.offer_answer(sdp).await
    }

    /// Forward one trickled remote ICE candidate to the helper.
    pub async fn add_ice_candidate(
        &self,
        session: Option<&str>,
        candidate: &str,
        sdp_mline_index: u32,
    ) -> Result<(), WebRtcError> {
        let helper = self.require_helper()?;
        helper
            .lock()
            .await
            .add_ice_candidate(session, candidate, sdp_mline_index)
            .await
    }

    /// Request a bitrate change in bits per second.
    ///
    /// [`WebRtcHelper::set_bitrate`] is kbps-based, so the controller's bps
    /// value is rounded down to a whole kbps before being sent back out as bps
    /// on the signaling socket (the helper clamps to 500 kbps–50 Mbps).
    pub async fn set_bitrate_bps(&self, bitrate_bps: u32) -> Result<(), WebRtcError> {
        if bitrate_bps == 0 {
            return Err(WebRtcError::InvalidConfig(
                "bitrate must be greater than zero".to_owned(),
            ));
        }
        let kbps = bitrate_bps / 1_000;
        let helper = self.require_helper()?;
        helper.lock().await.set_bitrate(kbps.max(1)).await
    }

    /// Stop the helper. Idempotent and safe to call after a failed start.
    pub async fn shutdown(&self) {
        let Some(helper) = self.helper.as_ref() else {
            return;
        };
        let mut helper = helper.lock().await;
        if let Err(error) = helper.stop().await {
            debug!(error = %error, "WebRTC helper did not stop cleanly");
        }
    }

    fn require_helper(&self) -> Result<&Arc<tokio::sync::Mutex<WebRtcHelper>>, WebRtcError> {
        if !self.available() {
            return Err(WebRtcError::NotRunning);
        }
        self.helper.as_ref().ok_or(WebRtcError::NotConfigured)
    }
}

// ---------------------------------------------------------------------------
// Frame conversion and feeder
// ---------------------------------------------------------------------------

async fn run_feeder(
    mut source: Box<dyn CaptureSource>,
    config: WebRtcConfig,
    expected_format: PixelFormat,
    status: WebRtcStatus,
    cancel: Arc<Notify>,
    child: Arc<Mutex<Option<Child>>>,
) {
    let interval_duration = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let mut interval = tokio::time::interval(interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut wrote_frame = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => {
                status.set_stopped("the WebRTC feeder stopped");
                break;
            }
            _ = interval.tick() => {}
        }

        let frame = tokio::select! {
            biased;
            _ = cancel.notified() => {
                status.set_stopped("the WebRTC feeder stopped");
                break;
            }
            frame = source.next_frame() => frame,
        };

        let frame = match frame {
            Ok(frame) => frame,
            Err(MediaError::EndOfStream) => {
                status.set_stopped("the WebRTC capture source reached the end of its stream");
                break;
            }
            Err(error) => {
                status.set_failed(format!(
                    "WebRTC capture source `{}` failed: {error}",
                    source.id()
                ));
                break;
            }
        };

        let bgra = match convert_to_bgra(&frame, expected_format, config.width, config.height) {
            Ok(bgra) => bgra,
            Err(error) => {
                status.set_failed(error.to_string());
                break;
            }
        };

        if let Err(error) = write_frame_atomically(&config, &bgra).await {
            status.set_failed(format!(
                "writing a WebRTC frame to `{}` failed: {error}",
                config.shm_path.display()
            ));
            break;
        }
        wrote_frame = true;
    }

    if !wrote_frame && status.state() == WebRtcState::Starting {
        status.set_failed("the WebRTC helper ended before receiving a frame");
    }
    let _ = source.stop().await;
    // A source end-of-stream or a feeder failure must not leave the helper
    // holding an encoder and an ICE session forever.
    kill_child(&child).await;
}

async fn kill_child(child: &Arc<Mutex<Option<Child>>>) {
    let child = child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(mut child) = child {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Convert one captured frame into packed BGRA, the worker's expected layout.
fn convert_to_bgra(
    frame: &Frame,
    expected_format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, WebRtcError> {
    let actual_format = frame.format().pixel_format;
    if actual_format != expected_format {
        return Err(WebRtcError::FormatMismatch {
            expected: expected_format,
            actual: actual_format,
        });
    }
    if frame.format().width != width || frame.format().height != height {
        return Err(WebRtcError::FrameDimensions {
            width: frame.format().width,
            height: frame.format().height,
            expected_width: width,
            expected_height: height,
        });
    }

    let data = frame.data();
    match actual_format {
        PixelFormat::Rgb24 => {
            let expected = byte_len(PixelFormat::Rgb24, width, height)?;
            if data.len() != expected {
                return Err(WebRtcError::FrameSize {
                    format: PixelFormat::Rgb24,
                    width,
                    height,
                    expected,
                    actual: data.len(),
                });
            }
            let mut bgra = Vec::with_capacity(expected / 3 * 4);
            for pixel in data.chunks_exact(3) {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xFF]);
            }
            Ok(bgra)
        }
        PixelFormat::Bgra => {
            let expected = byte_len(PixelFormat::Bgra, width, height)?;
            if data.len() != expected {
                return Err(WebRtcError::FrameSize {
                    format: PixelFormat::Bgra,
                    width,
                    height,
                    expected,
                    actual: data.len(),
                });
            }
            Ok(data.to_vec())
        }
        other => Err(WebRtcError::UnsupportedFormat(other)),
    }
}

fn byte_len(format: PixelFormat, width: u32, height: u32) -> Result<usize, WebRtcError> {
    format
        .nominal_len(width, height)
        .ok_or(WebRtcError::UnsupportedFormat(format))
}

fn ensure_supported(format: FrameFormat) -> Result<(), WebRtcError> {
    match format.pixel_format {
        PixelFormat::Rgb24 | PixelFormat::Bgra => Ok(()),
        other => Err(WebRtcError::UnsupportedFormat(other)),
    }
}

async fn write_frame_atomically(config: &WebRtcConfig, bgra: &[u8]) -> io::Result<()> {
    let temporary = config.shm_temp_path();
    tokio::fs::write(&temporary, bgra).await?;
    tokio::fs::rename(&temporary, &config.shm_path).await
}

// ---------------------------------------------------------------------------
// Signaling response parsing
// ---------------------------------------------------------------------------

fn parse_answer_details(line: &str) -> Result<WebRtcAnswer, WebRtcError> {
    let value = parse_json_object(line)?;
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Err(WebRtcError::HelperError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper rejected the offer")
                .to_owned(),
        ));
    }
    if let Some(message) = value.get("error").and_then(Value::as_str) {
        return Err(WebRtcError::HelperError(message.to_owned()));
    }
    let sdp = match value.get("sdp").and_then(Value::as_str) {
        Some(sdp) if !sdp.trim().is_empty() => sdp.to_owned(),
        _ => {
            return Err(WebRtcError::MalformedResponse(
                "the answer has no non-empty `sdp` string".to_owned(),
            ))
        }
    };
    let session = value
        .get("session")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .map(str::to_owned);
    Ok(WebRtcAnswer { sdp, session })
}

fn parse_ack(line: &str, operation: &'static str) -> Result<(), WebRtcError> {
    let value = parse_json_object(line)?;
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Err(WebRtcError::HelperError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper rejected the request")
                .to_owned(),
        ));
    }
    if value.get("ok") == Some(&Value::Bool(false)) {
        return Err(WebRtcError::HelperError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper did not apply the request")
                .to_owned(),
        ));
    }
    if value.get("ok") == Some(&Value::Bool(true)) {
        Ok(())
    } else {
        Err(WebRtcError::Unsupported(format!(
            "signaling {operation} messages (it did not acknowledge the request)"
        )))
    }
}

fn parse_bitrate_ack(line: &str) -> Result<(), WebRtcError> {
    let value = parse_json_object(line)?;
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Err(WebRtcError::HelperError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper rejected the bitrate change")
                .to_owned(),
        ));
    }
    if value.get("ok") == Some(&Value::Bool(false)) {
        return Err(WebRtcError::HelperError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper did not apply the bitrate change")
                .to_owned(),
        ));
    }
    let acknowledged = value.get("ok") == Some(&Value::Bool(true))
        || matches!(
            value.get("type").and_then(Value::as_str),
            Some("bitrate" | "bitrate-ack" | "ack")
        );
    if acknowledged {
        Ok(())
    } else {
        Err(WebRtcError::Unsupported(
            "signaling bitrate changes (it did not acknowledge the request)".to_owned(),
        ))
    }
}

fn parse_json_object(line: &str) -> Result<Value, WebRtcError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(WebRtcError::MalformedResponse(
            "the helper returned an empty response".to_owned(),
        ));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|error| {
        WebRtcError::MalformedResponse(format!("response is not JSON: {error}"))
    })?;
    if !value.is_object() {
        return Err(WebRtcError::MalformedResponse(
            "response is not a JSON object".to_owned(),
        ));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn is_empty_path(path: &Path) -> bool {
    path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty()
}

/// Validate one optional ICE server address.
///
/// Credentials are deliberately not accepted inside the address: TURN
/// credentials travel in separate fields so this string can be logged and
/// passed through a URI builder without leaking a password.
fn validate_ice_server(
    flag: &str,
    value: Option<&str>,
    expected_scheme: &str,
) -> Result<(), WebRtcError> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(WebRtcError::InvalidConfig(format!(
            "{flag} must not be empty when configured"
        )));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(WebRtcError::InvalidConfig(format!(
            "{flag} must not contain whitespace"
        )));
    }
    if value.contains('@') {
        return Err(WebRtcError::InvalidConfig(format!(
            "{flag} must not embed credentials; use --webrtc-turn-user/\
             --webrtc-turn-password for TURN"
        )));
    }
    let allowed: &[&str] = if expected_scheme == "stun" {
        &["stun", "stuns"]
    } else {
        &["turn", "turns"]
    };
    if let Some((scheme, rest)) = value.split_once(':') {
        let known_scheme = ["stun", "stuns", "turn", "turns"].contains(&scheme);
        if known_scheme || rest.starts_with("//") {
            if !allowed.contains(&scheme) {
                return Err(WebRtcError::InvalidConfig(format!(
                    "{flag} has unsupported scheme `{scheme}`; expected {expected_scheme}"
                )));
            }
            let rest = rest.trim_start_matches("//");
            if rest.is_empty() {
                return Err(WebRtcError::InvalidConfig(format!(
                    "{flag} is missing a host"
                )));
            }
        }
    }
    Ok(())
}

fn create_parent(label: &'static str, path: &Path) -> Result<(), WebRtcError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|error| WebRtcError::Directory {
        label,
        path: parent.to_path_buf(),
        message: error.to_string(),
    })
}

fn temp_path(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut name = name.to_os_string();
            name.push(".tmp");
            path.with_file_name(name)
        }
        None => path.with_extension("tmp"),
    }
}

fn best_effort_remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            debug!(path = %path.display(), error = %error, "could not remove WebRTC file")
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PendingSource {
        format: FrameFormat,
        stopped: Arc<AtomicBool>,
    }

    impl PendingSource {
        fn new(format: FrameFormat) -> Self {
            Self {
                format,
                stopped: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl CaptureSource for PendingSource {
        fn id(&self) -> &str {
            "pending"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            Err(MediaError::EndOfStream)
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeSource {
        format: FrameFormat,
        frames: VecDeque<Frame>,
    }

    impl FakeSource {
        fn new(width: u32, height: u32, pixel_format: PixelFormat) -> Self {
            Self {
                format: FrameFormat::new(pixel_format, width, height),
                frames: VecDeque::new(),
            }
        }

        fn with_frame(mut self, frame: Frame) -> Self {
            self.frames.push_back(frame);
            self
        }
    }

    #[async_trait]
    impl CaptureSource for FakeSource {
        fn id(&self) -> &str {
            "fake"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            match self.frames.pop_front() {
                Some(frame) => Ok(frame),
                None => Err(MediaError::EndOfStream),
            }
        }
    }

    struct RecordingSpawner {
        spawned: Mutex<Option<SpawnedHelper>>,
    }

    impl RecordingSpawner {
        fn new(signaler: Box<dyn Signaler>) -> Self {
            Self {
                spawned: Mutex::new(Some(SpawnedHelper {
                    child: None,
                    pid: Some(4_242),
                    signaler,
                })),
            }
        }
    }

    impl HelperSpawner for RecordingSpawner {
        fn spawn(&self, config: &WebRtcConfig) -> Result<SpawnedHelper, WebRtcError> {
            self.spawned
                .lock()
                .unwrap()
                .take()
                .ok_or(WebRtcError::Spawn {
                    program: config.helper_program.clone(),
                    message: "fake spawner has no helper left".to_owned(),
                })
        }
    }

    struct PairSignaler {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
    }

    #[async_trait]
    impl Signaler for PairSignaler {
        async fn write_line(&mut self, line: &str) -> Result<(), WebRtcError> {
            self.writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|error| WebRtcError::Io(error.to_string()))?;
            self.writer
                .flush()
                .await
                .map_err(|error| WebRtcError::Io(error.to_string()))
        }

        async fn read_line(&mut self) -> Result<Option<String>, WebRtcError> {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|error| WebRtcError::Io(error.to_string()))?;
            if read == 0 {
                Ok(None)
            } else {
                Ok(Some(line))
            }
        }
    }

    fn pair() -> (Box<dyn Signaler>, BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        let (client, server) = UnixStream::pair().expect("unix stream pair");
        let (client_reader, client_writer) = client.into_split();
        let (server_reader, server_writer) = server.into_split();
        (
            Box::new(PairSignaler {
                reader: BufReader::new(client_reader),
                writer: client_writer,
            }),
            BufReader::new(server_reader),
            server_writer,
        )
    }

    fn temp_dir(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ozma-webrtc-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("create temp directory");
        directory
    }

    fn config(label: &str) -> WebRtcConfig {
        let directory = temp_dir(label);
        WebRtcConfig {
            helper_program: PathBuf::from("/bin/true"),
            helper_script: None,
            socket_path: directory.join("helper.sock"),
            shm_path: directory.join("frame.bgra"),
            width: 2,
            height: 2,
            fps: 30,
            bitrate_kbps: 4_000,
            audio_source: None,
            audio_backend: WebRtcAudioBackend::Auto,
            stun_server: None,
            turn_server: None,
            turn_user: None,
            turn_password: None,
            startup_timeout: Duration::from_millis(150),
            offer_timeout: Duration::from_millis(150),
        }
    }

    fn rgb24_frame(width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame::with_data(FrameFormat::rgb24(width, height), 0, 0, true, data).expect("valid frame")
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration, context: &str) {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {context}");
    }

    #[test]
    fn argv_includes_program_script_socket_shm_and_geometry() {
        let mut config = config("argv");
        config.helper_program = PathBuf::from("/usr/bin/python3");
        config.helper_script = Some(PathBuf::from("/opt/ozma/gst_webrtc_worker.py"));
        config.width = 1920;
        config.height = 1080;
        config.fps = 60;

        assert_eq!(
            helper_argv(&config),
            vec![
                "/usr/bin/python3",
                "/opt/ozma/gst_webrtc_worker.py",
                "--sock",
                config.socket_path.to_str().unwrap(),
                "--shm",
                config.shm_path.to_str().unwrap(),
                "--width",
                "1920",
                "--height",
                "1080",
                "--fps",
                "60",
            ]
        );
    }

    #[test]
    fn argv_omits_the_script_for_a_direct_program() {
        let mut config = config("argv-direct");
        config.helper_program = PathBuf::from("/usr/local/bin/ozma-webrtc-helper");
        let argv = helper_argv(&config);
        assert_eq!(argv[0], "/usr/local/bin/ozma-webrtc-helper");
        assert_eq!(argv[1], "--sock");
    }

    #[test]
    fn argv_appends_audio_arguments_only_when_configured() {
        let mut config = config("argv-audio");
        config.audio_source = Some("ozma-vm1.monitor".to_owned());
        config.audio_backend = WebRtcAudioBackend::PipeWire;
        let argv = helper_argv(&config);
        assert_eq!(
            &argv[argv.len() - 4..],
            [
                "--audio-source",
                "ozma-vm1.monitor",
                "--audio-backend",
                "pipewire",
            ]
        );

        config.audio_source = None;
        let argv = helper_argv(&config);
        assert!(!argv.iter().any(|arg| arg == "--audio-source"));
        assert!(!argv.iter().any(|arg| arg == "--audio-backend"));
    }

    #[test]
    fn argv_appends_stun_and_turn_only_when_configured() {
        let mut config = config("argv-ice");
        let argv = helper_argv(&config);
        assert!(!argv.iter().any(|arg| arg == "--stun-server"));
        assert!(!argv.iter().any(|arg| arg == "--turn-server"));

        config.stun_server = Some("stun:stun.example.org:3478".to_owned());
        config.turn_server = Some("turn:turn.example.org:3478".to_owned());
        config.turn_user = Some("alice".to_owned());
        config.turn_password = Some("s3cret".to_owned());
        let argv = helper_argv(&config);
        assert_eq!(
            &argv[argv.len() - 8..],
            [
                "--stun-server",
                "stun:stun.example.org:3478",
                "--turn-server",
                "turn:turn.example.org:3478",
                "--turn-user",
                "alice",
                "--turn-password",
                "s3cret",
            ]
        );
    }

    #[test]
    fn config_debug_redacts_turn_credentials() {
        let mut config = config("debug-redaction");
        config.turn_server = Some("turn:turn.example.org:3478".to_owned());
        config.turn_user = Some("alice".to_owned());
        config.turn_password = Some("s3cret".to_owned());
        let debug = format!("{config:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert!(!debug.contains("alice"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("turn.example.org:3478"), "{debug}");
    }

    #[test]
    fn validation_rejects_incomplete_or_credentialed_ice_servers() {
        let mut invalid = config("validate-turn-without-auth");
        invalid.turn_server = Some("turn:turn.example.org:3478".to_owned());
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-turn-user-only");
        invalid.turn_user = Some("alice".to_owned());
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-stun-empty");
        invalid.stun_server = Some("   ".to_owned());
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-stun-credentials");
        invalid.stun_server = Some("stun://user:pass@stun.example.org:3478".to_owned());
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut valid = config("validate-ice-ok");
        valid.stun_server = Some("stun:stun.example.org:3478".to_owned());
        valid.turn_server = Some("turn:turn.example.org:3478".to_owned());
        valid.turn_user = Some("alice".to_owned());
        valid.turn_password = Some("s3cret".to_owned());
        valid.validate().expect("complete ICE config is valid");
    }

    #[test]
    fn answer_session_is_optional_and_parsed_when_present() {
        let plain = parse_answer_details("{\"type\":\"answer\",\"sdp\":\"v=0 a\"}").unwrap();
        assert_eq!(plain.sdp, "v=0 a");
        assert_eq!(plain.session, None);

        let trickled =
            parse_answer_details("{\"type\":\"answer\",\"sdp\":\"v=0 b\",\"session\":\"abc123\"}")
                .unwrap();
        assert_eq!(trickled.sdp, "v=0 b");
        assert_eq!(trickled.session.as_deref(), Some("abc123"));

        assert!(matches!(
            parse_answer_details("{\"type\":\"answer\",\"sdp\":\"v=0\",\"session\":\"  \"}")
                .unwrap()
                .session,
            None
        ));
    }

    #[tokio::test]
    async fn add_ice_candidate_round_trips_and_requires_an_ack() {
        let (signaler, mut reader, mut writer) = pair();
        let helper_task = tokio::spawn(async move {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["type"], "add-ice-candidate");
            assert_eq!(request["session"], "abc123");
            assert_eq!(
                request["candidate"],
                "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host"
            );
            assert_eq!(request["sdpMLineIndex"], 0);
            writer.write_all(b"{\"ok\":true}\n").await.unwrap();
            writer.flush().await.unwrap();
        });

        let mut config = config("candidate");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(signaler));
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut helper = WebRtcHelper::start_with_spawner(source, config, spawner).unwrap();

        helper
            .add_ice_candidate(
                Some("abc123"),
                "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host",
                0,
            )
            .await
            .expect("ack is success");
        helper.stop().await.unwrap();
        helper_task.await.unwrap();
    }

    #[test]
    fn candidate_ack_rejects_missing_or_false_acknowledgements() {
        assert!(parse_ack("{\"ok\":true}", "candidate").is_ok());
        assert!(matches!(
            parse_ack("{\"ok\":false,\"error\":\"unknown WebRTC session\"}", "candidate"),
            Err(WebRtcError::HelperError(message)) if message == "unknown WebRTC session"
        ));
        assert!(matches!(
            parse_ack("{\"type\":\"answer\",\"sdp\":\"v=0\"}", "candidate"),
            Err(WebRtcError::Unsupported(_))
        ));
    }

    #[test]
    fn validation_rejects_an_empty_configured_audio_source() {
        let mut config = config("validate-audio");
        config.audio_source = Some("   ".to_owned());
        assert!(matches!(
            config.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));
    }

    #[test]
    fn validation_rejects_zero_geometry_empty_paths_and_zero_timeouts() {
        let mut invalid = config("validate-empty");
        invalid.helper_program = PathBuf::new();
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-dimensions");
        invalid.width = 0;
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-fps");
        invalid.fps = 0;
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-bitrate");
        invalid.bitrate_kbps = 0;
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-socket");
        invalid.socket_path = PathBuf::new();
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));

        let mut invalid = config("validate-timeout");
        invalid.startup_timeout = Duration::ZERO;
        assert!(matches!(
            invalid.validate(),
            Err(WebRtcError::InvalidConfig(_))
        ));
    }

    #[test]
    fn validation_creates_the_socket_and_shm_parent_directories() {
        let directory = temp_dir("validate-parents");
        let mut config = config("validate-parents");
        config.socket_path = directory.join("nested/sockets/helper.sock");
        config.shm_path = directory.join("nested/shm/frame.bgra");

        config.validate().expect("parents are creatable");
        assert!(directory.join("nested/sockets").is_dir());
        assert!(directory.join("nested/shm").is_dir());
    }

    #[tokio::test]
    async fn start_times_out_when_the_helper_never_creates_the_socket() {
        let directory = temp_dir("startup-timeout");
        let script = directory.join("idle.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut config = config("startup-timeout");
        config.helper_program = PathBuf::from("/bin/sh");
        config.helper_script = Some(script);
        config.startup_timeout = Duration::from_millis(120);

        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let error = match WebRtcHelper::start(source, config) {
            Ok(mut helper) => {
                helper.stop().await.unwrap();
                panic!("the idle helper must not be reported as ready");
            }
            Err(error) => error,
        };
        assert!(
            matches!(error, WebRtcError::SocketTimeout { .. }),
            "expected a typed socket timeout, got {error}"
        );
    }

    #[test]
    fn missing_helper_program_is_a_typed_spawn_error() {
        let mut config = config("missing");
        config.helper_program = config.socket_path.with_file_name("does-not-exist");
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let error = match WebRtcHelper::start(source, config) {
            Ok(helper) => {
                drop(helper);
                panic!("a missing helper must not start");
            }
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                WebRtcError::Spawn { .. } | WebRtcError::SocketTimeout { .. }
            ),
            "expected a typed spawn failure, got {error}"
        );
    }

    #[tokio::test]
    async fn offer_round_trips_against_a_fake_unix_socket_helper() {
        let (signaler, mut reader, mut writer) = pair();
        let helper_task = tokio::spawn(async move {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["type"], "offer");
            assert_eq!(request["sdp"], "v=0 fake offer");
            writer
                .write_all(b"{\"type\":\"answer\",\"sdp\":\"v=0 fake answer\"}\n")
                .await
                .unwrap();
            writer.flush().await.unwrap();
        });

        let mut config = config("offer");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(signaler));
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut helper =
            WebRtcHelper::start_with_spawner(source, config, spawner).expect("fake helper starts");

        assert_eq!(helper.status().status_name(), "running");
        assert_eq!(helper.status().helper_pid(), Some(4_242));
        assert_eq!(
            helper.offer("v=0 fake offer").await.unwrap(),
            "v=0 fake answer"
        );
        helper.stop().await.unwrap();
        helper_task.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_answer_is_a_typed_error() {
        let (signaler, mut reader, mut writer) = pair();
        tokio::spawn(async move {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            writer.write_all(b"{\"type\":\"answer\"}\n").await.unwrap();
            writer.flush().await.unwrap();
        });

        let mut config = config("malformed");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(signaler));
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut helper = WebRtcHelper::start_with_spawner(source, config, spawner).unwrap();

        let error = helper.offer("v=0 fake offer").await.unwrap_err();
        assert!(
            matches!(error, WebRtcError::MalformedResponse(_)),
            "got {error}"
        );
        helper.stop().await.unwrap();
    }

    #[tokio::test]
    async fn offer_timeout_is_a_typed_error() {
        let (signaler, mut reader, _writer) = pair();
        tokio::spawn(async move {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            // Never answer.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let mut config = config("offer-timeout");
        config.helper_program = PathBuf::from("/fake/helper");
        config.offer_timeout = Duration::from_millis(50);
        let spawner = Arc::new(RecordingSpawner::new(signaler));
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut helper = WebRtcHelper::start_with_spawner(source, config, spawner).unwrap();

        let error = helper.offer("v=0 fake offer").await.unwrap_err();
        assert!(
            matches!(
                error,
                WebRtcError::Timeout {
                    operation: "offer",
                    ..
                }
            ),
            "got {error}"
        );
        helper.stop().await.unwrap();
    }

    #[test]
    fn bitrate_ack_parser_requires_an_explicit_acknowledgement() {
        assert!(parse_bitrate_ack("{\"ok\":true}").is_ok());
        assert!(parse_bitrate_ack("{\"ok\":true,\"bitrate\":8000000}").is_ok());
        assert!(matches!(
            parse_bitrate_ack("{\"ok\":false,\"error\":\"no video encoder\"}"),
            Err(WebRtcError::HelperError(message)) if message == "no video encoder"
        ));
        assert!(matches!(
            parse_bitrate_ack("{\"type\":\"answer\",\"sdp\":\"v=0\"}"),
            Err(WebRtcError::Unsupported(_))
        ));
        assert!(matches!(
            parse_bitrate_ack("not json"),
            Err(WebRtcError::MalformedResponse(_))
        ));
    }

    #[tokio::test]
    async fn bitrate_without_an_ack_is_not_a_fake_success() {
        // A helper that ignores `bitrate` must not be reported as success.
        let (signaler, mut reader, _writer) = pair();
        tokio::spawn(async move {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["type"], "bitrate");
            assert_eq!(request["bitrate"], 4_000_000);
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let mut config = config("bitrate-timeout");
        config.helper_program = PathBuf::from("/fake/helper");
        config.offer_timeout = Duration::from_millis(50);
        let spawner = Arc::new(RecordingSpawner::new(signaler));
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut helper = WebRtcHelper::start_with_spawner(source, config, spawner).unwrap();

        assert!(matches!(
            helper.set_bitrate(4_000).await,
            Err(WebRtcError::Timeout {
                operation: "bitrate",
                ..
            })
        ));
        helper.stop().await.unwrap();
    }

    #[tokio::test]
    async fn bitrate_ack_and_explicit_rejection_are_both_honest() {
        let (signaler, mut reader, mut writer) = pair();
        tokio::spawn(async move {
            for response in [
                &b"{\"ok\":true}\n"[..],
                &b"{\"ok\":false,\"error\":\"no video encoder\"}\n"[..],
            ] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                writer.write_all(response).await.unwrap();
                writer.flush().await.unwrap();
            }
        });

        let mut config = config("bitrate-acks");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(signaler));
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut helper = WebRtcHelper::start_with_spawner(source, config, spawner).unwrap();

        helper.set_bitrate(4_000).await.expect("ack is success");
        assert!(matches!(
            helper.set_bitrate(4_000).await,
            Err(WebRtcError::HelperError(_))
        ));
        helper.stop().await.unwrap();
    }

    #[tokio::test]
    async fn rgb24_frames_are_converted_to_packed_bgra_and_written_whole() {
        // 2x2: red, green / blue, white.
        let frame = rgb24_frame(
            2,
            2,
            vec![
                255, 0, 0, // red
                0, 255, 0, // green
                0, 0, 255, // blue
                255, 255, 255,
            ],
        );
        let source = Box::new(FakeSource::new(2, 2, PixelFormat::Rgb24).with_frame(frame));
        let mut config = config("rgb24-bgra");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(silent_signaler()));
        let mut helper = WebRtcHelper::start_with_spawner(source, config.clone(), spawner).unwrap();

        wait_until(
            || config.shm_path.is_file(),
            Duration::from_secs(2),
            "the feeder to write a frame",
        )
        .await;

        let bytes = std::fs::read(&config.shm_path).unwrap();
        assert_eq!(
            bytes,
            vec![
                0, 0, 255, 255, // red
                0, 255, 0, 255, // green
                255, 0, 0, 255, // blue
                255, 255, 255, 255,
            ]
        );
        assert!(
            !config.shm_temp_path().exists(),
            "the temporary file must be renamed, not left behind"
        );
        helper.stop().await.unwrap();
    }

    #[tokio::test]
    async fn bgra_frames_pass_through_unchanged() {
        let frame = Frame::with_data(
            FrameFormat::new(PixelFormat::Bgra, 1, 1),
            0,
            0,
            true,
            vec![1u8, 2, 3, 4],
        )
        .unwrap();
        let source = Box::new(FakeSource::new(1, 1, PixelFormat::Bgra).with_frame(frame));
        let mut config = config("bgra");
        config.width = 1;
        config.height = 1;
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(silent_signaler()));
        let mut helper = WebRtcHelper::start_with_spawner(source, config.clone(), spawner).unwrap();

        wait_until(
            || config.shm_path.is_file(),
            Duration::from_secs(2),
            "the feeder to write a BGRA frame",
        )
        .await;
        assert_eq!(std::fs::read(&config.shm_path).unwrap(), vec![1, 2, 3, 4]);
        helper.stop().await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_source_format_is_rejected_before_spawning() {
        let source = Box::new(FakeSource::new(2, 2, PixelFormat::Yuyv));
        let mut config = config("yuyv");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(silent_signaler()));

        let error = match WebRtcHelper::start_with_spawner(source, config, spawner) {
            Ok(mut helper) => {
                helper.stop().await.unwrap();
                panic!("YUYV must not be accepted");
            }
            Err(error) => error,
        };
        assert!(matches!(
            error,
            WebRtcError::UnsupportedFormat(PixelFormat::Yuyv)
        ));
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_marks_the_helper_stopped() {
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut config = config("stop");
        config.helper_program = PathBuf::from("/fake/helper");
        let spawner = Arc::new(RecordingSpawner::new(silent_signaler()));
        let mut helper = WebRtcHelper::start_with_spawner(source, config.clone(), spawner).unwrap();
        assert!(helper.is_running());

        helper.stop().await.unwrap();
        helper.stop().await.unwrap();
        assert!(!helper.is_running());
        assert_eq!(helper.status().status_name(), "failed");
        assert!(!config.socket_path.exists());
        assert!(!config.shm_path.exists());
        assert!(matches!(
            helper.offer("v=0 late offer").await,
            Err(WebRtcError::NotRunning)
        ));
    }

    #[tokio::test]
    async fn status_is_honest_before_and_after_a_failure() {
        assert_eq!(
            WebRtcStatus::not_configured().status_name(),
            "not-configured"
        );
        // Starting means the offer path is not ready, so it is not `running`.
        let starting = WebRtcStatus::starting();
        assert_eq!(starting.status_name(), "failed");
        assert!(starting.error().is_some());

        let failed = WebRtcControl::failed("helper program is missing");
        assert_eq!(failed.status_name(), "failed");
        assert_eq!(failed.error().as_deref(), Some("helper program is missing"));
        assert!(failed.unavailable_reason().is_some());
        assert!(matches!(
            failed.offer("v=0").await,
            Err(WebRtcError::NotRunning)
        ));
        assert!(!failed.available());
    }

    #[tokio::test]
    async fn control_reports_running_only_with_a_connected_helper() {
        let source = Box::new(PendingSource::new(FrameFormat::rgb24(2, 2)));
        let mut config = config("control");
        config.helper_program = PathBuf::from("/fake/helper");
        config.audio_source = Some("ozma-vm1.monitor".to_owned());
        let spawner = Arc::new(RecordingSpawner::new(silent_signaler()));
        let helper = WebRtcHelper::start_with_spawner(source, config, spawner).unwrap();

        let control = WebRtcControl::from_helper(helper);
        assert!(control.available());
        assert!(control.audio_configured());
        assert_eq!(control.status_name(), "running");
        assert!(control.error().is_none());
        assert!(control.unavailable_reason().is_none());

        control.shutdown().await;
        assert!(!control.available());
        assert_eq!(control.status_name(), "failed");
        // `shutdown` is idempotent.
        control.shutdown().await;
    }

    fn silent_signaler() -> Box<dyn Signaler> {
        let (signaler, _reader, _writer) = pair();
        signaler
    }

    /// Real helper check. Requires GStreamer + Python and is never part of the
    /// default suite:
    ///
    /// ```text
    /// OZMA_WEBRTC_GS_SCRIPT=/path/to/gst_webrtc_worker.py \
    ///   cargo test -p ozma-node --lib webrtc::tests::real_gstreamer_helper_starts -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires GStreamer + Python; set OZMA_WEBRTC_GS_SCRIPT"]
    async fn real_gstreamer_helper_starts_and_stops() {
        let Ok(script) = std::env::var("OZMA_WEBRTC_GS_SCRIPT") else {
            return;
        };
        let program = std::env::var("OZMA_WEBRTC_GS_PYTHON")
            .unwrap_or_else(|_| "/usr/bin/python3".to_owned());
        let directory = temp_dir("real-helper");
        let mut config = config("real-helper");
        config.helper_program = PathBuf::from(program);
        config.helper_script = Some(PathBuf::from(script));
        config.socket_path = directory.join("helper.sock");
        config.shm_path = directory.join("frame.bgra");
        config.width = 320;
        config.height = 240;
        config.startup_timeout = Duration::from_secs(20);

        let source = Box::new(PendingSource::new(FrameFormat::rgb24(320, 240)));
        let mut helper = WebRtcHelper::start(source, config).expect("helper starts");
        assert_eq!(helper.status().status_name(), "running");
        helper.stop().await.expect("helper stops");
    }

    /// Real optional-audio check. This proves the helper accepts the audio
    /// argv and stays up; it does not negotiate a browser and therefore does
    /// not prove the monitor produced samples.
    ///
    /// ```text
    /// OZMA_WEBRTC_GS_SCRIPT=/path/to/gst_webrtc_worker.py \
    /// OZMA_WEBRTC_GS_AUDIO_SOURCE=ozma-vm1.monitor \
    ///   cargo test -p ozma-node --lib webrtc::tests::real_gstreamer_audio_helper_starts_and_stops -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires GStreamer + Python + a monitor source; set OZMA_WEBRTC_GS_SCRIPT and OZMA_WEBRTC_GS_AUDIO_SOURCE"]
    async fn real_gstreamer_audio_helper_starts_and_stops() {
        let Ok(script) = std::env::var("OZMA_WEBRTC_GS_SCRIPT") else {
            return;
        };
        let Ok(audio_source) = std::env::var("OZMA_WEBRTC_GS_AUDIO_SOURCE") else {
            return;
        };
        let program = std::env::var("OZMA_WEBRTC_GS_PYTHON")
            .unwrap_or_else(|_| "/usr/bin/python3".to_owned());
        let directory = temp_dir("real-audio-helper");
        let mut config = config("real-audio-helper");
        config.helper_program = PathBuf::from(program);
        config.helper_script = Some(PathBuf::from(script));
        config.socket_path = directory.join("helper.sock");
        config.shm_path = directory.join("frame.bgra");
        config.width = 320;
        config.height = 240;
        config.audio_source = Some(audio_source);
        config.startup_timeout = Duration::from_secs(20);

        let source = Box::new(PendingSource::new(FrameFormat::rgb24(320, 240)));
        let mut helper = WebRtcHelper::start(source, config).expect("helper starts");
        assert_eq!(helper.status().status_name(), "running");
        assert!(helper.status().audio_configured());
        helper.stop().await.expect("helper stops");
    }
}
