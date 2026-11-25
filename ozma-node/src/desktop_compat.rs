// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Desktop-profile compatibility controls: display geometry, clipboard, and
//! wallpaper.
//!
//! These routes close the remaining `ozma_agent.py` desktop parity gaps for an
//! X11 seat. Every operation goes through a [`CommandRunner`], which is an
//! injectable process boundary:
//!
//! * `GET /display/geometry` runs `xrandr --query` and parses the current
//!   screen and output state.
//! * `POST /display/geometry` validates the requested output/mode/position/
//!   rotation against the advertised modes, applies one exact `xrandr` argv,
//!   re-queries the output, and rolls back to the previous mode if the apply or
//!   verification fails.
//! * `GET /clipboard` / `POST /clipboard` read and write UTF-8 text through
//!   `xclip` or `xsel` with a bounded size. There is no success without a real
//!   tool exit status of zero.
//! * `GET /wallpaper` / `POST /wallpaper` set a validated local image through
//!   `feh`, `xwallpaper`, or `swaybg`. Remote URLs are rejected; this node
//!   never downloads an image.
//!
//! The routes are mounted inside [`crate::controller_compat::compat_router`],
//! so they share the node API's bearer-token guard with the other mutating
//! compatibility controls. `/api/v1/connection.desktop_controls` advertises
//! the controls that are available by looking up tools in `PATH`; it never
//! runs them.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ozma_protocol::ApiError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::controller_compat::ConnectionState;

/// Largest clipboard payload accepted or returned, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
/// Largest local wallpaper file accepted, in bytes.
pub const MAX_WALLPAPER_BYTES: u64 = 64 * 1024 * 1024;
/// Largest command stdout retained by the runner.
pub const MAX_COMMAND_STDOUT: usize = 256 * 1024;
/// Largest command stderr retained by the runner.
pub const MAX_COMMAND_STDERR: usize = 16 * 1024;
/// Default timeout for a one-shot desktop tool.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a persistent wallpaper daemon is observed before it is considered
/// started. `swaybg` stays running by design.
pub const BACKGROUND_GRACE: Duration = Duration::from_millis(750);
/// Largest accepted output coordinate. `xrandr` uses signed positions, but a
/// sane seat never needs more than this.
pub const MAX_DISPLAY_COORDINATE: i32 = 65_535;
/// Files larger than this are not read while checking the wallpaper format.
const WALLPAPER_MAGIC_BYTES: usize = 12;

/// Typed failure from the process boundary.
#[derive(Clone, Debug, thiserror::Error)]
pub enum CommandError {
    /// The executable could not be spawned.
    #[error("could not start {program}: {message}")]
    Spawn {
        /// Program name.
        program: String,
        /// OS error text.
        message: String,
    },
    /// The command exceeded its deadline.
    #[error("{program} timed out after {timeout_ms} ms")]
    Timeout {
        /// Program name.
        program: String,
        /// Timeout in milliseconds.
        timeout_ms: u128,
    },
    /// Reading/waiting failed.
    #[error("{program}: {message}")]
    Io {
        /// Program name.
        program: String,
        /// OS error text.
        message: String,
    },
}

impl CommandError {
    fn spawn(program: &str, error: std::io::Error) -> Self {
        Self::Spawn {
            program: program.to_owned(),
            message: error.to_string(),
        }
    }

    fn io(program: &str, error: std::io::Error) -> Self {
        Self::Io {
            program: program.to_owned(),
            message: error.to_string(),
        }
    }
}

/// One command invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRequest {
    /// Program name or path.
    pub program: String,
    /// Arguments, excluding the program name.
    pub args: Vec<String>,
    /// Optional bounded stdin payload.
    pub stdin: Option<Vec<u8>>,
    /// Per-command deadline.
    pub timeout: Duration,
    /// Maximum retained stdout bytes.
    pub max_stdout: usize,
    /// Maximum retained stderr bytes.
    pub max_stderr: usize,
}

impl CommandRequest {
    /// A short-lived command with the default output bounds.
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            stdin: None,
            timeout: COMMAND_TIMEOUT,
            max_stdout: MAX_COMMAND_STDOUT,
            max_stderr: MAX_COMMAND_STDERR,
        }
    }

    /// Attach a bounded stdin payload.
    pub fn with_stdin(mut self, stdin: Vec<u8>) -> Self {
        self.stdin = Some(stdin);
        self
    }
}

/// One completed command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, or `None` when killed by a signal.
    pub status: Option<i32>,
    /// Retained stdout bytes.
    pub stdout: Vec<u8>,
    /// Retained stderr text (lossy UTF-8).
    pub stderr: String,
    /// Whether stdout exceeded the requested bound.
    pub stdout_truncated: bool,
}

impl CommandOutput {
    /// Only a zero exit code is success. A signal, timeout, or spawn failure is
    /// never success.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// Human-readable status for an error document.
    pub fn status_label(&self) -> String {
        match self.status {
            Some(code) => format!("exit {code}"),
            None => "terminated by signal".to_owned(),
        }
    }
}

/// A long-running process that owns a desktop setting (currently `swaybg`).
pub trait BackgroundHandle: Send + Sync {
    /// Ask the process to exit. The handle is dropped afterwards.
    fn kill(&mut self);
}

/// Result of starting a background tool.
pub enum BackgroundOutcome {
    /// The tool exited during the observation window.
    Exited(CommandOutput),
    /// The tool is still running and owns the setting.
    Running(Box<dyn BackgroundHandle>),
}

/// Injectable process boundary used by every desktop control.
///
/// Tests install a fake runner; the node uses [`SystemCommandRunner`]. A fake
/// runner is what makes exact argv, rollback, and failure-path assertions
/// possible without touching the host desktop.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Resolve a program in `PATH` without executing it.
    fn lookup(&self, program: &str) -> Option<PathBuf>;

    /// Run a command to completion within its deadline.
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError>;

    /// Start a command, observe it for `grace`, and keep it alive if it is
    /// still running.
    async fn spawn_background(
        &self,
        request: CommandRequest,
        grace: Duration,
    ) -> Result<BackgroundOutcome, CommandError>;
}

/// Real process runner.
///
/// Output is bounded, commands have a deadline, and `kill_on_drop` prevents a
/// timed-out child from outliving the request.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl SystemCommandRunner {
    fn resolve(&self, program: &str) -> Option<PathBuf> {
        if program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
            let path = PathBuf::from(program);
            return is_executable(&path).then_some(path);
        }
        let search = std::env::var_os("PATH")?;
        std::env::split_paths(&search)
            .map(|directory| directory.join(program))
            .find(|candidate| is_executable(candidate))
    }
}

async fn read_limited<R>(reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let mut limited = reader.take(limit as u64 + 1);
    limited.read_to_end(&mut buffer).await?;
    let truncated = buffer.len() > limit;
    if truncated {
        buffer.truncate(limit);
    }
    Ok((buffer, truncated))
}

#[async_trait]
impl CommandRunner for SystemCommandRunner {
    fn lookup(&self, program: &str) -> Option<PathBuf> {
        self.resolve(program)
    }

    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError> {
        let program = request.program.clone();
        let mut command = Command::new(&program);
        command
            .args(&request.args)
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| CommandError::spawn(&program, error))?;

        if let Some(stdin) = request.stdin {
            if let Some(mut handle) = child.stdin.take() {
                // The writer is detached so a payload larger than the pipe
                // buffer cannot deadlock against the child's stdout.
                tokio::spawn(async move {
                    let _ = handle.write_all(&stdin).await;
                    let _ = handle.shutdown().await;
                });
            }
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let max_stdout = request.max_stdout;
        let max_stderr = request.max_stderr;
        let timeout_program = program.clone();
        let program_for_wait = program.clone();
        let wait = async move {
            let (stdout_result, stderr_result, status_result) = tokio::join!(
                async move {
                    match stdout {
                        Some(reader) => read_limited(reader, max_stdout).await,
                        None => Ok((Vec::new(), false)),
                    }
                },
                async move {
                    match stderr {
                        Some(reader) => read_limited(reader, max_stderr).await,
                        None => Ok((Vec::new(), false)),
                    }
                },
                async move {
                    child
                        .wait()
                        .await
                        .map_err(|error| CommandError::io(&program_for_wait, error))
                }
            );
            let (stdout, stdout_truncated) =
                stdout_result.map_err(|error| CommandError::io(&program, error))?;
            let (stderr, _) = stderr_result.map_err(|error| CommandError::io(&program, error))?;
            let status = status_result?;
            Ok::<CommandOutput, CommandError>(CommandOutput {
                status: status.code(),
                stdout,
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                stdout_truncated,
            })
        };

        match tokio::time::timeout(request.timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(CommandError::Timeout {
                program: timeout_program,
                timeout_ms: request.timeout.as_millis(),
            }),
        }
    }

    async fn spawn_background(
        &self,
        request: CommandRequest,
        grace: Duration,
    ) -> Result<BackgroundOutcome, CommandError> {
        let program = request.program.clone();
        let mut command = Command::new(&program);
        command
            .args(&request.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| CommandError::spawn(&program, error))?;
        let stderr_task = child.stderr.take().map(|reader| {
            let limit = request.max_stderr;
            tokio::spawn(async move {
                match read_limited(reader, limit).await {
                    Ok((bytes, _)) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(error) => error.to_string(),
                }
            })
        });

        match tokio::time::timeout(grace, child.wait()).await {
            Ok(status) => {
                let status = status.map_err(|error| CommandError::io(&program, error))?;
                let stderr = match stderr_task {
                    Some(task) => task.await.unwrap_or_default(),
                    None => String::new(),
                };
                Ok(BackgroundOutcome::Exited(CommandOutput {
                    status: status.code(),
                    stdout: Vec::new(),
                    stderr,
                    stdout_truncated: false,
                }))
            }
            Err(_) => Ok(BackgroundOutcome::Running(Box::new(SystemBackground {
                child,
                stderr_task,
            }))),
        }
    }
}

struct SystemBackground {
    child: Child,
    stderr_task: Option<tokio::task::JoinHandle<String>>,
}

