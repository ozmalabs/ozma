// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Opt-in local Restic backup provider.
//!
//! The provider is deliberately small and local-only:
//!
//! * `--backup-repo PATH` must be an absolute *local* repository path. Remote
//!   backends (`s3:`, `sftp:`, `rest:`, …) are rejected; this node never loads
//!   cloud credentials.
//! * `--backup-password-file PATH` must be an absolute regular file owned by the
//!   operator and mode `0600`. The password is handed to restic through
//!   `RESTIC_PASSWORD_FILE`; it is never placed in argv, logs, or status JSON.
//! * `--backup-source PATH` is repeatable and is the complete allow-list for
//!   backup paths. There are **no default sources** and whole-system roots are
//!   rejected.
//! * `--backup-restore-dir PATH` is repeatable and is the complete allow-list for
//!   restore targets. Restores cannot escape these roots (including through
//!   symlinks or `..`).
//!
//! The restic process is started directly (no shell), with a cleared
//! environment and only the repository/password-file variables this module
//! sets. Operations are serialised, output is bounded, and every failure is a
//! typed [`BackupError`].

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Maximum number of configured backup source roots.
pub const MAX_SOURCES: usize = 16;
/// Maximum number of configured restore roots.
pub const MAX_RESTORE_ROOTS: usize = 8;
/// Maximum number of explicit paths accepted by one run request.
pub const MAX_RUN_PATHS: usize = 16;
/// Maximum number of tags accepted by one run request.
pub const MAX_TAGS: usize = 8;
/// Maximum tag length.
pub const MAX_TAG_LEN: usize = 64;
/// Maximum path length accepted from a request or flag.
pub const MAX_PATH_LEN: usize = 4096;
/// Default snapshot list size.
pub const DEFAULT_SNAPSHOT_LIMIT: usize = 20;
/// Maximum snapshot list size.
pub const MAX_SNAPSHOT_LIMIT: usize = 100;
/// Maximum stdout retained from a restic invocation.
const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum stderr retained from a restic invocation.
const MAX_STDERR_BYTES: usize = 8 * 1024;
/// Maximum length of an error detail copied into a response.
const MAX_ERROR_DETAIL: usize = 500;
/// Maximum accepted password file size.
const MAX_PASSWORD_FILE_BYTES: u64 = 4096;

/// Whole-directory roots that are never accepted as a backup source or restore
/// root. Subdirectories are allowed; naming `/etc` or `/home` itself is not.
const DENIED_ROOTS: &[&str] = &[
    "/", "/home", "/root", "/etc", "/usr", "/var", "/boot", "/dev", "/proc", "/sys", "/run",
    "/bin", "/sbin", "/lib", "/lib64", "/opt", "/srv", "/tmp",
];

/// Timeouts for each restic operation.
#[derive(Debug, Clone)]
pub struct BackupTimeouts {
    pub init: Duration,
    pub run: Duration,
    pub snapshots: Duration,
    pub restore: Duration,
    pub forget: Duration,
    pub stats: Duration,
    pub check: Duration,
    pub estimate: Duration,
}

impl Default for BackupTimeouts {
    fn default() -> Self {
        Self {
            init: Duration::from_secs(60),
            run: Duration::from_secs(60 * 60),
            snapshots: Duration::from_secs(30),
            restore: Duration::from_secs(60 * 60),
            forget: Duration::from_secs(30 * 60),
            stats: Duration::from_secs(5 * 60),
            check: Duration::from_secs(60 * 60),
            estimate: Duration::from_secs(60 * 60),
        }
    }
}

/// Validated local backup configuration.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub repo: PathBuf,
    pub password_file: PathBuf,
    pub sources: Vec<PathBuf>,
    pub restore_roots: Vec<PathBuf>,
    pub restic_bin: PathBuf,
    pub timeouts: BackupTimeouts,
}

impl BackupConfig {
    /// Build a config with the default `restic` binary and timeouts.
    pub fn local(
        repo: PathBuf,
        password_file: PathBuf,
        sources: Vec<PathBuf>,
        restore_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            repo,
            password_file,
            sources,
            restore_roots,
            restic_bin: PathBuf::from("restic"),
            timeouts: BackupTimeouts::default(),
        }
    }
}

/// Typed backup failure. The HTTP representation never contains the password or
/// the password file contents.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("the restic binary {program:?} is not available")]
    MissingBinary { program: String },
    #[error("backup is not configured on this node")]
    NotConfigured,
    #[error("the backup repository must be an absolute local path")]
    InvalidRepo,
    #[error("the backup password file is missing, empty, or not a regular file")]
    InvalidPasswordFile,
    #[error("the backup password file must be mode 0600 and not a symlink")]
    PasswordPermissions,
    #[error("the backup repository has not been initialized")]
    Uninitialized,
    #[error("restic rejected the backup password")]
    PasswordRejected,
    #[error("permission denied while accessing a backup path")]
    PermissionDenied,
    #[error("no backup source roots are configured")]
    NoSources,
    #[error("the path is outside the configured backup allow-list")]
    PathNotAllowed,
    #[error("no restore roots are configured")]
    RestoreNotConfigured,
    #[error("the restore target escapes the configured restore roots")]
    RestoreEscape,
    #[error("the restore target is required")]
    RestoreTargetRequired,
    #[error("the snapshot id is invalid")]
    InvalidSnapshot,
    #[error("the snapshot does not exist")]
    SnapshotNotFound,
    #[error("the tag is invalid")]
    InvalidTag,
    #[error("the request mode is not supported by the local provider")]
    UnsupportedMode,
    #[error("the read-data subset must be between 1 and 5 percent")]
    InvalidSubset,
    #[error("this restic version cannot produce a dry-run size estimate")]
    EstimateUnsupported,
    #[error("another backup operation is already running")]
    Busy,
    #[error("restic {operation} failed: {detail}")]
    ResticFailed {
        operation: &'static str,
        detail: String,
    },
    #[error("restic {operation} timed out")]
    Timeout { operation: &'static str },
    #[error("backup I/O failed")]
    Io,
}

impl BackupError {
    /// Stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingBinary { .. } => "backup.restic-missing",
            Self::NotConfigured => "backup.not-configured",
            Self::InvalidRepo => "backup.repo-invalid",
            Self::InvalidPasswordFile => "backup.password-file",
            Self::PasswordPermissions => "backup.password-permissions",
            Self::Uninitialized => "backup.repo-uninitialized",
            Self::PasswordRejected => "backup.password-rejected",
            Self::PermissionDenied => "backup.permission",
            Self::NoSources => "backup.no-sources",
            Self::PathNotAllowed => "backup.path-not-allowed",
            Self::RestoreNotConfigured => "backup.restore-not-configured",
            Self::RestoreEscape => "backup.restore-path-denied",
            Self::RestoreTargetRequired => "backup.restore-target-required",
            Self::InvalidSnapshot => "backup.snapshot-invalid",
            Self::SnapshotNotFound => "backup.snapshot-not-found",
            Self::InvalidTag => "backup.tag-invalid",
            Self::UnsupportedMode => "backup.mode-unsupported",
            Self::InvalidSubset => "backup.subset-invalid",
            Self::EstimateUnsupported => "backup.estimate-unsupported",
            Self::Busy => "backup.busy",
            Self::ResticFailed { .. } => "backup.restic-failed",
            Self::Timeout { .. } => "backup.timeout",
            Self::Io => "backup.io",
        }
    }

    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Busy => StatusCode::CONFLICT,
            Self::Uninitialized => StatusCode::CONFLICT,
            Self::SnapshotNotFound => StatusCode::NOT_FOUND,
            Self::PermissionDenied | Self::PasswordPermissions | Self::RestoreEscape => {
                StatusCode::FORBIDDEN
            }
            Self::UnsupportedMode | Self::EstimateUnsupported => StatusCode::NOT_IMPLEMENTED,
            Self::MissingBinary { .. } | Self::NotConfigured | Self::RestoreNotConfigured => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::ResticFailed { .. } => StatusCode::BAD_GATEWAY,
            Self::Io => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    /// Operator-facing remediation text.
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::MissingBinary { .. } => {
                "Install restic on this node or point --backup-restic at an executable."
            }
            Self::NotConfigured => {
                "Start the node with --backup-repo and --backup-password-file to enable local backups."
            }
            Self::InvalidRepo => {
                "Use an absolute local repository path; remote s3/sftp/rest repositories are not supported."
            }
            Self::InvalidPasswordFile => {
                "Create an absolute regular password file readable by the node user."
            }
            Self::PasswordPermissions => {
                "Run chmod 600 on the password file and replace any symlink with a regular file."
            }
            Self::Uninitialized => "Call POST /backup/init once for this repository.",
            Self::PasswordRejected => {
                "Use the password file that was used to initialize this repository."
            }
            Self::PermissionDenied => {
                "Check ownership and permissions on the repository, password file, and requested paths."
            }
            Self::NoSources => {
                "Restart with at least one --backup-source allow-listed absolute directory."
            }
            Self::PathNotAllowed => {
                "Only paths equal to or below a configured --backup-source root can be backed up."
            }
            Self::RestoreNotConfigured => {
                "Restart with --backup-restore-dir before requesting a restore."
            }
            Self::RestoreEscape => {
                "Choose a target below a configured --backup-restore-dir; symlinks and '..' are rejected."
            }
            Self::RestoreTargetRequired => {
                "Send an absolute target_path below a configured --backup-restore-dir."
            }
            Self::InvalidSnapshot => {
                "Use a restic snapshot id (8-64 hex characters) or 'latest' for restore."
            }
            Self::SnapshotNotFound => "List snapshots before deleting one.",
            Self::InvalidTag => {
                "Tags are limited to 64 characters from [A-Za-z0-9._-] and at most 8 per run."
            }
            Self::UnsupportedMode => {
                "The local provider supports smart/files/manual selection over --backup-source roots only."
            }
            Self::InvalidSubset => {
                "Send read_data_percent between 1 and 5, or a read_data_subset of '1%'..'5%'."
            }
            Self::EstimateUnsupported => {
                "Upgrade restic to a version that supports backup --dry-run --json."
            }
            Self::Busy => "Wait for the running backup/restore operation to finish.",
            Self::ResticFailed { .. } => {
                "Inspect the node log and the repository state; no snapshot was reported as successful."
            }
            Self::Timeout { .. } => "Retry with a smaller source set or a shorter operation.",
            Self::Io => "Inspect the local filesystem and node log.",
        }
    }

    fn body(&self) -> Value {
        json!({
            "ok": false,
            "supported": true,
            "code": self.code(),
            "error": self.to_string(),
            "remediation": self.remediation(),
        })
    }
}

