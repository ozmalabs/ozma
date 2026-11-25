// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, read-only host surfaces for storage, processes, and self status.
//!
//! These routes cover the operator-facing part of the deleted Python
//! `agent/storage_manager.py` and `agent/app_monitor.py` APIs without turning
//! them into privileged storage administration or parental-control enforcement.
//! Everything here is observational:
//!
//! * `GET /storage` parses `/proc/mounts`, keeps only mounts backed by a real
//!   block device (`/dev/...`), and reports `statvfs` capacity for at most
//!   [`MAX_STORAGE_FILESYSTEMS`] filesystems. It never mounts, unmounts, formats,
//!   scrubs, or writes a device.
//! * `GET /apps` walks numeric `/proc/<pid>` directories and reports `pid`,
//!   short `comm` name, CPU percentage, RSS, and the owning user. It never reads
//!   `cmdline`, never kills a process, and returns at most
//!   [`MAX_APPS`] entries (`limit` defaults to [`DEFAULT_APPS`]).
//! * `GET /self` reports this process (`std::process::id()`), its procfs
//!   uptime/RSS, the crate version and profile, the cgroup service unit when one
//!   is visible, and a restart count only when the supervisor explicitly
//!   exports `OZMA_RESTART_COUNT`. Absent facts are `null`, never invented.
//!
//! All filesystem reads are capped, all lists are bounded, and an unreadable
//! procfs/source is a typed `503` instead of an empty success. The routes are
//! mounted in the guarded controller-compatibility router next to `/hardware`;
//! `/api/v1/connection.host_surfaces` advertises their availability without
//! running a scan.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Controller-facing storage surface.
pub const STORAGE_PATH: &str = "/storage";
/// Versioned alias for the storage surface.
pub const STORAGE_API_PATH: &str = "/api/v1/storage";
/// Controller-facing process surface.
pub const APPS_PATH: &str = "/apps";
/// Versioned alias for the process surface.
pub const APPS_API_PATH: &str = "/api/v1/apps";
/// Controller-facing self-management surface.
pub const SELF_PATH: &str = "/self";
/// Versioned alias for the self-management surface.
pub const SELF_API_PATH: &str = "/api/v1/self";

/// Largest number of block-backed mounts returned by `/storage`.
pub const MAX_STORAGE_FILESYSTEMS: usize = 64;
/// Largest number of processes returned by `/apps`.
pub const MAX_APPS: usize = 100;
/// Default `/apps?limit=` when the query omits a limit.
pub const DEFAULT_APPS: usize = 20;
/// Largest `/etc/passwd` read used to resolve process owners.
pub const MAX_PASSWD_BYTES: u64 = 2 * 1024 * 1024;
/// Largest number of passwd entries accepted while resolving owners.
pub const MAX_PASSWD_ENTRIES: usize = 100_000;
/// Largest number of `/proc` entries inspected for one process report.
pub const MAX_PID_SCAN: usize = 16_384;
/// Minimum interval between two CPU samples before a delta is trustworthy.
pub const MIN_CPU_INTERVAL_MS: u64 = 50;
/// Longest process name emitted (the kernel's `comm` is 15 bytes; leave room).
const MAX_PROCESS_NAME_CHARS: usize = 64;
/// Maximum bytes read from a per-process procfs file.
const MAX_PROC_FILE_BYTES: u64 = 16 * 1024;
/// Maximum mount-list read.
const MAX_MOUNT_BYTES: u64 = 512 * 1024;

/// Filesystems that are backed by `/dev` but are still virtual/pseudo mounts.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "autofs",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "overlay",
    "pstore",
    "ramfs",
    "rpc_pipefs",
    "securityfs",
    "selinuxfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

/// Shared state behind the three read-only host routes.
#[derive(Clone)]
pub struct HostSurfacesState {
    inner: Arc<Inner>,
}

struct Inner {
    paths: HostPaths,
    availability: Availability,
    service: ServiceFacts,
    identity: HostIdentity,
    previous_apps: Mutex<Option<AppSample>>,
}

/// Node identity copied from the connection document for `/self`.
#[derive(Clone, Debug, Default)]
struct HostIdentity {
    node_id: String,
    profile: String,
}

/// Paths used by the host surfaces. Split out so tests can use a fake procfs.
#[derive(Clone, Debug)]
struct HostPaths {
    proc_root: PathBuf,
    mounts: PathBuf,
    passwd: PathBuf,
    cgroup: PathBuf,
}