impl BackgroundHandle for SystemBackground {
    fn kill(&mut self) {
        let _ = self.child.start_kill();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

/// Tool availability for one control, computed by `PATH` lookup only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ControlAvailability {
    /// True only when the display/session and a tool are both present.
    pub available: bool,
    /// Preferred tool name, when present.
    pub tool: Option<String>,
    /// Every candidate found in `PATH`.
    pub tools: Vec<String>,
    /// Why the control is unavailable, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Wallpaper last applied by this node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WallpaperApplied {
    /// Canonical local path.
    pub path: String,
    /// Tool that accepted it.
    pub tool: String,
}

/// `/api/v1/connection.desktop_controls`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DesktopControlsStatus {
    /// Explicit `--x11-display` or ambient `DISPLAY`.
    pub x11_display: Option<String>,
    /// Ambient `WAYLAND_DISPLAY`, used only for `swaybg`.
    pub wayland_display: Option<String>,
    /// Display geometry control availability.
    pub geometry: ControlAvailability,
    /// Clipboard control availability.
    pub clipboard: ControlAvailability,
    /// Wallpaper control availability.
    pub wallpaper: ControlAvailability,
    /// Last wallpaper accepted by this node, if any.
    pub last_wallpaper: Option<WallpaperApplied>,
    /// The node cannot read the compositor's current wallpaper back, so a GET
    /// never claims it is the current one.
    pub wallpaper_readback: bool,
}

/// One advertised `xrandr` mode.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct XrandrMode {
    /// Mode name (`1920x1080`, `720x480i`, ...).
    pub name: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Refresh rate in Hz, when parsed.
    pub refresh_hz: Option<f64>,
    /// The output currently scans this mode.
    pub current: bool,
    /// The server prefers this mode.
    pub preferred: bool,
}

impl XrandrMode {
    /// Stable label accepted by `POST /display/geometry` when two modes share a
    /// name but differ by refresh.
    pub fn label(&self) -> String {
        match self.refresh_hz {
            Some(refresh) if refresh > 0.0 => format!("{}@{refresh:.2}", self.name),
            _ => self.name.clone(),
        }
    }
}

/// One `xrandr` output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct XrandrOutput {
    /// Connector name (`DP-1`, `HDMI-1`, ...).
    pub name: String,
    /// Whether a display is connected.
    pub connected: bool,
    /// Whether the server marks it primary.
    pub primary: bool,
    /// Active pixel width (`0` when no mode is active).
    pub width: u32,
    /// Active pixel height (`0` when no mode is active).
    pub height: u32,
    /// Active position X.
    pub x: i32,
    /// Active position Y.
    pub y: i32,
    /// `normal`, `left`, `inverted`, or `right`.
    pub rotation: String,
    /// Advertised modes.
    pub modes: Vec<XrandrMode>,
}

impl XrandrOutput {
    fn current_mode(&self) -> Option<&XrandrMode> {
        self.modes.iter().find(|mode| mode.current)
    }

    fn state(&self) -> GeometryState {
        let current = self.current_mode();
        GeometryState {
            output: self.name.clone(),
            mode: current.map(|mode| mode.name.clone()),
            refresh_hz: current.and_then(|mode| mode.refresh_hz),
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            rotation: self.rotation.clone(),
            primary: self.primary,
        }
    }
}

/// Parsed `Screen N:` summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct XrandrScreen {
    /// Minimum framebuffer width.
    pub min_width: u32,
    /// Minimum framebuffer height.
    pub min_height: u32,
    /// Current framebuffer width.
    pub current_width: u32,
    /// Current framebuffer height.
    pub current_height: u32,
    /// Maximum framebuffer width.
    pub max_width: u32,
    /// Maximum framebuffer height.
    pub max_height: u32,
}

/// Python `DisplayInfo.get_screens()` compatible subset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GeometryScreen {
    /// Output name, or `screen` when the server names no output.
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// X position.
    pub x: i32,
    /// Y position.
    pub y: i32,
    /// Whether this is the primary output.
    pub primary: bool,
}

/// `GET /display/geometry` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GeometryStatus {
    /// Always true on a 200.
    pub ok: bool,
    /// Always true on a 200.
    pub available: bool,
    /// X11 display the query targeted.
    pub display: String,
    /// Resolved `xrandr` path.
    pub tool: String,
    /// Exact argv used for the query.
    pub argv: Vec<String>,
    /// `Screen N:` summary, when parsed.
    pub screen: Option<XrandrScreen>,
    /// Parsed outputs.
    pub outputs: Vec<XrandrOutput>,
    /// Flat screen list matching the Python agent's shape.
    pub screens: Vec<GeometryScreen>,
}

/// `POST /display/geometry` request.
#[derive(Clone, Debug, Deserialize)]
pub struct SetGeometryRequest {
    /// Connector name.
    pub output: String,
    /// Advertised mode name, `name@refresh`, `WxH`, or `WxH@refresh`.
    pub mode: String,
    /// Target X position; defaults to the current position.
    #[serde(default)]
    pub x: Option<i32>,
    /// Target Y position; defaults to the current position.
    #[serde(default)]
    pub y: Option<i32>,
    /// `normal`, `left`, `inverted`, or `right`; defaults to current.
    #[serde(default)]
    pub rotation: Option<String>,
}

/// Applied state of one output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GeometryState {
    /// Connector name.
    pub output: String,
    /// Active mode name.
    pub mode: Option<String>,
    /// Active refresh in Hz.
    pub refresh_hz: Option<f64>,
    /// Active width.
    pub width: u32,
    /// Active height.
    pub height: u32,
    /// Active X position.
    pub x: i32,
    /// Active Y position.
    pub y: i32,
    /// Active rotation.
    pub rotation: String,
    /// Primary flag.
    pub primary: bool,
}

/// Successful `POST /display/geometry` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GeometryChange {
    /// Always true on a 200.
    pub ok: bool,
    /// Whether the queried state differs from the previous state.
    pub changed: bool,
    /// X11 display changed.
    pub display: String,
    /// Resolved `xrandr` path.
    pub tool: String,
    /// Exact argv that was applied.
    pub argv: Vec<String>,
    /// State before the change.
    pub previous: GeometryState,
    /// State read back after the change.
    pub applied: GeometryState,
    /// Always false on success; rollback information only appears in errors.
    pub rolled_back: bool,
}

/// Clipboard read response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClipboardRead {
    /// Always true on a 200.
    pub ok: bool,
    /// UTF-8 clipboard text.
    pub text: String,
    /// Bytes in `text`.
    pub bytes: usize,
    /// Clipboard tool that produced it.
    pub tool: String,
    /// `clipboard` for both `xclip` and `xsel`.
    pub selection: &'static str,
}

/// Clipboard write response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClipboardWrite {
    /// Always true on a 200.
    pub ok: bool,
    /// Whether the tool exited zero.
    pub written: bool,
    /// Bytes written.
    pub bytes: usize,
    /// Clipboard tool that accepted it.
    pub tool: String,
}

/// `POST /clipboard` request.
#[derive(Clone, Debug, Deserialize)]
pub struct ClipboardWriteRequest {
    /// UTF-8 text; NUL and invalid UTF-8 are rejected.
    pub text: String,
}

/// `GET /wallpaper` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WallpaperStatus {
    /// Always true on a 200.
    pub ok: bool,
    /// Always true on a 200.
    pub available: bool,
    /// Tool that would be used.
    pub tool: String,
    /// X11 display, when present.
    pub x11_display: Option<String>,
    /// Wayland display, when present.
    pub wayland_display: Option<String>,
    /// Last wallpaper accepted by this node.
    pub last_applied: Option<WallpaperApplied>,
    /// `false`: the node has no portable read-back command.
    pub verified: bool,
    /// Explanation that `last_applied` is not a read-back.
    pub note: &'static str,
}

/// `POST /wallpaper` request.
#[derive(Clone, Debug, Deserialize)]
pub struct WallpaperRequest {
    /// Absolute local image path. URLs are rejected.
    pub path: String,
}

/// Successful `POST /wallpaper` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WallpaperChange {
    /// Always true on a 200.
    pub ok: bool,
    /// Whether the tool exited zero (`feh`/`xwallpaper`) or stayed running
    /// (`swaybg`).
    pub applied: bool,
    /// Canonical local path.
    pub path: String,
    /// Tool used.
    pub tool: String,
    /// Format detected from the file magic.
    pub format: String,
    /// File size in bytes.
    pub bytes: u64,
    /// Exact argv used.
    pub argv: Vec<String>,
    /// `swaybg` stays resident; `feh`/`xwallpaper` do not.
    pub persistent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WallpaperTool {
    Feh,
    Xwallpaper,
    Swaybg,
}

impl WallpaperTool {
    fn name(self) -> &'static str {
        match self {
            Self::Feh => "feh",
            Self::Xwallpaper => "xwallpaper",
            Self::Swaybg => "swaybg",
        }
    }

    fn args(self, path: &str) -> Vec<String> {
        match self {
            Self::Feh => vec!["--bg-fill".to_owned(), path.to_owned()],
            Self::Xwallpaper => vec!["--zoom".to_owned(), path.to_owned()],
            Self::Swaybg => vec![
                "-i".to_owned(),
                path.to_owned(),
                "-m".to_owned(),
                "fill".to_owned(),
            ],
        }
    }

    fn persistent(self) -> bool {
        matches!(self, Self::Swaybg)
    }
}

/// Typed desktop-control failure. Each variant has a stable status/code.
#[derive(Clone, Debug)]
pub enum DesktopError {
    /// The node is not running the desktop profile.
    NotEnabled,
    /// No X11 display was configured or found in the environment.
    NoDisplay {
        /// Human-readable detail.
        reason: String,
    },
    /// A required tool is absent.
    ToolMissing {
        /// Control name (`geometry`, `clipboard`, `wallpaper`).
        control: &'static str,
        /// Candidates that were looked up.
        candidates: Vec<String>,
        /// Human-readable detail.
        reason: String,
    },
    /// A query command failed.
    QueryFailed {
        /// Stable code.
        code: &'static str,
        /// Human-readable detail.
        message: String,
        /// Structured detail.
        details: Value,
    },
    /// The request was rejected before any command ran.
    Invalid {
        /// Stable code.
        code: &'static str,
        /// Human-readable detail.
        message: String,
        /// Structured detail.
        details: Value,
    },
    /// The request exceeded a bound.
    TooLarge {
        /// Stable code.
        code: &'static str,
        /// Human-readable detail.
        message: String,
        /// Structured detail.
        details: Value,
    },
    /// A command ran but did not achieve/verify the requested change.
    ApplyFailed {
        /// Stable code.
        code: &'static str,
        /// Human-readable detail.
        message: String,
        /// Structured detail.
        details: Value,
    },
}