impl IntoResponse for BackupError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, Json(self.body())).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
    }
}

/// One direct restic invocation. The fake runner in tests records this exact
/// structure, so argv and environment assertions exercise production code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResticInvocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub max_stdout: usize,
}

/// Bounded result of one restic invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResticOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
}

/// Runner abstraction. Production uses [`SystemResticRunner`]; tests inject a
/// fake and can inspect the exact argv/environment or return typed failures.
#[async_trait]
pub trait ResticRunner: Send + Sync {
    /// Resolve the configured binary to an executable path.
    fn resolve(&self, configured: &Path) -> Result<PathBuf, BackupError>;

    /// Run one invocation. Implementations must bound both output streams and
    /// kill the child on timeout.
    async fn invoke(&self, invocation: ResticInvocation) -> Result<ResticOutput, BackupError>;
}

/// Production restic runner: direct exec, cleared environment, bounded output.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemResticRunner;

#[async_trait]
impl ResticRunner for SystemResticRunner {
    fn resolve(&self, configured: &Path) -> Result<PathBuf, BackupError> {
        resolve_program(configured)
    }

    async fn invoke(&self, invocation: ResticInvocation) -> Result<ResticOutput, BackupError> {
        run_system(invocation).await
    }
}

/// A configured, cloneable local backup service.
#[derive(Clone)]
pub struct BackupService {
    inner: Arc<BackupInner>,
}

struct BackupInner {
    config: BackupConfig,
    runner: Arc<dyn ResticRunner>,
    operation: tokio::sync::Mutex<()>,
    status: std::sync::Mutex<RuntimeStatus>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeStatus {
    last_run_at: Option<u64>,
    last_success_at: Option<u64>,
    last_failure_at: Option<u64>,
    last_error: Option<String>,
    last_snapshot_id: Option<String>,
}

impl BackupService {
    /// Build a service with the production runner.
    pub fn new(config: BackupConfig) -> Result<Self, BackupError> {
        Self::with_runner(config, Arc::new(SystemResticRunner))
    }

    /// Build a service with an injected runner (tests, or a future wrapper).
    pub fn with_runner(
        config: BackupConfig,
        runner: Arc<dyn ResticRunner>,
    ) -> Result<Self, BackupError> {
        let config = validate_config(config)?;
        Ok(Self {
            inner: Arc::new(BackupInner {
                config,
                runner,
                operation: tokio::sync::Mutex::new(()),
                status: std::sync::Mutex::new(RuntimeStatus::default()),
            }),
        })
    }

    pub fn config(&self) -> &BackupConfig {
        &self.inner.config
    }

    fn resolve_program(&self) -> Result<PathBuf, BackupError> {
        self.inner.runner.resolve(&self.inner.config.restic_bin)
    }

    /// `GET /backup/status`.
    pub async fn status(&self) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        let program = self.resolve_program()?;
        let repo_ready = repo_initialized(&self.inner.config.repo);
        let runtime = self.runtime_status();

        let mut last_snapshot = Value::Null;
        let mut snapshot_error = Value::Null;
        if repo_ready {
            match self
                .snapshots_raw(self.inner.config.timeouts.snapshots)
                .await
            {
                Ok(snapshots) => {
                    if let Some(snapshot) = newest_snapshot(&snapshots) {
                        last_snapshot = sanitize_snapshot(snapshot);
                    }
                }
                Err(error) => {
                    snapshot_error = json!(error.code());
                }
            }
        }

        Ok(json!({
            "ok": true,
            "supported": true,
            "configured": true,
            "restic": {
                "available": true,
                "program": program.display().to_string(),
            },
            "repo": {
                "path": self.inner.config.repo.display().to_string(),
                "state": if repo_ready { "ready" } else { "uninitialized" },
            },
            "password": {"configured": true},
            "sources": path_strings(&self.inner.config.sources),
            "restore_roots": path_strings(&self.inner.config.restore_roots),
            "last_snapshot": last_snapshot,
            "snapshot_error": snapshot_error,
            "last_run": runtime_json(&runtime),
        }))
    }

    /// `POST /backup/init` (idempotent).
    pub async fn init(&self) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if repo_initialized(&self.inner.config.repo) {
            return Ok(json!({
                "ok": true,
                "initialized": true,
                "already": true,
                "repo": {"state": "ready"},
            }));
        }

        let _guard = self.try_lock()?;
        if repo_initialized(&self.inner.config.repo) {
            return Ok(json!({
                "ok": true,
                "initialized": true,
                "already": true,
                "repo": {"state": "ready"},
            }));
        }