impl HostPaths {
    fn host() -> Self {
        let proc_root = PathBuf::from("/proc");
        Self {
            mounts: proc_root.join("mounts"),
            passwd: PathBuf::from("/etc/passwd"),
            cgroup: proc_root.join("self/cgroup"),
            proc_root,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Availability {
    storage: bool,
    apps: bool,
    self_management: bool,
}

/// Systemd/supervisor facts captured once at startup.
///
/// The process environment is read only here; routes never inspect another
/// process's environment. Restart count is deliberately opt-in: systemd does
/// not export `NRestarts` to a service, so a supervisor that knows the value
/// must pass `OZMA_RESTART_COUNT`. Without it the field is `null`.
#[derive(Clone, Debug, Default)]
struct ServiceFacts {
    unit: Option<String>,
    unit_source: Option<&'static str>,
    restart_count: Option<u64>,
    restart_count_source: Option<&'static str>,
    systemd: bool,
}

impl ServiceFacts {
    fn from_env(cgroup_text: Option<&str>) -> Self {
        let unit_env = env_string("OZMA_UNIT");
        let (unit, unit_source) = match unit_env {
            Some(unit) => (Some(unit), Some("env:OZMA_UNIT")),
            None => match cgroup_text.and_then(unit_from_cgroup) {
                Some(unit) => (Some(unit), Some("cgroup")),
                None => (None, None),
            },
        };
        let (restart_count, restart_count_source) =
            match env_string("OZMA_RESTART_COUNT").and_then(|value| value.parse::<u64>().ok()) {
                Some(count) => (Some(count), Some("env:OZMA_RESTART_COUNT")),
                None => (None, None),
            };
        let systemd = env_string("INVOCATION_ID").is_some()
            || env_string("JOURNAL_STREAM").is_some()
            || unit
                .as_deref()
                .is_some_and(|unit| unit.ends_with(".service") || unit.ends_with(".scope"));
        Self {
            unit,
            unit_source,
            restart_count,
            restart_count_source,
            systemd,
        }
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The `/api/v1/connection.host_surfaces` advertisement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostSurfaceAdvertisement {
    /// Mounted block filesystems.
    pub storage: HostSurfaceCapability,
    /// Bounded process list.
    pub apps: HostSurfaceCapability,
    /// This process's own status.
    #[serde(rename = "self")]
    pub self_management: HostSurfaceCapability,
}

/// One advertised host surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostSurfaceCapability {
    /// This node implements the route.
    pub supported: bool,
    /// Stable controller path.
    pub path: &'static str,
    /// The backing source was readable when the node started.
    pub available: bool,
    /// The route is mounted behind the node bearer guard.
    pub guarded: bool,
    /// What the route reports, or why it cannot report it.
    pub reason: String,
    /// Output bound, when the surface has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl HostSurfacesState {
    /// Detect host availability without scanning mounts or processes.
    pub fn host() -> Self {
        Self::with_paths(HostPaths::host())
    }

    /// Detect host availability and copy the node identity for `/self`.
    ///
    /// The connection document remains the source of truth; this only stores
    /// the stable `node_id`/`profile` strings before the state is shared.
    pub fn host_with_identity(node_id: impl Into<String>, profile: impl Into<String>) -> Self {
        Self::with_paths_and_identity(
            HostPaths::host(),
            HostIdentity {
                node_id: node_id.into(),
                profile: profile.into(),
            },
        )
    }

    fn with_paths(paths: HostPaths) -> Self {
        Self::with_paths_and_identity(paths, HostIdentity::default())
    }

    fn with_paths_and_identity(paths: HostPaths, identity: HostIdentity) -> Self {
        let availability = Availability {
            storage: File::open(&paths.mounts).is_ok(),
            apps: paths.proc_root.is_dir() && fs::read_dir(&paths.proc_root).is_ok(),
            self_management: File::open(paths.proc_root.join("self/stat")).is_ok(),
        };
        let cgroup_text = read_bounded(&paths.cgroup, MAX_PROC_FILE_BYTES).ok();
        let service = ServiceFacts::from_env(cgroup_text.as_deref());
        Self {
            inner: Arc::new(Inner {
                paths,
                availability,
                service,
                identity,
                previous_apps: Mutex::new(None),
            }),
        }
    }

    /// Build a state over a synthetic procfs/passwd tree. Used by tests.
    #[cfg(test)]
    fn with_test_paths(root: &Path) -> Self {
        let proc_root = root.join("proc");
        Self::with_paths(HostPaths {
            mounts: proc_root.join("mounts"),
            passwd: root.join("passwd"),
            cgroup: proc_root.join("self/cgroup"),
            proc_root,
        })
    }

    /// Cheap startup availability, suitable for `/api/v1/connection`.
    pub fn advertisement(&self) -> HostSurfaceAdvertisement {
        let availability = self.inner.availability;
        HostSurfaceAdvertisement {
            storage: HostSurfaceCapability {
                supported: true,
                path: STORAGE_PATH,
                available: availability.storage,
                guarded: true,
                reason: if availability.storage {
                    "Reads mounted /dev block filesystems and statvfs capacity; read-only, no secrets, at most 64 entries.".to_owned()
                } else {
                    "The mount table could not be opened at startup; /storage returns a typed 503."
                        .to_owned()
                },
                limit: Some(MAX_STORAGE_FILESYSTEMS),
            },
            apps: HostSurfaceCapability {
                supported: true,
                path: APPS_PATH,
                available: availability.apps,
                guarded: true,
                reason: if availability.apps {
                    "Lists bounded process pid/name/CPU/RSS/user samples from procfs; never reads cmdline, at most 100 entries.".to_owned()
                } else {
                    "procfs could not be read at startup; /apps returns a typed 503.".to_owned()
                },
                limit: Some(MAX_APPS),
            },
            self_management: HostSurfaceCapability {
                supported: true,
                path: SELF_PATH,
                available: availability.self_management,
                guarded: true,
                reason: if availability.self_management {
                    "Reports this node process's pid, procfs uptime, RSS, version/profile, unit, and supervisor-exported restart count.".to_owned()
                } else {
                    "This process's procfs stat could not be read at startup; /self returns a typed 503."
                        .to_owned()
                },
                limit: None,
            },
        }
    }

    /// Collect the bounded mounted-block-filesystem report.
    pub fn storage(&self) -> Result<StorageReport, HostError> {
        let text = read_bounded(&self.inner.paths.mounts, MAX_MOUNT_BYTES).map_err(|error| {
            HostError::new(
                "storage.proc-unavailable",
                format!("could not read the mount table: {error}"),
            )
        })?;
        Ok(build_storage_report(
            &text,
            MAX_STORAGE_FILESYSTEMS,
            statvfs_usage,
        ))
    }

    /// Collect the bounded process report.
    pub fn apps(&self, limit: usize) -> Result<AppReport, HostError> {
        let limit = limit.clamp(1, MAX_APPS);
        let paths = &self.inner.paths;
        let entries = fs::read_dir(&paths.proc_root).map_err(|error| {
            HostError::new(
                "apps.proc-unavailable",
                format!("could not read procfs: {error}"),
            )
        })?;

        let uptime = read_bounded(&paths.proc_root.join("uptime"), MAX_PROC_FILE_BYTES)
            .ok()
            .and_then(|text| parse_uptime(&text));
        let users = read_bounded(&paths.passwd, MAX_PASSWD_BYTES)
            .ok()
            .map(|text| parse_passwd(&text))
            .unwrap_or_default();

        let now = Instant::now();
        let mut sampled: Vec<SampledProcess> = Vec::new();
        let mut unreadable = 0usize;
        let mut scanned = 0usize;
        let mut scan_truncated = false;

        for entry in entries.flatten() {
            if scanned >= MAX_PID_SCAN {
                scan_truncated = true;
                break;
            }
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            scanned += 1;
            let dir = entry.path();
            let Some(stat) = read_bounded(&dir.join("stat"), MAX_PROC_FILE_BYTES)
                .ok()
                .and_then(|text| parse_proc_stat(&text))
            else {
                unreadable += 1;
                continue;
            };
            let name = read_bounded(&dir.join("comm"), MAX_PROC_FILE_BYTES)
                .ok()
                .map(|text| sanitize_process_name(&text))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| sanitize_process_name(&stat.name));
            let uid = read_bounded(&dir.join("status"), MAX_PROC_FILE_BYTES)
                .ok()
                .and_then(|text| parse_status_uid(&text));
            let user = match uid {
                Some(uid) => users
                    .get(&uid)
                    .cloned()
                    .unwrap_or_else(|| format!("uid:{uid}")),
                None => "unknown".to_owned(),
            };
            let rss_bytes = stat.rss_pages.saturating_mul(page_size());
            sampled.push(SampledProcess {
                pid,
                name,
                ticks: stat.cpu_ticks,
                rss_bytes,
                uid,
                user,
            });
        }

        let (previous, elapsed) = {
            let mut guard = lock(&self.inner.previous_apps);
            let elapsed = guard
                .as_ref()
                .map(|sample| now.duration_since(sample.at).as_secs_f64());
            let previous = guard.take();
            *guard = Some(AppSample {
                at: now,
                ticks: sampled
                    .iter()
                    .map(|process| (process.pid, process.ticks))
                    .collect(),
            });
            (previous, elapsed)
        };

        let hz = clock_ticks();
        let use_delta = previous.as_ref().and_then(|_sample| {
            let elapsed = elapsed?;
            (elapsed >= MIN_CPU_INTERVAL_MS as f64 / 1000.0).then_some(elapsed)
        });
        let previous_ticks = previous.as_ref().map(|sample| &sample.ticks);

        let mut processes: Vec<AppProcess> = sampled
            .into_iter()
            .map(|process| {
                let (cpu_percent, cpu_window) = if let (Some(previous), Some(elapsed)) =
                    (previous_ticks, use_delta)
                {
                    match previous.get(&process.pid) {
                        Some(previous_ticks) => (
                            cpu_percent(process.ticks, Some(*previous_ticks), elapsed, hz),
                            Some("since-last-sample"),
                        ),
                        None => (
                            uptime.and_then(|uptime| cpu_percent(process.ticks, None, uptime, hz)),
                            Some("since-boot"),
                        ),
                    }
                } else {
                    (
                        uptime.and_then(|uptime| cpu_percent(process.ticks, None, uptime, hz)),
                        uptime.map(|_| "since-boot"),
                    )
                };
                AppProcess {
                    pid: process.pid,
                    name: process.name,
                    cpu_percent: cpu_percent.map(round2),
                    cpu_window,
                    rss_bytes: process.rss_bytes,
                    user: process.user,
                    uid: process.uid,
                }
            })
            .collect();

        let matched = processes.len();
        processes.sort_by(|left, right| {
            right
                .cpu_percent
                .unwrap_or(-1.0)
                .total_cmp(&left.cpu_percent.unwrap_or(-1.0))
                .then_with(|| right.rss_bytes.cmp(&left.rss_bytes))
                .then_with(|| left.pid.cmp(&right.pid))
        });
        processes.truncate(limit);
        let summary = summarize_apps(&processes);

        Ok(AppReport {
            ok: true,
            available: true,
            source: paths.proc_root.display().to_string(),
            count: processes.len(),
            matched,
            limit,
            truncated: matched > processes.len(),
            scan_truncated,
            unreadable,
            cpu_window: if use_delta.is_some() {
                "since-last-sample"
            } else {
                "since-boot"
            },
            summary,
            processes,
        })
    }

    /// Collect this process's status. `profile` and `node_id` come from the
    /// connection document so the report names the same node as registration.
    pub fn self_report(&self, profile: &str, node_id: &str) -> Result<SelfReport, HostError> {
        let paths = &self.inner.paths;
        let stat_text = read_bounded(&paths.proc_root.join("self/stat"), MAX_PROC_FILE_BYTES)
            .map_err(|error| {
                HostError::new(
                    "self.proc-unavailable",
                    format!("could not read this process's procfs stat: {error}"),
                )
            })?;
        let stat = parse_proc_stat(&stat_text).ok_or_else(|| {
            HostError::new(
                "self.proc-unavailable",
                "this process's procfs stat could not be parsed",
            )
        })?;
        let host_uptime = read_bounded(&paths.proc_root.join("uptime"), MAX_PROC_FILE_BYTES)
            .ok()
            .and_then(|text| parse_uptime(&text));
        let hz = clock_ticks();
        let process_uptime = host_uptime.and_then(|uptime| {
            let started = stat.start_ticks as f64 / hz as f64;
            (uptime >= started).then_some(uptime - started)
        });
        let memory = read_bounded(&paths.proc_root.join("self/statm"), MAX_PROC_FILE_BYTES)
            .ok()
            .and_then(|text| parse_statm(&text, page_size()));
        let service = &self.inner.service;
        let mut unavailable = Vec::new();
        if process_uptime.is_none() {
            unavailable.push("uptime");
        }
        if memory.is_none() {
            unavailable.push("memory");
        }
        if service.unit.is_none() {
            unavailable.push("unit");
        }
        if service.restart_count.is_none() {
            unavailable.push("restart_count");
        }

        Ok(SelfReport {
            ok: true,
            available: true,
            node_id: node_id.to_owned(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION"),
            profile: profile.to_owned(),
            uptime_seconds: process_uptime.map(round2),
            host_uptime_seconds: host_uptime.map(round2),
            memory,
            unit: service.unit.clone(),
            unit_source: service.unit_source,
            restart_count: service.restart_count,
            restart_count_source: service.restart_count_source,
            systemd: service.systemd,
            unavailable,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Build the guarded read-only host router.
pub fn router(state: HostSurfacesState) -> Router {
    Router::new()
        .route(STORAGE_PATH, get(storage_handler))
        .route(STORAGE_API_PATH, get(storage_handler))
        .route(APPS_PATH, get(apps_handler))
        .route(APPS_API_PATH, get(apps_handler))
        .route(SELF_PATH, get(self_handler))
        .route(SELF_API_PATH, get(self_handler))
        .with_state(state)
}

async fn storage_handler(State(state): State<HostSurfacesState>) -> Response {
    match tokio::task::spawn_blocking(move || state.storage()).await {
        Ok(Ok(report)) => json_response(report),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(HostError::new(
            "storage.task-failed",
            format!("storage collection failed: {error}"),
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
struct AppsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn apps_handler(
    State(state): State<HostSurfacesState>,
    Query(query): Query<AppsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(DEFAULT_APPS);
    match tokio::task::spawn_blocking(move || state.apps(limit)).await {
        Ok(Ok(report)) => json_response(report),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(HostError::new(
            "apps.task-failed",
            format!("process collection failed: {error}"),
        )),
    }
}

async fn self_handler(State(state): State<HostSurfacesState>) -> Response {
    let node_id = state.inner.identity.node_id.clone();
    let profile = state.inner.identity.profile.clone();
    match tokio::task::spawn_blocking(move || state.self_report(&profile, &node_id)).await {
        Ok(Ok(report)) => json_response(report),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(HostError::new(
            "self.task-failed",
            format!("self report failed: {error}"),
        )),
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

fn error_response(error: HostError) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "available": false,
            "code": error.code,
            "error": error.message,
        })),
    )
        .into_response()
}

/// Typed failure for an unreadable host source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// Stable code consumed by the controller/dashboard.
    pub code: &'static str,
    /// Operator-facing explanation; contains no secret or environment value.
    pub message: String,
}

impl HostError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One mounted block filesystem.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FilesystemUsage {
    /// Block device path from the mount table.
    pub device: String,
    /// Mount point.
    pub mount: String,
    /// Filesystem type.
    pub fs: String,
    /// Total bytes (`f_blocks * f_frsize`).
    pub total_bytes: u64,
    /// Used bytes (`total - f_bfree * f_frsize`).
    pub used_bytes: u64,
    /// Free bytes including root-reserved space (`f_bfree * f_frsize`).
    pub free_bytes: u64,
    /// `used / total * 100`, rounded to two decimals.
    pub percent_used: f64,
}

/// `GET /storage` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageReport {
    pub ok: bool,
    pub available: bool,
    pub source: String,
    pub filter: &'static str,
    /// Filesystems returned.
    pub count: usize,
    /// Block-backed mounts matched before the bound was applied.
    pub matched: usize,
    pub limit: usize,
    pub truncated: bool,
    /// Matched mounts whose `statvfs` failed (for example a stale device).
    pub skipped: usize,
    pub filesystems: Vec<FilesystemUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountRecord {
    device: String,
    mount: String,
    fs: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawUsage {
    total_bytes: u64,
    free_bytes: u64,
}

fn build_storage_report(
    text: &str,
    limit: usize,
    stat: impl Fn(&str) -> Option<RawUsage>,
) -> StorageReport {
    let mounts = parse_mounts(text);
    let matched = mounts.len();
    let mut filesystems = Vec::new();
    let mut skipped = 0usize;
    for mount in mounts {
        if filesystems.len() >= limit {
            break;
        }
        match stat(&mount.mount) {
            Some(raw) => filesystems.push(filesystem_usage(mount, raw)),
            None => skipped += 1,
        }
    }
    StorageReport {
        ok: true,
        available: true,
        source: "/proc/mounts".to_owned(),
        filter: "source starts with /dev/ and filesystem is a real block filesystem",
        count: filesystems.len(),
        matched,
        limit,
        truncated: matched > filesystems.len() + skipped,
        skipped,
        filesystems,
    }
}

fn filesystem_usage(mount: MountRecord, raw: RawUsage) -> FilesystemUsage {
    let total = raw.total_bytes;
    let free = raw.free_bytes.min(total);
    let used = total.saturating_sub(free);
    let percent_used = if total == 0 {
        0.0
    } else {
        round2(used as f64 * 100.0 / total as f64)
    };
    FilesystemUsage {
        device: sanitize_label(&mount.device),
        mount: sanitize_label(&mount.mount),
        fs: sanitize_label(&mount.fs),
        total_bytes: total,
        used_bytes: used,
        free_bytes: free,
        percent_used,
    }
}

fn parse_mounts(text: &str) -> Vec<MountRecord> {
    let mut mounts = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(device) = fields.next().map(unescape_mount) else {
            continue;
        };
        let Some(mount) = fields.next().map(unescape_mount) else {
            continue;
        };
        let Some(fs) = fields.next().map(unescape_mount) else {
            continue;
        };
        if !device.starts_with("/dev/") {
            continue;
        }
        if PSEUDO_FILESYSTEMS.contains(&fs.as_str()) {
            continue;
        }
        mounts.push(MountRecord { device, mount, fs });
    }
    mounts
}

/// Decode the octal escapes (`\040`, `\011`, `\012`, `\134`) used by procfs
/// mount tables. Other backslashes are preserved.
fn unescape_mount(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &bytes[index + 1..index + 4];
            if octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                let value = (octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0');
                out.push(value);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Replace control characters so a hostile process/mount name cannot inject
/// terminal control sequences into a dashboard.
fn sanitize_label(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

fn sanitize_process_name(value: &str) -> String {
    value
        .trim_matches(|character: char| character == '\0' || character == '\n' || character == '\r')
        .chars()
        .filter(|character| *character != '\0')
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .take(MAX_PROCESS_NAME_CHARS)
        .collect()
}

/// `statvfs` capacity for `path`, using `f_bfree` so `total = used + free`.
fn statvfs_usage(path: &str) -> Option<RawUsage> {
    let path = CString::new(path).ok()?;
    let mut buffer: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string and `buffer` is writable.
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut buffer) };
    if result != 0 {
        return None;
    }
    let block_size = buffer.f_frsize.max(1) as u64;
    let total_bytes = (buffer.f_blocks as u64).saturating_mul(block_size);
    let free_bytes = (buffer.f_bfree as u64).saturating_mul(block_size);
    Some(RawUsage {
        total_bytes,
        free_bytes: free_bytes.min(total_bytes),
    })
}

/// One process before sorting/truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SampledProcess {
    pid: u32,
    name: String,
    ticks: u64,
    rss_bytes: u64,
    uid: Option<u32>,
    user: String,
}

/// Previous per-process CPU ticks.
#[derive(Debug, Clone)]
struct AppSample {
    at: Instant,
    ticks: BTreeMap<u32, u64>,
}

/// One process in `GET /apps`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppProcess {
    pub pid: u32,
    /// Kernel `comm` value, control characters replaced and bounded to 64 chars.
    pub name: String,
    /// CPU usage in percent; `100` is one full core and multi-threaded processes
    /// can exceed it. `null` when procfs has no uptime and no previous sample.
    pub cpu_percent: Option<f64>,
    /// `since-last-sample` for a delta, `since-boot` for lifetime average.
    pub cpu_window: Option<&'static str>,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// Resolved passwd name, `uid:<n>` when passwd has no entry, or `unknown`.
    pub user: String,
    /// Numeric uid when `/proc/<pid>/status` was readable.
    pub uid: Option<u32>,
}

/// Summary counts for the returned process window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AppSummary {
    pub processes: usize,
    pub users: usize,
    pub total_rss_bytes: u64,
}

/// `GET /apps` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppReport {
    pub ok: bool,
    pub available: bool,
    pub source: String,
    /// Processes returned after the limit.
    pub count: usize,
    /// Numeric proc entries that produced a sample.
    pub matched: usize,
    pub limit: usize,
    pub truncated: bool,
    /// The scan itself hit [`MAX_PID_SCAN`].
    pub scan_truncated: bool,
    /// Proc entries that disappeared or could not be read during the scan.
    pub unreadable: usize,
    pub cpu_window: &'static str,
    pub summary: AppSummary,
    pub processes: Vec<AppProcess>,
}

fn summarize_apps(processes: &[AppProcess]) -> AppSummary {
    let mut users = BTreeMap::new();
    let mut total_rss_bytes = 0u64;
    for process in processes {
        users.insert(process.user.clone(), ());
        total_rss_bytes = total_rss_bytes.saturating_add(process.rss_bytes);
    }
    AppSummary {
        processes: processes.len(),
        users: users.len(),
        total_rss_bytes,
    }
}

/// `GET /self` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SelfReport {
    pub ok: bool,
    pub available: bool,
    pub node_id: String,
    /// This process's pid.
    pub pid: u32,
    /// `CARGO_PKG_VERSION` of the binary.
    pub version: &'static str,
    pub profile: String,
    /// Process uptime derived from procfs; `null` when it cannot be measured.
    pub uptime_seconds: Option<f64>,
    /// Host uptime from `/proc/uptime`; `null` when unreadable.
    pub host_uptime_seconds: Option<f64>,
    /// This process's `statm` memory, when readable.
    pub memory: Option<SelfMemory>,
    /// Systemd unit or supervisor-provided unit name.
    pub unit: Option<String>,
    /// `env:OZMA_UNIT` or `cgroup`.
    pub unit_source: Option<&'static str>,
    /// `OZMA_RESTART_COUNT` only; `null` when the supervisor did not export it.
    pub restart_count: Option<u64>,
    pub restart_count_source: Option<&'static str>,
    /// Whether systemd environment/cgroup markers were visible.
    pub systemd: bool,
    /// Report fields that could not be read.
    pub unavailable: Vec<&'static str>,
}

/// This process's memory from `/proc/self/statm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SelfMemory {
    pub virtual_bytes: u64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedStat {
    name: String,
    state: char,
    cpu_ticks: u64,
    start_ticks: u64,
    rss_pages: u64,
}

/// Parse `/proc/<pid>/stat` after `comm`, so names containing spaces or `)` do
/// not shift the numeric fields.
fn parse_proc_stat(text: &str) -> Option<ParsedStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let name = text[open + 1..close].to_owned();
    let rest = text[close + 1..].trim_start();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] = state; 11/12 = utime/stime; 19 = starttime; 21 = rss.
    if fields.len() < 22 {
        return None;
    }
    let state = fields[0].chars().next()?;
    let utime = fields[11].parse::<u64>().ok()?;
    let stime = fields[12].parse::<u64>().ok()?;
    let start_ticks = fields[19].parse::<u64>().ok()?;
    let rss_pages = fields[21].parse::<u64>().ok()?;
    Some(ParsedStat {
        name,
        state,
        cpu_ticks: utime.saturating_add(stime),
        start_ticks,
        rss_pages,
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

fn parse_passwd(text: &str) -> BTreeMap<u32, String> {
    let mut users = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_PASSWD_ENTRIES {
            break;
        }
        let mut fields = line.split(':');
        let Some(name) = fields.next() else {
            continue;
        };
        // Skip password field.
        if fields.next().is_none() {
            continue;
        }
        let Some(uid) = fields.next().and_then(|uid| uid.parse::<u32>().ok()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        users.insert(uid, sanitize_label(name));
    }
    users
}

fn parse_uptime(text: &str) -> Option<f64> {
    let value = text.split_whitespace().next()?.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn parse_statm(text: &str, page_size: u64) -> Option<SelfMemory> {
    let mut fields = text.split_whitespace();
    let virtual_pages = fields.next()?.parse::<u64>().ok()?;
    let rss_pages = fields.next()?.parse::<u64>().ok()?;
    Some(SelfMemory {
        virtual_bytes: virtual_pages.saturating_mul(page_size),
        rss_bytes: rss_pages.saturating_mul(page_size),
    })
}

fn cpu_percent(ticks: u64, previous: Option<u64>, elapsed_seconds: f64, hz: u64) -> Option<f64> {
    if elapsed_seconds <= 0.0 || hz == 0 {
        return None;
    }
    let delta = match previous {
        Some(previous) => ticks.checked_sub(previous)?,
        None => ticks,
    };
    let seconds = delta as f64 / hz as f64;
    Some(seconds / elapsed_seconds * 100.0)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn page_size() -> u64 {
    // SAFETY: sysconf is thread-safe and takes no pointers.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value > 0 {
        value as u64
    } else {
        4096
    }
}

fn clock_ticks() -> u64 {
    // SAFETY: sysconf is thread-safe and takes no pointers.
    let value = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if value > 0 {
        value as u64
    } else {
        100
    }
}

/// Find a systemd unit in a `/proc/self/cgroup` path.
fn unit_from_cgroup(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some((_, path)) = line.rsplit_once(':') else {
            continue;
        };
        let name = path.rsplit('/').next()?.trim();
        if name.ends_with(".service") || name.ends_with(".scope") {
            return Some(sanitize_label(name));
        }
    }
    None
}

fn read_bounded(path: &Path, limit: u64) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = file.take(limit);
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    String::from_utf8(buffer)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write fixture");
    }

    fn fixture_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ozma-host-surfaces-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture root");
        root
    }

    fn add_process(root: &Path, pid: u32, name: &str, ticks: u64, rss_pages: u64, uid: u32) {
        let dir = root.join("proc").join(pid.to_string());
        // `comm` may contain spaces; `stat` must parse after the last `)`.
        write(&dir.join("comm"), &format!("{name}\n"));
        let stat = format!(
            "{pid} ({name}) S 1 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 100 0 {rss_pages} 0\n"
        );
        write(&dir.join("stat"), &stat);
        write(
            &dir.join("status"),
            &format!("Name:\t{name}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        );
    }

    #[test]
    fn mounts_are_filtered_to_real_block_filesystems() {
        let text = "\
/dev/sda1 / ext4 rw,relatime 0 0
proc /proc proc rw 0 0
tmpfs /run tmpfs rw 0 0
overlay /var/lib/docker/overlay2/x overlay rw 0 0
/dev/sdb1 /mnt/data\\040disk xfs rw 0 0
/dev/disk/by-uuid/abc /boot ext4 rw 0 0
nas:/export /mnt/nas nfs4 rw 0 0
";
        let mounts = parse_mounts(text);
        let devices: Vec<&str> = mounts.iter().map(|mount| mount.device.as_str()).collect();
        assert_eq!(
            devices,
            vec!["/dev/sda1", "/dev/sdb1", "/dev/disk/by-uuid/abc"]
        );
        assert_eq!(mounts[1].mount, "/mnt/data disk");
        assert_eq!(mounts[1].fs, "xfs");
    }

    #[test]
    fn storage_report_is_bounded_and_reports_skips_and_truncation() {
        let text = (0..MAX_STORAGE_FILESYSTEMS + 3)
            .map(|index| format!("/dev/sd{index} /mnt/{index} ext4 rw 0 0\n"))
            .collect::<String>();
        let report = build_storage_report(&text, MAX_STORAGE_FILESYSTEMS, |path| {
            if path.ends_with("/1") {
                None
            } else {
                Some(RawUsage {
                    total_bytes: 1000,
                    free_bytes: 250,
                })
            }
        });
        assert_eq!(report.matched, MAX_STORAGE_FILESYSTEMS + 3);
        assert_eq!(report.count, MAX_STORAGE_FILESYSTEMS);
        assert_eq!(report.skipped, 1);
        assert!(report.truncated);
        assert_eq!(report.filesystems[0].total_bytes, 1000);
        assert_eq!(report.filesystems[0].used_bytes, 750);
        assert_eq!(report.filesystems[0].free_bytes, 250);
        assert_eq!(report.filesystems[0].percent_used, 75.0);
    }

    #[test]
    fn storage_empty_source_is_a_typed_success_with_no_entries() {
        let report = build_storage_report("", MAX_STORAGE_FILESYSTEMS, |_| None);
        assert!(report.ok);
        assert!(report.available);
        assert_eq!(report.count, 0);
        assert_eq!(report.matched, 0);
        assert!(report.filesystems.is_empty());
        assert!(!report.truncated);
    }

    #[test]
    fn storage_unreadable_proc_is_a_typed_error() {
        let root = fixture_root("storage-error");
        let state = HostSurfacesState::with_test_paths(&root);
        let error = state.storage().expect_err("missing mounts must be typed");
        assert_eq!(error.code, "storage.proc-unavailable");
        assert!(error.message.contains("mount table"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn statvfs_returns_zero_for_zero_capacity() {
        let usage = filesystem_usage(
            MountRecord {
                device: "/dev/zero".to_owned(),
                mount: "/zero".to_owned(),
                fs: "ext4".to_owned(),
            },
            RawUsage {
                total_bytes: 0,
                free_bytes: 0,
            },
        );
        assert_eq!(usage.percent_used, 0.0);
        assert_eq!(usage.total_bytes, 0);
    }

    #[test]
    fn app_report_parses_names_users_rss_and_cpu_and_never_reads_cmdline() {
        let root = fixture_root("apps");
        write(&root.join("proc/uptime"), "100.00 200.00\n");
        write(
            &root.join("passwd"),
            "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\n",
        );
        add_process(&root.join(""), 10, "alpha", 100, 2, 1000);
        add_process(&root.join(""), 20, "beta with ) space", 200, 4, 4242);
        // A cmdline file exists but must never appear in the response.
        write(
            &root.join("proc/10/cmdline"),
            "/usr/bin/alpha\0--secret-token\0",
        );
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state.apps(10).expect("apps report");
        assert_eq!(report.matched, 2);
        assert_eq!(report.count, 2);
        assert!(!report.truncated);
        assert_eq!(report.cpu_window, "since-boot");
        assert_eq!(report.summary.processes, 2);
        assert_eq!(report.summary.users, 2);
        assert_eq!(report.summary.total_rss_bytes, (2 + 4) * page_size());
        let by_pid: BTreeMap<u32, &AppProcess> = report
            .processes
            .iter()
            .map(|process| (process.pid, process))
            .collect();
        assert_eq!(by_pid[&10].name, "alpha");
        assert_eq!(by_pid[&10].user, "alice");
        assert_eq!(by_pid[&10].uid, Some(1000));
        assert_eq!(by_pid[&10].rss_bytes, 2 * page_size());
        assert_eq!(by_pid[&10].cpu_percent, Some(1.0));
        assert_eq!(by_pid[&20].name, "beta with ) space");
        assert_eq!(by_pid[&20].user, "uid:4242");
        assert_eq!(by_pid[&20].cpu_percent, Some(2.0));
        assert_eq!(report.processes[0].pid, 20);

        let json = serde_json::to_string(&report).expect("serialize");
        assert!(!json.contains("cmdline"));
        assert!(!json.contains("secret-token"));
        assert!(!json.contains("/usr/bin"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_limit_is_clamped_and_truncation_is_reported() {
        let root = fixture_root("apps-limit");
        write(&root.join("proc/uptime"), "100.00 0.00\n");
        write(&root.join("passwd"), "root:x:0:0::/root:/bin/sh\n");
        for pid in 1..=5 {
            add_process(&root.join(""), pid, "p", pid as u64, 1, 0);
        }
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state.apps(0).expect("apps report");
        assert_eq!(report.limit, 1);
        assert_eq!(report.count, 1);
        assert_eq!(report.matched, 5);
        assert!(report.truncated);

        let report = state.apps(usize::MAX).expect("apps report");
        assert_eq!(report.limit, MAX_APPS);
        assert_eq!(report.count, 5);
        assert!(!report.truncated);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_report_sorts_by_cpu_then_rss_then_pid() {
        let root = fixture_root("apps-sort");
        write(&root.join("proc/uptime"), "10.00 0.00\n");
        write(&root.join("passwd"), "root:x:0:0::/root:/bin/sh\n");
        add_process(&root.join(""), 1, "low", 1, 1, 0);
        add_process(&root.join(""), 2, "high-rss", 1, 9, 0);
        add_process(&root.join(""), 3, "p3", 5, 1, 0);
        add_process(&root.join(""), 4, "p4", 5, 2, 0);
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state.apps(10).expect("apps report");
        let pids: Vec<u32> = report.processes.iter().map(|process| process.pid).collect();
        assert_eq!(pids, vec![4, 3, 2, 1]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_report_uses_a_delta_and_labels_the_window() {
        let root = fixture_root("apps-delta");
        write(&root.join("proc/uptime"), "100.00 0.00\n");
        write(&root.join("passwd"), "root:x:0:0::/root:/bin/sh\n");
        add_process(&root.join(""), 7, "worker", 0, 1, 0);
        let state = HostSurfacesState::with_test_paths(&root);
        // Prime the sampler, then advance ticks past the minimum interval. Test
        // clock resolution is coarse; a bounded sleep is acceptable here.
        state.apps(10).expect("first sample");
        std::thread::sleep(std::time::Duration::from_millis(MIN_CPU_INTERVAL_MS + 20));
        add_process(&root.join(""), 7, "worker", 50, 1, 0);
        let report = state.apps(10).expect("second sample");
        assert_eq!(report.cpu_window, "since-last-sample");
        assert_eq!(report.processes[0].cpu_window, Some("since-last-sample"));
        assert!(report.processes[0].cpu_percent.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_scan_skips_unreadable_pids_and_reports_the_count() {
        let root = fixture_root("apps-unreadable");
        write(&root.join("proc/uptime"), "1.00 0.00\n");
        write(&root.join("passwd"), "root:x:0:0::/root:/bin/sh\n");
        write(&root.join("proc/not-a-pid/stat"), "ignored");
        add_process(&root.join(""), 9, "real", 1, 1, 0);
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state.apps(10).expect("apps report");
        assert_eq!(report.count, 1);
        assert_eq!(report.unreadable, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_unreadable_proc_is_a_typed_error() {
        let root = fixture_root("apps-error");
        let state = HostSurfacesState::with_test_paths(&root);
        let error = state.apps(10).expect_err("missing procfs must be typed");
        assert_eq!(error.code, "apps.proc-unavailable");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn self_report_reads_memory_uptime_and_cgroup_unit() {
        let root = fixture_root("self");
        write(&root.join("proc/uptime"), "1000.00 2000.00\n");
        write(
            &root.join("proc/self/cgroup"),
            "0::/system.slice/ozma-node.service\n",
        );
        let pages = 3u64;
        let rss = 2u64;
        write(
            &root.join("proc/self/statm"),
            &format!("{pages} {rss} 0 0 0 0 0\n"),
        );
        let pid = 4242u32;
        let hz = clock_ticks();
        // Started 100 seconds after boot.
        let start_ticks = 100 * hz;
        write(
            &root.join("proc/self/stat"),
            &format!(
                "{pid} (ozma-node) S 1 1 1 0 -1 0 0 0 0 0 1 2 0 0 20 0 1 0 {start_ticks} 0 0\n"
            ),
        );
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state
            .self_report("hardware", "node-1")
            .expect("self report");
        assert_eq!(report.pid, std::process::id());
        assert_eq!(report.node_id, "node-1");
        assert_eq!(report.profile, "hardware");
        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(report.host_uptime_seconds, Some(1000.0));
        assert_eq!(report.uptime_seconds, Some(900.0));
        assert_eq!(
            report.memory,
            Some(SelfMemory {
                virtual_bytes: pages * page_size(),
                rss_bytes: rss * page_size(),
            })
        );
        assert_eq!(report.unit.as_deref(), Some("ozma-node.service"));
        assert_eq!(report.unit_source, Some("cgroup"));
        assert!(report.systemd);
        assert_eq!(report.restart_count, None);
        assert!(report.unavailable.contains(&"restart_count"));

        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"pid\""));
        assert!(!json.contains("INVOCATION_ID"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn self_report_unreadable_stat_is_a_typed_error() {
        let root = fixture_root("self-error");
        write(&root.join("proc/uptime"), "1.00 0.00\n");
        let state = HostSurfacesState::with_test_paths(&root);
        let error = state.self_report("vm", "n").expect_err("missing stat");
        assert_eq!(error.code, "self.proc-unavailable");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cgroup_unit_parser_only_accepts_service_and_scope_names() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/ozma-node.service\n"),
            Some("ozma-node.service".to_owned())
        );
        assert_eq!(
            unit_from_cgroup("1:name=systemd:/user.slice/user-1000.slice/session-1.scope\n"),
            Some("session-1.scope".to_owned())
        );
        assert_eq!(unit_from_cgroup("0::/user.slice/user-1000.slice\n"), None);
        assert_eq!(unit_from_cgroup("garbage"), None);
    }

    #[test]
    fn passwd_parser_handles_missing_and_malformed_entries() {
        let users = parse_passwd(
            "root:x:0:0::/root:/bin/sh\n\
             malformed\n\
             nopass:1000\n\
             alice:x:1000:1000::/home/alice:/bin/sh\n\
             :x:1001:1001::/:/bin/sh\n",
        );
        assert_eq!(users.get(&0).map(String::as_str), Some("root"));
        assert_eq!(users.get(&1000).map(String::as_str), Some("alice"));
        assert!(!users.contains_key(&1001));
    }

    #[test]
    fn proc_stat_parser_uses_the_last_close_paren() {
        let stat = "42 (a ) b) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22\n";
        let parsed = parse_proc_stat(stat).expect("parse");
        assert_eq!(parsed.name, "a ) b");
        assert_eq!(parsed.state, 'R');
        assert_eq!(parsed.cpu_ticks, 23); // 11 + 12
        assert_eq!(parsed.start_ticks, 19);
        assert_eq!(parsed.rss_pages, 21);
    }

    #[test]
    fn mount_unescape_handles_octal_and_preserves_other_backslashes() {
        assert_eq!(unescape_mount("/mnt/a\\040b"), "/mnt/a b");
        assert_eq!(unescape_mount("/mnt/a\\011b"), "/mnt/a\tb");
        assert_eq!(unescape_mount("/mnt/back\\slash"), "/mnt/back\\slash");
    }

    #[test]
    fn control_characters_are_sanitized_in_labels_and_names() {
        assert_eq!(sanitize_label("ok\u{1b}[31m"), "ok?[31m");
        assert_eq!(sanitize_process_name("bad\u{1b}name\n"), "bad?name");
        assert_eq!(
            sanitize_process_name(&"x".repeat(100)).len(),
            MAX_PROCESS_NAME_CHARS
        );
    }

    #[test]
    fn advertisement_reports_availability_without_scanning() {
        let state = HostSurfacesState::host();
        let advertised = state.advertisement();
        assert!(advertised.storage.supported);
        assert_eq!(advertised.storage.path, STORAGE_PATH);
        assert_eq!(advertised.storage.limit, Some(MAX_STORAGE_FILESYSTEMS));
        assert!(advertised.storage.guarded);
        assert!(advertised.apps.supported);
        assert_eq!(advertised.apps.limit, Some(MAX_APPS));
        assert!(advertised.self_management.supported);
        assert_eq!(advertised.self_management.path, SELF_PATH);
        // The test host is Linux; procfs should be readable in CI.
        assert!(advertised.self_management.available);
    }

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn routes_return_bounded_json_and_typed_errors() {
        let root = fixture_root("routes");
        write(&root.join("proc/mounts"), "/dev/sda1 / ext4 rw 0 0\n");
        write(&root.join("proc/uptime"), "100.00 0.00\n");
        write(
            &root.join("passwd"),
            "alice:x:1000:1000::/home/alice:/bin/sh\n",
        );
        add_process(&root.join(""), 10, "alpha", 100, 1, 1000);
        add_process(&root.join(""), 20, "beta", 200, 2, 1000);
        write(
            &root.join("proc/10/cmdline"),
            "/usr/bin/alpha\0--password=hunter2\0",
        );
        let app = router(HostSurfacesState::with_test_paths(&root));

        let (status, storage) = get_json(&app, STORAGE_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(storage["ok"], true);
        assert_eq!(storage["count"], 1);
        assert_eq!(storage["filesystems"][0]["device"], "/dev/sda1");
        assert!(storage["filesystems"][0].get("mount").is_some());

        let (status, apps) = get_json(&app, "/apps?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(apps["count"], 1);
        assert_eq!(apps["limit"], 1);
        assert_eq!(apps["matched"], 2);
        assert_eq!(apps["truncated"], true);
        assert!(apps.get("processes").is_some());
        let body = apps.to_string();
        assert!(!body.contains("cmdline"));
        assert!(!body.contains("hunter2"));
        assert!(!body.contains("/usr/bin/alpha"));

        let missing = fixture_root("routes-missing");
        let missing_app = router(HostSurfacesState::with_test_paths(&missing));
        let (status, error) = get_json(&missing_app, STORAGE_PATH).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error["ok"], false);
        assert_eq!(error["code"], "storage.proc-unavailable");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(missing);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_host_storage_apps_and_self_are_bounded_and_private() {
        let state = HostSurfacesState::host_with_identity("live-node", "hardware");
        let storage = state.storage().expect("live storage");
        assert!(storage.count <= MAX_STORAGE_FILESYSTEMS);
        for filesystem in &storage.filesystems {
            assert!(filesystem.percent_used <= 100.0);
            assert!(filesystem.used_bytes <= filesystem.total_bytes);
        }

        let apps = state.apps(MAX_APPS).expect("live apps");
        assert!(apps.count <= MAX_APPS);
        assert_eq!(apps.limit, MAX_APPS);
        let json = serde_json::to_string(&apps).expect("serialize apps");
        assert!(!json.contains("cmdline"));

        let report = state
            .self_report("hardware", "live-node")
            .expect("live self");
        assert_eq!(report.pid, std::process::id());
        assert_eq!(report.node_id, "live-node");
        assert_eq!(report.profile, "hardware");
        assert!(report.memory.is_some());
    }

    #[test]
    fn router_builds() {
        let _ = router(HostSurfacesState::host());
    }
}
