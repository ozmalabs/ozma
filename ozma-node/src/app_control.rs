// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Opt-in, bounded application control on top of the read-only `/apps` monitor.
//!
//! The default node never signals another process. With `--app-control` the
//! guarded compatibility router exports:
//!
//! * `POST /apps/{pid}/terminate` — `SIGTERM`, then a bounded `SIGKILL` after
//!   the configured delay when the process is still present;
//! * `POST /apps/{pid}/signal` — `TERM`, `KILL`, `STOP`, or `CONT` only;
//! * `POST /apps/{pid}/suspend` / `resume` — convenience aliases for
//!   `SIGSTOP`/`SIGCONT`.
//!
//! Controls are addressed by exact pid; every signal is guarded by a fresh
//! kernel start-time read. Names are never matched, `/proc/<pid>/cmdline` is
//! never read, and every request is checked against the same policy immediately
//! before each signal:
//!
//! * pid 0 and pid 1 are refused;
//! * this node process and its process tree (ancestors and descendants) are
//!   refused;
//! * kernel threads and already-exited/zombie processes are refused;
//! * by default only processes owned by the node's effective user are allowed;
//! * an explicit `SIGKILL` request must carry `"confirm": true`;
//! * a caller-supplied `start_time` must match `/proc/<pid>/stat`, and the
//!   value is re-read immediately before every signal so a reused pid is never
//!   signalled.
//!
//! `GET /apps` itself is unchanged: it remains a bounded read-only list with no
//! `cmdline`, no start time, and no control side effects.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Default wait between `SIGTERM` and the bounded `SIGKILL` escalation.
pub const DEFAULT_KILL_AFTER: Duration = Duration::from_secs(2);
/// Lower bound accepted for `--app-control-kill-after-seconds`.
pub const MIN_KILL_AFTER: Duration = Duration::from_secs(1);
/// Upper bound accepted for `--app-control-kill-after-seconds`.
pub const MAX_KILL_AFTER: Duration = Duration::from_secs(30);
/// Largest pid accepted by the Linux procfs/signal backend.
const MAX_PID: u32 = i32::MAX as u32;
/// Longest ancestor/descendant walk used to classify the node process tree.
const MAX_ANCESTRY_DEPTH: usize = 128;
/// Linux `PF_KTHREAD` flag in `/proc/<pid>/stat`.
const PF_KTHREAD: u64 = 0x0000_0000_0020_0000;
/// Maximum bytes read from one procfs file.
const MAX_PROC_FILE_BYTES: u64 = 16 * 1024;

/// Effective application-control policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppControlConfig {
    /// Wait after `SIGTERM` before a still-present process receives `SIGKILL`.
    pub kill_after: Duration,
    /// Allow control of processes owned by another user. Off by default; the
    /// CLI exposes this only as an explicit opt-in.
    pub allow_other_users: bool,
}

impl AppControlConfig {
    /// Build a configuration with the production bounds applied to
    /// [`Self::kill_after`] (1–30 seconds).
    pub fn new(kill_after: Duration, allow_other_users: bool) -> Self {
        Self {
            kill_after: kill_after.clamp(MIN_KILL_AFTER, MAX_KILL_AFTER),
            allow_other_users,
        }
    }
}

impl Default for AppControlConfig {
    fn default() -> Self {
        Self::new(DEFAULT_KILL_AFTER, false)
    }
}

/// The four signals this surface may deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlSignal {
    /// Graceful termination request.
    Term,
    /// Forced termination.
    Kill,
    /// Stop the process (`SIGSTOP`).
    Stop,
    /// Continue a stopped process (`SIGCONT`).
    Cont,
}

impl ControlSignal {
    /// Parse an allow-listed signal name. `SIGTERM` and `TERM` are equivalent;
    /// any other name (including numeric values) is rejected.
    pub fn parse(value: &str) -> Option<Self> {
        let upper = value.trim().to_ascii_uppercase();
        let name = upper.strip_prefix("SIG").unwrap_or(&upper);
        match name {
            "TERM" => Some(Self::Term),
            "KILL" => Some(Self::Kill),
            "STOP" => Some(Self::Stop),
            "CONT" => Some(Self::Cont),
            _ => None,
        }
    }

    /// Stable uppercase name used in reports and errors.
    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "TERM",
            Self::Kill => "KILL",
            Self::Stop => "STOP",
            Self::Cont => "CONT",
        }
    }

    fn number(self) -> libc::c_int {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
            Self::Stop => libc::SIGSTOP,
            Self::Cont => libc::SIGCONT,
        }
    }
}

/// The subset of `/proc/<pid>/stat` and `/proc/<pid>/status` used by policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pid: u32,
    pub ppid: u32,
    /// Real user id from `/proc/<pid>/status`.
    pub uid: u32,
    /// Kernel start time in clock ticks since boot.
    pub start_time: u64,
    /// One-letter process state (`R`, `S`, `D`, `Z`, `T`, ...).
    pub state: char,
    /// `PF_KTHREAD` was set in the kernel stat flags.
    pub kernel_thread: bool,
}