        let output = self
            .invoke(
                vec!["--no-cache".to_owned(), "init".to_owned()],
                self.inner.config.timeouts.init,
                MAX_STDOUT_BYTES,
            )
            .await?;
        if output.code != Some(0) {
            let detail = clean_stderr(&output.stderr);
            if detail.to_ascii_lowercase().contains("already initialized")
                || detail
                    .to_ascii_lowercase()
                    .contains("repository already exists")
            {
                return Ok(json!({
                    "ok": true,
                    "initialized": true,
                    "already": true,
                    "repo": {"state": "ready"},
                }));
            }
            return Err(classify_failure("init", &output));
        }
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::ResticFailed {
                operation: "init",
                detail: "restic init exited successfully but the repository config is absent"
                    .to_owned(),
            });
        }
        Ok(json!({
            "ok": true,
            "initialized": true,
            "already": false,
            "repo": {"state": "ready"},
        }))
    }

    /// `POST /backup/run`.
    pub async fn run(&self, request: RunRequest) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }
        let mode = validate_mode(request.mode.as_deref())?;
        if self.inner.config.sources.is_empty() {
            return Err(BackupError::NoSources);
        }
        let paths = self.resolve_run_paths(&request.paths)?;
        let tags = validate_tags(&request.tags)?;

        let _guard = self.try_lock()?;
        let mut args = vec![
            "--no-cache".to_owned(),
            "backup".to_owned(),
            "--json".to_owned(),
        ];
        for tag in &tags {
            args.push("--tag".to_owned());
            args.push(tag.clone());
        }
        args.extend(paths.iter().map(|path| path.display().to_string()));

        let output = self
            .invoke(args, self.inner.config.timeouts.run, MAX_STDOUT_BYTES)
            .await;
        match output {
            Ok(output) if output.code == Some(0) => {
                let summary = parse_backup_summary(&output.stdout);
                let snapshot_id = summary
                    .as_ref()
                    .and_then(|summary| summary.get("snapshot_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut runtime = self
                    .inner
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let now = now_secs();
                runtime.last_run_at = Some(now);
                runtime.last_success_at = Some(now);
                runtime.last_error = None;
                runtime.last_snapshot_id = snapshot_id.clone();
                drop(runtime);

                Ok(json!({
                    "ok": true,
                    "mode": mode,
                    "selection": "configured-sources",
                    "paths": path_strings(&paths),
                    "tags": tags,
                    "snapshot_id": snapshot_id,
                    "summary": summary.unwrap_or(Value::Null),
                    "repo": {"state": "ready"},
                    "limitations": {
                        "app_inventory": false,
                        "db_hooks": false,
                        "cloud_destination": false,
                        "disk_image": false,
                    }
                }))
            }
            Ok(output) => {
                let error = classify_failure("backup", &output);
                self.record_failure(&error);
                Err(error)
            }
            Err(error) => {
                self.record_failure(&error);
                Err(error)
            }
        }
    }

    /// `GET /backup/snapshots`.
    pub async fn snapshots(&self, limit: Option<usize>) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }
        let limit = limit
            .unwrap_or(DEFAULT_SNAPSHOT_LIMIT)
            .clamp(1, MAX_SNAPSHOT_LIMIT);
        let snapshots = self
            .snapshots_raw(self.inner.config.timeouts.snapshots)
            .await?;
        let total = snapshots.len();
        let start = total.saturating_sub(limit);
        let selected: Vec<Value> = snapshots[start..]
            .iter()
            .rev()
            .map(sanitize_snapshot)
            .collect();
        Ok(json!({
            "ok": true,
            "snapshots": selected,
            "count": selected.len(),
            "total": total,
            "limit": limit,
            "truncated": total > limit,
        }))
    }

    /// `GET /backup/stats`. Reports repository and snapshot statistics from
    /// `restic stats --json`; only known numeric fields are copied into the
    /// response, so repository metadata cannot leak through unchanged.
    pub async fn stats(&self) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }

        let restore = self
            .stats_raw("restore-size", self.inner.config.timeouts.stats)
            .await?;
        let unique = self
            .stats_raw("files-by-contents", self.inner.config.timeouts.stats)
            .await?;
        let repository = self
            .stats_raw("raw-data", self.inner.config.timeouts.stats)
            .await?;

        Ok(json!({
            "ok": true,
            "supported": true,
            "repo": {
                "path": self.inner.config.repo.display().to_string(),
                "state": "ready",
            },
            "snapshots": {
                "count": restore
                    .get("snapshots_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            },
            "unique": {
                "size_bytes": unique
                    .get("total_size")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                "file_count": unique
                    .get("total_file_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            },
            "restore": {
                "size_bytes": restore
                    .get("total_size")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                "file_count": restore
                    .get("total_file_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            },
            "repository": {
                "size_bytes": repository
                    .get("total_size")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                "uncompressed_size_bytes": repository
                    .get("total_uncompressed_size")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                "blob_count": repository
                    .get("total_blob_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                "compression_ratio": repository
                    .get("compression_ratio")
                    .and_then(Value::as_f64),
                "compression_space_saving_percent": repository
                    .get("compression_space_saving")
                    .and_then(Value::as_f64),
            },
            "measured_with": "restic-stats",
            "generated_at": now_secs(),
        }))
    }

    /// `POST /backup/verify`. Runs `restic check` with a bounded read-data
    /// subset. Integrity is only reported when restic exits 0 and its JSON
    /// summary reports zero errors.
    pub async fn verify(&self, request: VerifyRequest) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }
        let percent = resolve_read_data_percent(
            request.read_data_percent,
            request.read_data_subset.as_deref(),
        )?;
        let subset = format!("{percent}%");

        let _guard = self.try_lock()?;
        let output = self
            .invoke(
                vec![
                    "--no-cache".to_owned(),
                    "check".to_owned(),
                    "--read-data-subset".to_owned(),
                    subset.clone(),
                    "--json".to_owned(),
                ],
                self.inner.config.timeouts.check,
                MAX_STDOUT_BYTES,
            )
            .await;
        match output {
            Ok(output) if output.code == Some(0) => {
                let summary = parse_check_summary(&output.stdout).ok_or_else(|| {
                    BackupError::ResticFailed {
                        operation: "check",
                        detail: "restic check returned no JSON summary".to_owned(),
                    }
                })?;
                let errors = summary
                    .get("num_errors")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if errors > 0 {
                    let error = check_error_from_summary(&summary);
                    self.record_failure(&error);
                    return Err(error);
                }
                Ok(json!({
                    "ok": true,
                    "supported": true,
                    "verified": true,
                    "integrity": "restic-check",
                    "read_data_subset": subset,
                    "read_data_percent": percent,
                    "errors": 0,
                    "summary": summary,
                    "repo": {"state": "ready"},
                    "checked_at": now_secs(),
                }))
            }
            Ok(output) => {
                let error = classify_check_failure(&output);
                self.record_failure(&error);
                Err(error)
            }
            Err(error) => {
                self.record_failure(&error);
                Err(error)
            }
        }
    }

    /// `POST /backup/estimate`. Uses `restic backup --dry-run --json` so the
    /// source allow-list and exact argv rules are the same as a real run. It
    /// never reports a snapshot as created.
    pub async fn estimate(&self, request: EstimateRequest) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }
        if self.inner.config.sources.is_empty() {
            return Err(BackupError::NoSources);
        }
        let paths = self.resolve_run_paths(&request.paths)?;

        let _guard = self.try_lock()?;
        let mut args = vec![
            "--no-cache".to_owned(),
            "backup".to_owned(),
            "--dry-run".to_owned(),
            "--json".to_owned(),
        ];
        args.extend(paths.iter().map(|path| path.display().to_string()));

        let output = self
            .invoke(args, self.inner.config.timeouts.estimate, MAX_STDOUT_BYTES)
            .await;
        match output {
            Ok(output) if output.code == Some(0) => {
                let mut summary = parse_backup_summary(&output.stdout).ok_or_else(|| {
                    BackupError::ResticFailed {
                        operation: "estimate",
                        detail: "restic dry-run returned no JSON summary".to_owned(),
                    }
                })?;
                let estimated_bytes = summary
                    .get("total_bytes_processed")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| BackupError::ResticFailed {
                        operation: "estimate",
                        detail: "restic dry-run summary had no total_bytes_processed".to_owned(),
                    })?;
                let file_count = summary.get("total_files_processed").and_then(Value::as_u64);
                if let Value::Object(map) = &mut summary {
                    // A dry-run summary id is not a stored snapshot.
                    map.remove("snapshot_id");
                }
                Ok(json!({
                    "ok": true,
                    "supported": true,
                    "estimated": true,
                    "method": "restic-backup-dry-run",
                    "paths": path_strings(&paths),
                    "estimated_bytes": estimated_bytes,
                    "file_count": file_count,
                    "summary": summary,
                    "repo": {"state": "ready"},
                    "generated_at": now_secs(),
                }))
            }
            Ok(output) => {
                let lower = output.stderr.to_ascii_lowercase();
                let error = if lower.contains("unknown flag")
                    || lower.contains("unknown shorthand flag")
                    || lower.contains("flag provided but not defined")
                {
                    BackupError::EstimateUnsupported
                } else {
                    classify_failure("estimate", &output)
                };
                self.record_failure(&error);
                Err(error)
            }
            Err(error) => {
                self.record_failure(&error);
                Err(error)
            }
        }
    }

    /// `POST /backup/restore`.
    pub async fn restore(&self, request: RestoreRequest) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }
        let snapshot = request
            .snapshot_id
            .clone()
            .or_else(|| request.snapshot.clone())
            .ok_or(BackupError::InvalidSnapshot)?;
        validate_snapshot_id(&snapshot, true)?;
        let snapshot = if snapshot == "latest" {
            snapshot
        } else {
            snapshot.to_ascii_lowercase()
        };
        let source_path = request.source_path.unwrap_or_else(|| "/".to_owned());
        validate_snapshot_path(&source_path)?;
        let source_path = normalize_lexical(Path::new(&source_path))
            .map_err(|_| BackupError::PathNotAllowed)?
            .display()
            .to_string();
        let target = request
            .target_path
            .ok_or(BackupError::RestoreTargetRequired)?;
        let target = self.resolve_restore_target(&target)?;

        let _guard = self.try_lock()?;
        let mut args = vec![
            "--no-cache".to_owned(),
            "restore".to_owned(),
            snapshot.clone(),
            "--target".to_owned(),
            target.display().to_string(),
        ];
        if source_path != "/" {
            args.push("--include".to_owned());
            args.push(source_path.clone());
        }
        let output = self
            .invoke(args, self.inner.config.timeouts.restore, MAX_STDOUT_BYTES)
            .await;
        match output {
            Ok(output) if output.code == Some(0) => Ok(json!({
                "ok": true,
                "snapshot_id": snapshot,
                "source_path": source_path,
                "target_path": target.display().to_string(),
                "restored": true,
            })),
            Ok(output) => {
                let error = classify_failure("restore", &output);
                self.record_failure(&error);
                Err(error)
            }
            Err(error) => {
                self.record_failure(&error);
                Err(error)
            }
        }
    }

    /// `DELETE /backup/snapshots/{id}`. Only the exact snapshot named by the
    /// caller is forgotten; `--prune` removes unreferenced data only. No
    /// retention policy is applied to other snapshots.
    pub async fn delete_snapshot(&self, id: &str) -> Result<Value, BackupError> {
        validate_password_file(&self.inner.config.password_file)?;
        self.resolve_program()?;
        if !repo_initialized(&self.inner.config.repo) {
            return Err(BackupError::Uninitialized);
        }
        validate_snapshot_id(id, false)?;
        let id = id.to_ascii_lowercase();

        let snapshots = self
            .snapshots_raw(self.inner.config.timeouts.snapshots)
            .await?;
        let matches: Vec<&Value> = snapshots
            .iter()
            .filter(|snapshot| {
                snapshot
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|full| full.eq_ignore_ascii_case(&id) || full.starts_with(&id))
            })
            .collect();
        let full_id = match matches.as_slice() {
            [snapshot] => snapshot
                .get("id")
                .and_then(Value::as_str)
                .ok_or(BackupError::SnapshotNotFound)?
                .to_owned(),
            [] => return Err(BackupError::SnapshotNotFound),
            _ => {
                return Err(BackupError::InvalidSnapshot);
            }
        };

        let _guard = self.try_lock()?;
        let output = self
            .invoke(
                vec![
                    "--no-cache".to_owned(),
                    "forget".to_owned(),
                    full_id.clone(),
                    "--prune".to_owned(),
                ],
                self.inner.config.timeouts.forget,
                MAX_STDOUT_BYTES,
            )
            .await;
        match output {
            Ok(output) if output.code == Some(0) => {
                let mut runtime = self
                    .inner
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if runtime.last_snapshot_id.as_deref() == Some(full_id.as_str()) {
                    runtime.last_snapshot_id = None;
                }
                drop(runtime);
                Ok(json!({
                    "ok": true,
                    "deleted": full_id,
                    "pruned": true,
                    "policy": "exact-snapshot-only",
                }))
            }
            Ok(output) => {
                let error = classify_failure("forget", &output);
                self.record_failure(&error);
                Err(error)
            }
            Err(error) => {
                self.record_failure(&error);
                Err(error)
            }
        }
    }

    fn try_lock(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, BackupError> {
        self.inner
            .operation
            .try_lock()
            .map_err(|_| BackupError::Busy)
    }

    async fn invoke(
        &self,
        args: Vec<String>,
        timeout: Duration,
        max_stdout: usize,
    ) -> Result<ResticOutput, BackupError> {
        let program = self.resolve_program()?;
        let invocation = ResticInvocation {
            program,
            args,
            env: vec![
                (
                    "RESTIC_REPOSITORY".to_owned(),
                    self.inner.config.repo.display().to_string(),
                ),
                (
                    "RESTIC_PASSWORD_FILE".to_owned(),
                    self.inner.config.password_file.display().to_string(),
                ),
                ("RESTIC_PROGRESS_FPS".to_owned(), "0".to_owned()),
            ],
            timeout,
            max_stdout,
        };
        self.inner.runner.invoke(invocation).await
    }

    async fn snapshots_raw(&self, timeout: Duration) -> Result<Vec<Value>, BackupError> {
        let output = self
            .invoke(
                vec![
                    "--no-cache".to_owned(),
                    "snapshots".to_owned(),
                    "--json".to_owned(),
                ],
                timeout,
                MAX_STDOUT_BYTES,
            )
            .await?;
        if output.code != Some(0) {
            return Err(classify_failure("snapshots", &output));
        }
        let value: Value =
            serde_json::from_str(&output.stdout).map_err(|_| BackupError::ResticFailed {
                operation: "snapshots",
                detail: "restic snapshots did not return valid JSON".to_owned(),
            })?;
        match value {
            Value::Array(snapshots) => Ok(snapshots),
            _ => Err(BackupError::ResticFailed {
                operation: "snapshots",
                detail: "restic snapshots JSON was not an array".to_owned(),
            }),
        }
    }

    async fn stats_raw(&self, mode: &str, timeout: Duration) -> Result<Value, BackupError> {
        let output = self
            .invoke(
                vec![
                    "--no-cache".to_owned(),
                    "stats".to_owned(),
                    "--mode".to_owned(),
                    mode.to_owned(),
                    "--json".to_owned(),
                ],
                timeout,
                MAX_STDOUT_BYTES,
            )
            .await?;
        if output.code != Some(0) {
            return Err(classify_failure("stats", &output));
        }
        let value: Value =
            serde_json::from_str(&output.stdout).map_err(|_| BackupError::ResticFailed {
                operation: "stats",
                detail: "restic stats did not return valid JSON".to_owned(),
            })?;
        if !value.is_object() {
            return Err(BackupError::ResticFailed {
                operation: "stats",
                detail: "restic stats JSON was not an object".to_owned(),
            });
        }
        Ok(value)
    }

    fn resolve_run_paths(&self, requested: &[String]) -> Result<Vec<PathBuf>, BackupError> {
        if requested.len() > MAX_RUN_PATHS {
            return Err(BackupError::PathNotAllowed);
        }
        let requested: Vec<String> = if requested.is_empty() {
            self.inner
                .config
                .sources
                .iter()
                .map(|path| path.display().to_string())
                .collect()
        } else {
            requested.to_vec()
        };
        let mut paths = Vec::with_capacity(requested.len());
        let mut seen = BTreeSet::new();
        for raw in requested {
            validate_path_text(&raw)?;
            let path = PathBuf::from(&raw);
            if !path.is_absolute() {
                return Err(BackupError::PathNotAllowed);
            }
            let canonical = std::fs::canonicalize(&path).map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => BackupError::PathNotAllowed,
                std::io::ErrorKind::PermissionDenied => BackupError::PermissionDenied,
                _ => BackupError::Io,
            })?;
            if !self
                .inner
                .config
                .sources
                .iter()
                .any(|source| canonical.starts_with(source))
            {
                return Err(BackupError::PathNotAllowed);
            }
            if seen.insert(canonical.clone()) {
                paths.push(canonical);
            }
        }
        if paths.is_empty() {
            return Err(BackupError::NoSources);
        }
        Ok(paths)
    }

    fn resolve_restore_target(&self, raw: &str) -> Result<PathBuf, BackupError> {
        if self.inner.config.restore_roots.is_empty() {
            return Err(BackupError::RestoreNotConfigured);
        }
        validate_path_text(raw).map_err(|_| BackupError::RestoreEscape)?;
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(BackupError::RestoreEscape);
        }
        let normalized = normalize_lexical(&path)?;
        let root = self
            .inner
            .config
            .restore_roots
            .iter()
            .find(|root| normalized.starts_with(root) && &normalized != *root)
            .ok_or(BackupError::RestoreEscape)?
            .clone();
        // Walk/create only inside the validated root. Any symlink component is
        // rejected rather than followed, so a target cannot hop outside.
        let relative = normalized
            .strip_prefix(&root)
            .map_err(|_| BackupError::RestoreEscape)?;
        let mut cursor = root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(BackupError::RestoreEscape);
            };
            cursor.push(name);
            match std::fs::symlink_metadata(&cursor) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(BackupError::RestoreEscape);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&cursor).map_err(|error| match error.kind() {
                        std::io::ErrorKind::PermissionDenied => BackupError::PermissionDenied,
                        _ => BackupError::Io,
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(BackupError::PermissionDenied);
                }
                Err(_) => return Err(BackupError::Io),
            }
        }
        let canonical =
            std::fs::canonicalize(&normalized).map_err(|_| BackupError::RestoreEscape)?;
        if !canonical.starts_with(&root) || canonical == root {
            return Err(BackupError::RestoreEscape);
        }
        if canonical.starts_with(&self.inner.config.repo) {
            return Err(BackupError::RestoreEscape);
        }
        Ok(canonical)
    }

    fn runtime_status(&self) -> RuntimeStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record_failure(&self, error: &BackupError) {
        let mut runtime = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.last_run_at = Some(now_secs());
        runtime.last_failure_at = Some(now_secs());
        runtime.last_error = Some(error.code().to_owned());
    }
}