impl DesktopError {
    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotEnabled => StatusCode::NOT_IMPLEMENTED,
            Self::NoDisplay { .. } | Self::ToolMissing { .. } | Self::QueryFailed { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Invalid { .. } => StatusCode::BAD_REQUEST,
            Self::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ApplyFailed { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    /// Convert to the compatibility error envelope.
    pub fn api_error(&self) -> ApiError {
        match self {
            Self::NotEnabled => ApiError::new(
                "desktop.not_enabled",
                "Desktop compatibility controls are only available with --profile desktop.",
            )
            .with_remediation(
                "Start ozma-node with --profile desktop on the seat whose display, clipboard, \
                 or wallpaper should be controlled.",
            ),
            Self::NoDisplay { reason } => ApiError::new("desktop.no_display", reason.clone())
                .with_remediation(
                    "Set --x11-display :0 (or start the node inside the X11 session so DISPLAY \
                     is present). No ambient X server is scanned and no display is fabricated.",
                ),
            Self::ToolMissing {
                control,
                candidates,
                reason,
            } => ApiError::new("desktop.tool_missing", reason.clone())
                .with_details(serde_json::json!({
                    "control": control,
                    "candidates": candidates,
                }))
                .with_remediation(format!(
                    "Install one of {} on the seat, or use a routed session that provides it.",
                    candidates.join(", ")
                )),
            Self::QueryFailed {
                code,
                message,
                details,
            } => ApiError::new(*code, message.clone())
                .with_details(details.clone())
                .retryable(),
            Self::Invalid {
                code,
                message,
                details,
            } => ApiError::new(*code, message.clone()).with_details(details.clone()),
            Self::TooLarge {
                code,
                message,
                details,
            } => ApiError::new(*code, message.clone()).with_details(details.clone()),
            Self::ApplyFailed {
                code,
                message,
                details,
            } => ApiError::new(*code, message.clone()).with_details(details.clone()),
        }
    }
}

fn error_response(error: DesktopError) -> Response {
    let status = error.status();
    let api = error.api_error();
    let mut body = serde_json::json!({
        "ok": false,
        "code": api.code,
        "error": api.message,
        "retryable": api.retryable,
    });
    if !api.details.is_null() {
        body["details"] = api.details;
    }
    if let Some(remediation) = api.remediation {
        body["remediation"] = Value::String(remediation);
    }
    (status, Json(body)).into_response()
}

fn json_rejection(error: JsonRejection) -> Response {
    error_response(DesktopError::Invalid {
        code: "invalid-json",
        message: error.body_text(),
        details: Value::Null,
    })
}

#[derive(Default)]
struct DesktopState {
    last_wallpaper: Option<WallpaperApplied>,
    background: Option<Box<dyn BackgroundHandle>>,
}

struct DesktopInner {
    runner: Arc<dyn CommandRunner>,
    display: Option<String>,
    wayland_display: Option<String>,
    state: Mutex<DesktopState>,
}

impl std::fmt::Debug for DesktopInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopInner")
            .field("display", &self.display)
            .field("wayland_display", &self.wayland_display)
            .finish_non_exhaustive()
    }
}

/// Shared, injectable desktop controls for one node.
#[derive(Clone)]
pub struct DesktopControls {
    inner: Arc<DesktopInner>,
}

impl std::fmt::Debug for DesktopControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopControls")
            .field("inner", &self.inner)
            .finish()
    }
}