/// Backend failures mapped to typed API errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The pid does not exist (or exited between reads).
    NotFound,
    /// The process exists but the backend may not inspect or signal it.
    Permission,
    /// Another read/signal failure, including malformed procfs data.
    Io(String),
}

/// Read and signal operations required by [`AppControl`].
///
/// Production uses [`ProcBackend`]. Tests substitute an in-memory backend so
/// the policy matrix and escalation timing do not signal real processes.
pub trait ProcessControlBackend: Send + Sync {
    /// Effective uid of the node process.
    fn node_uid(&self) -> u32;
    /// Read identity/policy facts for one pid.
    fn read(&self, pid: u32) -> Result<ProcessFacts, BackendError>;
    /// Deliver exactly one allow-listed signal to one pid.
    fn send(&self, pid: u32, signal: ControlSignal) -> Result<(), BackendError>;
}

/// Linux procfs/signal backend.
#[derive(Clone, Debug)]
pub struct ProcBackend {
    proc_root: PathBuf,
    uid: u32,
}

impl ProcBackend {
    /// Read `/proc` and compare against this process's effective uid.
    pub fn host() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            // SAFETY: geteuid is thread-safe and takes no pointers.
            uid: unsafe { libc::geteuid() },
        }
    }

    /// Build a backend rooted at a fake procfs tree. Tests use this only for
    /// read-only fixtures; the fake-signal tests use [`ProcessControlBackend`].
    #[cfg(test)]
    fn with_root(proc_root: &Path, uid: u32) -> Self {
        Self {
            proc_root: proc_root.to_path_buf(),
            uid,
        }
    }

    fn read(&self, pid: u32) -> Result<ProcessFacts, BackendError> {
        if pid == 0 || pid > MAX_PID {
            return Err(BackendError::NotFound);
        }
        let dir = self.proc_root.join(pid.to_string());
        let stat_text = read_bounded(&dir.join("stat")).map_err(backend_io)?;
        let parsed = parse_stat(&stat_text)
            .ok_or_else(|| BackendError::Io("malformed /proc/<pid>/stat".to_owned()))?;
        let status_text = read_bounded(&dir.join("status")).map_err(backend_io)?;
        let uid = parse_status_uid(&status_text)
            .ok_or_else(|| BackendError::Io("no Uid line in /proc/<pid>/status".to_owned()))?;
        Ok(ProcessFacts {
            pid,
            ppid: parsed.ppid,
            uid,
            start_time: parsed.start_time,
            state: parsed.state,
            kernel_thread: parsed.flags & PF_KTHREAD != 0,
        })
    }

    fn send(&self, pid: u32, signal: ControlSignal) -> Result<(), BackendError> {
        if pid == 0 || pid > MAX_PID {
            return Err(BackendError::NotFound);
        }
        // SAFETY: kill takes plain integer arguments and does not retain any
        // pointer. The exact pid and start time were checked by the caller.
        let result = unsafe { libc::kill(pid as libc::pid_t, signal.number()) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        Err(match error.raw_os_error() {
            Some(libc::ESRCH) => BackendError::NotFound,
            Some(libc::EPERM) => BackendError::Permission,
            _ => BackendError::Io(error.to_string()),
        })
    }
}

impl ProcessControlBackend for ProcBackend {
    fn node_uid(&self) -> u32 {
        self.uid
    }

    fn read(&self, pid: u32) -> Result<ProcessFacts, BackendError> {
        ProcBackend::read(self, pid)
    }

    fn send(&self, pid: u32, signal: ControlSignal) -> Result<(), BackendError> {
        ProcBackend::send(self, pid, signal)
    }
}

/// One control operation's body.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ControlRequest {
    /// Expected `/proc/<pid>/stat` start time. When present it must match;
    /// otherwise only the immediate pre-signal re-check protects the caller.
    #[serde(default)]
    pub start_time: Option<u64>,
    /// Required only for an explicit `SIGKILL` via `POST /apps/{pid}/signal`.
    #[serde(default)]
    pub confirm: bool,
}

/// `POST /apps/{pid}/signal` body.
#[derive(Clone, Debug, Deserialize)]
pub struct SignalRequest {
    /// `TERM`, `KILL`, `STOP`, or `CONT` (an optional `SIG` prefix is allowed).
    pub signal: String,
    #[serde(default)]
    pub start_time: Option<u64>,
    #[serde(default)]
    pub confirm: bool,
}

/// Successful control result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ControlReport {
    pub ok: bool,
    pub pid: u32,
    /// Kernel start time observed for this pid before the first signal.
    pub start_time: u64,
    /// `terminate`, `signal`, `suspend`, or `resume`.
    pub action: &'static str,
    /// Primary signal name (`TERM`, `KILL`, `STOP`, or `CONT`).
    pub requested: &'static str,
    /// Every signal delivered, in order.
    pub signals: Vec<&'static str>,
    /// A terminate request delivered the bounded `SIGKILL` escalation.
    pub escalated: bool,
    /// For `terminate`: `true` when the pid was absent or a zombie after the
    /// bounded wait, `false` when it was still running, and `None` when the
    /// final state could not be observed. Absent for other actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminated: Option<bool>,
}