/// Body for `POST /backup/run`. Unknown fields are ignored for controller
/// compatibility; only the bounded `paths`/`tags` fields affect the allow-list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunRequest {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Body for `POST /backup/restore`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RestoreRequest {
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub target_path: Option<String>,
}

/// Body for `POST /backup/verify`. The percent is clamped to the safe 1–5%
/// read-data subset already enforced by [`resolve_read_data_percent`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VerifyRequest {
    #[serde(default)]
    pub read_data_percent: Option<u8>,
    /// Restic-style subset such as `"2%"`; validated and never passed raw.
    #[serde(default)]
    pub read_data_subset: Option<String>,
}

/// Body for `POST /backup/estimate`. Unknown fields are ignored; `paths` uses
/// the same allow-list and bounds as `POST /backup/run`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EstimateRequest {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SnapshotQuery {
    limit: Option<usize>,
}

/// Router for the local backup provider. Mount this before the legacy wildcard
/// so the implemented core routes win and unsupported sub-surfaces fall through
/// to the typed 501 handler.
pub fn router(service: Option<BackupService>) -> Router {
    Router::new()
        .route("/backup/status", get(status_handler))
        .route("/backup/init", post(init_handler))
        .route("/backup/run", post(run_handler))
        .route("/backup/snapshots", get(snapshots_handler))
        .route("/backup/snapshots/:id", delete(delete_handler))
        .route("/backup/restore", post(restore_handler))
        .route("/backup/stats", get(stats_handler))
        .route("/backup/verify", post(verify_handler))
        .route("/backup/estimate", post(estimate_handler))
        .route("/backup/apps", get(apps_handler))
        .route("/backup/apps/restore", post(restore_apps_handler))
        .with_state(BackupRouteState { service })
}

#[derive(Clone)]
struct BackupRouteState {
    service: Option<BackupService>,
}

