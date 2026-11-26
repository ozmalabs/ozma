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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// Largest scale factor accepted for one output (`--scale`).
pub const MAX_DISPLAY_SCALE: f64 = 4.0;
/// Smallest scale factor accepted for one output (`--scale`).
pub const MIN_DISPLAY_SCALE: f64 = 0.25;
/// Largest number of output entries accepted in one layout request.
pub const MAX_LAYOUT_OUTPUTS: usize = 16;
/// Largest number of logical monitors accepted in one layout request.
pub const MAX_LAYOUT_MONITORS: usize = 16;
/// Largest logical-monitor name accepted, in bytes.
pub const MAX_MONITOR_NAME_BYTES: usize = 64;
/// Files larger than this are not read while checking the wallpaper format.
const WALLPAPER_MAGIC_BYTES: usize = 12;
/// How long after the direct child exits the runner waits for inherited
/// stdout/stderr pipes to reach EOF.
///
/// `xclip -i`/`xsel -i` fork a selection-owner child that inherits both pipes and
/// keeps them open for the lifetime of the clipboard owner. Treating pipe EOF as
/// command completion would therefore wait forever (and return a false timeout).
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(150);
/// Environment variable tagging the selection-owner process tree started by
/// this node. It lets shutdown/replacement find an owner even when the tool
/// daemonizes into a new session (where a process-group kill cannot reach it).
const SELECTION_OWNER_ENV: &str = "OZMA_CLIPBOARD_OWNER";
/// Monotonic suffix for selection-owner markers.
static OWNER_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    /// Extra environment variables (for example `DISPLAY`) for the child.
    pub env: Vec<(String, String)>,
    /// Start the child in a new process group so it and any forked selection
    /// owner can be terminated together. The group id is returned in
    /// [`CommandOutput::group_id`].
    pub isolate_process_group: bool,
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
            env: Vec::new(),
            isolate_process_group: false,
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

    /// Add one environment variable for the child.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Start the child in its own process group.
    pub fn with_process_group(mut self, isolate: bool) -> Self {
        self.isolate_process_group = isolate;
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
    /// Process-group id when [`CommandRequest::isolate_process_group`] was set.
    ///
    /// The direct child is the group leader. A tool that forks a long-lived
    /// helper (X11 clipboard owners) leaves that helper in this group unless it
    /// daemonizes into a new session.
    pub group_id: Option<u32>,
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

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

type SharedOutput = Arc<Mutex<CapturedOutput>>;

/// Read into a shared buffer so a bounded drain grace can keep whatever the
/// direct child wrote before an inherited pipe was closed.
async fn read_into_shared<R>(mut reader: R, limit: usize, output: SharedOutput)
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        let mut captured = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if captured.truncated {
            break;
        }
        let remaining = limit.saturating_sub(captured.bytes.len());
        if read > remaining {
            captured.bytes.extend_from_slice(&chunk[..remaining]);
            captured.truncated = true;
            break;
        }
        captured.bytes.extend_from_slice(&chunk[..read]);
    }
}