impl DesktopControls {
    /// Build controls from a runner and explicit session names.
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        display: Option<String>,
        wayland_display: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(DesktopInner {
                runner,
                display: normalize_session_name(display),
                wayland_display: normalize_session_name(wayland_display),
                state: Mutex::new(DesktopState::default()),
            }),
        }
    }

    /// Build real controls for a desktop seat.
    pub fn system(display: Option<String>, wayland_display: Option<String>) -> Self {
        Self::new(Arc::new(SystemCommandRunner), display, wayland_display)
    }

    /// X11 display used by geometry/clipboard, when present.
    pub fn display(&self) -> Option<&str> {
        self.inner.display.as_deref()
    }

    /// Advertise controls without running them.
    pub fn status(&self) -> DesktopControlsStatus {
        let geometry = self.geometry_availability();
        let clipboard = self.clipboard_availability();
        let wallpaper = self.wallpaper_availability();
        let last_wallpaper = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_wallpaper
            .clone();
        DesktopControlsStatus {
            x11_display: self.inner.display.clone(),
            wayland_display: self.inner.wayland_display.clone(),
            geometry,
            clipboard,
            wallpaper,
            last_wallpaper,
            wallpaper_readback: false,
        }
    }

    fn geometry_availability(&self) -> ControlAvailability {
        let tools = self.available_tools(&["xrandr"]);
        if self.inner.display.is_none() {
            return ControlAvailability {
                available: false,
                tool: tools.first().cloned(),
                tools,
                reason: Some("no X11 display (set --x11-display or DISPLAY)".to_owned()),
            };
        }
        ControlAvailability {
            available: !tools.is_empty(),
            tool: tools.first().cloned(),
            reason: tools
                .is_empty()
                .then(|| "xrandr was not found in PATH".to_owned()),
            tools,
        }
    }

    fn clipboard_availability(&self) -> ControlAvailability {
        let tools = self.available_tools(&["xclip", "xsel"]);
        if self.inner.display.is_none() {
            return ControlAvailability {
                available: false,
                tool: tools.first().cloned(),
                tools,
                reason: Some("no X11 display (set --x11-display or DISPLAY)".to_owned()),
            };
        }
        ControlAvailability {
            available: !tools.is_empty(),
            tool: tools.first().cloned(),
            reason: tools
                .is_empty()
                .then(|| "neither xclip nor xsel was found in PATH".to_owned()),
            tools,
        }
    }

    fn wallpaper_availability(&self) -> ControlAvailability {
        let mut tools = Vec::new();
        if self.inner.display.is_some() {
            tools.extend(self.available_tools(&["feh", "xwallpaper"]));
        }
        if self.inner.wayland_display.is_some() {
            tools.extend(self.available_tools(&["swaybg"]));
        }
        let reason = if self.inner.display.is_none() && self.inner.wayland_display.is_none() {
            Some("no X11 or Wayland session".to_owned())
        } else if tools.is_empty() {
            Some("feh, xwallpaper, and swaybg were not found in PATH".to_owned())
        } else {
            None
        };
        ControlAvailability {
            available: !tools.is_empty(),
            tool: tools.first().cloned(),
            tools,
            reason,
        }
    }

    fn available_tools(&self, candidates: &[&str]) -> Vec<String> {
        candidates
            .iter()
            .filter(|tool| self.inner.runner.lookup(tool).is_some())
            .map(|tool| (*tool).to_owned())
            .collect()
    }

    fn require_x11_tool(
        &self,
        control: &'static str,
        candidates: &[&str],
    ) -> Result<(String, String, String), DesktopError> {
        let display = self.display().ok_or_else(|| DesktopError::NoDisplay {
            reason: "No X11 display is configured or present in DISPLAY.".to_owned(),
        })?;
        let (tool, path) = candidates
            .iter()
            .find_map(|tool| {
                self.inner
                    .runner
                    .lookup(tool)
                    .map(|path| ((*tool).to_owned(), path))
            })
            .ok_or_else(|| DesktopError::ToolMissing {
                control,
                candidates: candidates.iter().map(|tool| (*tool).to_owned()).collect(),
                reason: format!(
                    "None of {} is available for the {control} control.",
                    candidates.join(", ")
                ),
            })?;
        Ok((
            display.to_owned(),
            tool,
            path.to_string_lossy().into_owned(),
        ))
    }

    fn require_geometry(&self) -> Result<(String, String, String), DesktopError> {
        self.require_x11_tool("geometry", &["xrandr"])
    }

    fn require_clipboard(&self) -> Result<(String, String, String), DesktopError> {
        self.require_x11_tool("clipboard", &["xclip", "xsel"])
    }

    fn wallpaper_tool(&self) -> Result<WallpaperTool, DesktopError> {
        if self.inner.display.is_some() {
            if self.inner.runner.lookup("feh").is_some() {
                return Ok(WallpaperTool::Feh);
            }
            if self.inner.runner.lookup("xwallpaper").is_some() {
                return Ok(WallpaperTool::Xwallpaper);
            }
        }
        if self.inner.wayland_display.is_some() && self.inner.runner.lookup("swaybg").is_some() {
            return Ok(WallpaperTool::Swaybg);
        }
        Err(DesktopError::ToolMissing {
            control: "wallpaper",
            candidates: vec![
                "feh".to_owned(),
                "xwallpaper".to_owned(),
                "swaybg".to_owned(),
            ],
            reason: "No supported wallpaper tool is available for this session.".to_owned(),
        })
    }

    async fn query_geometry(&self) -> Result<GeometryStatus, DesktopError> {
        let (display, tool_name, tool_path) = self.require_geometry()?;
        let argv = vec![
            "--display".to_owned(),
            display.clone(),
            "--query".to_owned(),
        ];
        let output = self
            .inner
            .runner
            .run(CommandRequest::new(tool_name, argv.clone()))
            .await
            .map_err(|error| self.query_command_error(&display, error))?;
        if !output.success() {
            let stderr = output.stderr.trim();
            if looks_like_no_display(stderr) {
                return Err(DesktopError::NoDisplay {
                    reason: format!("xrandr could not open display {display}: {stderr}"),
                });
            }
            return Err(DesktopError::QueryFailed {
                code: "display.geometry_query_failed",
                message: format!("xrandr --query failed ({}).", output.status_label()),
                details: serde_json::json!({
                    "display": display,
                    "argv": argv,
                    "status": output.status,
                    "stderr": stderr,
                }),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_xrandr_query(&text).map_err(|message| DesktopError::QueryFailed {
            code: "display.geometry_query_failed",
            message: format!("Could not parse xrandr --query output: {message}"),
            details: serde_json::json!({
                "display": display,
                "argv": argv,
                "stdout": text,
            }),
        })?;
        let screens = parsed
            .outputs
            .iter()
            .filter(|output| output.connected)
            .map(|output| GeometryScreen {
                name: output.name.clone(),
                width: output.width,
                height: output.height,
                x: output.x,
                y: output.y,
                primary: output.primary,
            })
            .collect();
        Ok(GeometryStatus {
            ok: true,
            available: true,
            display,
            tool: tool_path,
            argv,
            screen: parsed.screen,
            outputs: parsed.outputs,
            screens,
        })
    }

    fn query_command_error(&self, display: &str, error: CommandError) -> DesktopError {
        match error {
            CommandError::Timeout { .. } => DesktopError::QueryFailed {
                code: "display.geometry_query_failed",
                message: format!("xrandr --query timed out for display {display}."),
                details: serde_json::json!({ "display": display }),
            },
            CommandError::Spawn { message, .. } | CommandError::Io { message, .. } => {
                if looks_like_no_display(&message) {
                    DesktopError::NoDisplay {
                        reason: format!("xrandr could not open display {display}: {message}"),
                    }
                } else {
                    DesktopError::QueryFailed {
                        code: "display.geometry_query_failed",
                        message: format!("Could not run xrandr --query: {message}"),
                        details: serde_json::json!({ "display": display }),
                    }
                }
            }
        }
    }

    /// Read current display geometry.
    pub async fn geometry(&self) -> Result<GeometryStatus, DesktopError> {
        self.query_geometry().await
    }

    /// Apply one validated output mode/position/rotation.
    pub async fn set_geometry(
        &self,
        request: SetGeometryRequest,
    ) -> Result<GeometryChange, DesktopError> {
        let before = self.query_geometry().await?;
        let output = before
            .outputs
            .iter()
            .find(|output| output.name == request.output)
            .ok_or_else(|| DesktopError::Invalid {
                code: "display.mode_rejected",
                message: format!(
                    "Output {:?} is not advertised by xrandr on {}.",
                    request.output, before.display
                ),
                details: serde_json::json!({
                    "output": request.output,
                    "outputs": before.outputs.iter().map(|output| output.name.clone()).collect::<Vec<_>>(),
                }),
            })?;
        if !output.connected {
            return Err(DesktopError::Invalid {
                code: "display.mode_rejected",
                message: format!("Output {:?} is not connected.", request.output),
                details: serde_json::json!({ "output": request.output }),
            });
        }
        if output.modes.is_empty() {
            return Err(DesktopError::Invalid {
                code: "display.mode_rejected",
                message: format!("Output {:?} advertises no modes.", request.output),
                details: serde_json::json!({ "output": request.output }),
            });
        }

        let (mode, include_rate) = resolve_mode(output, request.mode.trim())?;
        let x = request.x.unwrap_or(output.x);
        let y = request.y.unwrap_or(output.y);
        for (name, value) in [("x", x), ("y", y)] {
            if !(-MAX_DISPLAY_COORDINATE..=MAX_DISPLAY_COORDINATE).contains(&value) {
                return Err(DesktopError::Invalid {
                    code: "display.mode_rejected",
                    message: format!(
                        "Position {name}={value} is outside ±{MAX_DISPLAY_COORDINATE}."
                    ),
                    details: serde_json::json!({ name: value }),
                });
            }
        }
        let rotation = request
            .rotation
            .as_deref()
            .unwrap_or(&output.rotation)
            .trim()
            .to_ascii_lowercase();
        if !matches!(rotation.as_str(), "normal" | "left" | "inverted" | "right") {
            return Err(DesktopError::Invalid {
                code: "display.mode_rejected",
                message: format!(
                    "Rotation {rotation:?} is not one of normal, left, inverted, right."
                ),
                details: serde_json::json!({ "rotation": rotation }),
            });
        }

        let mut argv = vec![
            "--display".to_owned(),
            before.display.clone(),
            "--output".to_owned(),
            output.name.clone(),
            "--mode".to_owned(),
            mode.name.clone(),
        ];
        if include_rate {
            if let Some(refresh) = mode.refresh_hz.filter(|refresh| *refresh > 0.0) {
                argv.push("--rate".to_owned());
                argv.push(format!("{refresh:.2}"));
            }
        }
        argv.push("--pos".to_owned());
        argv.push(format!("{x}x{y}"));
        argv.push("--rotate".to_owned());
        argv.push(rotation.clone());

        let previous = output.state();
        let output_name = output.name.clone();
        let display = before.display.clone();
        let applied_mode = mode.clone();

        let result = self
            .inner
            .runner
            .run(CommandRequest::new("xrandr", argv.clone()))
            .await;
        let failure_reason = match result {
            Err(error) => Some(format!("xrandr could not be run: {error}")),
            Ok(output) if !output.success() => Some(format!(
                "xrandr {} failed ({}): {}",
                argv.join(" "),
                output.status_label(),
                output.stderr.trim()
            )),
            Ok(_) => None,
        };
        if let Some(reason) = failure_reason {
            let rollback = self.rollback_geometry(&display, &previous).await;
            return Err(DesktopError::ApplyFailed {
                code: "display.geometry_apply_failed",
                message: format!("Display geometry was not changed: {reason}"),
                details: serde_json::json!({
                    "display": display,
                    "output": output_name,
                    "argv": argv,
                    "rolled_back": rollback.rolled_back,
                    "rollback_attempted": rollback.attempted,
                    "rollback_argv": rollback.argv,
                    "rollback_error": rollback.error,
                }),
            });
        }

        let after = match self.query_geometry().await {
            Ok(after) => after,
            Err(error) => {
                let rollback = self.rollback_geometry(&display, &previous).await;
                return Err(DesktopError::ApplyFailed {
                    code: "display.geometry_apply_failed",
                    message: format!(
                        "xrandr exited zero but the new geometry could not be read back: {}",
                        error.api_error().message
                    ),
                    details: serde_json::json!({
                        "display": display,
                        "output": output_name,
                        "argv": argv,
                        "rolled_back": rollback.rolled_back,
                        "rollback_attempted": rollback.attempted,
                        "rollback_argv": rollback.argv,
                        "rollback_error": rollback.error,
                    }),
                });
            }
        };
        let Some(applied_output) = after
            .outputs
            .iter()
            .find(|candidate| candidate.name == output_name)
        else {
            let rollback = self.rollback_geometry(&display, &previous).await;
            return Err(DesktopError::ApplyFailed {
                code: "display.geometry_apply_failed",
                message: format!(
                    "xrandr exited zero but output {output_name:?} disappeared from the query."
                ),
                details: serde_json::json!({
                    "display": display,
                    "output": output_name,
                    "argv": argv,
                    "rolled_back": rollback.rolled_back,
                    "rollback_attempted": rollback.attempted,
                    "rollback_argv": rollback.argv,
                    "rollback_error": rollback.error,
                }),
            });
        };
        let applied = applied_output.state();
        let matches = applied.width == applied_mode.width
            && applied.height == applied_mode.height
            && applied.x == x
            && applied.y == y
            && applied.rotation == rotation
            && (!include_rate || refresh_matches(applied.refresh_hz, applied_mode.refresh_hz));
        if !matches {
            let rollback = self.rollback_geometry(&display, &previous).await;
            return Err(DesktopError::ApplyFailed {
                code: "display.geometry_apply_failed",
                message: format!(
                    "xrandr exited zero but {output_name:?} did not read back as the requested \
                     mode/position/rotation."
                ),
                details: serde_json::json!({
                    "display": display,
                    "output": output_name,
                    "argv": argv,
                    "requested": {
                        "mode": applied_mode.name,
                        "refresh_hz": applied_mode.refresh_hz,
                        "x": x,
                        "y": y,
                        "rotation": rotation,
                    },
                    "observed": applied,
                    "rolled_back": rollback.rolled_back,
                    "rollback_attempted": rollback.attempted,
                    "rollback_argv": rollback.argv,
                    "rollback_error": rollback.error,
                }),
            });
        }

        Ok(GeometryChange {
            ok: true,
            changed: previous != applied,
            display,
            tool: before.tool,
            argv,
            previous,
            applied,
            rolled_back: false,
        })
    }

    async fn rollback_geometry(&self, display: &str, previous: &GeometryState) -> RollbackReport {
        let Some(mode) = previous.mode.as_deref() else {
            return RollbackReport {
                attempted: false,
                rolled_back: false,
                argv: Vec::new(),
                error: Some(
                    "the output had no active mode before the request, so there is nothing to \
                     restore"
                        .to_owned(),
                ),
            };
        };
        let mut argv = vec![
            "--display".to_owned(),
            display.to_owned(),
            "--output".to_owned(),
            previous.output.clone(),
            "--mode".to_owned(),
            mode.to_owned(),
        ];
        if let Some(refresh) = previous.refresh_hz.filter(|refresh| *refresh > 0.0) {
            argv.push("--rate".to_owned());
            argv.push(format!("{refresh:.2}"));
        }
        argv.push("--pos".to_owned());
        argv.push(format!("{}x{}", previous.x, previous.y));
        argv.push("--rotate".to_owned());
        argv.push(previous.rotation.clone());

        match self
            .inner
            .runner
            .run(CommandRequest::new("xrandr", argv.clone()))
            .await
        {
            Ok(output) if output.success() => {
                let verified = match self.query_geometry().await {
                    Ok(after) => after
                        .outputs
                        .iter()
                        .find(|candidate| candidate.name == previous.output)
                        .is_some_and(|candidate| {
                            geometry_state_matches(&candidate.state(), previous)
                        }),
                    Err(_) => false,
                };
                RollbackReport {
                    attempted: true,
                    rolled_back: verified,
                    argv,
                    error: (!verified).then(|| {
                        "the rollback command exited zero but the previous state did not read \
                         back"
                            .to_owned()
                    }),
                }
            }
            Ok(output) => RollbackReport {
                attempted: true,
                rolled_back: false,
                argv,
                error: Some(format!(
                    "rollback {} failed: {}",
                    output.status_label(),
                    output.stderr.trim()
                )),
            },
            Err(error) => RollbackReport {
                attempted: true,
                rolled_back: false,
                argv,
                error: Some(format!("rollback could not be run: {error}")),
            },
        }
    }

    /// Read UTF-8 text from the X11 clipboard.
    pub async fn clipboard(&self) -> Result<ClipboardRead, DesktopError> {
        let (_, tool, _) = self.require_clipboard()?;
        let (program, args) = clipboard_read_argv(&tool);
        let mut request =
            CommandRequest::new(program, args.iter().map(|arg| (*arg).to_owned()).collect());
        request.max_stdout = MAX_CLIPBOARD_BYTES + 1;
        let output =
            self.inner
                .runner
                .run(request)
                .await
                .map_err(|error| DesktopError::ApplyFailed {
                    code: "clipboard.read_failed",
                    message: format!("Could not run {tool}: {error}"),
                    details: serde_json::json!({ "tool": tool }),
                })?;
        if !output.success() {
            if looks_like_no_display(&output.stderr) {
                return Err(DesktopError::NoDisplay {
                    reason: format!(
                        "The X11 clipboard could not be read because the display is unavailable: \
                         {}",
                        output.stderr.trim()
                    ),
                });
            }
            return Err(DesktopError::ApplyFailed {
                code: "clipboard.read_failed",
                message: format!(
                    "{tool} failed ({}); the clipboard text is unknown.",
                    output.status_label()
                ),
                details: serde_json::json!({
                    "tool": tool,
                    "status": output.status,
                    "stderr": output.stderr.trim(),
                }),
            });
        }
        if output.stdout_truncated || output.stdout.len() > MAX_CLIPBOARD_BYTES {
            return Err(DesktopError::TooLarge {
                code: "clipboard.too_large",
                message: format!(
                    "The clipboard text exceeds the {MAX_CLIPBOARD_BYTES}-byte limit."
                ),
                details: serde_json::json!({ "limit": MAX_CLIPBOARD_BYTES, "tool": tool }),
            });
        }
        if output.stdout.contains(&0) {
            return Err(DesktopError::Invalid {
                code: "clipboard.encoding_unsupported",
                message:
                    "The clipboard contains NUL bytes (binary data); only UTF-8 text is supported."
                        .to_owned(),
                details: serde_json::json!({ "tool": tool }),
            });
        }
        let text = String::from_utf8(output.stdout).map_err(|error| DesktopError::Invalid {
            code: "clipboard.encoding_unsupported",
            message: "The clipboard is not valid UTF-8 text.".to_owned(),
            details: serde_json::json!({
                "tool": tool,
                "valid_up_to": error.utf8_error().valid_up_to(),
            }),
        })?;
        Ok(ClipboardRead {
            ok: true,
            bytes: text.len(),
            text,
            tool,
            selection: "clipboard",
        })
    }

    /// Write UTF-8 text to the X11 clipboard.
    pub async fn set_clipboard(&self, text: String) -> Result<ClipboardWrite, DesktopError> {
        let (_, tool, _) = self.require_clipboard()?;
        let bytes = text.as_bytes().to_vec();
        if bytes.len() > MAX_CLIPBOARD_BYTES {
            return Err(DesktopError::TooLarge {
                code: "clipboard.too_large",
                message: format!(
                    "The clipboard text is {} bytes; the limit is {MAX_CLIPBOARD_BYTES}.",
                    bytes.len()
                ),
                details: serde_json::json!({ "limit": MAX_CLIPBOARD_BYTES, "bytes": bytes.len() }),
            });
        }
        if bytes.contains(&0) {
            return Err(DesktopError::Invalid {
                code: "clipboard.encoding_unsupported",
                message: "Clipboard text must not contain NUL bytes.".to_owned(),
                details: Value::Null,
            });
        }
        let (program, args) = clipboard_write_argv(&tool);
        let request =
            CommandRequest::new(program, args.iter().map(|arg| (*arg).to_owned()).collect())
                .with_stdin(bytes.clone());
        let output =
            self.inner
                .runner
                .run(request)
                .await
                .map_err(|error| DesktopError::ApplyFailed {
                    code: "clipboard.write_failed",
                    message: format!("Could not run {tool}: {error}"),
                    details: serde_json::json!({ "tool": tool }),
                })?;
        if !output.success() {
            if looks_like_no_display(&output.stderr) {
                return Err(DesktopError::NoDisplay {
                    reason: format!(
                        "The X11 clipboard could not be written because the display is \
                         unavailable: {}",
                        output.stderr.trim()
                    ),
                });
            }
            return Err(DesktopError::ApplyFailed {
                code: "clipboard.write_failed",
                message: format!(
                    "{tool} failed ({}); the clipboard was not changed.",
                    output.status_label()
                ),
                details: serde_json::json!({
                    "tool": tool,
                    "status": output.status,
                    "stderr": output.stderr.trim(),
                }),
            });
        }
        Ok(ClipboardWrite {
            ok: true,
            written: true,
            bytes: bytes.len(),
            tool,
        })
    }

    /// Describe the wallpaper control without changing anything.
    pub async fn wallpaper(&self) -> Result<WallpaperStatus, DesktopError> {
        let tool = self.wallpaper_tool()?;
        let last_applied = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_wallpaper
            .clone();
        Ok(WallpaperStatus {
            ok: true,
            available: true,
            tool: tool.name().to_owned(),
            x11_display: self.inner.display.clone(),
            wayland_display: self.inner.wayland_display.clone(),
            last_applied,
            verified: false,
            note: "feh, xwallpaper, and swaybg do not expose a portable read-back command; \
                   last_applied is what this node set, not a claim about the current desktop.",
        })
    }

    /// Validate and apply one local image as the wallpaper.
    pub async fn set_wallpaper(&self, path: &str) -> Result<WallpaperChange, DesktopError> {
        let tool = self.wallpaper_tool()?;
        let (canonical, format, bytes) = validate_wallpaper_path(path).await?;
        let canonical_string = canonical.to_string_lossy().into_owned();
        let argv: Vec<String> = tool.args(&canonical_string);
        let program = tool.name().to_owned();
        let request = CommandRequest::new(program.clone(), argv.clone());

        if tool.persistent() {
            match self
                .inner
                .runner
                .spawn_background(request, BACKGROUND_GRACE)
                .await
            {
                Ok(BackgroundOutcome::Exited(output)) if output.success() => {}
                Ok(BackgroundOutcome::Exited(output)) => {
                    return Err(DesktopError::ApplyFailed {
                        code: "wallpaper.apply_failed",
                        message: format!(
                            "{program} exited immediately ({}); the wallpaper was not changed.",
                            output.status_label()
                        ),
                        details: serde_json::json!({
                            "tool": program,
                            "path": canonical_string,
                            "argv": argv,
                            "status": output.status,
                            "stderr": output.stderr.trim(),
                        }),
                    });
                }
                Ok(BackgroundOutcome::Running(handle)) => {
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(mut previous) = state.background.take() {
                        previous.kill();
                    }
                    state.background = Some(handle);
                }
                Err(error) => {
                    return Err(DesktopError::ApplyFailed {
                        code: "wallpaper.apply_failed",
                        message: format!("Could not start {program}: {error}"),
                        details: serde_json::json!({
                            "tool": program,
                            "path": canonical_string,
                            "argv": argv,
                        }),
                    });
                }
            }
        } else {
            let output = self.inner.runner.run(request).await.map_err(|error| {
                DesktopError::ApplyFailed {
                    code: "wallpaper.apply_failed",
                    message: format!("Could not run {program}: {error}"),
                    details: serde_json::json!({
                        "tool": program,
                        "path": canonical_string,
                        "argv": argv,
                    }),
                }
            })?;
            if !output.success() {
                return Err(DesktopError::ApplyFailed {
                    code: "wallpaper.apply_failed",
                    message: format!(
                        "{program} failed ({}); the wallpaper was not changed.",
                        output.status_label()
                    ),
                    details: serde_json::json!({
                        "tool": program,
                        "path": canonical_string,
                        "argv": argv,
                        "status": output.status,
                        "stderr": output.stderr.trim(),
                    }),
                });
            }
        }

        let applied = WallpaperApplied {
            path: canonical_string.clone(),
            tool: program.clone(),
        };
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.last_wallpaper = Some(applied);
        }
        Ok(WallpaperChange {
            ok: true,
            applied: true,
            path: canonical_string,
            tool: program,
            format,
            bytes,
            argv,
            persistent: tool.persistent(),
        })
    }
}

struct RollbackReport {
    attempted: bool,
    rolled_back: bool,
    argv: Vec<String>,
    error: Option<String>,
}

fn normalize_session_name(name: Option<String>) -> Option<String> {
    name.map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

fn looks_like_no_display(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("can't open display")
        || message.contains("cannot open display")
        || message.contains("unable to open display")
        || message.contains("couldn't connect to display")
        || message.contains("no display")
}

fn clipboard_read_argv(tool: &str) -> (&'static str, &'static [&'static str]) {
    if tool == "xsel" {
        ("xsel", &["-b", "-o"])
    } else {
        ("xclip", &["-selection", "clipboard", "-o"])
    }
}

fn clipboard_write_argv(tool: &str) -> (&'static str, &'static [&'static str]) {
    if tool == "xsel" {
        ("xsel", &["-b", "-i"])
    } else {
        ("xclip", &["-selection", "clipboard", "-i"])
    }
}

fn refresh_matches(applied: Option<f64>, requested: Option<f64>) -> bool {
    match (applied, requested) {
        (Some(applied), Some(requested)) => (applied - requested).abs() <= 0.05,
        _ => false,
    }
}

fn geometry_state_matches(candidate: &GeometryState, expected: &GeometryState) -> bool {
    candidate.width == expected.width
        && candidate.height == expected.height
        && candidate.x == expected.x
        && candidate.y == expected.y
        && candidate.rotation == expected.rotation
        && match (candidate.refresh_hz, expected.refresh_hz) {
            (Some(left), Some(right)) => (left - right).abs() <= 0.05,
            _ => true,
        }
}

fn resolve_mode<'a>(
    output: &'a XrandrOutput,
    requested: &str,
) -> Result<(&'a XrandrMode, bool), DesktopError> {
    if requested.is_empty() {
        return Err(DesktopError::Invalid {
            code: "display.mode_rejected",
            message: "mode must not be empty.".to_owned(),
            details: serde_json::json!({ "output": output.name }),
        });
    }
    let labels: Vec<String> = output.modes.iter().map(XrandrMode::label).collect();

    let exact: Vec<&XrandrMode> = output
        .modes
        .iter()
        .filter(|mode| mode.name == requested || mode.label() == requested)
        .collect();
    if exact.len() == 1 {
        let mode = exact[0];
        let include_rate = requested.contains('@') && mode.refresh_hz.is_some_and(|hz| hz > 0.0);
        return Ok((mode, include_rate));
    }
    if exact.len() > 1 {
        return Err(DesktopError::Invalid {
            code: "display.mode_rejected",
            message: format!(
                "Mode {requested:?} is advertised at several refresh rates; use one of the \
                 labels with @refresh."
            ),
            details: serde_json::json!({
                "output": output.name,
                "requested": requested,
                "labels": exact.iter().map(|mode| mode.label()).collect::<Vec<_>>(),
            }),
        });
    }

    let (width, height, refresh) =
        parse_requested_mode(requested).ok_or_else(|| DesktopError::Invalid {
            code: "display.mode_rejected",
            message: format!(
                "Mode {requested:?} is not an advertised mode name or WxH[@refresh] request."
            ),
            details: serde_json::json!({
                "output": output.name,
                "requested": requested,
                "modes": labels,
            }),
        })?;
    let candidates: Vec<&XrandrMode> = output
        .modes
        .iter()
        .filter(|mode| mode.width == width && mode.height == height)
        .filter(|mode| match (refresh, mode.refresh_hz) {
            (Some(requested), Some(actual)) => (requested - actual).abs() <= 0.05,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .collect();
    match candidates.len() {
        0 => Err(DesktopError::Invalid {
            code: "display.mode_rejected",
            message: format!(
                "Mode {requested:?} ({width}x{height}) is not advertised by output {:?}.",
                output.name
            ),
            details: serde_json::json!({
                "output": output.name,
                "requested": requested,
                "modes": labels,
            }),
        }),
        1 => Ok((candidates[0], refresh.is_some())),
        _ => Err(DesktopError::Invalid {
            code: "display.mode_rejected",
            message: format!(
                "Mode {requested:?} matches several advertised refresh rates; include \
                 @refresh and pick one of the labels."
            ),
            details: serde_json::json!({
                "output": output.name,
                "requested": requested,
                "labels": candidates.iter().map(|mode| mode.label()).collect::<Vec<_>>(),
            }),
        }),
    }
}

fn parse_requested_mode(requested: &str) -> Option<(u32, u32, Option<f64>)> {
    let (dims, refresh) = match requested.split_once('@') {
        Some((dims, refresh)) => (dims, Some(refresh.parse::<f64>().ok()?)),
        None => (requested, None),
    };
    let (width, height) = dims.split_once('x')?;
    let width = width.parse().ok()?;
    let height = parse_mode_height(height)?;
    Some((width, height, refresh))
}

fn parse_mode_height(value: &str) -> Option<u32> {
    let digits: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

struct ParsedQuery {
    screen: Option<XrandrScreen>,
    outputs: Vec<XrandrOutput>,
}

fn parse_xrandr_query(text: &str) -> Result<ParsedQuery, String> {
    let mut screen = None;
    let mut outputs: Vec<XrandrOutput> = Vec::new();
    let mut current: Option<XrandrOutput> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("Screen ") {
            if let Some(output) = current.take() {
                outputs.push(output);
            }
            screen = parse_screen_line(line).or(screen);
            continue;
        }
        if !raw.starts_with(char::is_whitespace) {
            if let Some(output) = current.take() {
                outputs.push(output);
            }
            if let Some(output) = parse_output_line(line) {
                current = Some(output);
            }
            continue;
        }
        if let Some(output) = current.as_mut() {
            output.modes.extend(parse_mode_line(line));
        }
    }
    if let Some(output) = current.take() {
        outputs.push(output);
    }
    if screen.is_none() && outputs.is_empty() {
        return Err("no screen or output lines were present".to_owned());
    }
    Ok(ParsedQuery { screen, outputs })
}

fn parse_screen_line(line: &str) -> Option<XrandrScreen> {
    let (_, rest) = line.split_once(':')?;
    let mut screen = XrandrScreen {
        min_width: 0,
        min_height: 0,
        current_width: 0,
        current_height: 0,
        max_width: 0,
        max_height: 0,
    };
    for part in rest.split(',') {
        let mut words = part.split_whitespace();
        let key = words.next()?;
        let width = words.next()?.parse::<u32>().ok()?;
        words.next()?;
        let height = words.next()?.parse::<u32>().ok()?;
        match key {
            "minimum" => {
                screen.min_width = width;
                screen.min_height = height;
            }
            "current" => {
                screen.current_width = width;
                screen.current_height = height;
            }
            "maximum" => {
                screen.max_width = width;
                screen.max_height = height;
            }
            _ => {}
        }
    }
    Some(screen)
}

fn parse_output_line(line: &str) -> Option<XrandrOutput> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?.to_owned();
    let status = parts.next()?;
    let connected = match status {
        "connected" => true,
        "disconnected" | "unknown" => false,
        _ => return None,
    };
    let mut primary = false;
    let mut geometry = None;
    let mut rotation = "normal".to_owned();
    for part in parts {
        if part.starts_with('(') {
            break;
        }
        if part == "primary" {
            primary = true;
            continue;
        }
        if let Some((width, height, x, y)) = parse_geometry_token(part) {
            geometry = Some((width, height, x, y));
            continue;
        }
        if matches!(part, "normal" | "left" | "inverted" | "right") {
            rotation = part.to_owned();
        }
    }
    let (width, height, x, y) = geometry.unwrap_or((0, 0, 0, 0));
    Some(XrandrOutput {
        name,
        connected,
        primary,
        width,
        height,
        x,
        y,
        rotation,
        modes: Vec::new(),
    })
}

fn parse_geometry_token(token: &str) -> Option<(u32, u32, i32, i32)> {
    let (dims, rest) = token.split_once('+')?;
    let mut positions = rest.split('+');
    let x = positions.next()?.parse::<i32>().ok()?;
    let y = positions.next()?.parse::<i32>().ok()?;
    if positions.next().is_some() {
        return None;
    }
    let (width, height) = dims.split_once('x')?;
    let width = width.parse().ok()?;
    let height = parse_mode_height(height)?;
    Some((width, height, x, y))
}

fn parse_mode_line(line: &str) -> Vec<XrandrMode> {
    let mut parts = line.split_whitespace();
    let Some(name) = parts.next().map(str::to_owned) else {
        return Vec::new();
    };
    let Some((width, height)) = name
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse().ok()?, parse_mode_height(height)?)))
    else {
        return Vec::new();
    };
    let mut modes = Vec::new();
    for token in parts {
        let mut numeric = token;
        let mut current = false;
        let mut preferred = false;
        while let Some(flag) = numeric.chars().last() {
            match flag {
                '*' => current = true,
                '+' => preferred = true,
                '!' => {}
                _ => break,
            }
            numeric = &numeric[..numeric.len() - flag.len_utf8()];
        }
        if let Ok(refresh_hz) = numeric.parse::<f64>() {
            modes.push(XrandrMode {
                name: name.clone(),
                width,
                height,
                refresh_hz: Some(refresh_hz),
                current,
                preferred,
            });
        }
    }
    if modes.is_empty() {
        // A mode line without a parsed refresh still names a usable mode.
        modes.push(XrandrMode {
            name,
            width,
            height,
            refresh_hz: None,
            current: false,
            preferred: false,
        });
    }
    modes
}

/// Every desktop control is a guarded control route, matching `/input/*`,
/// `/power/*`, and `/display/select`.
pub fn router(state: ConnectionState) -> Router {
    Router::new()
        .route(
            "/display/geometry",
            get(get_display_geometry).post(set_display_geometry),
        )
        .route("/clipboard", get(get_clipboard).post(set_clipboard))
        .route("/wallpaper", get(get_wallpaper).post(set_wallpaper))
        .with_state(state)
}

fn controls(state: &ConnectionState) -> Result<&DesktopControls, Response> {
    state
        .desktop_controls
        .as_ref()
        .ok_or_else(|| error_response(DesktopError::NotEnabled))
}

async fn get_display_geometry(State(state): State<ConnectionState>) -> Response {
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.geometry().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => error_response(error),
    }
}