fn not_configured() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "supported": true,
            "configured": false,
            "code": "backup.not-configured",
            "error": "backup is not configured on this node",
            "remediation": "Start the node with --backup-repo and --backup-password-file.",
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn status_handler(State(state): State<BackupRouteState>) -> Response {
    let Some(service) = state.service else {
        let body = json!({
            "ok": true,
            "supported": true,
            "configured": false,
            "restic": {"available": false},
            "repo": {"path": null, "state": "unconfigured"},
            "password": {"configured": false},
            "sources": [],
            "restore_roots": [],
            "last_snapshot": null,
            "snapshot_error": null,
            "last_run": null,
        });
        let mut response = Json(body).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        return response;
    };
    match service.status().await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn init_handler(State(state): State<BackupRouteState>) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.init().await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn run_handler(
    State(state): State<BackupRouteState>,
    Json(request): Json<RunRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.run(request).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn snapshots_handler(
    State(state): State<BackupRouteState>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.snapshots(query.limit).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn restore_handler(
    State(state): State<BackupRouteState>,
    Json(request): Json<RestoreRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.restore(request).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn delete_handler(
    State(state): State<BackupRouteState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.delete_snapshot(&id).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn stats_handler(State(state): State<BackupRouteState>) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.stats().await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn verify_handler(
    State(state): State<BackupRouteState>,
    Json(request): Json<VerifyRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.verify(request).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn estimate_handler(
    State(state): State<BackupRouteState>,
    Json(request): Json<EstimateRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.estimate(request).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn apps_handler(State(_state): State<BackupRouteState>) -> Response {
    app_inventory_unsupported("/backup/apps")
}

async fn restore_apps_handler(State(_state): State<BackupRouteState>) -> Response {
    app_inventory_unsupported("/backup/apps/restore")
}

/// App inventory is not derivable from a restic repository: snapshots contain
/// file trees, not installed OS packages or app definitions. Keep the surface
/// as an honest typed 501 instead of inventing an app list.
fn app_inventory_unsupported(surface: &'static str) -> Response {
    let reason = "This local restic provider does not record an OS application or \
                  package inventory; repository snapshots only contain file trees.";
    let body = json!({
        "ok": false,
        "supported": false,
        "code": "backup.unsupported",
        "surface": surface,
        "capability": "backup",
        "reason": reason,
        "error": reason,
        "python_reference": "agent/backup.py::BackupManager.get_app_inventory",
        "remediation": "Use the Python desktop agent for app inventory/restore-apps, or \
                        inspect snapshot file paths; this node does not fabricate an app list.",
    });
    let mut response = (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn json_no_store(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_config(mut config: BackupConfig) -> Result<BackupConfig, BackupError> {
    if !config.repo.is_absolute() {
        return Err(BackupError::InvalidRepo);
    }
    validate_path_text(&config.repo.display().to_string()).map_err(|_| BackupError::InvalidRepo)?;
    let repo_text = config.repo.display().to_string();
    let lower = repo_text.to_ascii_lowercase();
    if [
        "s3:", "sftp:", "rest:", "http:", "https:", "azure:", "gs:", "b2:", "rclone:", "swift:",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        return Err(BackupError::InvalidRepo);
    }
    let repo = normalize_lexical(&config.repo)?;
    if denied_root(&repo) {
        return Err(BackupError::InvalidRepo);
    }
    config.repo = repo;

    validate_password_file(&config.password_file)?;
    validate_path_text(&config.password_file.display().to_string())
        .map_err(|_| BackupError::InvalidPasswordFile)?;
    let password =
        normalize_lexical(&config.password_file).map_err(|_| BackupError::InvalidPasswordFile)?;
    if password.starts_with(&config.repo) {
        return Err(BackupError::InvalidPasswordFile);
    }
    config.password_file = password;

    if config.sources.len() > MAX_SOURCES {
        return Err(BackupError::PathNotAllowed);
    }
    let mut sources = Vec::with_capacity(config.sources.len());
    for source in config.sources {
        validate_path_text(&source.display().to_string())?;
        if !source.is_absolute() {
            return Err(BackupError::PathNotAllowed);
        }
        let canonical = std::fs::canonicalize(&source).map_err(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied => BackupError::PermissionDenied,
            _ => BackupError::PathNotAllowed,
        })?;
        if !canonical.is_dir() || denied_root(&canonical) {
            return Err(BackupError::PathNotAllowed);
        }
        if config.repo.starts_with(&canonical) || canonical.starts_with(&config.repo) {
            return Err(BackupError::PathNotAllowed);
        }
        if !sources.contains(&canonical) {
            sources.push(canonical);
        }
    }
    config.sources = sources;

    if config.restore_roots.len() > MAX_RESTORE_ROOTS {
        return Err(BackupError::RestoreEscape);
    }
    let mut roots = Vec::with_capacity(config.restore_roots.len());
    for root in config.restore_roots {
        if !root.is_absolute() || validate_path_text(&root.display().to_string()).is_err() {
            return Err(BackupError::RestoreEscape);
        }
        let canonical = std::fs::canonicalize(&root).map_err(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied => BackupError::PermissionDenied,
            _ => BackupError::RestoreEscape,
        })?;
        if !canonical.is_dir()
            || denied_root(&canonical)
            || canonical.starts_with(&config.repo)
            || config.repo.starts_with(&canonical)
        {
            return Err(BackupError::RestoreEscape);
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    config.restore_roots = roots;
    Ok(config)
}

fn validate_password_file(path: &Path) -> Result<(), BackupError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(BackupError::InvalidPasswordFile);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::PermissionDenied => BackupError::PermissionDenied,
        _ => BackupError::InvalidPasswordFile,
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(BackupError::InvalidPasswordFile);
    }
    if metadata.len() == 0 || metadata.len() > MAX_PASSWORD_FILE_BYTES {
        return Err(BackupError::InvalidPasswordFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != 0o600 {
            return Err(BackupError::PasswordPermissions);
        }
    }
    Ok(())
}

fn validate_mode(raw: Option<&str>) -> Result<String, BackupError> {
    let mode = raw.unwrap_or("smart").trim().to_ascii_lowercase();
    match mode.as_str() {
        "smart" | "files" | "manual" => Ok(mode),
        "disk_image" | "advanced" => Err(BackupError::UnsupportedMode),
        _ => Err(BackupError::UnsupportedMode),
    }
}

fn resolve_read_data_percent(percent: Option<u8>, subset: Option<&str>) -> Result<u8, BackupError> {
    if let Some(raw) = subset {
        let trimmed = raw.trim();
        let trimmed = trimmed.strip_suffix('%').unwrap_or(trimmed);
        // Only bare integer percentages are accepted; restic's other subset
        // syntaxes (n/t, byte sizes) are deliberately outside this bound.
        let value: u8 = trimmed.parse().map_err(|_| BackupError::InvalidSubset)?;
        return validate_subset_percent(value);
    }
    validate_subset_percent(percent.unwrap_or(1))
}

fn validate_subset_percent(value: u8) -> Result<u8, BackupError> {
    if (1..=5).contains(&value) {
        Ok(value)
    } else {
        Err(BackupError::InvalidSubset)
    }
}

fn validate_tags(tags: &[String]) -> Result<Vec<String>, BackupError> {
    if tags.len() > MAX_TAGS {
        return Err(BackupError::InvalidTag);
    }
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        if tag.is_empty()
            || tag.len() > MAX_TAG_LEN
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(BackupError::InvalidTag);
        }
        if !out.contains(tag) {
            out.push(tag.clone());
        }
    }
    Ok(out)
}

fn validate_snapshot_id(id: &str, allow_latest: bool) -> Result<(), BackupError> {
    if allow_latest && id == "latest" {
        return Ok(());
    }
    if !(8..=64).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BackupError::InvalidSnapshot);
    }
    Ok(())
}

fn validate_snapshot_path(path: &str) -> Result<(), BackupError> {
    validate_path_text(path)?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(BackupError::PathNotAllowed);
    }
    normalize_lexical(path).map(|_| ())
}

fn validate_path_text(raw: &str) -> Result<(), BackupError> {
    if raw.is_empty()
        || raw.len() > MAX_PATH_LEN
        || raw
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
    {
        return Err(BackupError::PathNotAllowed);
    }
    // Traversal components are rejected before any lexical/canonical cleanup.
    // This keeps `..` out of restic argv even when the normalized result would
    // happen to remain inside an allow-list root.
    if Path::new(raw)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(BackupError::PathNotAllowed);
    }
    Ok(())
}

fn normalize_lexical(path: &Path) -> Result<PathBuf, BackupError> {
    if !path.is_absolute() {
        return Err(BackupError::PathNotAllowed);
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => return Err(BackupError::PathNotAllowed),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(BackupError::PathNotAllowed);
                }
            }
            Component::Normal(name) => out.push(name),
        }
    }
    Ok(out)
}

fn denied_root(path: &Path) -> bool {
    DENIED_ROOTS.iter().any(|root| path == Path::new(root))
}

fn repo_initialized(repo: &Path) -> bool {
    repo.join("config").is_file()
}

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn runtime_json(runtime: &RuntimeStatus) -> Value {
    json!({
        "last_run_at": runtime.last_run_at,
        "last_success_at": runtime.last_success_at,
        "last_failure_at": runtime.last_failure_at,
        "last_error": runtime.last_error,
        "last_snapshot_id": runtime.last_snapshot_id,
    })
}

fn newest_snapshot(snapshots: &[Value]) -> Option<&Value> {
    snapshots.iter().max_by(|left, right| {
        let left = left.get("time").and_then(Value::as_str).unwrap_or("");
        let right = right.get("time").and_then(Value::as_str).unwrap_or("");
        left.cmp(right)
    })
}

fn sanitize_snapshot(snapshot: &Value) -> Value {
    let paths = snapshot
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .take(MAX_RUN_PATHS)
                .map(|path| {
                    if path.len() > MAX_PATH_LEN {
                        path[..MAX_PATH_LEN].to_owned()
                    } else {
                        path.to_owned()
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tags = snapshot
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .take(MAX_TAGS)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "id": snapshot.get("id").and_then(Value::as_str),
        "short_id": snapshot.get("short_id").and_then(Value::as_str),
        "time": snapshot.get("time").and_then(Value::as_str),
        "hostname": snapshot.get("hostname").and_then(Value::as_str),
        "tags": tags,
        "paths": paths,
        "program_version": snapshot.get("program_version").and_then(Value::as_str),
    })
}

fn parse_backup_summary(stdout: &str) -> Option<Value> {
    for line in stdout.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if value.get("message_type").and_then(Value::as_str) != Some("summary") {
            continue;
        }
        let keep = [
            "message_type",
            "files_new",
            "files_changed",
            "files_unmodified",
            "dirs_new",
            "dirs_changed",
            "dirs_unmodified",
            "total_files_processed",
            "total_bytes_processed",
            "total_duration",
            "data_added",
            "snapshot_id",
        ];
        let mut sanitized = serde_json::Map::new();
        for key in keep {
            if let Some(value) = value.get(key) {
                if value.is_string() || value.is_number() || value.is_boolean() || value.is_null() {
                    sanitized.insert(key.to_owned(), value.clone());
                }
            }
        }
        return Some(Value::Object(sanitized));
    }
    None
}

/// Extract a bounded, secret-free summary from `restic check --json`. Error
/// lines are ignored; only the final summary object is reported.
fn parse_check_summary(stdout: &str) -> Option<Value> {
    for line in stdout.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if value.get("message_type").and_then(Value::as_str) != Some("summary") {
            continue;
        }
        let num_errors = value.get("num_errors").and_then(Value::as_u64)?;
        let broken_packs = value
            .get("broken_packs")
            .and_then(Value::as_array)
            .map(|packs| {
                packs
                    .iter()
                    .filter_map(Value::as_str)
                    .take(16)
                    .map(|pack| pack.chars().take(64).collect::<String>())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return Some(json!({
            "num_errors": num_errors,
            "broken_packs": broken_packs,
            "suggest_repair_index": value
                .get("suggest_repair_index")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "suggest_prune": value
                .get("suggest_prune")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }));
    }
    None
}

fn check_error_from_summary(summary: &Value) -> BackupError {
    let errors = summary
        .get("num_errors")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let broken = summary
        .get("broken_packs")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let detail = if broken > 0 {
        format!("restic check found {errors} error(s) and {broken} damaged pack file(s)")
    } else {
        format!("restic check found {errors} error(s)")
    };
    BackupError::ResticFailed {
        operation: "check",
        detail,
    }
}

fn clean_stderr(stderr: &str) -> String {
    let cleaned: String = stderr
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_ERROR_DETAIL).collect()
}

fn classify_check_failure(output: &ResticOutput) -> BackupError {
    // `restic check --json` reports repository damage on stdout, not stderr.
    if let Some(summary) = parse_check_summary(&output.stdout) {
        if summary
            .get("num_errors")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
        {
            return check_error_from_summary(&summary);
        }
    }
    classify_failure("check", output)
}

fn classify_failure(operation: &'static str, output: &ResticOutput) -> BackupError {
    // Restic's documented exit codes are shared by check/stats/snapshots.
    match output.code {
        Some(10) => return BackupError::Uninitialized,
        Some(12) => return BackupError::PasswordRejected,
        _ => {}
    }
    let lower = output.stderr.to_ascii_lowercase();
    if lower.contains("is there a repository at the following location")
        || lower.contains("repository does not exist")
        || lower.contains("unable to open config file")
        || lower.contains("repository not found")
    {
        return BackupError::Uninitialized;
    }
    if lower.contains("wrong password") || lower.contains("invalid password") {
        return BackupError::PasswordRejected;
    }
    if lower.contains("permission denied") || lower.contains("access is denied") {
        return BackupError::PermissionDenied;
    }
    BackupError::ResticFailed {
        operation,
        detail: clean_stderr(&output.stderr),
    }
}