/// Typed control failure, serialized like the read-only host-surface errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlError {
    pub code: &'static str,
    pub message: String,
    pub status: StatusCode,
}

impl ControlError {
    fn disabled() -> Self {
        Self {
            code: "apps.control-disabled",
            message: "application control is disabled; start the node with --app-control"
                .to_owned(),
            status: StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn not_found(pid: u32) -> Self {
        Self {
            code: "apps.not-found",
            message: format!("pid {pid} does not exist"),
            status: StatusCode::NOT_FOUND,
        }
    }

    fn permission(message: impl Into<String>) -> Self {
        Self {
            code: "apps.permission",
            message: message.into(),
            status: StatusCode::FORBIDDEN,
        }
    }

    fn policy(message: impl Into<String>) -> Self {
        Self {
            code: "apps.policy",
            message: message.into(),
            status: StatusCode::FORBIDDEN,
        }
    }

    fn own_process(pid: u32) -> Self {
        Self {
            code: "apps.own-process",
            message: format!(
                "pid {pid} belongs to the ozma-node process tree and is never signalled"
            ),
            status: StatusCode::FORBIDDEN,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "apps.invalid-request",
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
        }
    }

    fn proc_unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "apps.proc-unavailable",
            message: message.into(),
            status: StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// Shared, cloneable application-control service.
#[derive(Clone)]
pub struct AppControl {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Arc<dyn ProcessControlBackend>,
    config: AppControlConfig,
    self_pid: u32,
}

impl AppControl {
    /// Build the production service over `/proc`.
    pub fn new(config: AppControlConfig) -> Self {
        Self::with_backend(config, Arc::new(ProcBackend::host()))
    }

    fn with_backend(config: AppControlConfig, backend: Arc<dyn ProcessControlBackend>) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                config,
                self_pid: std::process::id(),
            }),
        }
    }

    /// Effective policy, for startup logging and tests.
    pub fn config(&self) -> AppControlConfig {
        self.inner.config
    }

    /// `SIGTERM`, wait the configured bounded delay, then `SIGKILL` only if the
    /// same pid/start-time combination is still present and not a zombie.
    pub async fn terminate(
        &self,
        pid: u32,
        request: ControlRequest,
    ) -> Result<ControlReport, ControlError> {
        let target = self.validate_target(pid, request.start_time)?;
        let target = self.send_checked(&target, ControlSignal::Term)?;
        let mut signals = vec![ControlSignal::Term];
        let mut escalated = false;

        tokio::time::sleep(self.inner.config.kill_after).await;

        let terminated = match self.inner.backend.read(pid) {
            Ok(after) => {
                if after.start_time != target.start_time {
                    return Err(ControlError::policy(format!(
                        "pid {pid} was reused while waiting after SIGTERM; SIGKILL was not sent"
                    )));
                }
                if exited_state(after.state) {
                    Some(true)
                } else {
                    // The terminate action authorizes this one bounded
                    // escalation. send_checked re-applies every policy rule
                    // immediately before SIGKILL.
                    match self.send_checked(&after, ControlSignal::Kill) {
                        Ok(_) => {
                            signals.push(ControlSignal::Kill);
                            escalated = true;
                            match self.inner.backend.read(pid) {
                                Ok(final_facts) => Some(
                                    final_facts.start_time != target.start_time
                                        || exited_state(final_facts.state),
                                ),
                                Err(BackendError::NotFound) => Some(true),
                                Err(BackendError::Permission) | Err(BackendError::Io(_)) => None,
                            }
                        }
                        // The process exited between the wait and the
                        // escalation. SIGTERM was enough; no SIGKILL was sent.
                        Err(error) if error.code == "apps.not-found" => Some(true),
                        Err(error) => return Err(error),
                    }
                }
            }
            Err(BackendError::NotFound) => Some(true),
            Err(BackendError::Permission) => {
                return Err(ControlError::permission(format!(
                    "pid {pid} could not be inspected after SIGTERM; SIGKILL was not sent"
                )))
            }
            Err(BackendError::Io(message)) => {
                return Err(ControlError::proc_unavailable(format!(
                    "pid {pid} could not be inspected after SIGTERM: {message}"
                )))
            }
        };

        Ok(ControlReport {
            ok: true,
            pid: target.pid,
            start_time: target.start_time,
            action: "terminate",
            requested: ControlSignal::Term.name(),
            signals: signals.iter().map(|signal| signal.name()).collect(),
            escalated,
            terminated,
        })
    }

    /// Deliver one explicit allow-listed signal. `SIGKILL` needs confirmation.
    pub async fn signal(
        &self,
        pid: u32,
        request: SignalRequest,
    ) -> Result<ControlReport, ControlError> {
        let signal = ControlSignal::parse(&request.signal).ok_or_else(|| {
            ControlError::invalid("signal must be one of TERM, KILL, STOP, or CONT")
        })?;
        if signal == ControlSignal::Kill && !request.confirm {
            return Err(ControlError::policy(
                "SIGKILL requires explicit confirmation (\"confirm\": true)",
            ));
        }
        let target = self.validate_target(pid, request.start_time)?;
        let target = self.send_checked(&target, signal)?;
        Ok(self.report("signal", signal, target))
    }

    /// Convenience request for `SIGSTOP`.
    pub async fn suspend(
        &self,
        pid: u32,
        request: ControlRequest,
    ) -> Result<ControlReport, ControlError> {
        let target = self.validate_target(pid, request.start_time)?;
        let target = self.send_checked(&target, ControlSignal::Stop)?;
        Ok(self.report("suspend", ControlSignal::Stop, target))
    }

    /// Convenience request for `SIGCONT`.
    pub async fn resume(
        &self,
        pid: u32,
        request: ControlRequest,
    ) -> Result<ControlReport, ControlError> {
        let target = self.validate_target(pid, request.start_time)?;
        let target = self.send_checked(&target, ControlSignal::Cont)?;
        Ok(self.report("resume", ControlSignal::Cont, target))
    }

    fn report(
        &self,
        action: &'static str,
        signal: ControlSignal,
        target: ProcessFacts,
    ) -> ControlReport {
        ControlReport {
            ok: true,
            pid: target.pid,
            start_time: target.start_time,
            action,
            requested: signal.name(),
            signals: vec![signal.name()],
            escalated: false,
            terminated: None,
        }
    }

    fn validate_target(
        &self,
        pid: u32,
        start_time: Option<u64>,
    ) -> Result<ProcessFacts, ControlError> {
        if pid == 0 {
            return Err(ControlError::policy(
                "pid 0 names the caller's process group and is never signalled",
            ));
        }
        if pid == 1 {
            return Err(ControlError::policy(
                "pid 1 is the init process and is never signalled",
            ));
        }
        if pid > MAX_PID {
            return Err(ControlError::not_found(pid));
        }
        let facts = self.read(pid)?;
        if let Some(expected) = start_time {
            if facts.start_time != expected {
                return Err(ControlError::policy(format!(
                    "pid {pid} start time {} does not match the requested start time {expected}; \
                     the pid was reused",
                    facts.start_time
                )));
            }
        }
        self.check_facts(&facts)?;
        Ok(facts)
    }

    fn check_facts(&self, facts: &ProcessFacts) -> Result<(), ControlError> {
        if exited_state(facts.state) {
            return Err(ControlError::not_found(facts.pid));
        }
        if facts.kernel_thread {
            return Err(ControlError::policy(format!(
                "pid {} is a kernel thread and is never signalled",
                facts.pid
            )));
        }
        if self.is_own_tree(facts)? {
            return Err(ControlError::own_process(facts.pid));
        }
        if facts.uid != self.inner.backend.node_uid() && !self.inner.config.allow_other_users {
            return Err(ControlError::permission(format!(
                "pid {} is owned by uid {}; this node runs as uid {} and only controls its own \
                 user's processes",
                facts.pid,
                facts.uid,
                self.inner.backend.node_uid()
            )));
        }
        Ok(())
    }

    /// Re-read identity and policy, then deliver one signal. This is the only
    /// place a signal is sent, so every send is immediately preceded by a
    /// start-time and policy check.
    fn send_checked(
        &self,
        expected: &ProcessFacts,
        signal: ControlSignal,
    ) -> Result<ProcessFacts, ControlError> {
        let current = self.read(expected.pid)?;
        if current.start_time != expected.start_time {
            return Err(ControlError::policy(format!(
                "pid {} was reused before {} could be delivered; no signal was sent",
                expected.pid,
                signal.name()
            )));
        }
        self.check_facts(&current)?;
        self.inner
            .backend
            .send(current.pid, signal)
            .map_err(|error| self.backend_error(current.pid, error))?;
        Ok(current)
    }

    fn read(&self, pid: u32) -> Result<ProcessFacts, ControlError> {
        self.inner
            .backend
            .read(pid)
            .map_err(|error| self.backend_error(pid, error))
    }

    fn backend_error(&self, pid: u32, error: BackendError) -> ControlError {
        match error {
            BackendError::NotFound => ControlError::not_found(pid),
            BackendError::Permission => ControlError::permission(format!(
                "pid {pid} could not be inspected or signalled with this node's permissions"
            )),
            BackendError::Io(message) => ControlError::proc_unavailable(format!(
                "pid {pid} could not be inspected: {message}"
            )),
        }
    }

    /// True when `facts.pid` is this node, an ancestor, or a descendant.
    fn is_own_tree(&self, facts: &ProcessFacts) -> Result<bool, ControlError> {
        if facts.pid == self.inner.self_pid {
            return Ok(true);
        }
        // Is the target an ancestor of this node?
        let mut current = self.inner.self_pid;
        for _ in 0..MAX_ANCESTRY_DEPTH {
            let Some(parent) = self.parent_of(current)? else {
                break;
            };
            if parent == facts.pid {
                return Ok(true);
            }
            if parent <= 1 {
                break;
            }
            current = parent;
        }
        // Is the target a descendant of this node?
        let mut current = facts.pid;
        for _ in 0..MAX_ANCESTRY_DEPTH {
            if current <= 1 {
                break;
            }
            let Some(parent) = self.parent_of(current)? else {
                break;
            };
            if parent == self.inner.self_pid {
                return Ok(true);
            }
            if parent <= 1 {
                break;
            }
            current = parent;
        }
        Ok(false)
    }

    fn parent_of(&self, pid: u32) -> Result<Option<u32>, ControlError> {
        match self.inner.backend.read(pid) {
            Ok(facts) => Ok(Some(facts.ppid)),
            Err(BackendError::NotFound) => Ok(None),
            Err(BackendError::Permission) => Err(ControlError::permission(
                "process ancestry could not be inspected; refusing to signal",
            )),
            Err(BackendError::Io(message)) => Err(ControlError::proc_unavailable(format!(
                "process ancestry could not be inspected: {message}"
            ))),
        }
    }
}