async fn set_display_geometry(
    State(state): State<ConnectionState>,
    payload: Result<Json<SetGeometryRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.set_geometry(request).await {
        Ok(change) => Json(change).into_response(),
        Err(error) => error_response(error),
    }
}

async fn get_clipboard(State(state): State<ConnectionState>) -> Response {
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.clipboard().await {
        Ok(read) => Json(read).into_response(),
        Err(error) => error_response(error),
    }
}

async fn set_clipboard(
    State(state): State<ConnectionState>,
    payload: Result<Json<ClipboardWriteRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.set_clipboard(request.text).await {
        Ok(write) => Json(write).into_response(),
        Err(error) => error_response(error),
    }
}

async fn get_wallpaper(State(state): State<ConnectionState>) -> Response {
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.wallpaper().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => error_response(error),
    }
}

async fn set_wallpaper(
    State(state): State<ConnectionState>,
    payload: Result<Json<WallpaperRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.set_wallpaper(&request.path).await {
        Ok(change) => Json(change).into_response(),
        Err(error) => error_response(error),
    }
}

async fn validate_wallpaper_path(path: &str) -> Result<(PathBuf, String, u64), DesktopError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(invalid_wallpaper("path must not be empty."));
    }
    if trimmed.len() > 4096 {
        return Err(invalid_wallpaper("path is longer than 4096 bytes."));
    }
    if trimmed.contains("://") || trimmed.starts_with("http:") || trimmed.starts_with("https:") {
        return Err(DesktopError::Invalid {
            code: "wallpaper.remote_unsupported",
            message: "Wallpaper paths must be local files; this node does not download URLs."
                .to_owned(),
            details: serde_json::json!({ "path": trimmed }),
        });
    }
    let mut candidate = PathBuf::from(trimmed);
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let Some(home) = std::env::var_os("HOME") else {
            return Err(invalid_wallpaper(
                "cannot expand ~ because HOME is not set.",
            ));
        };
        candidate = PathBuf::from(home).join(rest);
    }
    if !candidate.is_absolute() {
        return Err(invalid_wallpaper(
            "path must be absolute (or start with ~/) so the node does not depend on its cwd.",
        ));
    }
    let canonical = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|error| invalid_wallpaper(&format!("path does not exist: {error}")))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|error| invalid_wallpaper(&format!("could not stat path: {error}")))?;
    if !metadata.is_file() {
        return Err(invalid_wallpaper("path must be a regular file."));
    }
    let bytes = metadata.len();
    if bytes == 0 {
        return Err(invalid_wallpaper("image file is empty."));
    }
    if bytes > MAX_WALLPAPER_BYTES {
        return Err(DesktopError::TooLarge {
            code: "wallpaper.too_large",
            message: format!(
                "Wallpaper is {bytes} bytes; the limit is {MAX_WALLPAPER_BYTES} bytes."
            ),
            details: serde_json::json!({ "bytes": bytes, "limit": MAX_WALLPAPER_BYTES }),
        });
    }
    let mut magic = [0_u8; WALLPAPER_MAGIC_BYTES];
    let mut file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|error| invalid_wallpaper(&format!("could not read image: {error}")))?;
    let read = file
        .read(&mut magic)
        .await
        .map_err(|error| invalid_wallpaper(&format!("could not read image: {error}")))?;
    let format = detect_image_format(&magic[..read]).ok_or_else(|| DesktopError::Invalid {
        code: "wallpaper.format_unsupported",
        message: "The file is not a supported PNG, JPEG, WebP, BMP, or GIF image.".to_owned(),
        details: serde_json::json!({ "path": canonical }),
    })?;
    Ok((canonical, format.to_owned(), bytes))
}