// ---------------------------------------------------------------------------
// System process runner
// ---------------------------------------------------------------------------

fn resolve_program(configured: &Path) -> Result<PathBuf, BackupError> {
    if configured.is_absolute() || configured.components().count() > 1 {
        return executable(configured).ok_or_else(|| BackupError::MissingBinary {
            program: configured.display().to_string(),
        });
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(configured);
        if let Some(candidate) = executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(BackupError::MissingBinary {
        program: configured.display().to_string(),
    })
}

fn executable(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(path.to_path_buf())
}

async fn run_system(invocation: ResticInvocation) -> Result<ResticOutput, BackupError> {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // A local repository needs no inherited credentials or restic overrides.
    // Set only the repository/password-file variables plus a minimal PATH for
    // any future local helper restic may exec.
    command
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C");
    for (key, value) in &invocation.env {
        command.env(key, value);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return match error.kind() {
                std::io::ErrorKind::NotFound => Err(BackupError::MissingBinary {
                    program: invocation.program.display().to_string(),
                }),
                std::io::ErrorKind::PermissionDenied => Err(BackupError::PermissionDenied),
                _ => Err(BackupError::Io),
            };
        }
    };
    let stdout = child
        .stdout
        .take()
        .ok_or(BackupError::Io)
        .map_err(|_| BackupError::Io)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(BackupError::Io)
        .map_err(|_| BackupError::Io)?;

    let stdout_limit = invocation.max_stdout.min(MAX_STDOUT_BYTES);
    let wait = async { child.wait().await };
    let read = async {
        let (stdout, stdout_truncated) = read_limited(stdout, stdout_limit).await?;
        let (stderr, _) = read_limited(stderr, MAX_STDERR_BYTES).await?;
        let status = wait.await?;
        Ok::<_, std::io::Error>((stdout, stdout_truncated, stderr, status))
    };

    match tokio::time::timeout(invocation.timeout, read).await {
        Ok(Ok((stdout, stdout_truncated, stderr, status))) => Ok(ResticOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_truncated,
        }),
        Ok(Err(error)) => Err(match error.kind() {
            std::io::ErrorKind::PermissionDenied => BackupError::PermissionDenied,
            _ => BackupError::Io,
        }),
        Err(_) => Err(BackupError::Timeout {
            operation: "restic",
        }),
    }
}