fn exited_state(state: char) -> bool {
    matches!(state, 'Z' | 'X' | 'x')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedStat {
    ppid: u32,
    flags: u64,
    start_time: u64,
    state: char,
}

/// Parse `/proc/<pid>/stat` after the final `)`, so comm names containing
/// spaces or parentheses do not shift positions.
fn parse_stat(text: &str) -> Option<ParsedStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let fields: Vec<&str> = text[close + 1..].split_whitespace().collect();
    if fields.len() < 22 {
        return None;
    }
    Some(ParsedStat {
        state: fields[0].chars().next()?,
        ppid: fields[1].parse::<u32>().ok()?,
        flags: fields[6].parse::<u64>().ok()?,
        start_time: fields[19].parse::<u64>().ok()?,
    })
}

fn parse_status_uid(text: &str) -> Option<u32> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        return rest.split_whitespace().next()?.parse::<u32>().ok();
    }
    None
}

fn read_bounded(path: &Path) -> Result<String, io::Error> {
    use std::io::Read;

    let file = fs::File::open(path)?;
    let mut reader = file.take(MAX_PROC_FILE_BYTES);
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    String::from_utf8(buffer).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn backend_io(error: io::Error) -> BackendError {
    match error.kind() {
        io::ErrorKind::NotFound => BackendError::NotFound,
        io::ErrorKind::PermissionDenied => BackendError::Permission,
        _ => BackendError::Io(error.to_string()),
    }
}

#[cfg(test)]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Build the guarded app-control router.
///
/// The routes are always mounted by [`crate::controller_compat::compat_router`]
/// so a disabled node answers a typed `503 apps.control-disabled` instead of
/// looking like a missing endpoint.
pub fn router(control: Option<AppControl>) -> Router {
    Router::new()
        .route("/apps/:pid/terminate", post(terminate_handler))
        .route("/api/v1/apps/:pid/terminate", post(terminate_handler))
        .route("/apps/:pid/signal", post(signal_handler))
        .route("/api/v1/apps/:pid/signal", post(signal_handler))
        .route("/apps/:pid/suspend", post(suspend_handler))
        .route("/api/v1/apps/:pid/suspend", post(suspend_handler))
        .route("/apps/:pid/resume", post(resume_handler))
        .route("/api/v1/apps/:pid/resume", post(resume_handler))
        .with_state(control)
}

async fn terminate_handler(
    State(control): State<Option<AppControl>>,
    AxumPath(pid): AxumPath<u32>,
    body: Option<Json<ControlRequest>>,
) -> Response {
    let Some(control) = control else {
        return error_response(ControlError::disabled());
    };
    let request = body.map(|Json(body)| body).unwrap_or_default();
    match control.terminate(pid, request).await {
        Ok(report) => json_response(report),
        Err(error) => error_response(error),
    }
}

async fn signal_handler(
    State(control): State<Option<AppControl>>,
    AxumPath(pid): AxumPath<u32>,
    body: Option<Json<SignalRequest>>,
) -> Response {
    let Some(control) = control else {
        return error_response(ControlError::disabled());
    };
    let Some(Json(request)) = body else {
        return error_response(ControlError::invalid(
            "a JSON body with {\"signal\": \"TERM|KILL|STOP|CONT\"} is required",
        ));
    };
    match control.signal(pid, request).await {
        Ok(report) => json_response(report),
        Err(error) => error_response(error),
    }
}

async fn suspend_handler(
    State(control): State<Option<AppControl>>,
    AxumPath(pid): AxumPath<u32>,
    body: Option<Json<ControlRequest>>,
) -> Response {
    let Some(control) = control else {
        return error_response(ControlError::disabled());
    };
    let request = body.map(|Json(body)| body).unwrap_or_default();
    match control.suspend(pid, request).await {
        Ok(report) => json_response(report),
        Err(error) => error_response(error),
    }
}

async fn resume_handler(
    State(control): State<Option<AppControl>>,
    AxumPath(pid): AxumPath<u32>,
    body: Option<Json<ControlRequest>>,
) -> Response {
    let Some(control) = control else {
        return error_response(ControlError::disabled());
    };
    let request = body.map(|Json(body)| body).unwrap_or_default();
    match control.resume(pid, request).await {
        Ok(report) => json_response(report),
        Err(error) => error_response(error),
    }
}

fn json_response<T: Serialize>(value: T) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(value),
    )
        .into_response()
}