fn invalid_wallpaper(message: &str) -> DesktopError {
    DesktopError::Invalid {
        code: "wallpaper.invalid_path",
        message: message.to_owned(),
        details: Value::Null,
    }
}

fn detect_image_format(magic: &[u8]) -> Option<&'static str> {
    if magic.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if magic.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpeg");
    }
    if magic.len() >= 12 && &magic[0..4] == b"RIFF" && &magic[8..12] == b"WEBP" {
        return Some("webp");
    }
    if magic.starts_with(b"BM") {
        return Some("bmp");
    }
    if magic.starts_with(b"GIF87a") || magic.starts_with(b"GIF89a") {
        return Some("gif");
    }
    None
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    use crate::controller_compat::{CompatStats, ConnectionInfo, SINK_FAKE_RECORDING};

    /// One scripted runner response.
    pub enum Script {
        /// Result returned by `run`.
        Run(Result<CommandOutput, CommandError>),
        /// Result returned by `spawn_background`.
        Background(Result<BackgroundOutcome, CommandError>),
    }

    /// Runner that records argv and replays a script.
    pub struct FakeCommandRunner {
        tools: Mutex<BTreeMap<String, PathBuf>>,
        scripts: Mutex<VecDeque<Script>>,
        calls: Mutex<Vec<CommandRequest>>,
    }

    impl FakeCommandRunner {
        /// Build a runner with the named tools present.
        pub fn new<P: AsRef<str>>(tools: &[P], scripts: Vec<Script>) -> Self {
            let tools = tools
                .iter()
                .map(|tool| {
                    let name = tool.as_ref().to_owned();
                    (name.clone(), PathBuf::from(format!("/usr/bin/{name}")))
                })
                .collect();
            Self {
                tools: Mutex::new(tools),
                scripts: Mutex::new(scripts.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Recorded invocations in order.
        pub fn calls(&self) -> Vec<CommandRequest> {
            self.calls.lock().unwrap().clone()
        }

        /// Append one scripted response.
        pub fn push(&self, script: Script) {
            self.scripts.lock().unwrap().push_back(script);
        }

        fn next_script(&self) -> Option<Script> {
            self.scripts.lock().unwrap().pop_front()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeCommandRunner {
        fn lookup(&self, program: &str) -> Option<PathBuf> {
            self.tools.lock().unwrap().get(program).cloned()
        }

        async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError> {
            self.calls.lock().unwrap().push(request.clone());
            match self.next_script() {
                Some(Script::Run(result)) => result,
                Some(Script::Background(_)) | None => Err(CommandError::Spawn {
                    program: request.program,
                    message: "no scripted run response".to_owned(),
                }),
            }
        }

        async fn spawn_background(
            &self,
            request: CommandRequest,
            _grace: Duration,
        ) -> Result<BackgroundOutcome, CommandError> {
            self.calls.lock().unwrap().push(request.clone());
            match self.next_script() {
                Some(Script::Background(result)) => result,
                Some(Script::Run(_)) | None => Err(CommandError::Spawn {
                    program: request.program,
                    message: "no scripted background response".to_owned(),
                }),
            }
        }
    }

    /// Successful command with text stdout.
    pub fn ok(stdout: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput {
            status: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: String::new(),
            stdout_truncated: false,
        })
    }

    /// Failing command with a status and stderr.
    pub fn failed(status: i32, stderr: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput {
            status: Some(status),
            stdout: Vec::new(),
            stderr: stderr.to_owned(),
            stdout_truncated: false,
        })
    }

    /// Build controls with `:0` and a fake runner.
    pub fn fake_controls<P: AsRef<str>>(
        tools: &[P],
        scripts: Vec<Script>,
    ) -> (DesktopControls, Arc<FakeCommandRunner>) {
        let runner = Arc::new(FakeCommandRunner::new(tools, scripts));
        (
            DesktopControls::new(runner.clone(), Some(":0".to_owned()), None),
            runner,
        )
    }

    /// Build connection state with desktop controls attached.
    pub fn state(controls: DesktopControls) -> ConnectionState {
        ConnectionState::new(
            ConnectionInfo::new(
                "desktop-test._ozma._udp.local.",
                "desktop",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_desktop_controls(Some(controls))
    }

    /// A representative two-output `xrandr --query` document.
    pub const GEOMETRY_QUERY: &str = r#"Screen 0: minimum 8 x 8, current 3840 x 1080, maximum 32767 x 32767
DP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00
   1280x720      60.00
HDMI-1 connected 1920x1080+1920+0 left (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+
HDMI-2 disconnected (normal left inverted right x axis y axis)
"#;

    /// The same document after DP-1 moved to 1280x720 at 0,0.
    pub const GEOMETRY_QUERY_CHANGED: &str = r#"Screen 0: minimum 8 x 8, current 3200 x 720, maximum 32767 x 32767
DP-1 connected primary 1280x720+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00+  50.00
   1280x720      60.00*
HDMI-1 connected 1920x1080+1920+0 left (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+
HDMI-2 disconnected (normal left inverted right x axis y axis)
"#;
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn parses_outputs_modes_rotation_and_primary() {
        let parsed = parse_xrandr_query(GEOMETRY_QUERY).unwrap();
        let screen = parsed.screen.unwrap();
        assert_eq!(screen.current_width, 3840);
        assert_eq!(screen.current_height, 1080);
        assert_eq!(screen.max_width, 32767);

        assert_eq!(parsed.outputs.len(), 3);
        let dp1 = &parsed.outputs[0];
        assert_eq!(dp1.name, "DP-1");
        assert!(dp1.connected && dp1.primary);
        assert_eq!((dp1.width, dp1.height, dp1.x, dp1.y), (1920, 1080, 0, 0));
        assert_eq!(dp1.rotation, "normal");
        assert_eq!(dp1.modes.len(), 3);
        assert!(dp1.modes[0].current && dp1.modes[0].preferred);
        assert_eq!(dp1.modes[0].refresh_hz, Some(60.0));
        assert_eq!(dp1.modes[1].refresh_hz, Some(50.0));
        assert!(!dp1.modes[1].current);

        let hdmi1 = &parsed.outputs[1];
        assert_eq!(hdmi1.name, "HDMI-1");
        assert_eq!(hdmi1.x, 1920);
        // `left` is before the parenthesised rotation list; the list itself
        // must not overwrite it.
        assert_eq!(hdmi1.rotation, "left");

        let disconnected = &parsed.outputs[2];
        assert_eq!(disconnected.name, "HDMI-2");
        assert!(!disconnected.connected);
        assert_eq!(disconnected.rotation, "normal");
    }

    #[tokio::test]
    async fn geometry_get_uses_exact_query_argv_and_python_screen_shape() {
        let (controls, runner) = fake_controls(&["xrandr"], vec![Script::Run(ok(GEOMETRY_QUERY))]);
        let status = controls.geometry().await.unwrap();
        assert_eq!(status.argv, ["--display", ":0", "--query"]);
        assert_eq!(status.tool, "/usr/bin/xrandr");
        assert_eq!(status.screens.len(), 2);
        assert_eq!(status.screens[0].name, "DP-1");
        assert!(status.screens[0].primary);
        assert_eq!(runner.calls().len(), 1);
    }

    #[tokio::test]
    async fn geometry_get_without_display_or_tool_is_typed_503() {
        let runner = Arc::new(FakeCommandRunner::new(&["xrandr"], Vec::new()));
        let no_display = DesktopControls::new(runner.clone(), None, None);
        let error = no_display.geometry().await.unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.api_error().code, "desktop.no_display");

        let (no_tool, _) = fake_controls::<&str>(&[], Vec::new());
        let error = no_tool.geometry().await.unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.api_error().code, "desktop.tool_missing");
        assert!(error.api_error().details["candidates"]
            .as_array()
            .unwrap()
            .contains(&Value::String("xrandr".to_owned())));
    }

    #[tokio::test]
    async fn geometry_apply_uses_exact_argv_and_reports_only_verified_success() {
        let (controls, runner) = fake_controls(
            &["xrandr"],
            vec![
                Script::Run(ok(GEOMETRY_QUERY)),
                Script::Run(ok("")),
                Script::Run(ok(GEOMETRY_QUERY_CHANGED)),
            ],
        );
        let change = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-1".to_owned(),
                mode: "1280x720".to_owned(),
                x: Some(0),
                y: Some(0),
                rotation: Some("normal".to_owned()),
            })
            .await
            .unwrap();
        assert!(change.ok && change.changed && !change.rolled_back);
        assert_eq!(change.applied.width, 1280);
        assert_eq!(change.applied.height, 720);
        assert_eq!(
            change.argv,
            [
                "--display",
                ":0",
                "--output",
                "DP-1",
                "--mode",
                "1280x720",
                "--pos",
                "0x0",
                "--rotate",
                "normal"
            ]
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].program, "xrandr");
        assert_eq!(calls[1].args, change.argv);
    }

    #[tokio::test]
    async fn geometry_refresh_label_adds_exact_rate_argument() {
        let (controls, _) = fake_controls(
            &["xrandr"],
            vec![
                Script::Run(ok(GEOMETRY_QUERY)),
                Script::Run(ok("")),
                Script::Run(ok(GEOMETRY_QUERY)),
            ],
        );
        let change = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-1".to_owned(),
                mode: "1920x1080@60.00".to_owned(),
                x: Some(0),
                y: Some(0),
                rotation: Some("normal".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(
            change.argv,
            [
                "--display",
                ":0",
                "--output",
                "DP-1",
                "--mode",
                "1920x1080",
                "--rate",
                "60.00",
                "--pos",
                "0x0",
                "--rotate",
                "normal"
            ]
        );
    }

    #[tokio::test]
    async fn geometry_rejects_unknown_output_mode_and_ambiguous_refresh() {
        let (controls, _) = fake_controls(&["xrandr"], vec![Script::Run(ok(GEOMETRY_QUERY))]);
        let error = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-9".to_owned(),
                mode: "1280x720".to_owned(),
                x: None,
                y: None,
                rotation: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.api_error().code, "display.mode_rejected");

        let (controls, _) = fake_controls(&["xrandr"], vec![Script::Run(ok(GEOMETRY_QUERY))]);
        let error = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-1".to_owned(),
                mode: "1920x1080".to_owned(),
                x: None,
                y: None,
                rotation: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.api_error().code, "display.mode_rejected");
        assert!(error.api_error().details["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|label| label == "1920x1080@60.00"));

        let (controls, _) = fake_controls(&["xrandr"], vec![Script::Run(ok(GEOMETRY_QUERY))]);
        let error = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-1".to_owned(),
                mode: "999x999".to_owned(),
                x: None,
                y: None,
                rotation: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.api_error().code, "display.mode_rejected");
    }

    #[tokio::test]
    async fn geometry_failed_apply_rolls_back_to_previous_mode() {
        let (controls, runner) = fake_controls(
            &["xrandr"],
            vec![
                Script::Run(ok(GEOMETRY_QUERY)),
                Script::Run(failed(1, "cannot find mode")),
                Script::Run(ok("")),
                Script::Run(ok(GEOMETRY_QUERY)),
            ],
        );
        let error = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-1".to_owned(),
                mode: "1280x720".to_owned(),
                x: Some(0),
                y: Some(0),
                rotation: Some("normal".to_owned()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.api_error().code, "display.geometry_apply_failed");
        assert_eq!(error.api_error().details["rolled_back"], true);
        assert!(error.api_error().details.get("applied").is_none());
        let rollback = error.api_error().details["rollback_argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            rollback,
            [
                "--display",
                ":0",
                "--output",
                "DP-1",
                "--mode",
                "1920x1080",
                "--rate",
                "60.00",
                "--pos",
                "0x0",
                "--rotate",
                "normal"
            ]
        );
        assert_eq!(runner.calls().len(), 4);
    }

    #[tokio::test]
    async fn geometry_verification_failure_rolls_back() {
        let (controls, _) = fake_controls(
            &["xrandr"],
            vec![
                Script::Run(ok(GEOMETRY_QUERY)),
                Script::Run(ok("")),
                // The tool claimed success but the output still reads old state.
                Script::Run(ok(GEOMETRY_QUERY)),
                Script::Run(ok("")),
                Script::Run(ok(GEOMETRY_QUERY)),
            ],
        );
        let error = controls
            .set_geometry(SetGeometryRequest {
                output: "DP-1".to_owned(),
                mode: "1280x720".to_owned(),
                x: Some(0),
                y: Some(0),
                rotation: Some("normal".to_owned()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.api_error().code, "display.geometry_apply_failed");
        assert_eq!(error.api_error().details["rolled_back"], true);
        assert!(error.api_error().details["observed"].is_object());
    }

    #[tokio::test]
    async fn clipboard_read_uses_exact_argv_and_rejects_oversize_or_binary() {
        let (controls, runner) = fake_controls(
            &["xclip"],
            vec![
                Script::Run(ok("hello")),
                Script::Run(Ok(CommandOutput {
                    status: Some(0),
                    stdout: vec![b'x'; MAX_CLIPBOARD_BYTES + 1],
                    stderr: String::new(),
                    stdout_truncated: true,
                })),
                Script::Run(Ok(CommandOutput {
                    status: Some(0),
                    stdout: vec![0xFF, 0xFE],
                    stderr: String::new(),
                    stdout_truncated: false,
                })),
            ],
        );
        let read = controls.clipboard().await.unwrap();
        assert_eq!(read.text, "hello");
        assert_eq!(read.tool, "xclip");
        assert_eq!(runner.calls()[0].args, ["-selection", "clipboard", "-o"]);

        let error = controls.clipboard().await.unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.api_error().code, "clipboard.too_large");

        let error = controls.clipboard().await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.api_error().code, "clipboard.encoding_unsupported");
    }

    #[tokio::test]
    async fn clipboard_write_requires_zero_exit_and_bounded_text() {
        let (controls, runner) = fake_controls(
            &["xsel"],
            vec![Script::Run(ok("")), Script::Run(failed(1, "no display"))],
        );
        let write = controls.set_clipboard("hello".to_owned()).await.unwrap();
        assert!(write.written);
        assert_eq!(write.bytes, 5);
        assert_eq!(write.tool, "xsel");
        assert_eq!(runner.calls()[0].args, ["-b", "-i"]);
        assert_eq!(
            runner.calls()[0].stdin.as_deref(),
            Some(b"hello".as_slice())
        );

        let error = controls
            .set_clipboard("changed".to_owned())
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.api_error().code, "desktop.no_display");

        let error = controls
            .set_clipboard("x".repeat(MAX_CLIPBOARD_BYTES + 1))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn clipboard_missing_tool_is_typed_503() {
        let (controls, _) = fake_controls::<&str>(&[], Vec::new());
        let error = controls.clipboard().await.unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.api_error().code, "desktop.tool_missing");
        assert_eq!(error.api_error().details["control"], "clipboard");
    }

    #[tokio::test]
    async fn wallpaper_validates_path_format_and_size_before_running_a_tool() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("wall.png");
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0_u8; 32]);
        tokio::fs::write(&png, &bytes).await.unwrap();

        let (controls, runner) = fake_controls(&["feh"], vec![Script::Run(ok(""))]);
        let change = controls.set_wallpaper(png.to_str().unwrap()).await.unwrap();
        assert_eq!(change.format, "png");
        assert!(!change.persistent);
        assert_eq!(runner.calls()[0].args, ["--bg-fill", png.to_str().unwrap()]);

        let relative = controls.set_wallpaper("relative.png").await.unwrap_err();
        assert_eq!(relative.api_error().code, "wallpaper.invalid_path");

        let remote = controls
            .set_wallpaper("https://example.com/wall.png")
            .await
            .unwrap_err();
        assert_eq!(remote.api_error().code, "wallpaper.remote_unsupported");

        let missing = controls
            .set_wallpaper("/definitely/not/here.png")
            .await
            .unwrap_err();
        assert_eq!(missing.api_error().code, "wallpaper.invalid_path");

        let text = directory.path().join("not-image.txt");
        tokio::fs::write(&text, b"not an image").await.unwrap();
        let bad_format = controls
            .set_wallpaper(text.to_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(bad_format.api_error().code, "wallpaper.format_unsupported");

        let big = directory.path().join("big.png");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_WALLPAPER_BYTES + 1).unwrap();
        drop(file);
        let too_large = controls
            .set_wallpaper(big.to_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(too_large.api_error().code, "wallpaper.too_large");
    }

    #[tokio::test]
    async fn wallpaper_tool_failure_does_not_update_last_applied() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("wall.png");
        tokio::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
            .await
            .unwrap();
        let (controls, _) = fake_controls(&["feh"], vec![Script::Run(failed(1, "boom"))]);
        let error = controls
            .set_wallpaper(png.to_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.api_error().code, "wallpaper.apply_failed");
        assert!(controls.status().last_wallpaper.is_none());
        assert_eq!(controls.wallpaper().await.unwrap().last_applied, None);
    }

    #[tokio::test]
    async fn wallpaper_swaybg_running_is_success_and_persistent() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("wall.png");
        tokio::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
            .await
            .unwrap();
        struct FakeHandle;
        impl BackgroundHandle for FakeHandle {
            fn kill(&mut self) {}
        }
        let runner = Arc::new(FakeCommandRunner::new(
            &["swaybg"],
            vec![Script::Background(Ok(BackgroundOutcome::Running(
                Box::new(FakeHandle),
            )))],
        ));
        let controls = DesktopControls::new(runner.clone(), None, Some("wayland-0".to_owned()));
        let change = controls.set_wallpaper(png.to_str().unwrap()).await.unwrap();
        assert!(change.persistent);
        assert_eq!(change.tool, "swaybg");
        assert_eq!(
            runner.calls()[0].args,
            ["-i", png.to_str().unwrap(), "-m", "fill"]
        );
        assert_eq!(change.applied, true);
        assert_eq!(
            controls.status().last_wallpaper.as_ref().unwrap().tool,
            "swaybg"
        );
    }

    #[tokio::test]
    async fn status_advertises_tools_without_running_them() {
        let runner = Arc::new(FakeCommandRunner::new(
            &["xrandr", "xclip", "feh"],
            Vec::new(),
        ));
        let controls = DesktopControls::new(runner.clone(), Some(":0".to_owned()), None);
        let status = controls.status();
        assert!(status.geometry.available);
        assert!(status.clipboard.available);
        assert!(status.wallpaper.available);
        assert_eq!(status.geometry.tool.as_deref(), Some("xrandr"));
        assert_eq!(status.clipboard.tool.as_deref(), Some("xclip"));
        assert_eq!(status.wallpaper.tool.as_deref(), Some("feh"));
        assert_eq!(status.wallpaper_readback, false);
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn routes_return_typed_errors_and_success_with_fake_controls() {
        let (controls, _) = fake_controls(
            &["xrandr", "xclip", "feh"],
            vec![
                Script::Run(ok(GEOMETRY_QUERY)),
                Script::Run(ok("clip text")),
                Script::Run(ok("")),
            ],
        );
        let app = router(state(controls));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/geometry")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/clipboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["text"],
            "clip text"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/clipboard")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn routes_without_desktop_controls_are_typed_501() {
        let state = ConnectionState::new(
            crate::controller_compat::ConnectionInfo::new(
                "no-desktop._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                crate::controller_compat::SINK_FAKE_RECORDING,
            ),
            crate::controller_compat::CompatStats::default(),
        );
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/display/geometry")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["code"],
            "desktop.not_enabled"
        );
    }

    #[tokio::test]
    async fn real_runner_bounds_output_and_times_out() {
        let runner = SystemCommandRunner;
        let output = runner
            .run(CommandRequest::new(
                "sh",
                vec!["-c".to_owned(), "printf 0123456789".to_owned()],
            ))
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, b"0123456789");

        let mut bounded =
            CommandRequest::new("sh", vec!["-c".to_owned(), "printf 0123456789".to_owned()]);
        bounded.max_stdout = 4;
        let output = runner.run(bounded).await.unwrap();
        assert!(output.stdout_truncated);
        assert_eq!(output.stdout, b"0123");

        let mut slow = CommandRequest::new("sleep", vec!["5".to_owned()]);
        slow.timeout = Duration::from_millis(50);
        let error = runner.run(slow).await.unwrap_err();
        assert!(matches!(error, CommandError::Timeout { .. }));
    }

    #[tokio::test]
    #[ignore = "starts a throwaway Xvfb on a private display; set OZMA_DESKTOP_LIVE=1"]
    async fn live_xvfb_geometry_query_and_idempotent_set() {
        if std::env::var("OZMA_DESKTOP_LIVE").as_deref() != Ok("1") {
            eprintln!("OZMA_DESKTOP_LIVE is not 1; skipping live Xvfb test");
            return;
        }
        if SystemCommandRunner.lookup("Xvfb").is_none()
            || SystemCommandRunner.lookup("xrandr").is_none()
        {
            eprintln!("Xvfb/xrandr not installed; skipping live test");
            return;
        }
        let number = 90 + (std::process::id() % 50) as u16;
        let display = format!(":{number}");
        let mut child = std::process::Command::new("Xvfb")
            .args([&display, "-screen", "0", "1280x720x24", "-nolisten", "tcp"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn Xvfb");
        let controls =
            DesktopControls::new(Arc::new(SystemCommandRunner), Some(display.clone()), None);
        let mut status = None;
        for _ in 0..50 {
            match controls.geometry().await {
                Ok(value) => {
                    status = Some(value);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let status = status.expect("Xvfb xrandr query");
        let output = status
            .outputs
            .iter()
            .find(|output| output.connected)
            .expect("Xvfb connected output");
        let mode = output
            .current_mode()
            .expect("Xvfb current mode")
            .name
            .clone();

        let change = controls
            .set_geometry(SetGeometryRequest {
                output: output.name.clone(),
                mode: mode.clone(),
                x: Some(output.x),
                y: Some(output.y),
                rotation: Some(output.rotation.clone()),
            })
            .await
            .expect("idempotent mode set");
        assert!(change.ok);
        assert_eq!(change.applied.width, output.width);
        assert_eq!(change.applied.height, output.height);

        let error = controls
            .set_geometry(SetGeometryRequest {
                output: output.name.clone(),
                mode: "4096x2160".to_owned(),
                x: Some(0),
                y: Some(0),
                rotation: Some("normal".to_owned()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.api_error().code, "display.mode_rejected");

        let _ = child.kill();
        let _ = child.wait();
    }
}