async fn read_limited<R>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8192];
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len() < limit {
            let take = read.min(limit - output.len());
            output.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Debug, Clone)]
    struct FakeResponse {
        code: Option<i32>,
        stdout: String,
        stderr: String,
        create_repo: bool,
    }

    impl FakeResponse {
        fn ok(stdout: &str) -> Self {
            Self {
                code: Some(0),
                stdout: stdout.to_owned(),
                stderr: String::new(),
                create_repo: false,
            }
        }

        fn fail(stderr: &str) -> Self {
            Self {
                code: Some(1),
                stdout: String::new(),
                stderr: stderr.to_owned(),
                create_repo: false,
            }
        }
    }

    struct FakeRunner {
        calls: Mutex<Vec<ResticInvocation>>,
        responses: Mutex<VecDeque<FakeResponse>>,
        repo: PathBuf,
        available: bool,
    }

    impl FakeRunner {
        fn new(repo: PathBuf) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
                repo,
                available: true,
            }
        }

        fn push(&self, response: FakeResponse) {
            self.responses.lock().unwrap().push_back(response);
        }

        fn calls(&self) -> Vec<ResticInvocation> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ResticRunner for FakeRunner {
        fn resolve(&self, _configured: &Path) -> Result<PathBuf, BackupError> {
            if self.available {
                Ok(PathBuf::from("/usr/bin/restic"))
            } else {
                Err(BackupError::MissingBinary {
                    program: "restic".to_owned(),
                })
            }
        }

        async fn invoke(&self, invocation: ResticInvocation) -> Result<ResticOutput, BackupError> {
            self.calls.lock().unwrap().push(invocation.clone());
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| FakeResponse::ok("[]"));
            if response.create_repo {
                std::fs::create_dir_all(&self.repo).unwrap();
                std::fs::write(self.repo.join("config"), b"fake").unwrap();
            }
            Ok(ResticOutput {
                code: response.code,
                stdout: response.stdout,
                stderr: response.stderr,
                stdout_truncated: false,
            })
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        repo: PathBuf,
        source: PathBuf,
        restore: PathBuf,
        password: PathBuf,
        secret: String,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            let source = temp.path().join("source");
            let restore = temp.path().join("restore");
            let password = temp.path().join("password");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::create_dir_all(&restore).unwrap();
            let secret = "correct-horse-battery-staple".to_owned();
            std::fs::write(&password, &secret).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            Self {
                _temp: temp,
                repo,
                source,
                restore,
                password,
                secret,
            }
        }

        fn config(&self) -> BackupConfig {
            BackupConfig::local(
                self.repo.clone(),
                self.password.clone(),
                vec![self.source.clone()],
                vec![self.restore.clone()],
            )
        }

        fn service(&self, runner: Arc<FakeRunner>) -> BackupService {
            BackupService::with_runner(self.config(), runner).unwrap()
        }

        fn initialize_repo(&self) {
            std::fs::create_dir_all(&self.repo).unwrap();
            std::fs::write(self.repo.join("config"), b"fake").unwrap();
        }
    }

    fn snapshot_json(id: &str, time: &str) -> String {
        json!([{
            "id": id,
            "short_id": &id[..8],
            "time": time,
            "hostname": "node",
            "tags": ["t1"],
            "paths": ["/tmp/source"],
            "program_version": "restic 0.20.0",
        }])
        .to_string()
    }

    #[tokio::test]
    async fn run_uses_exact_argv_and_never_puts_the_password_in_argv_or_env() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(
            &json!({
                "message_type": "summary",
                "snapshot_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "total_files_processed": 2,
                "total_bytes_processed": 42,
            })
            .to_string(),
        ));
        let service = fixture.service(runner.clone());

        let body = service
            .run(RunRequest {
                mode: Some("smart".to_owned()),
                paths: vec![fixture.source.display().to_string()],
                tags: vec!["manual".to_owned()],
            })
            .await
            .unwrap();

        assert_eq!(
            body["snapshot_id"],
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(
            call.args,
            vec![
                "--no-cache".to_owned(),
                "backup".to_owned(),
                "--json".to_owned(),
                "--tag".to_owned(),
                "manual".to_owned(),
                fixture.source.display().to_string(),
            ]
        );
        assert!(call
            .env
            .iter()
            .any(|(key, value)| key == "RESTIC_PASSWORD_FILE"
                && value == &fixture.password.display().to_string()));
        assert!(!call.env.iter().any(|(key, _)| key == "RESTIC_PASSWORD"));
        assert!(!call
            .args
            .iter()
            .chain(call.env.iter().map(|(_, value)| value))
            .any(|value| value.contains(&fixture.secret)));
    }

    #[tokio::test]
    async fn init_is_idempotent_and_runs_once() {
        let fixture = Fixture::new();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            create_repo: true,
        });
        let service = fixture.service(runner.clone());

        let first = service.init().await.unwrap();
        let second = service.init().await.unwrap();
        assert_eq!(first["already"], false);
        assert_eq!(second["already"], true);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "second init must not invoke restic");
        assert_eq!(
            calls[0].args,
            vec!["--no-cache".to_owned(), "init".to_owned()]
        );
    }

    #[tokio::test]
    async fn run_rejects_paths_outside_the_allowlist_and_invalid_tags() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        let service = fixture.service(runner.clone());

        let outside = fixture._temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let error = service
            .run(RunRequest {
                mode: None,
                paths: vec![outside.display().to_string()],
                tags: vec![],
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.path-not-allowed");

        let error = service
            .run(RunRequest {
                mode: None,
                paths: vec![],
                tags: vec!["bad tag".to_owned()],
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.tag-invalid");

        std::fs::create_dir_all(fixture.source.join("sub")).unwrap();
        let traversal = fixture.source.join("sub").join("..").join("hello.txt");
        let error = service
            .run(RunRequest {
                mode: None,
                paths: vec![traversal.display().to_string()],
                tags: vec![],
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.path-not-allowed");
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn snapshots_are_bounded_and_do_not_include_secrets() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        let mut entries = Vec::new();
        for index in 0..5 {
            entries.push(json!({
                "id": format!("{:064x}", index),
                "short_id": format!("{:08x}", index),
                "time": format!("2026-11-2{}T00:00:00Z", index),
                "hostname": "node",
                "tags": [],
                "paths": [fixture.source.display().to_string()],
            }));
        }
        runner.push(FakeResponse::ok(&Value::Array(entries).to_string()));
        let service = fixture.service(runner.clone());

        let body = service.snapshots(Some(2)).await.unwrap();
        assert_eq!(body["count"], 2);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["snapshots"][0]["id"], format!("{:064x}", 4));
        assert!(!body.to_string().contains(&fixture.secret));
        assert_eq!(
            runner.calls()[0].args,
            vec![
                "--no-cache".to_owned(),
                "snapshots".to_owned(),
                "--json".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn stats_extracts_known_numbers_and_never_passes_extra_fields_through() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(
            &json!({
                "total_size": 1000,
                "total_file_count": 3,
                "snapshots_count": 2,
            })
            .to_string(),
        ));
        runner.push(FakeResponse::ok(
            &json!({
                "total_size": 800,
                "total_file_count": 2,
                "snapshots_count": 2,
            })
            .to_string(),
        ));
        runner.push(FakeResponse::ok(
            &json!({
                "total_size": 600,
                "total_uncompressed_size": 900,
                "total_blob_count": 4,
                "compression_ratio": 1.5,
                "compression_space_saving": 33.3,
                "snapshots_count": 2,
                "unexpected": fixture.secret,
            })
            .to_string(),
        ));
        let service = fixture.service(runner.clone());

        let body = service.stats().await.unwrap();
        assert_eq!(body["snapshots"]["count"], 2);
        assert_eq!(body["restore"]["size_bytes"], 1000);
        assert_eq!(body["restore"]["file_count"], 3);
        assert_eq!(body["unique"]["size_bytes"], 800);
        assert_eq!(body["unique"]["file_count"], 2);
        assert_eq!(body["repository"]["size_bytes"], 600);
        assert_eq!(body["repository"]["uncompressed_size_bytes"], 900);
        assert_eq!(body["repository"]["blob_count"], 4);
        assert_eq!(body["repository"]["compression_ratio"], 1.5);
        assert!(!body.to_string().contains(&fixture.secret));
        assert!(body.get("unexpected").is_none());

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        for (call, mode) in calls
            .iter()
            .zip(["restore-size", "files-by-contents", "raw-data"])
        {
            assert_eq!(
                call.args,
                vec![
                    "--no-cache".to_owned(),
                    "stats".to_owned(),
                    "--mode".to_owned(),
                    mode.to_owned(),
                    "--json".to_owned(),
                ]
            );
        }
    }

    #[tokio::test]
    async fn verify_uses_a_bounded_read_data_subset_and_reports_success_only_from_restic() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(
            &json!({
                "message_type": "summary",
                "num_errors": 0,
                "broken_packs": null,
                "suggest_repair_index": false,
                "suggest_prune": false,
            })
            .to_string(),
        ));
        let service = fixture.service(runner.clone());

        let body = service
            .verify(VerifyRequest {
                read_data_percent: Some(2),
                read_data_subset: None,
            })
            .await
            .unwrap();
        assert_eq!(body["verified"], true);
        assert_eq!(body["integrity"], "restic-check");
        assert_eq!(body["read_data_subset"], "2%");
        assert_eq!(body["read_data_percent"], 2);
        assert_eq!(body["errors"], 0);
        assert_eq!(
            runner.calls()[0].args,
            vec![
                "--no-cache".to_owned(),
                "check".to_owned(),
                "--read-data-subset".to_owned(),
                "2%".to_owned(),
                "--json".to_owned(),
            ]
        );

        runner.push(FakeResponse::ok(
            &json!({
                "message_type": "summary",
                "num_errors": 0,
            })
            .to_string(),
        ));
        let body = service
            .verify(VerifyRequest {
                read_data_percent: None,
                read_data_subset: Some("5%".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(body["read_data_subset"], "5%");
    }

    #[tokio::test]
    async fn verify_failures_never_claim_integrity() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        let service = fixture.service(runner.clone());

        for (percent, subset) in [
            (Some(0), None),
            (Some(6), None),
            (None, Some("7%".to_owned())),
            (None, Some("2/t".to_owned())),
        ] {
            let error = service
                .verify(VerifyRequest {
                    read_data_percent: percent,
                    read_data_subset: subset,
                })
                .await
                .unwrap_err();
            assert_eq!(error.code(), "backup.subset-invalid");
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        }
        assert!(runner.calls().is_empty());

        runner.push(FakeResponse {
            code: Some(1),
            stdout: json!({
                "message_type": "summary",
                "num_errors": 3,
                "broken_packs": ["a".repeat(64)],
            })
            .to_string(),
            stderr: String::new(),
            create_repo: false,
        });
        let error = service.verify(VerifyRequest::default()).await.unwrap_err();
        assert_eq!(error.code(), "backup.restic-failed");
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        let message = error.to_string();
        assert!(message.contains("3 error"), "{message}");
        assert!(!message.to_ascii_lowercase().contains("verified"));

        runner.push(FakeResponse {
            code: Some(0),
            stdout: json!({
                "message_type": "summary",
                "num_errors": 2,
            })
            .to_string(),
            stderr: String::new(),
            create_repo: false,
        });
        let error = service.verify(VerifyRequest::default()).await.unwrap_err();
        assert_eq!(error.code(), "backup.restic-failed");

        runner.push(FakeResponse {
            code: Some(0),
            stdout: json!({"message_type": "summary"}).to_string(),
            stderr: String::new(),
            create_repo: false,
        });
        let error = service.verify(VerifyRequest::default()).await.unwrap_err();
        assert_eq!(error.code(), "backup.restic-failed");
    }

    #[tokio::test]
    async fn estimate_uses_the_run_allow_list_and_dry_run_argv() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(
            &json!({
                "message_type": "summary",
                "total_files_processed": 5,
                "total_bytes_processed": 12345,
                "snapshot_id": "not-a-real-snapshot",
            })
            .to_string(),
        ));
        let service = fixture.service(runner.clone());

        let body = service
            .estimate(EstimateRequest {
                paths: vec![fixture.source.display().to_string()],
            })
            .await
            .unwrap();
        assert_eq!(body["estimated"], true);
        assert_eq!(body["estimated_bytes"], 12345);
        assert_eq!(body["file_count"], 5);
        assert_eq!(body["method"], "restic-backup-dry-run");
        assert_eq!(body["paths"][0], fixture.source.display().to_string());
        assert!(body["summary"].get("snapshot_id").is_none());
        assert_eq!(
            runner.calls()[0].args,
            vec![
                "--no-cache".to_owned(),
                "backup".to_owned(),
                "--dry-run".to_owned(),
                "--json".to_owned(),
                fixture.source.display().to_string(),
            ]
        );

        let outside = fixture._temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let error = service
            .estimate(EstimateRequest {
                paths: vec![outside.display().to_string()],
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.path-not-allowed");
        assert_eq!(runner.calls().len(), 1);
    }

    #[tokio::test]
    async fn estimate_reports_unsupported_restic_versions_and_missing_summaries_typed() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        let service = fixture.service(runner.clone());

        runner.push(FakeResponse::fail("unknown flag: --dry-run\n"));
        let error = service
            .estimate(EstimateRequest::default())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.estimate-unsupported");
        assert_eq!(error.status(), StatusCode::NOT_IMPLEMENTED);

        runner.push(FakeResponse::ok(""));
        let error = service
            .estimate(EstimateRequest::default())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.restic-failed");
        assert!(!error.to_string().to_ascii_lowercase().contains("estimated"));
    }

    #[tokio::test]
    async fn app_inventory_surfaces_stay_typed_501_and_do_not_invent_apps() {
        let app = router(None);
        let (status, body) = get(app.clone(), "/backup/apps").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["ok"], false);
        assert_eq!(body["supported"], false);
        assert_eq!(body["capability"], "backup");
        assert_eq!(body["code"], "backup.unsupported");
        assert!(body["reason"]
            .as_str()
            .unwrap()
            .contains("does not record an OS application"));
        assert!(body.get("apps").is_none());

        let (status, body) = post(app, "/backup/apps/restore", json!({"names": ["firefox"]})).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "backup.unsupported");
        assert_eq!(body["surface"], "/backup/apps/restore");
        assert!(body.get("restored").is_none());
    }

    #[tokio::test]
    async fn stats_verify_estimate_routes_are_typed_not_configured_without_a_repo() {
        let app = router(None);
        let (status, body) = get(app.clone(), "/backup/stats").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "backup.not-configured");

        for (uri, body) in [
            ("/backup/verify", json!({})),
            ("/backup/estimate", json!({})),
        ] {
            let (status, response) = post(app.clone(), uri, body).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(response["code"], "backup.not-configured", "{uri}");
        }
    }

    #[tokio::test]
    async fn restore_rejects_traversal_symlink_and_unknown_roots() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        let service = fixture.service(runner.clone());

        for target in [
            "/etc/passwd".to_owned(),
            fixture
                .restore
                .join("..")
                .join("escape")
                .display()
                .to_string(),
            fixture
                .restore
                .join("inside")
                .join("..")
                .join("other")
                .display()
                .to_string(),
            fixture.restore.display().to_string(),
        ] {
            let error = service
                .restore(RestoreRequest {
                    snapshot_id: Some("0123456789abcdef".to_owned()),
                    source_path: Some("/".to_owned()),
                    target_path: Some(target),
                    snapshot: None,
                })
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error.code(),
                    "backup.restore-path-denied" | "backup.restore-target-required"
                ),
                "unexpected {error:?}"
            );
        }

        #[cfg(unix)]
        {
            let link = fixture.restore.join("link");
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            let error = service
                .restore(RestoreRequest {
                    snapshot_id: Some("0123456789abcdef".to_owned()),
                    source_path: Some("/".to_owned()),
                    target_path: Some(link.join("sub").display().to_string()),
                    snapshot: None,
                })
                .await
                .unwrap_err();
            assert_eq!(error.code(), "backup.restore-path-denied");
        }
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn restore_uses_exact_argv_and_a_validated_target() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(""));
        let service = fixture.service(runner.clone());
        let target = fixture.restore.join("bundle").join("files");

        let body = service
            .restore(RestoreRequest {
                snapshot_id: Some("0123456789abcdef".to_owned()),
                source_path: Some("/srv/data".to_owned()),
                target_path: Some(target.display().to_string()),
                snapshot: None,
            })
            .await
            .unwrap();
        assert_eq!(body["restored"], true);
        let canonical = std::fs::canonicalize(&target).unwrap();
        assert_eq!(
            runner.calls()[0].args,
            vec![
                "--no-cache".to_owned(),
                "restore".to_owned(),
                "0123456789abcdef".to_owned(),
                "--target".to_owned(),
                canonical.display().to_string(),
                "--include".to_owned(),
                "/srv/data".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn delete_is_exact_and_rejects_latest_or_ambiguous_prefixes() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        runner.push(FakeResponse::ok(&snapshot_json(id, "2026-11-25T00:00:00Z")));
        runner.push(FakeResponse::ok(""));
        let service = fixture.service(runner.clone());

        let error = service.delete_snapshot("latest").await.unwrap_err();
        assert_eq!(error.code(), "backup.snapshot-invalid");

        let body = service.delete_snapshot(&id[..8]).await.unwrap();
        assert_eq!(body["deleted"], id);
        assert_eq!(body["policy"], "exact-snapshot-only");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].args,
            vec![
                "--no-cache".to_owned(),
                "forget".to_owned(),
                id.to_owned(),
                "--prune".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn missing_binary_and_password_permissions_are_typed() {
        let fixture = Fixture::new();
        let runner = Arc::new(FakeRunner {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
            repo: fixture.repo.clone(),
            available: false,
        });
        let service = fixture.service(runner);
        let error = service.status().await.unwrap_err();
        assert_eq!(error.code(), "backup.restic-missing");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
            let service = fixture.service(runner);
            std::fs::set_permissions(&fixture.password, std::fs::Permissions::from_mode(0o644))
                .unwrap();
            let error = service.status().await.unwrap_err();
            assert_eq!(error.code(), "backup.password-permissions");
            assert_eq!(error.status(), StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn typed_failures_are_classified() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::fail(
            "Fatal: wrong password or no key found\n",
        ));
        let service = fixture.service(runner.clone());
        let error = service
            .run(RunRequest {
                mode: None,
                paths: vec![],
                tags: vec![],
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.password-rejected");
        assert!(!error.to_string().contains(&fixture.secret));
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(app: Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn routes_show_unconfigured_status_and_typed_errors() {
        let app = router(None);
        let (status, body) = get(app.clone(), "/backup/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], false);
        assert_eq!(body["repo"]["state"], "unconfigured");

        let (status, body) = post(app, "/backup/run", json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "backup.not-configured");
    }

    #[tokio::test]
    async fn routes_return_ready_status_and_real_run_summary() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(&snapshot_json(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "2026-11-25T00:00:00Z",
        )));
        runner.push(FakeResponse::ok(
            &json!({"message_type": "summary", "snapshot_id": "abc"}).to_string(),
        ));
        let app = router(Some(fixture.service(runner)));

        let (status, body) = get(app.clone(), "/backup/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["repo"]["state"], "ready");
        assert_eq!(body["last_snapshot"]["short_id"], "01234567");

        let (status, body) = post(
            app,
            "/backup/run",
            json!({"mode": "smart", "tags": ["manual"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["snapshot_id"], "abc");
        assert_eq!(body["paths"][0], fixture.source.display().to_string());
    }

    #[tokio::test]
    async fn routes_expose_stats_verify_and_estimate_documents() {
        let fixture = Fixture::new();
        fixture.initialize_repo();
        let runner = Arc::new(FakeRunner::new(fixture.repo.clone()));
        runner.push(FakeResponse::ok(
            &json!({
                "total_size": 1000,
                "total_file_count": 3,
                "snapshots_count": 1,
            })
            .to_string(),
        ));
        runner.push(FakeResponse::ok(
            &json!({
                "total_size": 800,
                "total_file_count": 2,
                "snapshots_count": 1,
            })
            .to_string(),
        ));
        runner.push(FakeResponse::ok(
            &json!({
                "total_size": 600,
                "total_uncompressed_size": 900,
                "total_blob_count": 4,
                "snapshots_count": 1,
            })
            .to_string(),
        ));
        runner.push(FakeResponse::ok(
            &json!({"message_type": "summary", "num_errors": 0}).to_string(),
        ));
        runner.push(FakeResponse::ok(
            &json!({
                "message_type": "summary",
                "total_files_processed": 3,
                "total_bytes_processed": 1000,
                "snapshot_id": "dry-run",
            })
            .to_string(),
        ));
        let app = router(Some(fixture.service(runner)));

        let (status, body) = get(app.clone(), "/backup/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["snapshots"]["count"], 1);
        assert_eq!(body["repository"]["size_bytes"], 600);
        assert_eq!(body["unique"]["size_bytes"], 800);

        let (status, body) = post(
            app.clone(),
            "/backup/verify",
            json!({"read_data_percent": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["verified"], true);
        assert_eq!(body["read_data_subset"], "1%");

        let (status, body) = post(
            app,
            "/backup/estimate",
            json!({"paths": [fixture.source.display().to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["estimated_bytes"], 1000);
        assert_eq!(body["file_count"], 3);
        assert_eq!(body["method"], "restic-backup-dry-run");
    }

    /// Live end-to-end against the real restic binary. Skips cleanly when the
    /// host has no restic, so the normal unit suite stays portable.
    #[tokio::test]
    async fn live_restic_init_run_restore_delete_round_trip() {
        if std::process::Command::new("restic")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping live restic test: restic is not installed");
            return;
        }

        let fixture = Fixture::new();
        let nested = fixture.source.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let text_path = fixture.source.join("hello.txt");
        let nested_path = nested.join("bytes.bin");
        let empty_path = nested.join("empty");
        let text = b"hello from the ozma live backup test\n";
        let bytes: Vec<u8> = (0u8..=255).cycle().take(8192).collect();
        std::fs::write(&text_path, text).unwrap();
        std::fs::write(&nested_path, &bytes).unwrap();
        std::fs::write(&empty_path, b"").unwrap();

        let service = BackupService::new(fixture.config()).unwrap();
        let first = service.init().await.unwrap();
        assert_eq!(first["initialized"], true);
        assert_eq!(first["already"], false);
        let second = service.init().await.unwrap();
        assert_eq!(second["already"], true);

        let run = service
            .run(RunRequest {
                mode: Some("smart".to_owned()),
                paths: vec![fixture.source.display().to_string()],
                tags: vec!["live-test".to_owned()],
            })
            .await
            .unwrap();
        let snapshot_id = run["snapshot_id"]
            .as_str()
            .expect("live restic run returns a snapshot id")
            .to_owned();
        assert_eq!(snapshot_id.len(), 64, "{snapshot_id}");

        let snapshots = service.snapshots(Some(10)).await.unwrap();
        assert_eq!(snapshots["count"], 1);
        assert_eq!(snapshots["snapshots"][0]["id"], snapshot_id);

        let target = fixture.restore.join("live-restore");
        let restored = service
            .restore(RestoreRequest {
                snapshot_id: Some(snapshot_id.clone()),
                source_path: Some("/".to_owned()),
                target_path: Some(target.display().to_string()),
                snapshot: None,
            })
            .await
            .unwrap();
        assert_eq!(restored["restored"], true);

        // Restic restores absolute snapshot paths below --target.
        let restored_root = target.join(fixture.source.strip_prefix("/").unwrap());
        assert_eq!(
            std::fs::read(restored_root.join("hello.txt")).unwrap(),
            text
        );
        assert_eq!(
            std::fs::read(restored_root.join("nested/bytes.bin")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read(restored_root.join("nested/empty")).unwrap(),
            b""
        );

        let status = service.status().await.unwrap();
        assert_eq!(status["repo"]["state"], "ready");
        assert_eq!(status["sources"][0], fixture.source.display().to_string());

        let deleted = service.delete_snapshot(&snapshot_id).await.unwrap();
        assert_eq!(deleted["deleted"], snapshot_id);
        assert_eq!(deleted["policy"], "exact-snapshot-only");
        let after = service.snapshots(Some(10)).await.unwrap();
        assert_eq!(after["count"], 0);
    }

    /// Live stats/estimate/verify evidence against a real restic repository,
    /// including a real damaged-pack failure that must never report success.
    #[tokio::test]
    async fn live_restic_stats_estimate_verify_and_damaged_pack_failure() {
        if std::process::Command::new("restic")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping live restic test: restic is not installed");
            return;
        }

        let fixture = Fixture::new();
        std::fs::write(fixture.source.join("hello.txt"), b"live stats and verify\n").unwrap();
        std::fs::write(fixture.source.join("payload.bin"), vec![7u8; 8192]).unwrap();

        let service = BackupService::new(fixture.config()).unwrap();
        service.init().await.unwrap();
        let run = service
            .run(RunRequest {
                mode: Some("smart".to_owned()),
                paths: vec![],
                tags: vec!["live-stats".to_owned()],
            })
            .await
            .unwrap();
        assert_eq!(run["ok"], true);

        let estimate = service.estimate(EstimateRequest::default()).await.unwrap();
        assert_eq!(estimate["estimated"], true);
        assert_eq!(estimate["method"], "restic-backup-dry-run");
        assert!(
            estimate["estimated_bytes"].as_u64().unwrap() >= 8192,
            "{estimate}"
        );
        assert_eq!(estimate["file_count"], 2);

        let stats = service.stats().await.unwrap();
        assert_eq!(stats["snapshots"]["count"], 1);
        assert!(stats["repository"]["size_bytes"].as_u64().unwrap() > 0);
        assert!(stats["unique"]["size_bytes"].as_u64().unwrap() >= 8192);
        assert!(stats["restore"]["file_count"].as_u64().unwrap() >= 2);

        let verify = service
            .verify(VerifyRequest {
                read_data_percent: Some(1),
                read_data_subset: None,
            })
            .await
            .unwrap();
        assert_eq!(verify["verified"], true);
        assert_eq!(verify["read_data_subset"], "1%");
        assert_eq!(verify["errors"], 0);

        // Remove one pack file so restic check must report repository damage.
        let mut pack = None;
        for shard in std::fs::read_dir(fixture.repo.join("data")).unwrap() {
            let shard = shard.unwrap();
            if !shard.file_type().unwrap().is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path()).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    pack = Some(entry.path());
                    break;
                }
            }
            if pack.is_some() {
                break;
            }
        }
        std::fs::remove_file(pack.expect("repository contains a pack file")).unwrap();

        let error = service
            .verify(VerifyRequest {
                read_data_percent: Some(1),
                read_data_subset: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "backup.restic-failed");
        assert!(error.to_string().contains("error"), "{error}");
        assert!(!error.to_string().to_ascii_lowercase().contains("verified"));
    }
}