fn snapshot_output(output: &SharedOutput) -> (Vec<u8>, bool) {
    let captured = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (captured.bytes.clone(), captured.truncated)
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
        for (key, value) in &request.env {
            command.env(key, value);
        }
        if request.isolate_process_group {
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| CommandError::spawn(&program, error))?;
        let group_id = request.isolate_process_group.then(|| child.id()).flatten();

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

        let max_stdout = request.max_stdout;
        let max_stderr = request.max_stderr;
        let stdout_output = Arc::new(Mutex::new(CapturedOutput::default()));
        let stderr_output = Arc::new(Mutex::new(CapturedOutput::default()));
        let mut stdout_task = child.stdout.take().map(|reader| {
            let output = stdout_output.clone();
            tokio::spawn(read_into_shared(reader, max_stdout, output))
        });
        let mut stderr_task = child.stderr.take().map(|reader| {
            let output = stderr_output.clone();
            tokio::spawn(read_into_shared(reader, max_stderr, output))
        });
        let timeout_program = program.clone();
        let program_for_wait = program.clone();

        // Command completion is the direct child's exit, not pipe EOF: tools
        // such as `xclip -i` fork a selection owner that inherits stdout/stderr
        // and stays alive. Readers run concurrently (so a large output cannot
        // deadlock the child) and are given a short bounded grace after exit.
        let status = match tokio::time::timeout(request.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                if let Some(group_id) = group_id {
                    terminate_process_group(group_id);
                }
                return Err(CommandError::io(&program_for_wait, error));
            }
            Err(_) => {
                if let Some(group_id) = group_id {
                    terminate_process_group(group_id);
                }
                return Err(CommandError::Timeout {
                    program: timeout_program,
                    timeout_ms: request.timeout.as_millis(),
                });
            }
        };

        let drain = async {
            if let Some(task) = stdout_task.as_mut() {
                let _ = task.await;
            }
            if let Some(task) = stderr_task.as_mut() {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(OUTPUT_DRAIN_GRACE, drain)
            .await
            .is_err()
        {
            if let Some(task) = stdout_task.as_ref() {
                task.abort();
            }
            if let Some(task) = stderr_task.as_ref() {
                task.abort();
            }
        }
        let (stdout, stdout_truncated) = snapshot_output(&stdout_output);
        let (stderr, _) = snapshot_output(&stderr_output);

        Ok(CommandOutput {
            status: status.code(),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_truncated,
            group_id,
        })
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
        for (key, value) in &request.env {
            command.env(key, value);
        }
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
                    Some(mut task) => {
                        match tokio::time::timeout(OUTPUT_DRAIN_GRACE, &mut task).await {
                            Ok(joined) => joined.unwrap_or_default(),
                            Err(_) => {
                                // A forked helper still holds stderr; the direct
                                // child's exit is the completion signal.
                                task.abort();
                                String::new()
                            }
                        }
                    }
                    None => String::new(),
                };
                Ok(BackgroundOutcome::Exited(CommandOutput {
                    status: status.code(),
                    stdout: Vec::new(),
                    stderr,
                    stdout_truncated: false,
                    group_id: None,
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

/// A process tree this node started for a long-lived desktop setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessOwner {
    /// Group id returned by the direct child, when it was isolated.
    pub group_id: Option<u32>,
    /// Marker carried in [`SELECTION_OWNER_ENV`] by the child and its forks.
    pub marker: Option<String>,
}

/// Injectable owner-termination boundary.
///
/// The real implementation sends `SIGTERM` to an isolated process group and to
/// processes carrying this node's marker. Tests substitute a recorder so owner
/// replacement and shutdown are observable without touching real processes.
pub trait ProcessController: Send + Sync {
    /// Terminate the process tree described by `owner`. Best-effort.
    fn terminate(&self, owner: &ProcessOwner);
}

/// Real process-tree terminator.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcessController;

impl ProcessController for SystemProcessController {
    fn terminate(&self, owner: &ProcessOwner) {
        let Some(marker) = owner.marker.as_deref() else {
            // Without a marker there is no way to prove a recycled process
            // group still belongs to this node, so do not risk killing it.
            return;
        };
        let marked = marked_processes(marker);
        if marked.is_empty() {
            return;
        }
        // A process-group kill is only safe when a process carrying this
        // node's unique marker is still in that group. Otherwise the id may
        // have been recycled by an unrelated process tree.
        if let Some(group_id) = owner
            .group_id
            .filter(|group| marked.iter().any(|(_, pgid)| pgid == group))
        {
            terminate_process_group(group_id);
            return;
        }
        for (pid, _) in marked {
            terminate_pid(pid);
        }
    }
}

fn terminate_pid(pid: i32) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` has no memory-safety preconditions. The pid came
        // from `/proc` and carries this node's unique marker.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

fn terminate_process_group(group_id: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` has no memory-safety preconditions. A negative pid
        // addresses the process group this node created for the child.
        unsafe {
            libc::kill(-(group_id as i32), libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = group_id;
    }
}

/// Find `(pid, process-group id)` for every live process carrying `marker`.
fn marked_processes(marker: &str) -> Vec<(i32, u32)> {
    #[cfg(target_os = "linux")]
    {
        let needle = format!("{SELECTION_OWNER_ENV}={marker}");
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let own_pid = std::process::id() as i32;
        let mut marked = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if pid == own_pid {
                continue;
            }
            let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            let matched = environ
                .split(|byte| *byte == 0)
                .any(|variable| variable == needle.as_bytes());
            if !matched {
                continue;
            }
            if let Some(pgid) = process_group_of(pid) {
                marked.push((pid, pgid));
            }
        }
        marked
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = marker;
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn process_group_of(pid: i32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field may contain spaces or parentheses, so parse after its
    // final `)`. Remaining fields start at state (3), ppid (4), pgrp (5).
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(2)?.parse().ok()
}

fn next_owner_marker() -> String {
    format!(
        "ozma-{}-{}",
        std::process::id(),
        OWNER_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
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
    /// Multi-output display layout control availability.
    pub layout: ControlAvailability,
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

/// One output as reported by `GET /display/layout`.
///
/// Unlike [`XrandrOutput`] this carries the active transform and physical size,
/// which are needed to verify a scale change and to restore a previous layout.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayoutOutput {
    /// Connector name (`DP-1`, `HDMI-1`, ...).
    pub name: String,
    /// Whether a display is connected.
    pub connected: bool,
    /// Whether the output has an active mode (a CRTC is scanning it).
    pub enabled: bool,
    /// Whether the server marks it primary.
    pub primary: bool,
    /// Active mode name.
    pub mode: Option<String>,
    /// Active refresh in Hz.
    pub refresh_hz: Option<f64>,
    /// Active mode width, before rotation/scaling.
    pub width: u32,
    /// Active mode height, before rotation/scaling.
    pub height: u32,
    /// Active X position in framebuffer coordinates.
    pub x: i32,
    /// Active Y position in framebuffer coordinates.
    pub y: i32,
    /// `normal`, `left`, `inverted`, or `right`.
    pub rotation: String,
    /// Horizontal scale from the output's transform matrix.
    pub scale_x: f64,
    /// Vertical scale from the output's transform matrix.
    pub scale_y: f64,
    /// Full 3x3 transform matrix as reported by `xrandr --verbose`.
    pub transform: [f64; 9],
    /// Physical width in millimetres, when advertised.
    pub mm_width: u32,
    /// Physical height in millimetres, when advertised.
    pub mm_height: u32,
    /// Advertised modes.
    pub modes: Vec<XrandrMode>,
}

/// One logical monitor from `xrandr --listmonitors`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct XrandrMonitor {
    /// Zero-based index in the server's monitor list.
    pub index: u32,
    /// Monitor name.
    pub name: String,
    /// True for a server-generated monitor (shown with `+`).
    pub automatic: bool,
    /// True when this monitor is primary (shown with `*`).
    pub primary: bool,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Physical width in millimetres.
    pub mm_width: u32,
    /// Physical height in millimetres.
    pub mm_height: u32,
    /// X position.
    pub x: i32,
    /// Y position.
    pub y: i32,
    /// Outputs this monitor covers.
    pub outputs: Vec<String>,
}

/// Output and monitor state used for change detection and rollback.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayoutState {
    /// Every advertised output.
    pub outputs: Vec<LayoutOutput>,
    /// Every defined logical monitor.
    pub monitors: Vec<XrandrMonitor>,
}

/// `GET /display/layout` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DisplayLayoutStatus {
    /// Always true on a 200.
    pub ok: bool,
    /// Always true on a 200.
    pub available: bool,
    /// X11 display the query targeted.
    pub display: String,
    /// Resolved `xrandr` path.
    pub tool: String,
    /// Exact argv used for the output query.
    pub query_argv: Vec<String>,
    /// Exact argv used for the transform/physical-size query.
    pub verbose_argv: Vec<String>,
    /// Exact argv used for the logical-monitor query.
    pub monitors_argv: Vec<String>,
    /// `Screen N:` summary, when parsed.
    pub screen: Option<XrandrScreen>,
    /// Every advertised output, including disabled/disconnected ones.
    pub outputs: Vec<LayoutOutput>,
    /// Logical monitors from `xrandr --listmonitors`.
    pub monitors: Vec<XrandrMonitor>,
}

impl DisplayLayoutStatus {
    fn state(&self) -> LayoutState {
        LayoutState {
            outputs: self.outputs.clone(),
            monitors: self.monitors.clone(),
        }
    }
}

/// One output in a `POST /display/layout` request.
#[derive(Clone, Debug, Deserialize)]
pub struct LayoutOutputRequest {
    /// Connector name.
    pub output: String,
    /// Enable (`true`) or disable (`--off`) the output. Defaults to the current
    /// state unless another field explicitly asks for an active output.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Advertised mode name, `name@refresh`, `WxH`, or `WxH@refresh`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Target X position. Defaults to the current position.
    #[serde(default)]
    pub x: Option<i32>,
    /// Target Y position. Defaults to the current position.
    #[serde(default)]
    pub y: Option<i32>,
    /// `normal`, `left`, `inverted`, or `right`; defaults to current.
    #[serde(default)]
    pub rotation: Option<String>,
    /// Set this output as the primary output (`true`) or leave the primary flag
    /// alone (`false`/absent).
    #[serde(default)]
    pub primary: Option<bool>,
    /// Mirror this output at another output's position (`--same-as`).
    #[serde(default)]
    pub same_as: Option<String>,
    /// Basic scale in the form `XxY` (for example `1.5x1.5`), bounded to
    /// [`MIN_DISPLAY_SCALE`]..=[`MAX_DISPLAY_SCALE`].
    #[serde(default)]
    pub scale: Option<String>,
}

/// One logical monitor in a `POST /display/layout` request.
#[derive(Clone, Debug, Deserialize)]
pub struct LayoutMonitorRequest {
    /// Monitor name (`main`, `left`, ...).
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Physical width in millimetres; defaults to `0`.
    #[serde(default)]
    pub mm_width: Option<u32>,
    /// Physical height in millimetres; defaults to `0`.
    #[serde(default)]
    pub mm_height: Option<u32>,
    /// X position; defaults to `0`.
    #[serde(default)]
    pub x: Option<i32>,
    /// Y position; defaults to `0`.
    #[serde(default)]
    pub y: Option<i32>,
    /// Outputs covered by this monitor. Must be enabled in the resulting layout.
    pub outputs: Vec<String>,
}

/// `POST /display/layout` request.
#[derive(Clone, Debug, Deserialize)]
pub struct SetLayoutRequest {
    /// Outputs to configure. Outputs not listed keep their current state.
    pub outputs: Vec<LayoutOutputRequest>,
    /// Logical monitors to define. `null`/absent leaves monitors untouched;
    /// `[]` removes the monitors this node previously defined.
    #[serde(default)]
    pub monitors: Option<Vec<LayoutMonitorRequest>>,
}

/// Successful `POST /display/layout` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayoutChange {
    /// Always true on a 200.
    pub ok: bool,
    /// True only when the read-back matches the requested layout (and every
    /// requested logical monitor is present).
    pub applied: bool,
    /// Whether the read-back differs from the state before the request.
    pub changed: bool,
    /// X11 display changed.
    pub display: String,
    /// Resolved `xrandr` path.
    pub tool: String,
    /// Exact argv that was applied.
    pub argv: Vec<String>,
    /// State before the change.
    pub previous: LayoutState,
    /// State read back after the change.
    pub layout: LayoutState,
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
    /// Resolved X11 display (`--x11-display`, else ambient `DISPLAY`).
    pub display: Option<String>,
    /// X11 display, when present. Kept as an explicit name for clients that
    /// read the same field from `desktop_controls`.
    pub x11_display: Option<String>,
    /// Wayland display, when present.
    pub wayland_display: Option<String>,
    /// Last wallpaper accepted by this node. Mirrors `last_applied` under the
    /// same name used by `/api/v1/connection.desktop_controls`.
    pub last_wallpaper: Option<WallpaperApplied>,
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
            // `--no-fehbg` keeps the operator's home directory untouched:
            // feh otherwise writes `~/.fehbg` as a side effect.
            Self::Feh => vec![
                "--no-fehbg".to_owned(),
                "--bg-fill".to_owned(),
                path.to_owned(),
            ],
            Self::Xwallpaper => vec!["--zoom".to_owned(), path.to_owned()],
            Self::Swaybg => vec![
                "-i".to_owned(),
                path.to_owned(),
                "-m".to_owned(),
                "fill".to_owned(),
            ],
        }
    }

    /// Session environment for this tool. The resolved display always wins
    /// over ambient values so a node cannot paint another seat.
    fn env(
        self,
        x11_display: Option<&str>,
        wayland_display: Option<&str>,
    ) -> Vec<(String, String)> {
        match self {
            Self::Feh | Self::Xwallpaper => x11_display
                .map(|display| vec![("DISPLAY".to_owned(), display.to_owned())])
                .unwrap_or_default(),
            Self::Swaybg => wayland_display
                .map(|display| vec![("WAYLAND_DISPLAY".to_owned(), display.to_owned())])
                .unwrap_or_default(),
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
    selection_owner: Option<ProcessOwner>,
    /// Logical monitor names this node created with the last successful layout.
    layout_monitors: Vec<String>,
}

struct DesktopInner {
    runner: Arc<dyn CommandRunner>,
    process: Arc<dyn ProcessController>,
    display: Option<String>,
    wayland_display: Option<String>,
    state: Mutex<DesktopState>,
}

impl Drop for DesktopInner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(owner) = state.selection_owner.take() {
            self.process.terminate(&owner);
        }
        if let Some(mut background) = state.background.take() {
            background.kill();
        }
    }
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
        Self::new_with_process(
            runner,
            Arc::new(SystemProcessController),
            display,
            wayland_display,
        )
    }

    fn new_with_process(
        runner: Arc<dyn CommandRunner>,
        process: Arc<dyn ProcessController>,
        display: Option<String>,
        wayland_display: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(DesktopInner {
                runner,
                process,
                display: normalize_session_name(display),
                wayland_display: normalize_session_name(wayland_display),
                state: Mutex::new(DesktopState::default()),
            }),
        }
    }

    /// Replace the owner-termination boundary. Test-only.
    #[cfg(test)]
    pub fn with_process_controller(mut self, process: Arc<dyn ProcessController>) -> Self {
        self.inner = Arc::new(DesktopInner {
            runner: self.inner.runner.clone(),
            process,
            display: self.inner.display.clone(),
            wayland_display: self.inner.wayland_display.clone(),
            state: Mutex::new(DesktopState::default()),
        });
        self
    }

    /// Stop the selection owner and any persistent wallpaper process this node
    /// started. Idempotent; called on node shutdown.
    pub fn shutdown(&self) {
        let (owner, background) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (state.selection_owner.take(), state.background.take())
        };
        if let Some(owner) = owner {
            self.inner.process.terminate(&owner);
        }
        if let Some(mut background) = background {
            background.kill();
        }
    }

    fn replace_selection_owner(&self, owner: ProcessOwner) {
        let previous = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.selection_owner.replace(owner)
        };
        if let Some(previous) = previous {
            self.inner.process.terminate(&previous);
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
        let layout = self.layout_availability();
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
            layout,
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

    fn layout_availability(&self) -> ControlAvailability {
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
        self.query_geometry_with_code("display.geometry_query_failed")
            .await
    }

    async fn query_geometry_with_code(
        &self,
        code: &'static str,
    ) -> Result<GeometryStatus, DesktopError> {
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
            .map_err(|error| self.query_command_error(&display, code, error))?;
        if !output.success() {
            let stderr = output.stderr.trim();
            if looks_like_no_display(stderr) {
                return Err(DesktopError::NoDisplay {
                    reason: format!("xrandr could not open display {display}: {stderr}"),
                });
            }
            return Err(DesktopError::QueryFailed {
                code,
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
            code,
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

    fn query_command_error(
        &self,
        display: &str,
        code: &'static str,
        error: CommandError,
    ) -> DesktopError {
        match error {
            CommandError::Timeout { .. } => DesktopError::QueryFailed {
                code,
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
                        code,
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

    /// Read every output and logical monitor.
    pub async fn layout(&self) -> Result<DisplayLayoutStatus, DesktopError> {
        self.query_layout().await
    }

    async fn query_layout(&self) -> Result<DisplayLayoutStatus, DesktopError> {
        let (display, tool_name, tool_path) = self.require_geometry()?;

        let verbose_argv = vec![
            "--display".to_owned(),
            display.clone(),
            "--verbose".to_owned(),
        ];
        let verbose = self
            .inner
            .runner
            .run(CommandRequest::new(tool_name.clone(), verbose_argv.clone()))
            .await
            .map_err(|error| {
                self.layout_command_error(&display, &verbose_argv, "xrandr --verbose", error)
            })?;
        if !verbose.success() {
            return Err(self.layout_query_failure(
                &display,
                &verbose_argv,
                "xrandr --verbose",
                &verbose,
            ));
        }
        if verbose.stdout_truncated {
            return Err(DesktopError::QueryFailed {
                code: "display.layout_query_failed",
                message: format!(
                    "xrandr --verbose exceeded the {MAX_COMMAND_STDOUT}-byte output bound."
                ),
                details: serde_json::json!({
                    "display": display,
                    "argv": verbose_argv,
                    "limit": MAX_COMMAND_STDOUT,
                }),
            });
        }
        let verbose_text = String::from_utf8_lossy(&verbose.stdout);
        let details = parse_verbose_details(&verbose_text);

        let query = self
            .query_geometry_with_code("display.layout_query_failed")
            .await?;

        let (monitors_argv, monitors) = self.query_monitors(&display).await?;

        let outputs = query
            .outputs
            .into_iter()
            .map(|output| {
                let detail = details.get(&output.name).copied().unwrap_or_default();
                let (mode, refresh_hz) = output
                    .current_mode()
                    .map(|mode| (Some(mode.name.clone()), mode.refresh_hz))
                    .unwrap_or((None, None));
                LayoutOutput {
                    name: output.name,
                    connected: output.connected,
                    enabled: output.width > 0 && output.height > 0,
                    primary: output.primary,
                    mode,
                    refresh_hz,
                    width: output.width,
                    height: output.height,
                    x: output.x,
                    y: output.y,
                    rotation: output.rotation,
                    scale_x: detail.transform[0],
                    scale_y: detail.transform[4],
                    transform: detail.transform,
                    mm_width: detail.mm_width,
                    mm_height: detail.mm_height,
                    modes: output.modes,
                }
            })
            .collect();

        Ok(DisplayLayoutStatus {
            ok: true,
            available: true,
            display,
            tool: tool_path,
            query_argv: query.argv,
            verbose_argv,
            monitors_argv,
            screen: query.screen,
            outputs,
            monitors,
        })
    }

    async fn query_monitors(
        &self,
        display: &str,
    ) -> Result<(Vec<String>, Vec<XrandrMonitor>), DesktopError> {
        let argv = vec![
            "--display".to_owned(),
            display.to_owned(),
            "--listmonitors".to_owned(),
        ];
        let output = self
            .inner
            .runner
            .run(CommandRequest::new("xrandr", argv.clone()))
            .await
            .map_err(|error| {
                self.layout_command_error(display, &argv, "xrandr --listmonitors", error)
            })?;
        if !output.success() {
            return Err(self.layout_query_failure(
                display,
                &argv,
                "xrandr --listmonitors",
                &output,
            ));
        }
        if output.stdout_truncated {
            return Err(DesktopError::QueryFailed {
                code: "display.layout_query_failed",
                message: format!(
                    "xrandr --listmonitors exceeded the {MAX_COMMAND_STDOUT}-byte output bound."
                ),
                details: serde_json::json!({
                    "display": display,
                    "argv": argv,
                    "limit": MAX_COMMAND_STDOUT,
                }),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let monitors = parse_listmonitors(&text).map_err(|message| DesktopError::QueryFailed {
            code: "display.layout_query_failed",
            message: format!("Could not parse xrandr --listmonitors output: {message}"),
            details: serde_json::json!({
                "display": display,
                "argv": argv,
                "stdout": text,
            }),
        })?;
        Ok((argv, monitors))
    }

    fn layout_command_error(
        &self,
        display: &str,
        argv: &[String],
        command: &str,
        error: CommandError,
    ) -> DesktopError {
        match error {
            CommandError::Timeout { .. } => DesktopError::QueryFailed {
                code: "display.layout_query_failed",
                message: format!("{command} timed out for display {display}."),
                details: serde_json::json!({ "display": display, "argv": argv }),
            },
            CommandError::Spawn { message, .. } | CommandError::Io { message, .. } => {
                if looks_like_no_display(&message) {
                    DesktopError::NoDisplay {
                        reason: format!("xrandr could not open display {display}: {message}"),
                    }
                } else {
                    DesktopError::QueryFailed {
                        code: "display.layout_query_failed",
                        message: format!("Could not run {command}: {message}"),
                        details: serde_json::json!({ "display": display, "argv": argv }),
                    }
                }
            }
        }
    }

    fn layout_query_failure(
        &self,
        display: &str,
        argv: &[String],
        command: &str,
        output: &CommandOutput,
    ) -> DesktopError {
        let stderr = output.stderr.trim();
        if looks_like_no_display(stderr) {
            return DesktopError::NoDisplay {
                reason: format!("xrandr could not open display {display}: {stderr}"),
            };
        }
        DesktopError::QueryFailed {
            code: "display.layout_query_failed",
            message: format!("{command} failed ({}).", output.status_label()),
            details: serde_json::json!({
                "display": display,
                "argv": argv,
                "status": output.status,
                "stderr": stderr,
            }),
        }
    }

    /// Validate and apply a multi-output layout.
    pub async fn set_layout(
        &self,
        request: SetLayoutRequest,
    ) -> Result<LayoutChange, DesktopError> {
        let before = self.query_layout().await?;
        let tracked_monitors = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.layout_monitors.clone()
        };
        let plan = plan_layout(&before, &request, &tracked_monitors)?;
        let display = before.display.clone();
        let previous = before.state();

        let result = self
            .inner
            .runner
            .run(CommandRequest::new("xrandr", plan.argv.clone()))
            .await;
        let failure_reason = match result {
            Err(error) => Some(format!("xrandr could not be run: {error}")),
            Ok(output) if !output.success() => Some(format!(
                "xrandr {} failed ({}): {}",
                plan.argv.join(" "),
                output.status_label(),
                output.stderr.trim()
            )),
            Ok(_) => None,
        };
        if let Some(reason) = failure_reason {
            let rollback = self
                .rollback_layout(&display, &previous, &plan.created_monitors)
                .await;
            return Err(DesktopError::ApplyFailed {
                code: "display.layout_apply_failed",
                message: format!("Display layout was not changed: {reason}"),
                details: layout_failure_details(&display, &plan, &rollback, Value::Null),
            });
        }

        let after = match self.query_layout().await {
            Ok(after) => after,
            Err(error) => {
                let rollback = self
                    .rollback_layout(&display, &previous, &plan.created_monitors)
                    .await;
                return Err(DesktopError::ApplyFailed {
                    code: "display.layout_apply_failed",
                    message: format!(
                        "xrandr exited zero but the new layout could not be read back: {}",
                        error.api_error().message
                    ),
                    details: layout_failure_details(&display, &plan, &rollback, Value::Null),
                });
            }
        };
        let mismatches = verify_layout(&plan, &after);
        if !mismatches.is_empty() {
            let rollback = self
                .rollback_layout(&display, &previous, &plan.created_monitors)
                .await;
            return Err(DesktopError::ApplyFailed {
                code: "display.layout_apply_failed",
                message: "xrandr exited zero but the layout did not read back as requested."
                    .to_owned(),
                details: layout_failure_details(
                    &display,
                    &plan,
                    &rollback,
                    serde_json::json!({
                        "mismatches": mismatches,
                        "observed": after.state(),
                    }),
                ),
            });
        }

        if let Some(monitors) = plan.monitors.as_ref() {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.layout_monitors = monitors
                .iter()
                .map(|monitor| monitor.name.clone())
                .collect();
        }

        let applied_state = after.state();
        Ok(LayoutChange {
            ok: true,
            applied: true,
            changed: !layout_states_match(&previous, &applied_state),
            display: before.display,
            tool: before.tool,
            argv: plan.argv,
            previous,
            layout: applied_state,
            rolled_back: false,
        })
    }

    async fn rollback_layout(
        &self,
        display: &str,
        previous: &LayoutState,
        created_monitors: &[String],
    ) -> LayoutRollbackReport {
        let output_argv = layout_rollback_output_argv(display, previous);
        let mut attempted = false;
        let mut errors = Vec::new();
        let mut monitor_ops = Vec::new();

        let mut output_ok = true;
        if output_argv.len() > 2 {
            attempted = true;
            match self
                .inner
                .runner
                .run(CommandRequest::new("xrandr", output_argv.clone()))
                .await
            {
                Ok(output) if output.success() => {}
                Ok(output) => {
                    output_ok = false;
                    errors.push(format!(
                        "output rollback {} failed: {}",
                        output.status_label(),
                        output.stderr.trim()
                    ));
                }
                Err(error) => {
                    output_ok = false;
                    errors.push(format!("output rollback could not be run: {error}"));
                }
            }
        }

        // Monitor deletion must only name a monitor the server actually has;
        // `xrandr --delmonitor missing` fails the whole invocation.
        let current_monitors = self
            .query_monitors(display)
            .await
            .map(|(_, monitors)| monitors)
            .unwrap_or_default();
        let previous_names: BTreeSet<&str> = previous
            .monitors
            .iter()
            .filter(|monitor| !monitor.automatic)
            .map(|monitor| monitor.name.as_str())
            .collect();

        let mut monitor_commands: Vec<Vec<String>> = Vec::new();
        for monitor in &current_monitors {
            if !monitor.automatic
                && created_monitors.iter().any(|name| name == &monitor.name)
                && !previous_names.contains(monitor.name.as_str())
            {
                monitor_commands.push(vec![
                    "--display".to_owned(),
                    display.to_owned(),
                    "--delmonitor".to_owned(),
                    monitor.name.clone(),
                ]);
            }
        }
        for monitor in previous
            .monitors
            .iter()
            .filter(|monitor| !monitor.automatic)
        {
            monitor_commands.push(vec![
                "--display".to_owned(),
                display.to_owned(),
                "--setmonitor".to_owned(),
                monitor.name.clone(),
                xrandr_monitor_geometry(monitor),
                monitor.outputs.join(","),
            ]);
        }

        let mut monitor_ok = true;
        for argv in monitor_commands {
            attempted = true;
            monitor_ops.push(argv.clone());
            match self
                .inner
                .runner
                .run(CommandRequest::new("xrandr", argv))
                .await
            {
                Ok(output) if output.success() => {}
                Ok(output) => {
                    monitor_ok = false;
                    errors.push(format!(
                        "monitor rollback {} failed: {}",
                        output.status_label(),
                        output.stderr.trim()
                    ));
                }
                Err(error) => {
                    monitor_ok = false;
                    errors.push(format!("monitor rollback could not be run: {error}"));
                }
            }
        }

        let mut verified = false;
        if output_ok && monitor_ok {
            match self.query_layout().await {
                Ok(after) => verified = layout_states_match(previous, &after.state()),
                Err(error) => errors.push(format!(
                    "layout rollback verification query failed: {}",
                    error.api_error().message
                )),
            }
        }

        LayoutRollbackReport {
            attempted,
            rolled_back: verified,
            output_argv,
            monitor_ops,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    /// Read UTF-8 text from the X11 clipboard.
    pub async fn clipboard(&self) -> Result<ClipboardRead, DesktopError> {
        let (display, tool, _) = self.require_clipboard()?;
        let (program, args) = clipboard_read_argv(&tool);
        let mut request =
            CommandRequest::new(program, args.iter().map(|arg| (*arg).to_owned()).collect())
                .with_env("DISPLAY", display.clone());
        request.max_stdout = MAX_CLIPBOARD_BYTES + 1;
        let output = self.inner.runner.run(request).await.map_err(|error| {
            if looks_like_no_display(&error.to_string()) {
                DesktopError::NoDisplay {
                    reason: format!(
                        "The X11 clipboard could not be read because display {display} is \
                         unavailable: {error}"
                    ),
                }
            } else {
                DesktopError::ApplyFailed {
                    code: "clipboard.read_failed",
                    message: format!("Could not run {tool}: {error}"),
                    details: serde_json::json!({ "tool": tool, "display": display }),
                }
            }
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
        let (display, tool, _) = self.require_clipboard()?;
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
        let marker = next_owner_marker();
        let request =
            CommandRequest::new(program, args.iter().map(|arg| (*arg).to_owned()).collect())
                .with_stdin(bytes.clone())
                .with_env("DISPLAY", display.clone())
                .with_env(SELECTION_OWNER_ENV, marker.clone())
                // `xclip`/`xsel` fork a selection owner. Isolating the process
                // group lets a later write or shutdown terminate exactly that
                // owner (plus the marker fallback for a daemonized child).
                .with_process_group(true);
        let output = self.inner.runner.run(request).await.map_err(|error| {
            if looks_like_no_display(&error.to_string()) {
                DesktopError::NoDisplay {
                    reason: format!(
                        "The X11 clipboard could not be written because display {display} is \
                         unavailable: {error}"
                    ),
                }
            } else {
                DesktopError::ApplyFailed {
                    code: "clipboard.write_failed",
                    message: format!("Could not run {tool}: {error}"),
                    details: serde_json::json!({ "tool": tool, "display": display }),
                }
            }
        })?;
        let candidate = ProcessOwner {
            group_id: output.group_id,
            marker: Some(marker),
        };
        if !output.success() {
            self.inner.process.terminate(&candidate);
            if looks_like_no_display(&output.stderr) {
                return Err(DesktopError::NoDisplay {
                    reason: format!(
                        "The X11 clipboard could not be written because display {display} is \
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
                    "display": display,
                    "status": output.status,
                    "stderr": output.stderr.trim(),
                }),
            });
        }
        self.replace_selection_owner(candidate);
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
            display: self.inner.display.clone(),
            x11_display: self.inner.display.clone(),
            wayland_display: self.inner.wayland_display.clone(),
            last_wallpaper: last_applied.clone(),
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
        let mut request = CommandRequest::new(program.clone(), argv.clone());
        for (key, value) in tool.env(
            self.inner.display.as_deref(),
            self.inner.wayland_display.as_deref(),
        ) {
            request = request.with_env(key, value);
        }

        if tool.persistent() {
            match self
                .inner
                .runner
                .spawn_background(request, BACKGROUND_GRACE)
                .await
            {
                Ok(BackgroundOutcome::Exited(output)) if output.success() => {}
                Ok(BackgroundOutcome::Exited(output)) if looks_like_no_display(&output.stderr) => {
                    return Err(DesktopError::NoDisplay {
                        reason: format!(
                            "The wallpaper could not be applied because the {program} session is \
                             unavailable: {}",
                            output.stderr.trim()
                        ),
                    });
                }
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
                if looks_like_no_display(&error.to_string()) {
                    DesktopError::NoDisplay {
                        reason: format!(
                            "The wallpaper could not be applied because the {program} display is \
                             unavailable: {error}"
                        ),
                    }
                } else {
                    DesktopError::ApplyFailed {
                        code: "wallpaper.apply_failed",
                        message: format!("Could not run {program}: {error}"),
                        details: serde_json::json!({
                            "tool": program,
                            "path": canonical_string,
                            "argv": argv,
                        }),
                    }
                }
            })?;
            if !output.success() {
                if looks_like_no_display(&output.stderr) {
                    return Err(DesktopError::NoDisplay {
                        reason: format!(
                            "The wallpaper could not be applied because the {program} display is \
                             unavailable: {}",
                            output.stderr.trim()
                        ),
                    });
                }
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

const IDENTITY_TRANSFORM: [f64; 9] = [
    1.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, //
    0.0, 0.0, 1.0,
];

#[derive(Clone, Copy, Debug)]
struct VerboseOutput {
    transform: [f64; 9],
    mm_width: u32,
    mm_height: u32,
}

impl Default for VerboseOutput {
    fn default() -> Self {
        Self {
            transform: IDENTITY_TRANSFORM,
            mm_width: 0,
            mm_height: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LayoutRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug)]
struct PlannedOutput {
    name: String,
    enabled: bool,
    mode: Option<XrandrMode>,
    x: i32,
    y: i32,
    rotation: String,
    scale_x: f64,
    scale_y: f64,
    transform: [f64; 9],
    primary: bool,
}

#[derive(Clone, Debug)]
struct PlannedMonitor {
    name: String,
    width: u32,
    height: u32,
    mm_width: u32,
    mm_height: u32,
    x: i32,
    y: i32,
    outputs: Vec<String>,
}

struct LayoutPlan {
    argv: Vec<String>,
    outputs: Vec<PlannedOutput>,
    monitors: Option<Vec<PlannedMonitor>>,
    deleted_monitors: Vec<String>,
    created_monitors: Vec<String>,
}

impl LayoutPlan {
    fn requested_json(&self) -> Value {
        let outputs = self
            .outputs
            .iter()
            .map(|output| {
                serde_json::json!({
                    "output": output.name,
                    "enabled": output.enabled,
                    "mode": output.mode.as_ref().map(|mode| mode.name.clone()),
                    "refresh_hz": output.mode.as_ref().and_then(|mode| mode.refresh_hz),
                    "x": output.x,
                    "y": output.y,
                    "rotation": output.rotation,
                    "primary": output.primary,
                    "scale_x": output.scale_x,
                    "scale_y": output.scale_y,
                })
            })
            .collect::<Vec<_>>();
        let monitors = self.monitors.as_ref().map(|monitors| {
            monitors
                .iter()
                .map(|monitor| {
                    serde_json::json!({
                        "name": monitor.name,
                        "width": monitor.width,
                        "height": monitor.height,
                        "mm_width": monitor.mm_width,
                        "mm_height": monitor.mm_height,
                        "x": monitor.x,
                        "y": monitor.y,
                        "outputs": monitor.outputs,
                    })
                })
                .collect::<Vec<_>>()
        });
        serde_json::json!({
            "outputs": outputs,
            "monitors": monitors,
            "deleted_monitors": self.deleted_monitors,
        })
    }
}

struct LayoutRollbackReport {
    attempted: bool,
    rolled_back: bool,
    output_argv: Vec<String>,
    monitor_ops: Vec<Vec<String>>,
    error: Option<String>,
}

fn layout_invalid(message: impl Into<String>, details: Value) -> DesktopError {
    DesktopError::Invalid {
        code: "display.layout_rejected",
        message: message.into(),
        details,
    }
}

fn layout_too_large(message: impl Into<String>, details: Value) -> DesktopError {
    DesktopError::TooLarge {
        code: "display.layout_too_large",
        message: message.into(),
        details,
    }
}

fn parse_float_row(value: &str) -> Option<Vec<f64>> {
    let values: Vec<f64> = value
        .split_whitespace()
        .filter_map(|token| token.trim_end_matches(',').parse::<f64>().ok())
        .collect();
    (values.len() == 3).then_some(values)
}

fn finish_verbose_output(
    current: &mut Option<(String, VerboseOutput)>,
    rows: &mut Vec<Vec<f64>>,
    details: &mut BTreeMap<String, VerboseOutput>,
) {
    if let Some((name, mut detail)) = current.take() {
        if rows.len() == 3 && rows.iter().all(|row| row.len() == 3) {
            let mut transform = [0.0; 9];
            for (index, row) in rows.iter().enumerate() {
                transform[index * 3..index * 3 + 3].copy_from_slice(row);
            }
            detail.transform = transform;
        }
        details.insert(name, detail);
    }
    rows.clear();
}

fn parse_verbose_details(text: &str) -> BTreeMap<String, VerboseOutput> {
    let mut details = BTreeMap::new();
    let mut current: Option<(String, VerboseOutput)> = None;
    let mut rows: Vec<Vec<f64>> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if !raw.starts_with(char::is_whitespace) {
            finish_verbose_output(&mut current, &mut rows, &mut details);
            if let Some(output) = parse_output_line(line) {
                let (mm_width, mm_height) = parse_physical_size(line).unwrap_or((0, 0));
                current = Some((
                    output.name,
                    VerboseOutput {
                        transform: IDENTITY_TRANSFORM,
                        mm_width,
                        mm_height,
                    },
                ));
            }
            continue;
        }
        if current.is_none() {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Transform:") {
            rows.clear();
            if let Some(row) = parse_float_row(rest) {
                rows.push(row);
            }
        } else if !rows.is_empty() && rows.len() < 3 {
            if let Some(row) = parse_float_row(trimmed) {
                rows.push(row);
            }
        }
    }
    finish_verbose_output(&mut current, &mut rows, &mut details);
    details
}

fn parse_physical_size(line: &str) -> Option<(u32, u32)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for (index, token) in tokens.iter().enumerate() {
        let Some(width) = token
            .strip_suffix("mm")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if tokens.get(index + 1) != Some(&"x") {
            continue;
        }
        let Some(height) = tokens
            .get(index + 2)
            .and_then(|value| value.strip_suffix("mm"))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        return Some((width, height));
    }
    None
}

fn parse_monitor_geometry(value: &str) -> Result<(u32, u32, u32, u32, i32, i32), String> {
    let bad = || format!("invalid monitor geometry {value:?}");
    let (width, rest) = value.split_once('/').ok_or_else(bad)?;
    let (mm_width, rest) = rest.split_once('x').ok_or_else(bad)?;
    let (height, rest) = rest.split_once('/').ok_or_else(bad)?;
    let (mm_height, rest) = rest.split_once('+').ok_or_else(bad)?;
    let (x, y) = rest.split_once('+').ok_or_else(bad)?;
    Ok((
        width.parse().map_err(|_| bad())?,
        mm_width.parse().map_err(|_| bad())?,
        height.parse().map_err(|_| bad())?,
        mm_height.parse().map_err(|_| bad())?,
        x.parse().map_err(|_| bad())?,
        y.parse().map_err(|_| bad())?,
    ))
}

fn parse_listmonitors(text: &str) -> Result<Vec<XrandrMonitor>, String> {
    let mut monitors = Vec::new();
    let mut saw_header = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Monitors:") {
            saw_header = true;
            let count = rest
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid monitor count {:?}", rest.trim()))?;
            if count > 1024 {
                return Err(format!("unreasonable monitor count {count}"));
            }
            continue;
        }
        if !saw_header {
            continue;
        }
        let Some((index, rest)) = line.split_once(':') else {
            continue;
        };
        let index: u32 = index
            .trim()
            .parse()
            .map_err(|_| format!("invalid monitor index {index:?}"))?;
        let rest = rest.trim_start();
        let mut offset = 0;
        let bytes = rest.as_bytes();
        let automatic = bytes.get(offset) == Some(&b'+');
        if automatic {
            offset += 1;
        }
        let primary = bytes.get(offset) == Some(&b'*');
        if primary {
            offset += 1;
        }
        let rest = &rest[offset..];
        let mut parts = rest.split_whitespace();
        let name = parts
            .next()
            .ok_or_else(|| "monitor line has no name".to_owned())?
            .to_owned();
        let geometry = parts
            .next()
            .ok_or_else(|| format!("monitor {name:?} has no geometry"))?;
        let (width, mm_width, height, mm_height, x, y) = parse_monitor_geometry(geometry)?;
        let outputs = parts
            .flat_map(|part| part.split(','))
            .filter(|part| !part.is_empty() && *part != "none")
            .map(str::to_owned)
            .collect();
        monitors.push(XrandrMonitor {
            index,
            name,
            automatic,
            primary,
            width,
            height,
            mm_width,
            mm_height,
            x,
            y,
            outputs,
        });
    }
    if !saw_header {
        return Err("no Monitors: header was present".to_owned());
    }
    Ok(monitors)
}

fn layout_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    rotation: &str,
    scale_x: f64,
    scale_y: f64,
) -> LayoutRect {
    let scale_x = if scale_x.is_finite() && scale_x > 0.0 {
        scale_x
    } else {
        1.0
    };
    let scale_y = if scale_y.is_finite() && scale_y > 0.0 {
        scale_y
    } else {
        1.0
    };
    let mut width = ((width as f64) * scale_x).round().max(1.0) as u32;
    let mut height = ((height as f64) * scale_y).round().max(1.0) as u32;
    if matches!(rotation, "left" | "right") {
        std::mem::swap(&mut width, &mut height);
    }
    LayoutRect {
        x,
        y,
        width,
        height,
    }
}

fn rects_overlap(left: LayoutRect, right: LayoutRect) -> bool {
    let left_right = i64::from(left.x) + i64::from(left.width);
    let right_right = i64::from(right.x) + i64::from(right.width);
    let left_bottom = i64::from(left.y) + i64::from(left.height);
    let right_bottom = i64::from(right.y) + i64::from(right.height);
    i64::from(left.x) < right_right
        && i64::from(right.x) < left_right
        && i64::from(left.y) < right_bottom
        && i64::from(right.y) < left_bottom
}

fn check_layout_bounds(
    rect: LayoutRect,
    screen: Option<&XrandrScreen>,
    what: &str,
) -> Result<(), DesktopError> {
    if rect.x < 0
        || rect.y < 0
        || rect.x > MAX_DISPLAY_COORDINATE
        || rect.y > MAX_DISPLAY_COORDINATE
    {
        return Err(layout_invalid(
            format!(
                "{what} position {}x{} is outside 0..={MAX_DISPLAY_COORDINATE}.",
                rect.x, rect.y
            ),
            serde_json::json!({ "x": rect.x, "y": rect.y }),
        ));
    }
    let max_width = screen
        .map(|screen| screen.max_width)
        .filter(|value| *value > 0)
        .unwrap_or(MAX_DISPLAY_COORDINATE as u32);
    let max_height = screen
        .map(|screen| screen.max_height)
        .filter(|value| *value > 0)
        .unwrap_or(MAX_DISPLAY_COORDINATE as u32);
    if rect.width == 0 || rect.height == 0 {
        return Err(layout_invalid(
            format!("{what} has zero width or height."),
            serde_json::json!({ "width": rect.width, "height": rect.height }),
        ));
    }
    if i64::from(rect.x) + i64::from(rect.width) > i64::from(max_width)
        || i64::from(rect.y) + i64::from(rect.height) > i64::from(max_height)
    {
        return Err(layout_invalid(
            format!(
                "{what} {}x{}+{}+{} exceeds the screen maximum {max_width}x{max_height}.",
                rect.width, rect.height, rect.x, rect.y
            ),
            serde_json::json!({
                "x": rect.x,
                "y": rect.y,
                "width": rect.width,
                "height": rect.height,
                "max_width": max_width,
                "max_height": max_height,
            }),
        ));
    }
    Ok(())
}

fn parse_layout_scale(value: &str) -> Result<(f64, f64), DesktopError> {
    let trimmed = value.trim();
    let (x, y) = match trimmed.split_once('x').or_else(|| trimmed.split_once('X')) {
        Some((x, y)) => (x.trim(), y.trim()),
        None => (trimmed, trimmed),
    };
    let parse = |part: &str| {
        part.parse::<f64>().map_err(|_| {
            layout_invalid(
                format!("Scale {value:?} is not a number in the form XxY."),
                serde_json::json!({ "scale": value }),
            )
        })
    };
    let scale_x = parse(x)?;
    let scale_y = parse(y)?;
    if !scale_x.is_finite()
        || !scale_y.is_finite()
        || !(MIN_DISPLAY_SCALE..=MAX_DISPLAY_SCALE).contains(&scale_x)
        || !(MIN_DISPLAY_SCALE..=MAX_DISPLAY_SCALE).contains(&scale_y)
    {
        return Err(layout_invalid(
            format!("Scale {value:?} is outside {MIN_DISPLAY_SCALE}..={MAX_DISPLAY_SCALE}."),
            serde_json::json!({
                "scale": value,
                "min": MIN_DISPLAY_SCALE,
                "max": MAX_DISPLAY_SCALE,
            }),
        ));
    }
    Ok((scale_x, scale_y))
}

fn valid_monitor_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MONITOR_NAME_BYTES
        && name != "none"
        && !name.starts_with('+')
        && !name.starts_with('*')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '.' | ':' | '@' | '/')
        })
}

fn normalize_rotation(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    matches!(value.as_str(), "normal" | "left" | "inverted" | "right").then_some(value)
}

fn layout_current_mode(output: &LayoutOutput) -> Option<XrandrMode> {
    if !output.enabled {
        return None;
    }
    output
        .modes
        .iter()
        .find(|mode| mode.current && output.mode.as_deref() == Some(mode.name.as_str()))
        .cloned()
        .or_else(|| {
            output
                .mode
                .as_ref()
                .and_then(|name| output.modes.iter().find(|mode| &mode.name == name).cloned())
        })
        .or_else(|| {
            (output.width > 0 && output.height > 0).then(|| XrandrMode {
                name: format!("{}x{}", output.width, output.height),
                width: output.width,
                height: output.height,
                refresh_hz: output.refresh_hz,
                current: true,
                preferred: false,
            })
        })
}

fn planned_from_current(output: &LayoutOutput) -> PlannedOutput {
    PlannedOutput {
        name: output.name.clone(),
        enabled: output.enabled,
        mode: layout_current_mode(output),
        x: output.x,
        y: output.y,
        rotation: output.rotation.clone(),
        scale_x: output.scale_x.max(f64::EPSILON),
        scale_y: output.scale_y.max(f64::EPSILON),
        transform: output.transform,
        primary: output.primary,
    }
}

fn resolve_layout_mode(
    output: &LayoutOutput,
    requested: Option<&str>,
) -> Result<XrandrMode, DesktopError> {
    if let Some(requested) = requested {
        return Ok(
            resolve_mode_from_modes(&output.modes, &output.name, requested.trim())?
                .0
                .clone(),
        );
    }
    if let Some(mode) = layout_current_mode(output) {
        return Ok(mode);
    }
    output
        .modes
        .iter()
        .find(|mode| mode.preferred)
        .or_else(|| output.modes.first())
        .cloned()
        .ok_or_else(|| {
            layout_invalid(
                format!("Output {:?} advertises no modes.", output.name),
                serde_json::json!({ "output": output.name }),
            )
        })
}

fn planned_rect(output: &PlannedOutput) -> Option<LayoutRect> {
    let mode = output.mode.as_ref()?;
    if !output.enabled || mode.width == 0 || mode.height == 0 {
        return None;
    }
    Some(layout_rect(
        output.x,
        output.y,
        mode.width,
        mode.height,
        &output.rotation,
        output.scale_x,
        output.scale_y,
    ))
}

fn plan_layout(
    status: &DisplayLayoutStatus,
    request: &SetLayoutRequest,
    tracked_monitors: &[String],
) -> Result<LayoutPlan, DesktopError> {
    if request.outputs.len() > MAX_LAYOUT_OUTPUTS {
        return Err(layout_too_large(
            format!(
                "The layout lists {} outputs; the limit is {MAX_LAYOUT_OUTPUTS}.",
                request.outputs.len()
            ),
            serde_json::json!({ "limit": MAX_LAYOUT_OUTPUTS }),
        ));
    }
    if request.outputs.is_empty() && request.monitors.is_none() {
        return Err(layout_invalid(
            "The layout has no outputs and no monitor list.",
            Value::Null,
        ));
    }

    let mut seen_outputs = BTreeSet::new();
    for output in &request.outputs {
        if !seen_outputs.insert(output.output.as_str()) {
            return Err(layout_invalid(
                format!("Output {:?} is listed more than once.", output.output),
                serde_json::json!({ "output": output.output }),
            ));
        }
    }

    let mut resolved: BTreeMap<String, PlannedOutput> = status
        .outputs
        .iter()
        .map(|output| (output.name.clone(), planned_from_current(output)))
        .collect();
    let request_by_name: BTreeMap<&str, &LayoutOutputRequest> = request
        .outputs
        .iter()
        .map(|request| (request.output.as_str(), request))
        .collect();

    for output_request in &request.outputs {
        let Some(current) = status
            .outputs
            .iter()
            .find(|output| output.name == output_request.output)
        else {
            return Err(layout_invalid(
                format!(
                    "Output {:?} is not advertised by xrandr on {}.",
                    output_request.output, status.display
                ),
                serde_json::json!({
                    "output": output_request.output,
                    "outputs": status.outputs.iter().map(|output| output.name.clone()).collect::<Vec<_>>(),
                }),
            ));
        };

        let wants_enable = output_request.mode.is_some()
            || output_request.x.is_some()
            || output_request.y.is_some()
            || output_request.rotation.is_some()
            || output_request.scale.is_some()
            || output_request.same_as.is_some()
            || output_request.primary == Some(true);
        let enabled = output_request
            .enabled
            .unwrap_or(wants_enable || current.enabled);

        if !enabled {
            if output_request.mode.is_some()
                || output_request.x.is_some()
                || output_request.y.is_some()
                || output_request.rotation.is_some()
                || output_request.scale.is_some()
                || output_request.same_as.is_some()
                || output_request.primary == Some(true)
            {
                return Err(layout_invalid(
                    format!(
                        "Output {:?} is disabled but the request also sets an active-only field.",
                        output_request.output
                    ),
                    serde_json::json!({ "output": output_request.output }),
                ));
            }
            resolved.insert(
                output_request.output.clone(),
                PlannedOutput {
                    name: output_request.output.clone(),
                    enabled: false,
                    mode: None,
                    x: current.x,
                    y: current.y,
                    rotation: current.rotation.clone(),
                    scale_x: current.scale_x.max(f64::EPSILON),
                    scale_y: current.scale_y.max(f64::EPSILON),
                    transform: current.transform,
                    primary: false,
                },
            );
            continue;
        }

        if !current.connected {
            return Err(layout_invalid(
                format!("Output {:?} is not connected.", output_request.output),
                serde_json::json!({ "output": output_request.output }),
            ));
        }

        let mode = resolve_layout_mode(current, output_request.mode.as_deref())?;
        let rotation = match output_request.rotation.as_deref() {
            Some(rotation) => normalize_rotation(rotation).ok_or_else(|| {
                layout_invalid(
                    format!("Rotation {rotation:?} is not one of normal, left, inverted, right."),
                    serde_json::json!({
                        "output": output_request.output,
                        "rotation": rotation,
                    }),
                )
            })?,
            None => current.rotation.clone(),
        };
        let (scale_x, scale_y, transform) = match output_request.scale.as_deref() {
            Some(scale) => {
                let (scale_x, scale_y) = parse_layout_scale(scale)?;
                (
                    scale_x,
                    scale_y,
                    [
                        scale_x, 0.0, 0.0, //
                        0.0, scale_y, 0.0, //
                        0.0, 0.0, 1.0,
                    ],
                )
            }
            None => (
                current.scale_x.max(f64::EPSILON),
                current.scale_y.max(f64::EPSILON),
                current.transform,
            ),
        };
        let x = output_request.x.unwrap_or(current.x);
        let y = output_request.y.unwrap_or(current.y);
        for (name, value) in [("x", x), ("y", y)] {
            if !(0..=MAX_DISPLAY_COORDINATE).contains(&value) {
                return Err(layout_invalid(
                    format!("Position {name}={value} is outside 0..={MAX_DISPLAY_COORDINATE}."),
                    serde_json::json!({ "output": output_request.output, name: value }),
                ));
            }
        }

        resolved.insert(
            output_request.output.clone(),
            PlannedOutput {
                name: output_request.output.clone(),
                enabled: true,
                mode: Some(mode),
                x,
                y,
                rotation,
                scale_x,
                scale_y,
                transform,
                primary: false,
            },
        );
    }

    // Mirroring is resolved after every requested output exists. A mirror
    // must use the target's mode geometry, rotation, and scale so `--same-as`
    // really produces a clone rather than two overlapping framebuffers.
    for output_request in &request.outputs {
        let Some(target_name) = output_request.same_as.as_deref() else {
            continue;
        };
        let target_name = target_name.trim();
        if target_name == output_request.output {
            return Err(layout_invalid(
                format!("Output {:?} cannot mirror itself.", output_request.output),
                serde_json::json!({ "output": output_request.output }),
            ));
        }
        if output_request.x.is_some() || output_request.y.is_some() {
            return Err(layout_invalid(
                format!(
                    "Output {:?} uses same_as; its position comes from {target_name:?}, so x/y \
                     must be omitted.",
                    output_request.output
                ),
                serde_json::json!({
                    "output": output_request.output,
                    "same_as": target_name,
                }),
            ));
        }
        let planned = resolved
            .get(&output_request.output)
            .expect("requested output was resolved")
            .clone();
        if !planned.enabled {
            return Err(layout_invalid(
                format!(
                    "Output {:?} is disabled but asks to mirror {target_name:?}.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output }),
            ));
        }
        let Some(target) = resolved.get(target_name).cloned() else {
            return Err(layout_invalid(
                format!(
                    "Output {:?} cannot mirror unknown output {target_name:?}.",
                    output_request.output
                ),
                serde_json::json!({
                    "output": output_request.output,
                    "same_as": target_name,
                    "outputs": resolved.keys().cloned().collect::<Vec<_>>(),
                }),
            ));
        };
        if !target.enabled {
            return Err(layout_invalid(
                format!(
                    "Output {:?} cannot mirror disabled output {target_name:?}.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        }
        if request_by_name
            .get(target_name)
            .is_some_and(|request| request.same_as.is_some())
        {
            return Err(layout_invalid(
                format!("Mirror chains are not supported ({target_name:?} also mirrors)."),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        }
        let (Some(planned_mode), Some(target_mode)) = (planned.mode.as_ref(), target.mode.as_ref())
        else {
            return Err(layout_invalid(
                format!(
                    "Output {:?} and mirror target {target_name:?} must both have a mode.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        };
        if planned_mode.width != target_mode.width || planned_mode.height != target_mode.height {
            return Err(layout_invalid(
                format!(
                    "Output {:?} mirrors {target_name:?} but their modes have different sizes.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        }
        if !refresh_pair_matches(planned_mode.refresh_hz, target_mode.refresh_hz) {
            return Err(layout_invalid(
                format!(
                    "Output {:?} mirrors {target_name:?} but their refresh rates differ.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        }
        if planned.rotation != target.rotation {
            return Err(layout_invalid(
                format!(
                    "Output {:?} mirrors {target_name:?} but their rotations differ.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        }
        if !scale_pair_matches(
            (planned.scale_x, planned.scale_y),
            (target.scale_x, target.scale_y),
        ) {
            return Err(layout_invalid(
                format!(
                    "Output {:?} mirrors {target_name:?} but their scales differ.",
                    output_request.output
                ),
                serde_json::json!({ "output": output_request.output, "same_as": target_name }),
            ));
        }
        let (x, y) = (target.x, target.y);
        if let Some(output) = resolved.get_mut(&output_request.output) {
            output.x = x;
            output.y = y;
        }
    }

    let explicit_primary: Vec<&str> = request
        .outputs
        .iter()
        .filter(|request| request.primary == Some(true))
        .map(|request| request.output.as_str())
        .collect();
    if explicit_primary.len() > 1 {
        return Err(layout_invalid(
            "At most one output can be primary.",
            serde_json::json!({ "primary": explicit_primary }),
        ));
    }
    let current_primary = status
        .outputs
        .iter()
        .find(|output| output.primary)
        .map(|output| output.name.clone());
    let mut clear_primary = false;
    if let Some(primary) = explicit_primary.first() {
        let Some(planned) = resolved.get_mut(*primary) else {
            return Err(layout_invalid(
                format!("Primary output {primary:?} is not in the layout."),
                serde_json::json!({ "output": primary }),
            ));
        };
        if !planned.enabled {
            return Err(layout_invalid(
                format!("Primary output {primary:?} is disabled."),
                serde_json::json!({ "output": primary }),
            ));
        }
        for output in resolved.values_mut() {
            output.primary = false;
        }
        resolved
            .get_mut(*primary)
            .expect("primary was resolved")
            .primary = true;
    } else if let Some(primary) = current_primary {
        let explicitly_cleared = request_by_name
            .get(primary.as_str())
            .is_some_and(|request| request.primary == Some(false));
        if explicitly_cleared {
            clear_primary = true;
            for output in resolved.values_mut() {
                output.primary = false;
            }
        } else if resolved.get(&primary).is_some_and(|output| output.enabled) {
            for output in resolved.values_mut() {
                output.primary = output.name == primary;
            }
        } else {
            // The previous primary is being disabled and no replacement was
            // requested. Clear it so the server cannot silently promote an
            // unrelated output behind the layout's back.
            clear_primary = true;
            for output in resolved.values_mut() {
                output.primary = false;
            }
        }
    } else {
        for output in resolved.values_mut() {
            output.primary = false;
        }
    }

    for output in resolved.values().filter(|output| output.enabled) {
        let Some(rect) = planned_rect(output) else {
            continue;
        };
        check_layout_bounds(
            rect,
            status.screen.as_ref(),
            &format!("Output {:?}", output.name),
        )?;
    }

    let planned_names: BTreeSet<&str> = request
        .outputs
        .iter()
        .map(|request| request.output.as_str())
        .collect();
    let mirrors: BTreeMap<&str, &str> = request
        .outputs
        .iter()
        .filter_map(|request| {
            request
                .same_as
                .as_deref()
                .map(|target| (request.output.as_str(), target.trim()))
        })
        .collect();
    let entries: Vec<&PlannedOutput> = resolved.values().filter(|output| output.enabled).collect();
    for (index, left) in entries.iter().enumerate() {
        for right in entries.iter().skip(index + 1) {
            if !planned_names.contains(left.name.as_str())
                && !planned_names.contains(right.name.as_str())
            {
                continue;
            }
            let (Some(left_rect), Some(right_rect)) = (planned_rect(left), planned_rect(right))
            else {
                continue;
            };
            if !rects_overlap(left_rect, right_rect) {
                continue;
            }
            let mirrored = mirrors.get(left.name.as_str()) == Some(&right.name.as_str())
                || mirrors.get(right.name.as_str()) == Some(&left.name.as_str())
                || (mirrors.get(left.name.as_str()).is_some()
                    && mirrors.get(left.name.as_str()) == mirrors.get(right.name.as_str()));
            if !mirrored {
                return Err(layout_invalid(
                    format!(
                        "Outputs {:?} and {:?} overlap; use same_as for a mirror or move one.",
                        left.name, right.name
                    ),
                    serde_json::json!({
                        "outputs": [left.name, right.name],
                        "left": {
                            "x": left_rect.x, "y": left_rect.y,
                            "width": left_rect.width, "height": left_rect.height,
                        },
                        "right": {
                            "x": right_rect.x, "y": right_rect.y,
                            "width": right_rect.width, "height": right_rect.height,
                        },
                    }),
                ));
            }
        }
    }

    let monitors = if let Some(requested_monitors) = request.monitors.as_ref() {
        if requested_monitors.len() > MAX_LAYOUT_MONITORS {
            return Err(layout_too_large(
                format!(
                    "The layout lists {} logical monitors; the limit is {MAX_LAYOUT_MONITORS}.",
                    requested_monitors.len()
                ),
                serde_json::json!({ "limit": MAX_LAYOUT_MONITORS }),
            ));
        }
        let mut names = BTreeSet::new();
        let mut covered_outputs = BTreeSet::new();
        let mut planned_monitors = Vec::new();
        for monitor_request in requested_monitors {
            if !valid_monitor_name(&monitor_request.name) {
                return Err(layout_invalid(
                    format!(
                        "Logical monitor name {:?} is invalid (1..={MAX_MONITOR_NAME_BYTES} \
                         characters from A-Z, a-z, 0-9, _, -, ., :, @, /).",
                        monitor_request.name
                    ),
                    serde_json::json!({ "name": monitor_request.name }),
                ));
            }
            if !names.insert(monitor_request.name.clone()) {
                return Err(layout_invalid(
                    format!(
                        "Logical monitor {:?} is listed more than once.",
                        monitor_request.name
                    ),
                    serde_json::json!({ "name": monitor_request.name }),
                ));
            }
            if monitor_request.width == 0
                || monitor_request.height == 0
                || monitor_request.width > MAX_DISPLAY_COORDINATE as u32
                || monitor_request.height > MAX_DISPLAY_COORDINATE as u32
            {
                return Err(layout_invalid(
                    format!(
                        "Logical monitor {:?} has invalid size {}x{}.",
                        monitor_request.name, monitor_request.width, monitor_request.height
                    ),
                    serde_json::json!({
                        "name": monitor_request.name,
                        "width": monitor_request.width,
                        "height": monitor_request.height,
                    }),
                ));
            }
            let mm_width = monitor_request.mm_width.unwrap_or(0);
            let mm_height = monitor_request.mm_height.unwrap_or(0);
            if mm_width > MAX_DISPLAY_COORDINATE as u32 || mm_height > MAX_DISPLAY_COORDINATE as u32
            {
                return Err(layout_invalid(
                    format!(
                        "Logical monitor {:?} has invalid physical size.",
                        monitor_request.name
                    ),
                    serde_json::json!({
                        "name": monitor_request.name,
                        "mm_width": mm_width,
                        "mm_height": mm_height,
                    }),
                ));
            }
            let x = monitor_request.x.unwrap_or(0);
            let y = monitor_request.y.unwrap_or(0);
            let mut outputs = Vec::new();
            for output_name in &monitor_request.outputs {
                let output_name = output_name.trim();
                if output_name.is_empty() {
                    return Err(layout_invalid(
                        format!(
                            "Logical monitor {:?} has an empty output name.",
                            monitor_request.name
                        ),
                        serde_json::json!({ "name": monitor_request.name }),
                    ));
                }
                let Some(output) = resolved.get(output_name) else {
                    return Err(layout_invalid(
                        format!(
                            "Logical monitor {:?} names unknown output {output_name:?}.",
                            monitor_request.name
                        ),
                        serde_json::json!({
                            "name": monitor_request.name,
                            "output": output_name,
                        }),
                    ));
                };
                if !output.enabled {
                    return Err(layout_invalid(
                        format!(
                            "Logical monitor {:?} covers disabled output {output_name:?}.",
                            monitor_request.name
                        ),
                        serde_json::json!({
                            "name": monitor_request.name,
                            "output": output_name,
                        }),
                    ));
                }
                if !covered_outputs.insert(output_name.to_owned()) {
                    return Err(layout_invalid(
                        format!(
                            "Output {output_name:?} is covered by more than one logical monitor."
                        ),
                        serde_json::json!({
                            "name": monitor_request.name,
                            "output": output_name,
                        }),
                    ));
                }
                outputs.push(output_name.to_owned());
            }
            if outputs.is_empty() {
                return Err(layout_invalid(
                    format!(
                        "Logical monitor {:?} must cover at least one output.",
                        monitor_request.name
                    ),
                    serde_json::json!({ "name": monitor_request.name }),
                ));
            }
            let rect = LayoutRect {
                x,
                y,
                width: monitor_request.width,
                height: monitor_request.height,
            };
            check_layout_bounds(
                rect,
                status.screen.as_ref(),
                &format!("Logical monitor {:?}", monitor_request.name),
            )?;
            planned_monitors.push(PlannedMonitor {
                name: monitor_request.name.clone(),
                width: monitor_request.width,
                height: monitor_request.height,
                mm_width,
                mm_height,
                x,
                y,
                outputs,
            });
        }
        for (index, left) in planned_monitors.iter().enumerate() {
            let left_rect = LayoutRect {
                x: left.x,
                y: left.y,
                width: left.width,
                height: left.height,
            };
            for right in planned_monitors.iter().skip(index + 1) {
                let right_rect = LayoutRect {
                    x: right.x,
                    y: right.y,
                    width: right.width,
                    height: right.height,
                };
                if rects_overlap(left_rect, right_rect) && left_rect != right_rect {
                    return Err(layout_invalid(
                        format!(
                            "Logical monitors {:?} and {:?} overlap.",
                            left.name, right.name
                        ),
                        serde_json::json!({
                            "monitors": [left.name, right.name],
                            "left": {
                                "x": left.x, "y": left.y,
                                "width": left.width, "height": left.height,
                            },
                            "right": {
                                "x": right.x, "y": right.y,
                                "width": right.width, "height": right.height,
                            },
                        }),
                    ));
                }
            }
        }
        Some(planned_monitors)
    } else {
        None
    };

    let requested_monitor_names: BTreeSet<String> = monitors
        .as_ref()
        .map(|monitors| {
            monitors
                .iter()
                .map(|monitor| monitor.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut deleted_monitors = Vec::new();
    if request.monitors.is_some() {
        for name in tracked_monitors {
            if !requested_monitor_names.contains(name)
                && status.monitors.iter().any(|monitor| &monitor.name == name)
            {
                deleted_monitors.push(name.clone());
            }
        }
    }

    let mut argv = vec!["--display".to_owned(), status.display.clone()];
    for output_request in &request.outputs {
        let planned = resolved
            .get(&output_request.output)
            .expect("requested output was resolved");
        argv.push("--output".to_owned());
        argv.push(planned.name.clone());
        if !planned.enabled {
            argv.push("--off".to_owned());
            continue;
        }
        if let Some(mode) = planned.mode.as_ref() {
            argv.push("--mode".to_owned());
            argv.push(mode.name.clone());
            if let Some(refresh) = mode.refresh_hz.filter(|refresh| *refresh > 0.0) {
                argv.push("--rate".to_owned());
                argv.push(format!("{refresh:.2}"));
            }
        }
        if let Some(target) = output_request.same_as.as_deref() {
            argv.push("--same-as".to_owned());
            argv.push(target.trim().to_owned());
        } else {
            argv.push("--pos".to_owned());
            argv.push(format!("{}x{}", planned.x, planned.y));
        }
        argv.push("--rotate".to_owned());
        argv.push(planned.rotation.clone());
        if !transform_is_identity(&planned.transform) {
            if transform_is_scale(&planned.transform) {
                argv.push("--scale".to_owned());
                argv.push(format!(
                    "{}x{}",
                    format_factor(planned.scale_x),
                    format_factor(planned.scale_y)
                ));
            } else {
                argv.push("--transform".to_owned());
                argv.push(format_transform(&planned.transform));
            }
        }
        if planned.primary {
            argv.push("--primary".to_owned());
        }
    }
    if clear_primary {
        argv.push("--noprimary".to_owned());
    }
    for name in &deleted_monitors {
        argv.push("--delmonitor".to_owned());
        argv.push(name.clone());
    }
    if let Some(monitors) = monitors.as_ref() {
        for monitor in monitors {
            argv.push("--setmonitor".to_owned());
            argv.push(monitor.name.clone());
            argv.push(planned_monitor_geometry(monitor));
            argv.push(monitor.outputs.join(","));
        }
    }

    let outputs = request
        .outputs
        .iter()
        .map(|request| {
            resolved
                .get(&request.output)
                .expect("requested output was resolved")
                .clone()
        })
        .collect();
    let created_monitors = monitors
        .as_ref()
        .map(|monitors| {
            monitors
                .iter()
                .map(|monitor| monitor.name.clone())
                .collect()
        })
        .unwrap_or_default();

    Ok(LayoutPlan {
        argv,
        outputs,
        monitors,
        deleted_monitors,
        created_monitors,
    })
}

fn planned_monitor_geometry(monitor: &PlannedMonitor) -> String {
    format!(
        "{}/{}x{}/{}+{}+{}",
        monitor.width, monitor.mm_width, monitor.height, monitor.mm_height, monitor.x, monitor.y
    )
}

fn xrandr_monitor_geometry(monitor: &XrandrMonitor) -> String {
    format!(
        "{}/{}x{}/{}+{}+{}",
        monitor.width, monitor.mm_width, monitor.height, monitor.mm_height, monitor.x, monitor.y
    )
}

fn refresh_pair_matches(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() <= 0.05,
        _ => true,
    }
}

fn scale_pair_matches(left: (f64, f64), right: (f64, f64)) -> bool {
    (left.0 - right.0).abs() <= 0.001 && (left.1 - right.1).abs() <= 0.001
}

fn transform_is_identity(transform: &[f64; 9]) -> bool {
    transform
        .iter()
        .zip(IDENTITY_TRANSFORM.iter())
        .all(|(left, right)| (left - right).abs() <= 0.000_001)
}

fn transform_is_scale(transform: &[f64; 9]) -> bool {
    transform[0] > 0.0
        && transform[4] > 0.0
        && (transform[1].abs() <= 0.000_001)
        && (transform[2].abs() <= 0.000_001)
        && (transform[3].abs() <= 0.000_001)
        && (transform[5].abs() <= 0.000_001)
        && (transform[6].abs() <= 0.000_001)
        && (transform[7].abs() <= 0.000_001)
        && (transform[8] - 1.0).abs() <= 0.000_001
}

fn transform_matches(left: &[f64; 9], right: &[f64; 9]) -> bool {
    left.iter()
        .zip(right.iter())
        .all(|(left, right)| (left - right).abs() <= 0.001)
}

fn format_factor(value: f64) -> String {
    let mut rendered = format!("{value:.6}");
    while rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.pop();
    }
    if rendered.is_empty() {
        "0".to_owned()
    } else {
        rendered
    }
}

fn format_transform(transform: &[f64; 9]) -> String {
    transform
        .iter()
        .map(|value| format!("{value:.6}"))
        .collect::<Vec<_>>()
        .join(",")
}

impl PlannedOutput {
    fn mismatch(&self, actual: &LayoutOutput) -> Option<String> {
        if actual.enabled != self.enabled {
            return Some(format!(
                "output {:?} enabled is {}, expected {}",
                self.name, actual.enabled, self.enabled
            ));
        }
        if !self.enabled {
            return None;
        }
        let Some(mode) = self.mode.as_ref() else {
            return Some(format!("output {:?} has no resolved mode", self.name));
        };
        if actual.width != mode.width || actual.height != mode.height {
            return Some(format!(
                "output {:?} read back {}x{}, expected {}x{}",
                self.name, actual.width, actual.height, mode.width, mode.height
            ));
        }
        if mode
            .refresh_hz
            .is_some_and(|refresh| !refresh_matches(actual.refresh_hz, Some(refresh)))
        {
            return Some(format!(
                "output {:?} read back refresh {:?}, expected {:?}",
                self.name, actual.refresh_hz, mode.refresh_hz
            ));
        }
        if actual.x != self.x || actual.y != self.y {
            return Some(format!(
                "output {:?} read back at {}x{}, expected {}x{}",
                self.name, actual.x, actual.y, self.x, self.y
            ));
        }
        if actual.rotation != self.rotation {
            return Some(format!(
                "output {:?} read back rotation {:?}, expected {:?}",
                self.name, actual.rotation, self.rotation
            ));
        }
        if actual.primary != self.primary {
            return Some(format!(
                "output {:?} read back primary {}, expected {}",
                self.name, actual.primary, self.primary
            ));
        }
        if !transform_matches(&actual.transform, &self.transform) {
            return Some(format!(
                "output {:?} read back transform {:?}, expected {:?}",
                self.name, actual.transform, self.transform
            ));
        }
        None
    }
}

impl PlannedMonitor {
    fn mismatch(&self, actual: &XrandrMonitor) -> Option<String> {
        if actual.width != self.width
            || actual.height != self.height
            || actual.mm_width != self.mm_width
            || actual.mm_height != self.mm_height
            || actual.x != self.x
            || actual.y != self.y
        {
            return Some(format!(
                "monitor {:?} read back {}/{}x{}/{}+{}+{}, expected {}/{}x{}/{}+{}+{}",
                self.name,
                actual.width,
                actual.mm_width,
                actual.height,
                actual.mm_height,
                actual.x,
                actual.y,
                self.width,
                self.mm_width,
                self.height,
                self.mm_height,
                self.x,
                self.y
            ));
        }
        let mut actual_outputs = actual.outputs.clone();
        actual_outputs.sort();
        let mut expected_outputs = self.outputs.clone();
        expected_outputs.sort();
        if actual_outputs != expected_outputs {
            return Some(format!(
                "monitor {:?} covers {:?}, expected {:?}",
                self.name, actual.outputs, self.outputs
            ));
        }
        None
    }
}

fn verify_layout(plan: &LayoutPlan, after: &DisplayLayoutStatus) -> Vec<String> {
    let mut mismatches = Vec::new();
    let outputs: BTreeMap<&str, &LayoutOutput> = after
        .outputs
        .iter()
        .map(|output| (output.name.as_str(), output))
        .collect();
    for expected in &plan.outputs {
        match outputs.get(expected.name.as_str()) {
            Some(actual) => {
                if let Some(reason) = expected.mismatch(actual) {
                    mismatches.push(reason);
                }
            }
            None => mismatches.push(format!(
                "output {:?} disappeared from the read-back",
                expected.name
            )),
        }
    }
    if let Some(monitors) = plan.monitors.as_ref() {
        let actual: BTreeMap<&str, &XrandrMonitor> = after
            .monitors
            .iter()
            .map(|monitor| (monitor.name.as_str(), monitor))
            .collect();
        for expected in monitors {
            match actual.get(expected.name.as_str()) {
                Some(found) => {
                    if let Some(reason) = expected.mismatch(found) {
                        mismatches.push(reason);
                    }
                }
                None => mismatches.push(format!(
                    "logical monitor {:?} is missing from the read-back",
                    expected.name
                )),
            }
        }
        for deleted in &plan.deleted_monitors {
            if actual.contains_key(deleted.as_str()) {
                mismatches.push(format!("logical monitor {deleted:?} was not deleted"));
            }
        }
    }
    mismatches
}

fn layout_output_state_matches(left: &LayoutOutput, right: &LayoutOutput) -> bool {
    left.connected == right.connected
        && left.enabled == right.enabled
        && left.primary == right.primary
        && left.width == right.width
        && left.height == right.height
        && left.x == right.x
        && left.y == right.y
        && left.rotation == right.rotation
        && refresh_pair_matches(left.refresh_hz, right.refresh_hz)
        && transform_matches(&left.transform, &right.transform)
        && left.mm_width == right.mm_width
        && left.mm_height == right.mm_height
}

fn layout_states_match(left: &LayoutState, right: &LayoutState) -> bool {
    if left.outputs.len() != right.outputs.len() {
        return false;
    }
    let right_outputs: BTreeMap<&str, &LayoutOutput> = right
        .outputs
        .iter()
        .map(|output| (output.name.as_str(), output))
        .collect();
    for output in &left.outputs {
        let Some(other) = right_outputs.get(output.name.as_str()) else {
            return false;
        };
        if !layout_output_state_matches(output, other) {
            return false;
        }
    }

    // Automatic monitors are server-derived; only monitors this node or the
    // operator defined are meaningful for rollback equality.
    let left_monitors: Vec<&XrandrMonitor> = left
        .monitors
        .iter()
        .filter(|monitor| !monitor.automatic)
        .collect();
    let right_monitors: Vec<&XrandrMonitor> = right
        .monitors
        .iter()
        .filter(|monitor| !monitor.automatic)
        .collect();
    if left_monitors.len() != right_monitors.len() {
        return false;
    }
    let right_by_name: BTreeMap<&str, &XrandrMonitor> = right_monitors
        .into_iter()
        .map(|monitor| (monitor.name.as_str(), monitor))
        .collect();
    for monitor in left_monitors {
        let Some(other) = right_by_name.get(monitor.name.as_str()) else {
            return false;
        };
        if monitor.width != other.width
            || monitor.height != other.height
            || monitor.mm_width != other.mm_width
            || monitor.mm_height != other.mm_height
            || monitor.x != other.x
            || monitor.y != other.y
        {
            return false;
        }
        let mut left_outputs = monitor.outputs.clone();
        left_outputs.sort();
        let mut right_outputs = other.outputs.clone();
        right_outputs.sort();
        if left_outputs != right_outputs {
            return false;
        }
    }
    true
}

fn layout_rollback_output_argv(display: &str, previous: &LayoutState) -> Vec<String> {
    let mut argv = vec!["--display".to_owned(), display.to_owned()];
    let mut any_primary = false;
    for output in previous.outputs.iter().filter(|output| output.connected) {
        argv.push("--output".to_owned());
        argv.push(output.name.clone());
        if !output.enabled {
            argv.push("--off".to_owned());
            continue;
        }
        if let Some(mode) = output.mode.as_ref() {
            argv.push("--mode".to_owned());
            argv.push(mode.clone());
        } else if output.width > 0 && output.height > 0 {
            argv.push("--mode".to_owned());
            argv.push(format!("{}x{}", output.width, output.height));
        }
        if let Some(refresh) = output.refresh_hz.filter(|refresh| *refresh > 0.0) {
            argv.push("--rate".to_owned());
            argv.push(format!("{refresh:.2}"));
        }
        argv.push("--pos".to_owned());
        argv.push(format!("{}x{}", output.x, output.y));
        argv.push("--rotate".to_owned());
        argv.push(output.rotation.clone());
        if !transform_is_identity(&output.transform) {
            if transform_is_scale(&output.transform) {
                argv.push("--scale".to_owned());
                argv.push(format!(
                    "{}x{}",
                    format_factor(output.scale_x),
                    format_factor(output.scale_y)
                ));
            } else {
                argv.push("--transform".to_owned());
                argv.push(format_transform(&output.transform));
            }
        }
        if output.primary {
            argv.push("--primary".to_owned());
            any_primary = true;
        }
    }
    if !any_primary && argv.len() > 2 {
        argv.push("--noprimary".to_owned());
    }
    argv
}

fn layout_failure_details(
    display: &str,
    plan: &LayoutPlan,
    rollback: &LayoutRollbackReport,
    extra: Value,
) -> Value {
    let mut details = serde_json::json!({
        "display": display,
        "argv": plan.argv.clone(),
        "requested": plan.requested_json(),
        "rolled_back": rollback.rolled_back,
        "rollback_attempted": rollback.attempted,
        "rollback_argv": rollback.output_argv.clone(),
        "rollback_monitor_argv": rollback.monitor_ops.clone(),
        "rollback_error": rollback.error,
    });
    if let Value::Object(extra) = extra {
        if let Value::Object(details) = &mut details {
            for (key, value) in extra {
                details.insert(key, value);
            }
        }
    }
    details
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
        || message.contains("can't open x display")
        || message.contains("cannot open display")
        || message.contains("cannot open x display")
        || message.contains("unable to open display")
        || message.contains("couldn't connect to display")
        || message.contains("failed to connect to wayland")
        || message.contains("could not connect to wayland")
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
    resolve_mode_from_modes(&output.modes, &output.name, requested)
}

fn resolve_mode_from_modes<'a>(
    modes: &'a [XrandrMode],
    output: &str,
    requested: &str,
) -> Result<(&'a XrandrMode, bool), DesktopError> {
    if requested.is_empty() {
        return Err(DesktopError::Invalid {
            code: "display.mode_rejected",
            message: "mode must not be empty.".to_owned(),
            details: serde_json::json!({ "output": output }),
        });
    }
    let labels: Vec<String> = modes.iter().map(XrandrMode::label).collect();

    let exact: Vec<&XrandrMode> = modes
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
                "output": output,
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
                "output": output,
                "requested": requested,
                "modes": labels,
            }),
        })?;
    let candidates: Vec<&XrandrMode> = modes
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
                "Mode {requested:?} ({width}x{height}) is not advertised by output {output:?}."
            ),
            details: serde_json::json!({
                "output": output,
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
                "output": output,
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
        .route(
            "/display/layout",
            get(get_display_layout).post(set_display_layout),
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

async fn get_display_layout(State(state): State<ConnectionState>) -> Response {
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.layout().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => error_response(error),
    }
}

async fn set_display_layout(
    State(state): State<ConnectionState>,
    payload: Result<Json<SetLayoutRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let controls = match controls(&state) {
        Ok(controls) => controls,
        Err(response) => return response,
    };
    match controls.set_layout(request).await {
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
            group_id: None,
        })
    }

    /// Failing command with a status and stderr.
    pub fn failed(status: i32, stderr: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput {
            status: Some(status),
            stdout: Vec::new(),
            stderr: stderr.to_owned(),
            stdout_truncated: false,
            group_id: None,
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

    /// The three scripted queries `GET /display/layout` performs.
    pub fn layout_query_scripts(verbose: &str, query: &str, monitors: &str) -> Vec<Script> {
        vec![
            Script::Run(ok(verbose)),
            Script::Run(ok(query)),
            Script::Run(ok(monitors)),
        ]
    }

    /// A layout request that moves DP-1 to 1280x720, disables HDMI-1, and
    /// defines one custom logical monitor.
    pub fn basic_layout_request() -> SetLayoutRequest {
        SetLayoutRequest {
            outputs: vec![
                LayoutOutputRequest {
                    output: "DP-1".to_owned(),
                    enabled: None,
                    mode: Some("1280x720".to_owned()),
                    x: Some(0),
                    y: Some(0),
                    rotation: Some("normal".to_owned()),
                    primary: Some(true),
                    same_as: None,
                    scale: Some("1x1".to_owned()),
                },
                LayoutOutputRequest {
                    output: "HDMI-1".to_owned(),
                    enabled: Some(false),
                    mode: None,
                    x: None,
                    y: None,
                    rotation: None,
                    primary: None,
                    same_as: None,
                    scale: None,
                },
            ],
            monitors: Some(vec![LayoutMonitorRequest {
                name: "main".to_owned(),
                width: 1280,
                height: 720,
                mm_width: Some(338),
                mm_height: Some(203),
                x: Some(0),
                y: Some(0),
                outputs: vec!["DP-1".to_owned()],
            }]),
        }
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

    /// Two-output layout query used by the layout tests.
    pub const LAYOUT_QUERY: &str = r#"Screen 0: minimum 8 x 8, current 3200 x 1080, maximum 32767 x 32767
DP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00
   1280x720      60.00
HDMI-1 connected 1280x720+1920+0 (normal left inverted right x axis y axis) 338mm x 203mm
   1920x1080     60.00
   1280x720      60.00*+
HDMI-2 disconnected (normal left inverted right x axis y axis)
"#;

    /// Verbose companion to [`LAYOUT_QUERY`], with identity transforms.
    pub const LAYOUT_VERBOSE: &str = r#"Screen 0: minimum 8 x 8, current 3200 x 1080, maximum 32767 x 32767
DP-1 connected primary 1920x1080+0+0 (0x40) normal (normal left inverted right x axis y axis) 527mm x 296mm
	Identifier: 0x41
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
	           filter:
	non-desktop: 0
HDMI-1 connected 1280x720+1920+0 (0x41) normal (normal left inverted right x axis y axis) 338mm x 203mm
	Identifier: 0x42
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
	           filter:
	non-desktop: 0
HDMI-2 disconnected (normal left inverted right x axis y axis) 0mm x 0mm
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
"#;

    /// Automatic monitors matching [`LAYOUT_QUERY`].
    pub const LAYOUT_MONITORS: &str = r#"Monitors: 2
 0: +*DP-1 1920/527x1080/296+0+0  DP-1
 1: +HDMI-1 1280/338x720/203+1920+0  HDMI-1
"#;

    /// The layout after DP-1 moves to 1280x720 and HDMI-1 is disabled.
    pub const LAYOUT_QUERY_CHANGED: &str = r#"Screen 0: minimum 8 x 8, current 1280 x 720, maximum 32767 x 32767
DP-1 connected primary 1280x720+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00+  50.00
   1280x720      60.00*
HDMI-1 connected (normal left inverted right x axis y axis) 338mm x 203mm
   1920x1080     60.00
   1280x720      60.00+
HDMI-2 disconnected (normal left inverted right x axis y axis)
"#;

    /// Verbose companion to [`LAYOUT_QUERY_CHANGED`].
    pub const LAYOUT_VERBOSE_CHANGED: &str = r#"Screen 0: minimum 8 x 8, current 1280 x 720, maximum 32767 x 32767
DP-1 connected primary 1280x720+0+0 (0x40) normal (normal left inverted right x axis y axis) 527mm x 296mm
	Identifier: 0x41
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
HDMI-1 connected (0x41) normal (normal left inverted right x axis y axis) 338mm x 203mm
	Identifier: 0x42
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
HDMI-2 disconnected (normal left inverted right x axis y axis) 0mm x 0mm
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
"#;

    /// The custom logical monitor the layout test defines.
    pub const LAYOUT_MONITORS_CHANGED: &str = r#"Monitors: 1
 0: main 1280/338x720/203+0+0  DP-1
"#;

    /// Verbose details after a `--scale 1.5x1.5` request.
    pub const LAYOUT_VERBOSE_SCALED: &str = r#"Screen 0: minimum 8 x 8, current 3200 x 1080, maximum 32767 x 32767
DP-1 connected primary 1920x1080+0+0 (0x40) normal (normal left inverted right x axis y axis) 527mm x 296mm
	Transform:  1.500000 0.000000 0.000000
	            0.000000 1.500000 0.000000
	            0.000000 0.000000 1.000000
HDMI-1 connected 1280x720+1920+0 (0x41) normal (normal left inverted right x axis y axis) 338mm x 203mm
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
HDMI-2 disconnected (normal left inverted right x axis y axis) 0mm x 0mm
	Transform:  1.000000 0.000000 0.000000
	            0.000000 1.000000 0.000000
	            0.000000 0.000000 1.000000
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
                    group_id: None,
                })),
                Script::Run(Ok(CommandOutput {
                    status: Some(0),
                    stdout: vec![0xFF, 0xFE],
                    stderr: String::new(),
                    stdout_truncated: false,
                    group_id: None,
                })),
            ],
        );
        let read = controls.clipboard().await.unwrap();
        assert_eq!(read.text, "hello");
        assert_eq!(read.tool, "xclip");
        assert_eq!(runner.calls()[0].args, ["-selection", "clipboard", "-o"]);
        assert_eq!(
            runner.calls()[0].env,
            [("DISPLAY".to_owned(), ":0".to_owned())],
            "the resolved display must be passed, not read from ambient DISPLAY"
        );

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
        assert_eq!(
            runner.calls()[0].args,
            ["--no-fehbg", "--bg-fill", png.to_str().unwrap()]
        );
        assert_eq!(
            runner.calls()[0].env,
            [("DISPLAY".to_owned(), ":0".to_owned())]
        );
        let status = controls.wallpaper().await.unwrap();
        assert_eq!(status.display.as_deref(), Some(":0"));
        assert_eq!(status.last_wallpaper, status.last_applied);
        assert_eq!(
            status
                .last_wallpaper
                .as_ref()
                .map(|applied| applied.path.as_str()),
            Some(png.to_str().unwrap())
        );

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
    async fn wallpaper_no_display_is_typed_503_not_502() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("wall.png");
        tokio::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
            .await
            .unwrap();
        let (controls, _) = fake_controls(
            &["feh"],
            vec![Script::Run(failed(
                1,
                "feh ERROR: Can't open X display. It *is* running, yeah?",
            ))],
        );
        let error = controls
            .set_wallpaper(png.to_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.api_error().code, "desktop.no_display");
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
        assert_eq!(
            runner.calls()[0].env,
            [("WAYLAND_DISPLAY".to_owned(), "wayland-0".to_owned())]
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
        assert!(status.layout.available);
        assert!(status.clipboard.available);
        assert!(status.wallpaper.available);
        assert_eq!(status.geometry.tool.as_deref(), Some("xrandr"));
        assert_eq!(status.layout.tool.as_deref(), Some("xrandr"));
        assert_eq!(status.clipboard.tool.as_deref(), Some("xclip"));
        assert_eq!(status.wallpaper.tool.as_deref(), Some("feh"));
        assert_eq!(status.wallpaper_readback, false);
        assert!(runner.calls().is_empty());
    }

    fn layout_output(output: &str) -> LayoutOutputRequest {
        LayoutOutputRequest {
            output: output.to_owned(),
            enabled: None,
            mode: None,
            x: None,
            y: None,
            rotation: None,
            primary: None,
            same_as: None,
            scale: None,
        }
    }

    async fn layout_error(request: SetLayoutRequest) -> DesktopError {
        let (controls, _) = fake_controls(
            &["xrandr"],
            layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS),
        );
        controls.set_layout(request).await.unwrap_err()
    }

    #[test]
    fn parses_layout_transforms_and_logical_monitors() {
        let details = parse_verbose_details(LAYOUT_VERBOSE);
        let dp1 = details.get("DP-1").expect("DP-1 transform");
        assert_eq!(dp1.transform, IDENTITY_TRANSFORM);
        assert_eq!((dp1.mm_width, dp1.mm_height), (527, 296));
        let hdmi1 = details.get("HDMI-1").expect("HDMI-1 transform");
        assert_eq!((hdmi1.mm_width, hdmi1.mm_height), (338, 203));

        let scaled = parse_verbose_details(LAYOUT_VERBOSE_SCALED);
        assert!((scaled["DP-1"].transform[0] - 1.5).abs() < 0.000_001);
        assert!((scaled["DP-1"].transform[4] - 1.5).abs() < 0.000_001);

        let monitors = parse_listmonitors(LAYOUT_MONITORS).unwrap();
        assert_eq!(monitors.len(), 2);
        assert!(monitors[0].automatic && monitors[0].primary);
        assert_eq!(monitors[0].name, "DP-1");
        assert_eq!((monitors[0].width, monitors[0].height), (1920, 1080));
        assert_eq!(monitors[0].outputs, ["DP-1"]);
        assert_eq!(monitors[1].index, 1);
        assert_eq!(monitors[1].x, 1920);
        assert_eq!(monitors[1].outputs, ["HDMI-1"]);

        let custom = parse_listmonitors(LAYOUT_MONITORS_CHANGED).unwrap();
        assert_eq!(custom.len(), 1);
        assert!(!custom[0].automatic && !custom[0].primary);
        assert_eq!(custom[0].name, "main");
        assert_eq!(xrandr_monitor_geometry(&custom[0]), "1280/338x720/203+0+0");
    }

    #[tokio::test]
    async fn layout_get_uses_exact_argv_and_reports_outputs_and_monitors() {
        let (controls, runner) = fake_controls(
            &["xrandr"],
            layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS),
        );
        let status = controls.layout().await.unwrap();
        assert_eq!(status.query_argv, ["--display", ":0", "--query"]);
        assert_eq!(status.verbose_argv, ["--display", ":0", "--verbose"]);
        assert_eq!(status.monitors_argv, ["--display", ":0", "--listmonitors"]);
        assert_eq!(status.tool, "/usr/bin/xrandr");
        assert_eq!(status.outputs.len(), 3);
        let dp1 = &status.outputs[0];
        assert!(dp1.connected && dp1.enabled && dp1.primary);
        assert_eq!(dp1.mode.as_deref(), Some("1920x1080"));
        assert_eq!(dp1.refresh_hz, Some(60.0));
        assert_eq!((dp1.x, dp1.y), (0, 0));
        assert_eq!(dp1.scale_x, 1.0);
        assert_eq!((dp1.mm_width, dp1.mm_height), (527, 296));
        assert_eq!(status.outputs[1].scale_x, 1.0);
        assert!(!status.outputs[2].connected && !status.outputs[2].enabled);
        assert_eq!(status.monitors.len(), 2);
        assert!(status.monitors[0].automatic && status.monitors[0].primary);
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].program, "xrandr");
        assert_eq!(calls[0].args, ["--display", ":0", "--verbose"]);
        assert_eq!(calls[1].args, ["--display", ":0", "--query"]);
        assert_eq!(calls[2].args, ["--display", ":0", "--listmonitors"]);
    }

    #[tokio::test]
    async fn layout_apply_builds_exact_argv_and_verifies_readback() {
        let mut scripts = layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS);
        scripts.push(Script::Run(ok("")));
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE_CHANGED,
            LAYOUT_QUERY_CHANGED,
            LAYOUT_MONITORS_CHANGED,
        ));
        let (controls, runner) = fake_controls(&["xrandr"], scripts);

        let change = controls.set_layout(basic_layout_request()).await.unwrap();
        assert!(change.ok && change.applied && change.changed);
        assert!(!change.rolled_back);
        assert_eq!(
            change.argv,
            [
                "--display",
                ":0",
                "--output",
                "DP-1",
                "--mode",
                "1280x720",
                "--rate",
                "60.00",
                "--pos",
                "0x0",
                "--rotate",
                "normal",
                "--primary",
                "--output",
                "HDMI-1",
                "--off",
                "--setmonitor",
                "main",
                "1280/338x720/203+0+0",
                "DP-1"
            ]
        );
        assert_eq!(change.layout.outputs[0].width, 1280);
        assert!(!change.layout.outputs[1].enabled);
        assert_eq!(change.layout.monitors.len(), 1);
        assert_eq!(change.layout.monitors[0].name, "main");
        let calls = runner.calls();
        assert_eq!(calls.len(), 7);
        assert_eq!(calls[3].program, "xrandr");
        assert_eq!(calls[3].args, change.argv);
    }

    #[tokio::test]
    async fn layout_scale_uses_bounded_argv_and_verifies_transform() {
        let mut scripts = layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS);
        scripts.push(Script::Run(ok("")));
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE_SCALED,
            LAYOUT_QUERY_CHANGED,
            LAYOUT_MONITORS,
        ));
        let (controls, _) = fake_controls(&["xrandr"], scripts);
        let mut request = basic_layout_request();
        request.monitors = None;
        request.outputs[0].scale = Some("1.5x1.5".to_owned());

        let change = controls.set_layout(request).await.unwrap();
        assert!(change.applied && change.changed);
        assert_eq!(
            change.argv,
            [
                "--display",
                ":0",
                "--output",
                "DP-1",
                "--mode",
                "1280x720",
                "--rate",
                "60.00",
                "--pos",
                "0x0",
                "--rotate",
                "normal",
                "--scale",
                "1.5x1.5",
                "--primary",
                "--output",
                "HDMI-1",
                "--off"
            ]
        );
        assert!((change.layout.outputs[0].scale_x - 1.5).abs() < 0.000_001);
    }

    #[tokio::test]
    async fn layout_readback_mismatch_rolls_back_to_previous_layout() {
        let mut scripts = layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS);
        scripts.push(Script::Run(ok("")));
        // xrandr exited zero but the read-back is still the old layout.
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE,
            LAYOUT_QUERY,
            LAYOUT_MONITORS,
        ));
        scripts.push(Script::Run(ok("")));
        scripts.push(Script::Run(ok(LAYOUT_MONITORS)));
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE,
            LAYOUT_QUERY,
            LAYOUT_MONITORS,
        ));
        let (controls, runner) = fake_controls(&["xrandr"], scripts);

        let error = controls
            .set_layout(basic_layout_request())
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.api_error().code, "display.layout_apply_failed");
        assert_eq!(error.api_error().details["rolled_back"], true);
        assert_eq!(error.api_error().details["rollback_attempted"], true);
        assert_eq!(
            error.api_error().details["rollback_argv"],
            serde_json::json!([
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
                "normal",
                "--primary",
                "--output",
                "HDMI-1",
                "--mode",
                "1280x720",
                "--rate",
                "60.00",
                "--pos",
                "1920x0",
                "--rotate",
                "normal"
            ])
        );
        assert_eq!(
            error.api_error().details["rollback_monitor_argv"],
            serde_json::json!([])
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 12);
        assert_eq!(calls[8].args, ["--display", ":0", "--listmonitors"]);
        assert_eq!(calls[11].args, ["--display", ":0", "--listmonitors"]);
    }

    #[tokio::test]
    async fn layout_tool_failure_rolls_back_to_previous_layout() {
        let mut scripts = layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS);
        scripts.push(Script::Run(failed(1, "cannot find mode")));
        scripts.push(Script::Run(ok("")));
        scripts.push(Script::Run(ok(LAYOUT_MONITORS)));
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE,
            LAYOUT_QUERY,
            LAYOUT_MONITORS,
        ));
        let (controls, _) = fake_controls(&["xrandr"], scripts);

        let error = controls
            .set_layout(basic_layout_request())
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.api_error().code, "display.layout_apply_failed");
        assert_eq!(error.api_error().details["rolled_back"], true);
        assert!(error.api_error().details.get("applied").is_none());
    }

    #[tokio::test]
    async fn layout_rejects_unknown_mode_scale_overlap_bounds_and_bad_mirrors() {
        let error = layout_error(SetLayoutRequest {
            outputs: vec![layout_output("DP-9")],
            monitors: None,
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");

        let mut output = layout_output("DP-1");
        output.mode = Some("999x999".to_owned());
        let error = layout_error(SetLayoutRequest {
            outputs: vec![output],
            monitors: None,
        })
        .await;
        assert_eq!(error.api_error().code, "display.mode_rejected");

        let mut output = layout_output("DP-1");
        output.scale = Some("5x5".to_owned());
        let error = layout_error(SetLayoutRequest {
            outputs: vec![output],
            monitors: None,
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");

        let mut left = layout_output("DP-1");
        left.mode = Some("1920x1080@60.00".to_owned());
        left.x = Some(0);
        left.y = Some(0);
        let mut right = layout_output("HDMI-1");
        right.mode = Some("1280x720".to_owned());
        right.x = Some(1000);
        right.y = Some(0);
        let error = layout_error(SetLayoutRequest {
            outputs: vec![left, right],
            monitors: None,
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");
        assert!(
            error.api_error().details["outputs"]
                .as_array()
                .unwrap()
                .len()
                == 2
        );

        let mut output = layout_output("DP-1");
        output.mode = Some("1920x1080@60.00".to_owned());
        output.x = Some(32000);
        let error = layout_error(SetLayoutRequest {
            outputs: vec![output],
            monitors: None,
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");

        let mut mirror = layout_output("HDMI-1");
        mirror.mode = Some("1280x720".to_owned());
        mirror.same_as = Some("DP-1".to_owned());
        let error = layout_error(SetLayoutRequest {
            outputs: vec![mirror],
            monitors: None,
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");
        assert!(error.api_error().message.contains("different sizes"));

        let mut left = layout_output("DP-1");
        left.mode = Some("1920x1080@60.00".to_owned());
        left.x = Some(0);
        let mut right = layout_output("HDMI-1");
        right.mode = Some("1280x720".to_owned());
        right.x = Some(2000);
        let error = layout_error(SetLayoutRequest {
            outputs: vec![left, right],
            monitors: Some(vec![
                LayoutMonitorRequest {
                    name: "left".to_owned(),
                    width: 1000,
                    height: 500,
                    mm_width: None,
                    mm_height: None,
                    x: Some(0),
                    y: Some(0),
                    outputs: vec!["DP-1".to_owned()],
                },
                LayoutMonitorRequest {
                    name: "right".to_owned(),
                    width: 1000,
                    height: 500,
                    mm_width: None,
                    mm_height: None,
                    x: Some(500),
                    y: Some(0),
                    outputs: vec!["HDMI-1".to_owned()],
                },
            ]),
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");
        assert!(error.api_error().message.contains("overlap"));

        let error = layout_error(SetLayoutRequest {
            outputs: Vec::new(),
            monitors: Some(vec![LayoutMonitorRequest {
                name: "main".to_owned(),
                width: 1280,
                height: 720,
                mm_width: None,
                mm_height: None,
                x: Some(0),
                y: Some(0),
                outputs: vec!["DP-9".to_owned()],
            }]),
        })
        .await;
        assert_eq!(error.api_error().code, "display.layout_rejected");
        assert!(error.api_error().message.contains("unknown output"));
    }

    #[tokio::test]
    async fn layout_mirror_and_stale_monitor_planning() {
        let (controls, _) = fake_controls(
            &["xrandr"],
            layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS),
        );
        let status = controls.layout().await.unwrap();
        let mut mirror = layout_output("HDMI-1");
        mirror.mode = Some("1920x1080".to_owned());
        mirror.same_as = Some("DP-1".to_owned());
        let plan = plan_layout(
            &status,
            &SetLayoutRequest {
                outputs: vec![mirror],
                monitors: None,
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.argv,
            [
                "--display",
                ":0",
                "--output",
                "HDMI-1",
                "--mode",
                "1920x1080",
                "--rate",
                "60.00",
                "--same-as",
                "DP-1",
                "--rotate",
                "normal"
            ]
        );

        let (controls, _) = fake_controls(
            &["xrandr"],
            layout_query_scripts(
                LAYOUT_VERBOSE_CHANGED,
                LAYOUT_QUERY_CHANGED,
                LAYOUT_MONITORS_CHANGED,
            ),
        );
        let status = controls.layout().await.unwrap();
        let plan = plan_layout(
            &status,
            &SetLayoutRequest {
                outputs: Vec::new(),
                monitors: Some(Vec::new()),
            },
            &["main".to_owned()],
        )
        .unwrap();
        assert_eq!(plan.argv, ["--display", ":0", "--delmonitor", "main"]);
    }

    #[tokio::test]
    async fn layout_route_returns_status_and_typed_501() {
        let mut scripts = layout_query_scripts(LAYOUT_VERBOSE, LAYOUT_QUERY, LAYOUT_MONITORS);
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE,
            LAYOUT_QUERY,
            LAYOUT_MONITORS,
        ));
        scripts.push(Script::Run(ok("")));
        scripts.extend(layout_query_scripts(
            LAYOUT_VERBOSE_CHANGED,
            LAYOUT_QUERY_CHANGED,
            LAYOUT_MONITORS_CHANGED,
        ));
        let (controls, _) = fake_controls(&["xrandr"], scripts);
        let app = router(state(controls));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/layout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let document = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(document["outputs"].as_array().unwrap().len(), 3);
        assert_eq!(document["monitors"].as_array().unwrap().len(), 2);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/display/layout")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "outputs": [
                                {"output": "DP-1", "mode": "1280x720", "x": 0, "y": 0,
                                 "rotation": "normal", "primary": true},
                                {"output": "HDMI-1", "enabled": false}
                            ],
                            "monitors": [
                                {"name": "main", "width": 1280, "height": 720,
                                 "mm_width": 338, "mm_height": 203, "x": 0, "y": 0,
                                 "outputs": ["DP-1"]}
                            ]
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let document = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(document["applied"], true);
        assert_eq!(document["layout"]["monitors"][0]["name"], "main");

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
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/display/layout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
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
    async fn real_runner_returns_when_the_direct_child_exits_despite_inherited_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("grandchild.pid");
        // The background child inherits stdout/stderr. The old runner waited for
        // pipe EOF and therefore timed out after the direct shell exited.
        let script = format!(
            "sleep 30 & echo $! > '{}'; exit 0",
            pid_file.to_string_lossy()
        );
        let started = std::time::Instant::now();
        let output = SystemCommandRunner
            .run(CommandRequest::new("sh", vec!["-c".to_owned(), script]))
            .await
            .expect("direct child exits successfully");
        assert!(output.success());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "runner waited for the inherited pipe: {:?}",
            started.elapsed()
        );

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("numeric pid");
        // SAFETY: `kill(2)` has no memory-safety preconditions. The pid came
        // from the background process started by this test.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        wait_until_process_gone(pid);
    }

    struct RecordingProcessController {
        terminated: Mutex<Vec<ProcessOwner>>,
    }

    impl RecordingProcessController {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                terminated: Mutex::new(Vec::new()),
            })
        }

        fn terminated(&self) -> Vec<ProcessOwner> {
            self.terminated.lock().unwrap().clone()
        }
    }

    impl ProcessController for RecordingProcessController {
        fn terminate(&self, owner: &ProcessOwner) {
            self.terminated.lock().unwrap().push(owner.clone());
        }
    }

    fn grouped(group_id: u32) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput {
            status: Some(0),
            stdout: Vec::new(),
            stderr: String::new(),
            stdout_truncated: false,
            group_id: Some(group_id),
        })
    }

    #[tokio::test]
    async fn clipboard_write_tracks_one_owner_and_replaces_it_on_the_next_write() {
        let runner = Arc::new(FakeCommandRunner::new(
            &["xclip"],
            vec![Script::Run(grouped(4101)), Script::Run(grouped(4102))],
        ));
        let process = RecordingProcessController::new();
        let controls = DesktopControls::new(runner.clone(), Some(":0".to_owned()), None)
            .with_process_controller(process.clone());

        controls.set_clipboard("one".to_owned()).await.unwrap();
        assert!(process.terminated().is_empty(), "first owner stays live");
        controls.set_clipboard("two".to_owned()).await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(call.env[0], ("DISPLAY".to_owned(), ":0".to_owned()));
            assert!(call
                .env
                .iter()
                .any(|(key, value)| { key == SELECTION_OWNER_ENV && value.starts_with("ozma-") }));
            assert!(call.isolate_process_group);
        }
        let terminated = process.terminated();
        assert_eq!(terminated.len(), 1, "only the previous owner is replaced");
        assert_eq!(terminated[0].group_id, Some(4101));

        controls.shutdown();
        let terminated = process.terminated();
        assert_eq!(terminated.len(), 2);
        assert_eq!(terminated[1].group_id, Some(4102));
        assert!(terminated.iter().all(|owner| owner.group_id.is_some()));
    }

    struct ScriptToolRunner {
        script: PathBuf,
    }

    #[async_trait::async_trait]
    impl CommandRunner for ScriptToolRunner {
        fn lookup(&self, program: &str) -> Option<PathBuf> {
            matches!(program, "xclip" | "xsel").then(|| self.script.clone())
        }

        async fn run(&self, mut request: CommandRequest) -> Result<CommandOutput, CommandError> {
            if matches!(request.program.as_str(), "xclip" | "xsel") {
                // Execute through `sh` so the just-written script cannot hit
                // ETXTBSY on the test filesystem.
                request.program = "sh".to_owned();
                request
                    .args
                    .insert(0, self.script.to_string_lossy().into_owned());
            }
            SystemCommandRunner.run(request).await
        }

        async fn spawn_background(
            &self,
            request: CommandRequest,
            grace: Duration,
        ) -> Result<BackgroundOutcome, CommandError> {
            SystemCommandRunner.spawn_background(request, grace).await
        }
    }

    fn process_alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for the process; it does not deliver a
        // signal.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_until_process_gone(pid: i32) {
        for _ in 0..200 {
            if !process_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process {pid} survived owner termination");
    }

    #[tokio::test]
    async fn real_write_completes_and_keeps_exactly_one_fake_selection_owner() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("xclip");
        let owners = directory.path().join("owners");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat >/dev/null\n( sleep 60 ) &\necho $! >> '{}'\nexit 0\n",
                owners.to_string_lossy()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let controls = DesktopControls::new(
            Arc::new(ScriptToolRunner { script }),
            Some(":0".to_owned()),
            None,
        );
        let started = std::time::Instant::now();
        let write = controls
            .set_clipboard("one".to_owned())
            .await
            .expect("write must not wait for the forked owner");
        assert!(write.written);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "clipboard write blocked on the owner: {:?}",
            started.elapsed()
        );

        controls.set_clipboard("two".to_owned()).await.unwrap();
        let pids: Vec<i32> = std::fs::read_to_string(&owners)
            .unwrap()
            .lines()
            .map(|line| line.trim().parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2, "one owner per write: {pids:?}");
        wait_until_process_gone(pids[0]);
        assert!(
            process_alive(pids[1]),
            "second owner must stay alive until replaced"
        );

        controls.shutdown();
        wait_until_process_gone(pids[1]);
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

    #[tokio::test]
    #[ignore = "starts a throwaway Xvfb on a private display; set OZMA_DESKTOP_LIVE=1"]
    async fn live_xvfb_layout_query_and_idempotent_set() {
        if std::env::var("OZMA_DESKTOP_LIVE").as_deref() != Ok("1") {
            eprintln!("OZMA_DESKTOP_LIVE is not 1; skipping live Xvfb layout test");
            return;
        }
        if SystemCommandRunner.lookup("Xvfb").is_none()
            || SystemCommandRunner.lookup("xrandr").is_none()
        {
            eprintln!("Xvfb/xrandr not installed; skipping live layout test");
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
            match controls.layout().await {
                Ok(value) => {
                    status = Some(value);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let status = status.expect("Xvfb xrandr layout query");
        assert!(
            !status.monitors.is_empty(),
            "Xvfb reports a logical monitor"
        );
        let output = status
            .outputs
            .iter()
            .find(|output| output.enabled)
            .expect("Xvfb enabled output");
        let mode = output.mode.clone().expect("Xvfb current mode");

        let change = controls
            .set_layout(SetLayoutRequest {
                outputs: vec![LayoutOutputRequest {
                    output: output.name.clone(),
                    enabled: Some(true),
                    mode: Some(mode),
                    x: Some(output.x),
                    y: Some(output.y),
                    rotation: Some(output.rotation.clone()),
                    primary: Some(output.primary),
                    same_as: None,
                    scale: Some("1x1".to_owned()),
                }],
                monitors: None,
            })
            .await
            .expect("idempotent layout set");
        assert!(change.applied);
        assert_eq!(change.layout.outputs[0].width, output.width);
        assert_eq!(change.layout.outputs[0].height, output.height);

        let error = controls
            .set_layout(SetLayoutRequest {
                outputs: vec![LayoutOutputRequest {
                    output: output.name.clone(),
                    enabled: Some(true),
                    mode: Some("4096x2160".to_owned()),
                    x: Some(0),
                    y: Some(0),
                    rotation: Some("normal".to_owned()),
                    primary: None,
                    same_as: None,
                    scale: None,
                }],
                monitors: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.api_error().code, "display.mode_rejected");

        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    #[ignore = "uses an external throwaway X display from OZMA_LAYOUT_LIVE_DISPLAY; set OZMA_DESKTOP_LIVE=1"]
    async fn live_external_display_custom_monitor_round_trip() {
        if std::env::var("OZMA_DESKTOP_LIVE").as_deref() != Ok("1") {
            eprintln!("OZMA_DESKTOP_LIVE is not 1; skipping external layout test");
            return;
        }
        let Ok(display) = std::env::var("OZMA_LAYOUT_LIVE_DISPLAY") else {
            eprintln!("OZMA_LAYOUT_LIVE_DISPLAY is not set; skipping external layout test");
            return;
        };
        let controls = DesktopControls::system(Some(display), None);
        let status = controls
            .layout()
            .await
            .expect("external display layout query");
        let output = status
            .outputs
            .iter()
            .find(|output| output.enabled)
            .expect("external display enabled output");

        let monitor = LayoutMonitorRequest {
            name: "ozma-test".to_owned(),
            width: output.width,
            height: output.height,
            mm_width: Some(output.mm_width),
            mm_height: Some(output.mm_height),
            x: Some(output.x),
            y: Some(output.y),
            outputs: vec![output.name.clone()],
        };
        let change = controls
            .set_layout(SetLayoutRequest {
                outputs: Vec::new(),
                monitors: Some(vec![monitor]),
            })
            .await
            .expect("set custom logical monitor");
        assert!(change.applied);
        assert!(change
            .layout
            .monitors
            .iter()
            .any(|monitor| monitor.name == "ozma-test"));

        let change = controls
            .set_layout(SetLayoutRequest {
                outputs: Vec::new(),
                monitors: Some(Vec::new()),
            })
            .await
            .expect("remove custom logical monitor");
        assert!(change.applied);
        assert!(!change
            .layout
            .monitors
            .iter()
            .any(|monitor| monitor.name == "ozma-test"));

        let _ = controls.shutdown();
    }
}