fn error_response(error: ControlError) -> Response {
    (
        error.status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({
            "ok": false,
            "available": false,
            "code": error.code,
            "error": error.message,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[derive(Default)]
    struct FakeBackend {
        uid: u32,
        facts: Mutex<BTreeMap<u32, ProcessFacts>>,
        signals: Mutex<Vec<(u32, ControlSignal)>>,
        exit_on_term: bool,
    }

    impl FakeBackend {
        fn new(uid: u32) -> Self {
            Self {
                uid,
                facts: Mutex::new(BTreeMap::new()),
                signals: Mutex::new(Vec::new()),
                exit_on_term: false,
            }
        }

        fn add(&self, facts: ProcessFacts) {
            lock(&self.facts).insert(facts.pid, facts);
        }

        fn signals(&self) -> Vec<(u32, ControlSignal)> {
            lock(&self.signals).clone()
        }
    }

    impl ProcessControlBackend for FakeBackend {
        fn node_uid(&self) -> u32 {
            self.uid
        }

        fn read(&self, pid: u32) -> Result<ProcessFacts, BackendError> {
            lock(&self.facts)
                .get(&pid)
                .cloned()
                .ok_or(BackendError::NotFound)
        }

        fn send(&self, pid: u32, signal: ControlSignal) -> Result<(), BackendError> {
            let mut facts = lock(&self.facts);
            let target = facts.get_mut(&pid).ok_or(BackendError::NotFound)?;
            lock(&self.signals).push((pid, signal));
            match signal {
                ControlSignal::Term if self.exit_on_term => target.state = 'Z',
                ControlSignal::Kill => target.state = 'Z',
                _ => {}
            }
            Ok(())
        }
    }

    fn facts(
        pid: u32,
        ppid: u32,
        uid: u32,
        start_time: u64,
        state: char,
        kernel_thread: bool,
    ) -> ProcessFacts {
        ProcessFacts {
            pid,
            ppid,
            uid,
            start_time,
            state,
            kernel_thread,
        }
    }

    /// Node pid + parent chain, an unrelated same-user process, a child, an
    /// ancestor, a kernel thread, a zombie, and an other-user process.
    fn fake_backend(exit_on_term: bool) -> (AppControl, Arc<FakeBackend>) {
        let node_pid = std::process::id();
        let backend = Arc::new(FakeBackend {
            exit_on_term,
            ..FakeBackend::new(1000)
        });
        backend.add(facts(node_pid, 100, 1000, 9000, 'S', false));
        backend.add(facts(100, 1, 1000, 8000, 'S', false));
        backend.add(facts(200, node_pid, 1000, 7000, 'S', false));
        backend.add(facts(300, 1, 2000, 6000, 'S', false));
        backend.add(facts(400, 1, 1000, 5000, 'S', false));
        backend.add(facts(401, 1, 1000, 4500, 'S', false));
        backend.add(facts(500, 2, 1000, 4000, 'S', true));
        backend.add(facts(600, 1, 1000, 3000, 'Z', false));
        let control = AppControl::with_backend(
            AppControlConfig {
                kill_after: Duration::from_millis(5),
                allow_other_users: false,
            },
            backend.clone(),
        );
        (control, backend)
    }

    #[tokio::test]
    async fn policy_matrix_refuses_privileged_self_tree_other_user_missing_and_reused_pids() {
        let (control, backend) = fake_backend(false);
        let node_pid = std::process::id();
        let cases = [
            (0, None, "apps.policy", StatusCode::FORBIDDEN),
            (1, None, "apps.policy", StatusCode::FORBIDDEN),
            (node_pid, None, "apps.own-process", StatusCode::FORBIDDEN),
            (100, None, "apps.own-process", StatusCode::FORBIDDEN),
            (200, None, "apps.own-process", StatusCode::FORBIDDEN),
            (300, None, "apps.permission", StatusCode::FORBIDDEN),
            (9999, None, "apps.not-found", StatusCode::NOT_FOUND),
            (400, Some(4000), "apps.policy", StatusCode::FORBIDDEN),
            (500, None, "apps.policy", StatusCode::FORBIDDEN),
            (600, None, "apps.not-found", StatusCode::NOT_FOUND),
        ];
        for (pid, start_time, code, status) in cases {
            let error = control
                .terminate(
                    pid,
                    ControlRequest {
                        start_time,
                        confirm: false,
                    },
                )
                .await
                .expect_err("control must be refused");
            assert_eq!(error.code, code, "pid {pid}");
            assert_eq!(error.status, status, "pid {pid}");
        }
        assert!(backend.signals().is_empty());
    }

    #[tokio::test]
    async fn terminate_escalates_to_sigkill_after_the_bounded_wait() {
        let (control, backend) = fake_backend(false);
        let started = std::time::Instant::now();
        let report = control
            .terminate(
                400,
                ControlRequest {
                    start_time: Some(5000),
                    confirm: false,
                },
            )
            .await
            .expect("terminate");
        assert_eq!(report.action, "terminate");
        assert_eq!(report.requested, "TERM");
        assert_eq!(report.signals, vec!["TERM", "KILL"]);
        assert!(report.escalated);
        assert_eq!(report.terminated, Some(true));
        assert!(started.elapsed() >= control.config().kill_after);
        assert_eq!(
            backend.signals(),
            vec![(400, ControlSignal::Term), (400, ControlSignal::Kill)]
        );
    }

    #[tokio::test]
    async fn terminate_does_not_escalate_when_the_process_exits_after_sigterm() {
        let (control, backend) = fake_backend(true);
        let report = control
            .terminate(
                400,
                ControlRequest {
                    start_time: Some(5000),
                    confirm: false,
                },
            )
            .await
            .expect("terminate");
        assert_eq!(report.signals, vec!["TERM"]);
        assert!(!report.escalated);
        assert_eq!(report.terminated, Some(true));
        assert_eq!(backend.signals(), vec![(400, ControlSignal::Term)]);
    }

    #[tokio::test]
    async fn sigkill_requires_confirmation_but_other_signals_do_not() {
        let (control, backend) = fake_backend(false);
        let error = control
            .signal(
                400,
                SignalRequest {
                    signal: "KILL".to_owned(),
                    start_time: Some(5000),
                    confirm: false,
                },
            )
            .await
            .expect_err("unconfirmed KILL");
        assert_eq!(error.code, "apps.policy");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert!(backend.signals().is_empty());

        let report = control
            .signal(
                400,
                SignalRequest {
                    signal: "SIGKILL".to_owned(),
                    start_time: Some(5000),
                    confirm: true,
                },
            )
            .await
            .expect("confirmed KILL");
        assert_eq!(report.signals, vec!["KILL"]);
        assert_eq!(report.requested, "KILL");

        let report = control
            .signal(
                401,
                SignalRequest {
                    signal: "STOP".to_owned(),
                    start_time: None,
                    confirm: false,
                },
            )
            .await
            .expect("STOP");
        assert_eq!(report.signals, vec!["STOP"]);

        let error = control
            .signal(
                401,
                SignalRequest {
                    signal: "HUP".to_owned(),
                    start_time: None,
                    confirm: false,
                },
            )
            .await
            .expect_err("allowlist");
        assert_eq!(error.code, "apps.invalid-request");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn suspend_and_resume_map_to_stop_and_cont() {
        let (control, backend) = fake_backend(false);
        let suspend = control
            .suspend(400, ControlRequest::default())
            .await
            .expect("suspend");
        assert_eq!(suspend.signals, vec!["STOP"]);
        let resume = control
            .resume(400, ControlRequest::default())
            .await
            .expect("resume");
        assert_eq!(resume.signals, vec!["CONT"]);
        assert_eq!(
            backend.signals(),
            vec![(400, ControlSignal::Stop), (400, ControlSignal::Cont)]
        );
    }

    #[tokio::test]
    async fn other_user_policy_can_be_explicitly_widened() {
        let node_pid = std::process::id();
        let backend = Arc::new(FakeBackend::new(1000));
        backend.add(facts(node_pid, 100, 1000, 9000, 'S', false));
        backend.add(facts(100, 1, 1000, 8000, 'S', false));
        backend.add(facts(300, 1, 2000, 6000, 'S', false));
        let control = AppControl::with_backend(
            AppControlConfig {
                kill_after: Duration::from_millis(5),
                allow_other_users: true,
            },
            backend.clone(),
        );
        let report = control
            .suspend(300, ControlRequest::default())
            .await
            .expect("explicit opt-in");
        assert_eq!(report.signals, vec!["STOP"]);
    }

    #[test]
    fn production_kill_delay_is_bounded() {
        assert_eq!(
            AppControlConfig::new(Duration::ZERO, false).kill_after,
            MIN_KILL_AFTER
        );
        assert_eq!(
            AppControlConfig::new(Duration::from_secs(3600), false).kill_after,
            MAX_KILL_AFTER
        );
        assert_eq!(AppControlConfig::default().kill_after, DEFAULT_KILL_AFTER);
    }

    #[test]
    fn signal_names_are_allow_listed() {
        assert_eq!(ControlSignal::parse("term"), Some(ControlSignal::Term));
        assert_eq!(ControlSignal::parse("SIGTERM"), Some(ControlSignal::Term));
        assert_eq!(ControlSignal::parse("kill"), Some(ControlSignal::Kill));
        assert_eq!(ControlSignal::parse("STOP"), Some(ControlSignal::Stop));
        assert_eq!(ControlSignal::parse("cont"), Some(ControlSignal::Cont));
        assert_eq!(ControlSignal::parse("HUP"), None);
        assert_eq!(ControlSignal::parse("9"), None);
    }

    /// `/proc/<pid>/stat` fixtures with a comm containing spaces and `)`.
    fn write_stat(root: &Path, pid: u32, ppid: u32, flags: u64, start_time: u64, state: char) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).expect("proc fixture dir");
        let line = format!(
            "{pid} (odd (name)) {state} {ppid} 1 1 0 -1 {flags} 0 0 0 0 0 0 0 0 20 0 1 0 \
             {start_time} 0 100 0\n"
        );
        fs::write(dir.join("stat"), line).expect("stat fixture");
        fs::write(
            dir.join("status"),
            format!("Name:\todd\nUid:\t1000\t1000\t1000\t1000\n"),
        )
        .expect("status fixture");
    }

    #[test]
    fn proc_backend_parses_start_time_ppid_flags_and_uid() {
        let root =
            std::env::temp_dir().join(format!("ozma-app-control-proc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write_stat(&root, 77, 12, PF_KTHREAD, 4242, 'S');
        let backend = ProcBackend::with_root(&root, 1000);
        let facts = backend.read(77).expect("fixture read");
        assert_eq!(facts.pid, 77);
        assert_eq!(facts.ppid, 12);
        assert_eq!(facts.start_time, 4242);
        assert_eq!(facts.uid, 1000);
        assert!(facts.kernel_thread);
        assert_eq!(backend.read(78), Err(BackendError::NotFound));
        let _ = fs::remove_dir_all(root);
    }

    async fn post_json(
        app: &Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, serde_json::from_slice(&bytes).expect("json body"))
    }

    #[tokio::test]
    async fn routes_return_typed_errors_and_disabled_documents() {
        let (control, _backend) = fake_backend(false);
        let app = router(Some(control));
        let node_pid = std::process::id();

        let (status, body) = post_json(
            &app,
            &format!("/apps/{node_pid}/terminate"),
            json!({"start_time": 9000}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "apps.own-process");

        let (status, body) = post_json(
            &app,
            "/apps/400/signal",
            json!({"signal": "KILL", "start_time": 5000}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "apps.policy");
        assert!(body["error"].as_str().unwrap_or("").contains("confirm"));

        let disabled = router(None);
        let (status, body) = post_json(&disabled, "/apps/400/terminate", json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "apps.control-disabled");
    }

    #[tokio::test]
    async fn terminate_route_escalates_and_reports_the_bound() {
        let (control, backend) = fake_backend(false);
        let app = router(Some(control));
        let (status, body) = post_json(
            &app,
            "/apps/400/terminate",
            json!({"start_time": 5000, "confirm": false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["signals"], json!(["TERM", "KILL"]));
        assert_eq!(body["terminated"], true);
        assert_eq!(
            backend.signals(),
            vec![(400, ControlSignal::Term), (400, ControlSignal::Kill)]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_terminate_scratch_process_and_refuse_node_pid() {
        struct KillOnDrop {
            pid: u32,
            start_time: u64,
        }

        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if let Ok(facts) = ProcBackend::host().read(self.pid) {
                    if facts.start_time == self.start_time {
                        // Test cleanup only: the scratch process is not part of
                        // the node tree because the spawning shell exits.
                        unsafe {
                            libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
                        }
                    }
                }
            }
        }

        // Spawn a scratch sleep from a short-lived shell. `nohup` lets the
        // shell exit immediately; the sleep is reparented to init, so it is not
        // a descendant of this test/node process and is eligible by policy.
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg("nohup sleep 30 >/dev/null 2>&1 & echo $!")
            .output()
            .expect("spawn scratch sleep");
        assert!(output.status.success(), "scratch shell failed");
        let pid: u32 = String::from_utf8(output.stdout)
            .expect("pid text")
            .trim()
            .parse()
            .expect("scratch pid");
        assert_ne!(pid, std::process::id());

        let backend = Arc::new(ProcBackend::host());
        let control = AppControl::with_backend(
            AppControlConfig {
                kill_after: Duration::from_millis(100),
                allow_other_users: false,
            },
            backend.clone(),
        );
        let mut facts = None;
        for _ in 0..200 {
            match backend.read(pid) {
                Ok(read) => match control.is_own_tree(&read) {
                    // The shell may not have fully reparented the orphan yet.
                    Ok(true) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Ok(false) => {
                        facts = Some(read);
                        break;
                    }
                    Err(error) => panic!("scratch ancestry unreadable: {error:?}"),
                },
                Err(BackendError::NotFound) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("scratch process unreadable: {error:?}"),
            }
        }
        let facts = facts.expect("scratch process appeared outside the node tree");
        let _guard = KillOnDrop {
            pid,
            start_time: facts.start_time,
        };

        let report = control
            .terminate(
                pid,
                ControlRequest {
                    start_time: Some(facts.start_time),
                    confirm: false,
                },
            )
            .await
            .expect("terminate scratch process");
        assert_eq!(report.requested, "TERM");
        assert_eq!(report.signals[0], "TERM");

        let mut gone = false;
        for _ in 0..200 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "scratch process {pid} survived terminate");

        let error = control
            .terminate(std::process::id(), ControlRequest::default())
            .await
            .expect_err("the node must refuse its own pid");
        assert_eq!(error.code, "apps.own-process");

        let error = control
            .terminate(1, ControlRequest::default())
            .await
            .expect_err("the node must refuse pid 1");
        assert_eq!(error.code, "apps.policy");
    }
}
