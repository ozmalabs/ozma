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
//! Opt-in exact-pid process control is a separate service in
//! [`crate::app_control`]; it never changes the `/apps` document.
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
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Controller-facing storage surface.
pub const STORAGE_PATH: &str = "/storage";
/// Versioned alias for the storage surface.
pub const STORAGE_API_PATH: &str = "/api/v1/storage";
/// Per-device SMART and mdraid health.
pub const STORAGE_HEALTH_PATH: &str = "/storage/health";
/// Versioned alias for the storage-health surface.
pub const STORAGE_HEALTH_API_PATH: &str = "/api/v1/storage/health";
/// One explicitly named block device's normalized SMART health.
pub const STORAGE_SMART_PATH: &str = "/storage/smart";
/// Versioned alias for the per-device SMART surface.
pub const STORAGE_SMART_API_PATH: &str = "/api/v1/storage/smart";
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

/// Largest number of block devices reported by `/storage/health`.
pub const MAX_BLOCK_DEVICES: usize = 32;
/// Largest number of md arrays reported by `/storage/health`.
pub const MAX_MD_ARRAYS: usize = 32;
/// Largest number of member devices reported per md array.
pub const MAX_MD_MEMBERS: usize = 64;
/// Largest number of normalized SMART attributes returned when explicitly
/// requested with `?attributes=true`.
pub const MAX_SMART_ATTRIBUTES: usize = 64;
/// Longest accepted `/dev/<name>` query value.
const MAX_DEVICE_PATH_CHARS: usize = 128;
/// Longest accepted block-device basename.
const MAX_DEVICE_NAME_CHARS: usize = 64;
/// Longest model string emitted.
const MAX_MODEL_CHARS: usize = 64;
/// Longest serial string emitted, and only with explicit `?serials=true`.
const MAX_SERIAL_CHARS: usize = 64;
/// Maximum read from one small sysfs fact file.
const MAX_SYSFS_BYTES: u64 = 4096;
/// Maximum `/proc/mdstat` read.
const MAX_MDSTAT_BYTES: u64 = 256 * 1024;
/// Maximum `mdadm --detail --scan` read.
const MAX_MDADM_BYTES: usize = 64 * 1024;
/// Maximum `smartctl` JSON read.
const MAX_SMART_BYTES: usize = 1024 * 1024;
/// Maximum `smartctl` stderr retained for a bounded error message.
const MAX_SMART_STDERR_BYTES: usize = 2048;
/// `smartctl -j -H -A` deadline per device.
const SMART_TIMEOUT: Duration = Duration::from_secs(5);
/// `mdadm --detail --scan` deadline.
const MDADM_TIMEOUT: Duration = Duration::from_secs(3);
/// Kernel block devices that are RAM/loop pseudo-disks, not storage health
/// candidates.
const PSEUDO_BLOCK_PREFIXES: &[&str] = &["loop", "ram", "zram"];

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
    /// `/sys/block` or a fixture equivalent.
    sys_block: PathBuf,
    /// `/dev` or a fixture equivalent. Smartctl is always invoked on a path
    /// rebuilt from a validated basename under this directory.
    dev_root: PathBuf,
    /// `/proc/mdstat` or a fixture equivalent.
    mdstat: PathBuf,
    /// `smartctl` binary (bare name, looked up on PATH, or absolute).
    smartctl: PathBuf,
    /// `mdadm` binary (bare name, looked up on PATH, or absolute).
    mdadm: PathBuf,
}

impl HostPaths {
    fn host() -> Self {
        let proc_root = PathBuf::from("/proc");
        Self {
            mounts: proc_root.join("mounts"),
            passwd: PathBuf::from("/etc/passwd"),
            cgroup: proc_root.join("self/cgroup"),
            proc_root,
            sys_block: PathBuf::from("/sys/block"),
            dev_root: PathBuf::from("/dev"),
            mdstat: PathBuf::from("/proc/mdstat"),
            smartctl: PathBuf::from("smartctl"),
            mdadm: PathBuf::from("mdadm"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Availability {
    storage: bool,
    storage_health: bool,
    smart: bool,
    md: bool,
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
    /// Per-device SMART and mdraid health.
    pub health: HostSurfaceCapability,
    /// One explicitly named block device's normalized SMART report.
    pub smart: HostSurfaceCapability,
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
            storage_health: paths.sys_block.is_dir(),
            smart: paths.sys_block.is_dir() && resolve_program(&paths.smartctl).is_some(),
            md: File::open(&paths.mdstat).is_ok(),
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
            mdstat: proc_root.join("mdstat"),
            proc_root,
            sys_block: root.join("sys/block"),
            dev_root: root.join("dev"),
            smartctl: root.join("bin/smartctl"),
            mdadm: root.join("bin/mdadm"),
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
            health: HostSurfaceCapability {
                supported: true,
                path: STORAGE_HEALTH_PATH,
                available: availability.storage_health,
                guarded: true,
                reason: if availability.storage_health {
                    "Reports bounded /sys/block inventory, normalized SMART health, and /proc/mdstat arrays; read-only, serials opt-in, at most 32 devices.".to_owned()
                } else {
                    "/sys/block could not be read at startup; /storage/health returns a typed 503."
                        .to_owned()
                },
                limit: Some(MAX_BLOCK_DEVICES),
            },
            smart: HostSurfaceCapability {
                supported: true,
                path: STORAGE_SMART_PATH,
                available: availability.smart,
                guarded: true,
                reason: if availability.smart {
                    "Runs bounded smartctl -j -H -A for one validated /dev block device; no tests, no raw output, serials opt-in.".to_owned()
                } else {
                    "smartctl or /sys/block was unavailable at startup; /storage/smart returns a typed 503."
                        .to_owned()
                },
                limit: Some(1),
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

    /// Collect bounded block-device and mdraid health.
    ///
    /// SMART and mdraid are optional enrichment: when `smartctl` is absent or
    /// the kernel has no md support the report stays a `200` and names the
    /// unavailable section with a stable `storage.smart-unavailable` /
    /// `storage.md-unavailable` code. An unreadable `/sys/block` is a typed
    /// `503` because it is the required inventory source.
    pub async fn storage_health(
        &self,
        include_serials: bool,
        include_attributes: bool,
    ) -> Result<StorageHealthReport, HostError> {
        let paths = &self.inner.paths;
        let names = read_block_device_names(&paths.sys_block).map_err(|error| {
            HostError::new(
                "storage.block-unavailable",
                format!("could not read /sys/block: {error}"),
            )
        })?;
        let smart_tool = resolve_program(&paths.smartctl);
        let smart_tool_present = smart_tool.is_some();

        let mut devices = Vec::with_capacity(names.len().min(MAX_BLOCK_DEVICES));
        for name in names.iter().take(MAX_BLOCK_DEVICES) {
            let device_path = paths.dev_root.join(name);
            let smart = if smart_tool_present {
                if device_path.exists() {
                    match run_smartctl(
                        smart_tool.as_deref().unwrap_or(&paths.smartctl),
                        &device_path,
                    )
                    .await
                    {
                        Ok(document) => {
                            parse_smart_document(&document, include_serials, include_attributes)
                        }
                        Err(reason) => SmartHealth::unavailable(reason),
                    }
                } else {
                    SmartHealth::unavailable(format!("no /dev node exists for block device {name}"))
                }
            } else {
                SmartHealth::unavailable(
                    "smartctl was not found on PATH; install smartmontools to read SMART health",
                )
            };
            let model = read_sysfs_text(&paths.sys_block.join(name).join("device/model"))
                .map(|value| bounded_label(&value, MAX_MODEL_CHARS))
                .filter(|value| !value.is_empty())
                .or_else(|| smart.model.clone());
            let serial = include_serials
                .then(|| {
                    read_sysfs_text(&paths.sys_block.join(name).join("device/serial"))
                        .map(|value| bounded_label(&value, MAX_SERIAL_CHARS))
                        .filter(|value| !value.is_empty())
                        .or_else(|| smart.serial.clone())
                })
                .flatten();
            devices.push(BlockDeviceHealth {
                name: name.clone(),
                device: format!("/dev/{name}"),
                model,
                serial,
                size_bytes: read_sysfs_u64(&paths.sys_block.join(name).join("size"))
                    .map(|sectors| sectors.saturating_mul(512)),
                rotational: read_sysfs_u64(&paths.sys_block.join(name).join("queue/rotational"))
                    .map(|value| value != 0),
                smart,
            });
        }

        let md = self.md_status().await;
        let smart_available = devices.iter().any(|device| device.smart.available);
        let smart_error = if smart_available {
            None
        } else if smart_tool_present {
            Some(
                "smartctl was found but returned no usable SMART health for any block device"
                    .to_owned(),
            )
        } else {
            Some(
                "smartctl was not found on PATH; install smartmontools to read SMART health"
                    .to_owned(),
            )
        };
        let mut unavailable = Vec::new();
        if !smart_available {
            unavailable.push("smart");
        }
        if !md.available {
            unavailable.push("md");
        }
        Ok(StorageHealthReport {
            ok: true,
            available: true,
            source: "/sys/block + smartctl + /proc/mdstat",
            count: devices.len(),
            matched: names.len(),
            limit: MAX_BLOCK_DEVICES,
            truncated: names.len() > devices.len(),
            serials_included: include_serials,
            attributes_included: include_attributes,
            smart: SourceStatus {
                available: smart_available,
                code: (!smart_available).then_some("storage.smart-unavailable"),
                error: smart_error,
            },
            md,
            unavailable,
            devices,
        })
    }

    /// Read normalized SMART health for one explicitly named block device.
    pub async fn smart(
        &self,
        device: &str,
        include_serials: bool,
        include_attributes: bool,
    ) -> Result<SmartReport, HostError> {
        let paths = &self.inner.paths;
        let name = validate_block_device(device, &paths.sys_block, &paths.dev_root)?;
        let device_path = paths.dev_root.join(&name);
        let tool = resolve_program(&paths.smartctl).ok_or_else(|| {
            HostError::new(
                "storage.smart-unavailable",
                "smartctl was not found on PATH; install smartmontools to read SMART health",
            )
        })?;
        let document = run_smartctl(&tool, &device_path)
            .await
            .map_err(|reason| HostError::new("storage.smart-unavailable", reason))?;
        let smart = parse_smart_document(&document, include_serials, include_attributes);
        if !smart.available {
            return Err(HostError::new(
                "storage.smart-unavailable",
                smart.error.clone().unwrap_or_else(|| {
                    "smartctl returned no SMART health for this device".to_owned()
                }),
            ));
        }
        let model = read_sysfs_text(&paths.sys_block.join(&name).join("device/model"))
            .map(|value| bounded_label(&value, MAX_MODEL_CHARS))
            .filter(|value| !value.is_empty())
            .or_else(|| smart.model.clone());
        let serial = include_serials
            .then(|| {
                read_sysfs_text(&paths.sys_block.join(&name).join("device/serial"))
                    .map(|value| bounded_label(&value, MAX_SERIAL_CHARS))
                    .filter(|value| !value.is_empty())
                    .or_else(|| smart.serial.clone())
            })
            .flatten();
        Ok(SmartReport {
            ok: true,
            available: true,
            source: "/sys/block + smartctl",
            device: format!("/dev/{name}"),
            name: name.clone(),
            model,
            serial,
            serials_included: include_serials,
            attributes_included: include_attributes,
            size_bytes: read_sysfs_u64(&paths.sys_block.join(&name).join("size"))
                .map(|sectors| sectors.saturating_mul(512)),
            rotational: read_sysfs_u64(&paths.sys_block.join(&name).join("queue/rotational"))
                .map(|value| value != 0),
            protocol: smart.protocol.clone(),
            smart,
        })
    }

    /// Read mdraid arrays from `/proc/mdstat`, optionally enriched by
    /// `mdadm --detail --scan`.
    async fn md_status(&self) -> MdStatus {
        let paths = &self.inner.paths;
        let mdadm_present = resolve_program(&paths.mdadm).is_some();
        let text = match read_bounded(&paths.mdstat, MAX_MDSTAT_BYTES) {
            Ok(text) => text,
            Err(error) => {
                return MdStatus {
                    available: false,
                    code: Some("storage.md-unavailable"),
                    error: Some(bounded_error(&format!(
                        "could not read /proc/mdstat: {error}"
                    ))),
                    source: "/proc/mdstat",
                    mdadm_available: mdadm_present,
                    mdadm_error: None,
                    count: 0,
                    arrays: Vec::new(),
                };
            }
        };
        let mut arrays = parse_mdstat(&text);
        let (mdadm_available, mdadm_error, scan) = match resolve_program(&paths.mdadm) {
            Some(program) => match run_mdadm_scan(&program).await {
                Ok(scan) => (true, None, scan),
                Err(reason) => (false, Some(reason), Vec::new()),
            },
            None => (
                false,
                Some("mdadm was not found on PATH".to_owned()),
                Vec::new(),
            ),
        };
        for array in &mut arrays {
            if let Some(scanned) = scan.iter().find(|scanned| scanned.device == array.device) {
                array.uuid = scanned.uuid.clone();
                array.metadata = scanned.metadata.clone();
            }
        }
        arrays.truncate(MAX_MD_ARRAYS);
        MdStatus {
            available: true,
            code: None,
            error: None,
            source: "/proc/mdstat",
            mdadm_available,
            mdadm_error,
            count: arrays.len(),
            arrays,
        }
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
        .route(STORAGE_HEALTH_PATH, get(storage_health_handler))
        .route(STORAGE_HEALTH_API_PATH, get(storage_health_handler))
        .route(STORAGE_SMART_PATH, get(storage_smart_handler))
        .route(STORAGE_SMART_API_PATH, get(storage_smart_handler))
        .route(APPS_PATH, get(apps_handler))
        .route(APPS_API_PATH, get(apps_handler))
        .route(SELF_PATH, get(self_handler))
        .route(SELF_API_PATH, get(self_handler))
        .with_state(state)
}

#[derive(Debug, Default, Deserialize)]
struct StorageHealthQuery {
    #[serde(default)]
    serials: Option<bool>,
    #[serde(default)]
    attributes: Option<bool>,
}

async fn storage_health_handler(
    State(state): State<HostSurfacesState>,
    Query(query): Query<StorageHealthQuery>,
) -> Response {
    let serials = query.serials.unwrap_or(false);
    let attributes = query.attributes.unwrap_or(false);
    match state.storage_health(serials, attributes).await {
        Ok(report) => json_response(report),
        Err(error) => error_response(error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct StorageSmartQuery {
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    serials: Option<bool>,
    #[serde(default)]
    attributes: Option<bool>,
}

async fn storage_smart_handler(
    State(state): State<HostSurfacesState>,
    Query(query): Query<StorageSmartQuery>,
) -> Response {
    let Some(device) = query.device.as_deref().filter(|value| !value.is_empty()) else {
        return error_response(HostError::invalid(
            "storage.invalid-device",
            "the device query parameter is required and must be /dev/<block-device>",
        ));
    };
    match state
        .smart(
            device,
            query.serials.unwrap_or(false),
            query.attributes.unwrap_or(false),
        )
        .await
    {
        Ok(report) => json_response(report),
        Err(error) => error_response(error),
    }
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
        error.status,
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
    /// HTTP status: `503` for an unavailable source, `400` for invalid input.
    pub status: StatusCode,
}

impl HostError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            status: StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
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

// ---------------------------------------------------------------------------
// Storage health: block inventory, SMART, mdraid
// ---------------------------------------------------------------------------

/// Normalized SMART health for one device.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SmartHealth {
    /// Smartctl returned usable health fields.
    pub available: bool,
    /// Stable code when `available` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    /// Bounded explanation when `available` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `smart_status.passed` when the device reports overall health.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    /// Current temperature in degrees Celsius.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<i64>,
    /// NVMe percentage of endurance consumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage_used: Option<u64>,
    /// NVMe available spare percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_spare_percent: Option<u64>,
    /// NVMe media/integrity errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_errors: Option<u64>,
    /// NVMe critical-warning bit field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_warning: Option<u64>,
    /// ATA attribute 5, reallocated sector count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reallocated_sectors: Option<u64>,
    /// ATA attribute 197, current pending sector count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_sectors: Option<u64>,
    /// ATA attribute 198, offline uncorrectable sector count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncorrectable_sectors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_on_hours: Option<u64>,
    /// Device model from smartctl, when it reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Serial number; present only when the caller explicitly asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Bounded normalized ATA attributes; present only with `?attributes=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<SmartAttribute>>,
}

impl SmartHealth {
    fn unavailable(reason: impl Into<String>) -> Self {
        let reason: String = reason.into();
        Self {
            available: false,
            code: Some("storage.smart-unavailable"),
            error: Some(bounded_error(&reason)),
            ..Self::default()
        }
    }
}

/// One normalized ATA SMART attribute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmartAttribute {
    pub id: u64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_value: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_text: Option<String>,
}

/// Availability of one optional health source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One block device in `/storage/health`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockDeviceHealth {
    /// Sysfs basename (`nvme0n1`).
    pub name: String,
    /// Controller-facing device path (`/dev/nvme0n1`).
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Present only with `?serials=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotational: Option<bool>,
    pub smart: SmartHealth,
}

/// `GET /storage/health` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageHealthReport {
    pub ok: bool,
    pub available: bool,
    pub source: &'static str,
    pub count: usize,
    /// Block devices found before the bound was applied.
    pub matched: usize,
    pub limit: usize,
    pub truncated: bool,
    pub serials_included: bool,
    pub attributes_included: bool,
    pub smart: SourceStatus,
    pub md: MdStatus,
    /// Optional sections that could not be read.
    pub unavailable: Vec<&'static str>,
    pub devices: Vec<BlockDeviceHealth>,
}

/// `GET /storage/smart` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmartReport {
    pub ok: bool,
    pub available: bool,
    pub source: &'static str,
    pub device: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Present only with `?serials=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub serials_included: bool,
    pub attributes_included: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotational: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub smart: SmartHealth,
}

/// mdraid section of `/storage/health`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MdStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub source: &'static str,
    pub mdadm_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdadm_error: Option<String>,
    pub count: usize,
    pub arrays: Vec<MdArray>,
}

/// One md array parsed from `/proc/mdstat`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MdArray {
    pub name: String,
    pub device: String,
    pub state: String,
    pub level: String,
    pub degraded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_devices: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_devices: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Bounded `[UU]`-style slot map from `/proc/mdstat`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_map: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    pub members: Vec<MdMember>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<MdSync>,
}

/// One md member device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MdMember {
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    /// `active`, `failed`, or `spare`.
    pub state: &'static str,
}

/// In-progress md resync/recovery/reshape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MdSync {
    pub operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_minutes: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedArray {
    device: String,
    uuid: Option<String>,
    metadata: Option<String>,
}

fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.is_absolute() || program.components().count() > 1 {
        return executable(program);
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    std::env::split_paths(&path).find_map(|directory| executable(&directory.join(program)))
}

fn executable(path: &Path) -> Option<PathBuf> {
    let metadata = fs::metadata(path).ok()?;
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

fn read_block_device_names(sys_block: &Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(sys_block)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty()
            || name.len() > MAX_DEVICE_NAME_CHARS
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
        {
            continue;
        }
        if PSEUDO_BLOCK_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn read_sysfs_text(path: &Path) -> Option<String> {
    read_bounded(path, MAX_SYSFS_BYTES)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_sysfs_u64(path: &Path) -> Option<u64> {
    read_sysfs_text(path)?.parse::<u64>().ok()
}

fn bounded_label(value: &str, limit: usize) -> String {
    sanitize_label(value.trim()).chars().take(limit).collect()
}

fn bounded_error(value: &str) -> String {
    sanitize_label(value.trim())
        .chars()
        .take(240)
        .collect::<String>()
}

fn valid_block_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DEVICE_NAME_CHARS
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
}

/// Validate `device` as `/dev/<basename>` and confirm the basename is a block
/// device known to sysfs or a real `/dev` node. The returned basename is the
/// only value passed on to `smartctl`; the caller-supplied string is never
/// used as a filesystem path.
fn validate_block_device(
    device: &str,
    sys_block: &Path,
    dev_root: &Path,
) -> Result<String, HostError> {
    let invalid = || {
        HostError::invalid(
            "storage.invalid-device",
            "the device must name one block device as /dev/<name>",
        )
    };
    if device.len() > MAX_DEVICE_PATH_CHARS || device.chars().any(char::is_control) {
        return Err(invalid());
    }
    let name = device.strip_prefix("/dev/").ok_or_else(invalid)?;
    if !valid_block_name(name) {
        return Err(invalid());
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(invalid());
    }
    if !sys_block.join(name).exists() && !dev_root.join(name).exists() {
        return Err(HostError::invalid(
            "storage.invalid-device",
            "no such block device under /sys/block or /dev",
        ));
    }
    Ok(name.to_owned())
}

async fn run_smartctl(program: &Path, device: &Path) -> Result<Value, String> {
    let mut command = Command::new(program);
    command
        .args(["-j", "-H", "-A"])
        .arg(device)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => {
                    "smartctl was not found on PATH; install smartmontools".to_owned()
                }
                std::io::ErrorKind::PermissionDenied => {
                    "smartctl could not be executed; permission denied".to_owned()
                }
                _ => format!("smartctl could not be started: {error}"),
            });
        }
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "smartctl stdout was not captured".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "smartctl stderr was not captured".to_owned())?;

    let read = async {
        let (stdout, stdout_truncated) = read_limited_async(stdout, MAX_SMART_BYTES).await?;
        let (stderr, _) = read_limited_async(stderr, MAX_SMART_STDERR_BYTES).await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stdout_truncated, stderr, status))
    };
    let (stdout, stdout_truncated, stderr, _status) =
        match tokio::time::timeout(SMART_TIMEOUT, read).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("smartctl I/O failed: {error}")),
            Err(_) => return Err("smartctl timed out while reading SMART health".to_owned()),
        };
    if stdout_truncated {
        return Err("smartctl output exceeded the 1 MiB bound".to_owned());
    }
    if stdout.is_empty() {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(if detail.trim().is_empty() {
            "smartctl returned no output".to_owned()
        } else {
            bounded_error(detail.as_ref())
        });
    }
    serde_json::from_slice(&stdout).map_err(|_| {
        let detail = String::from_utf8_lossy(&stderr);
        if detail.trim().is_empty() {
            "smartctl did not return valid JSON".to_owned()
        } else {
            bounded_error(detail.as_ref())
        }
    })
}

async fn read_limited_async<R>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
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
            truncated |= take < read;
        } else {
            truncated = true;
        }
    }
    Ok((output, truncated))
}

async fn run_mdadm_scan(program: &Path) -> Result<Vec<ScannedArray>, String> {
    let mut command = Command::new(program);
    command
        .args(["--detail", "--scan"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => {
                    "mdadm was not found on PATH; md state comes from /proc/mdstat only".to_owned()
                }
                _ => format!("mdadm could not be started: {error}"),
            });
        }
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "mdadm stdout was not captured".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "mdadm stderr was not captured".to_owned())?;
    let read = async {
        let (stdout, stdout_truncated) = read_limited_async(stdout, MAX_MDADM_BYTES).await?;
        let (stderr, _) = read_limited_async(stderr, MAX_MDADM_BYTES).await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stdout_truncated, stderr, status))
    };
    let (stdout, stdout_truncated, stderr, status) =
        match tokio::time::timeout(MDADM_TIMEOUT, read).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("mdadm I/O failed: {error}")),
            Err(_) => return Err("mdadm timed out reading array metadata".to_owned()),
        };
    if stdout_truncated {
        return Err("mdadm output exceeded the 64 KiB bound".to_owned());
    }
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(if detail.trim().is_empty() {
            format!("mdadm exited with status {}", status.code().unwrap_or(-1))
        } else {
            bounded_error(detail.as_ref())
        });
    }
    Ok(parse_mdadm_scan(&String::from_utf8_lossy(&stdout)))
}

fn parse_mdadm_scan(text: &str) -> Vec<ScannedArray> {
    let mut arrays = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("ARRAY ") else {
            continue;
        };
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let Some(device) = tokens.first() else {
            continue;
        };
        if !device.starts_with("/dev/") {
            continue;
        }
        let uuid = tokens.iter().find_map(|token| token.strip_prefix("UUID="));
        let metadata = tokens
            .iter()
            .find_map(|token| token.strip_prefix("metadata="));
        arrays.push(ScannedArray {
            device: bounded_label(device, MAX_DEVICE_PATH_CHARS),
            uuid: uuid.map(|value| bounded_label(value, MAX_SERIAL_CHARS)),
            metadata: metadata.map(|value| bounded_label(value, MAX_SERIAL_CHARS)),
        });
        if arrays.len() >= MAX_MD_ARRAYS {
            break;
        }
    }
    arrays
}

fn parse_mdstat(text: &str) -> Vec<MdArray> {
    let mut arrays: Vec<MdArray> = Vec::new();
    let mut current: Option<MdArray> = None;
    for line in text.lines() {
        if let Some((name, rest)) = line.split_once(':') {
            let name = name.trim();
            if is_md_name(name) {
                if let Some(array) = current.take() {
                    push_md_array(&mut arrays, array);
                }
                if arrays.len() >= MAX_MD_ARRAYS {
                    current = None;
                    continue;
                }
                let tokens: Vec<&str> = rest.split_whitespace().collect();
                let state = tokens.first().copied().unwrap_or("unknown");
                let level = tokens
                    .iter()
                    .find(|token| is_md_level(token))
                    .copied()
                    .unwrap_or("unknown");
                let members = tokens
                    .iter()
                    .filter_map(|token| parse_md_member(token))
                    .take(MAX_MD_MEMBERS)
                    .collect();
                current = Some(MdArray {
                    name: bounded_label(name, MAX_DEVICE_NAME_CHARS),
                    device: format!("/dev/{}", bounded_label(name, MAX_DEVICE_NAME_CHARS)),
                    state: bounded_label(state, 32),
                    level: bounded_label(level, 32),
                    degraded: false,
                    total_devices: None,
                    active_devices: None,
                    size_bytes: None,
                    slot_map: None,
                    uuid: None,
                    metadata: None,
                    members,
                    sync: None,
                });
                continue;
            }
        }
        let Some(array) = current.as_mut() else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some((total, active)) = parse_md_counts(line) {
            array.total_devices = Some(total);
            array.active_devices = Some(active);
            array.degraded |= active < total;
        }
        if let Some(blocks) = parse_md_blocks(line) {
            array.size_bytes = Some(blocks.saturating_mul(1024));
        }
        if let Some(slot_map) = parse_slot_map(line) {
            array.degraded |= slot_map.contains('_');
            array.slot_map = Some(slot_map);
        }
        if let Some(sync) = parse_md_sync(line) {
            array.sync = Some(sync);
        }
        if array.members.len() < MAX_MD_MEMBERS {
            for token in line.split_whitespace() {
                if let Some(member) = parse_md_member(token) {
                    if array.members.len() >= MAX_MD_MEMBERS {
                        break;
                    }
                    array.members.push(member);
                }
            }
        }
    }
    if let Some(array) = current.take() {
        push_md_array(&mut arrays, array);
    }
    for array in &mut arrays {
        if array.members.iter().any(|member| member.state == "failed") {
            array.degraded = true;
        }
    }
    arrays
}

fn push_md_array(arrays: &mut Vec<MdArray>, array: MdArray) {
    if arrays.len() < MAX_MD_ARRAYS {
        arrays.push(array);
    }
}

fn is_md_name(name: &str) -> bool {
    name.strip_prefix("md").is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|character| character.is_ascii_digit())
    })
}

fn is_md_level(token: &str) -> bool {
    token.starts_with("raid") || matches!(token, "linear" | "multipath" | "faulty" | "container")
}

fn parse_md_member(token: &str) -> Option<MdMember> {
    let open = token.find('[')?;
    let close = token[open + 1..].find(']')? + open + 1;
    let base = &token[..open];
    if !valid_block_name(base) || base.starts_with("md") {
        return None;
    }
    let slot = token[open + 1..close].parse::<u32>().ok()?;
    let flags = &token[close + 1..];
    let state = if flags.contains("(F)") {
        "failed"
    } else if flags.contains("(S)") {
        "spare"
    } else {
        "active"
    };
    Some(MdMember {
        device: bounded_label(base, MAX_DEVICE_NAME_CHARS),
        slot: Some(slot),
        state,
    })
}

fn parse_md_counts(line: &str) -> Option<(usize, usize)> {
    for token in line.split_whitespace() {
        let Some(inner) = token
            .strip_prefix('[')
            .and_then(|token| token.strip_suffix(']'))
        else {
            continue;
        };
        let Some((total, active)) = inner.split_once('/') else {
            continue;
        };
        if let (Ok(total), Ok(active)) = (total.parse::<usize>(), active.parse::<usize>()) {
            return Some((total, active));
        }
    }
    None
}

fn parse_md_blocks(line: &str) -> Option<u64> {
    let mut tokens = line.split_whitespace();
    let blocks = tokens.next()?.parse::<u64>().ok()?;
    (tokens.next()? == "blocks").then_some(blocks)
}

fn parse_slot_map(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|token| {
        let inner = token.strip_prefix('[')?.strip_suffix(']')?;
        (!inner.is_empty()
            && !inner.contains('/')
            && inner
                .chars()
                .all(|character| character == 'U' || character == '_'))
        .then(|| bounded_label(inner, MAX_MD_MEMBERS))
    })
}

fn parse_md_sync(line: &str) -> Option<MdSync> {
    let operation = if line.contains("resync") {
        "resync"
    } else if line.contains("recovery") {
        "recovery"
    } else if line.contains("reshape") {
        "reshape"
    } else if line.contains("check") {
        "check"
    } else {
        return None;
    };
    let mut percent = None;
    let mut speed_kib = None;
    let mut finish_minutes = None;
    for token in line.split_whitespace() {
        if let Some(value) = token.strip_suffix('%') {
            percent = value.parse::<f64>().ok().map(round2);
        } else if let Some(value) = token.strip_prefix("speed=") {
            speed_kib = parse_md_speed(value);
        } else if let Some(value) = token.strip_prefix("finish=") {
            finish_minutes = value
                .trim_end_matches("min")
                .parse::<f64>()
                .ok()
                .map(round2);
        }
    }
    if percent.is_none() && speed_kib.is_none() && finish_minutes.is_none() {
        return None;
    }
    Some(MdSync {
        operation,
        percent,
        speed_kib,
        finish_minutes,
    })
}

fn parse_md_speed(value: &str) -> Option<u64> {
    let (number, factor) = if let Some(number) = value.strip_suffix("G/sec") {
        (number, 1024.0 * 1024.0)
    } else if let Some(number) = value.strip_suffix("M/sec") {
        (number, 1024.0)
    } else if let Some(number) = value.strip_suffix("K/sec") {
        (number, 1.0)
    } else {
        (value, 1.0)
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| (number * factor) as u64)
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        })
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
}

fn json_string<'a>(value: Option<&'a Value>) -> Option<&'a str> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_smart_document(
    value: &Value,
    include_serials: bool,
    include_attributes: bool,
) -> SmartHealth {
    let mut smart = SmartHealth::default();
    smart.protocol = value
        .get("device")
        .and_then(|device| device.get("protocol").or_else(|| device.get("type")))
        .and_then(Value::as_str)
        .map(|value| bounded_label(value, 32))
        .filter(|value| !value.is_empty());
    smart.model = json_string(value.get("model_name"))
        .map(|value| bounded_label(value, MAX_MODEL_CHARS))
        .filter(|value| !value.is_empty());
    if include_serials {
        smart.serial = json_string(value.get("serial_number"))
            .map(|value| bounded_label(value, MAX_SERIAL_CHARS))
            .filter(|value| !value.is_empty());
    }
    smart.passed = value
        .get("smart_status")
        .and_then(|status| status.get("passed"))
        .and_then(Value::as_bool);

    if let Some(log) = value.get("nvme_smart_health_information_log") {
        smart.temperature_c = json_i64(log.get("temperature"));
        smart.percentage_used = json_u64(log.get("percentage_used"));
        smart.available_spare_percent = json_u64(log.get("available_spare"));
        smart.media_errors = json_u64(log.get("media_errors"));
        smart.critical_warning = json_u64(log.get("critical_warning"));
        smart.power_on_hours = json_u64(log.get("power_on_hours"));
    }
    smart.temperature_c = smart
        .temperature_c
        .or_else(|| json_i64(value.pointer("/temperature/current")));
    smart.percentage_used = smart
        .percentage_used
        .or_else(|| json_u64(value.pointer("/endurance_used/current_percent")));
    smart.available_spare_percent = smart
        .available_spare_percent
        .or_else(|| json_u64(value.pointer("/spare_available/current_percent")));
    smart.power_on_hours = smart
        .power_on_hours
        .or_else(|| json_u64(value.pointer("/power_on_time/hours")));

    if let Some(table) = value
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
    {
        smart.reallocated_sectors = ata_raw_value(table, 5);
        smart.pending_sectors = ata_raw_value(table, 197);
        smart.uncorrectable_sectors = ata_raw_value(table, 198);
        if include_attributes {
            smart.attributes = Some(collect_smart_attributes(table));
        }
    }

    let has_data = smart.passed.is_some()
        || smart.temperature_c.is_some()
        || smart.percentage_used.is_some()
        || smart.available_spare_percent.is_some()
        || smart.media_errors.is_some()
        || smart.reallocated_sectors.is_some()
        || smart.pending_sectors.is_some()
        || smart.uncorrectable_sectors.is_some()
        || smart.attributes.is_some();
    if !has_data {
        let message = value
            .pointer("/smartctl/messages")
            .and_then(Value::as_array)
            .and_then(|messages| {
                messages.iter().find_map(|message| {
                    (message.get("severity").and_then(Value::as_str) == Some("error"))
                        .then(|| json_string(message.get("string")))
                        .flatten()
                })
            })
            .unwrap_or("smartctl returned no SMART health for this device");
        smart.available = false;
        smart.code = Some("storage.smart-unavailable");
        smart.error = Some(bounded_error(message));
        return smart;
    }
    smart.available = true;
    smart.code = None;
    smart.error = None;
    smart
}

fn ata_raw_value(table: &[Value], id: u64) -> Option<u64> {
    let attribute = table
        .iter()
        .find(|attribute| json_u64(attribute.get("id")) == Some(id))?;
    let raw = attribute.get("raw")?;
    json_u64(raw.get("value")).or_else(|| {
        json_string(raw.get("string")).and_then(|value| {
            let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().ok()
        })
    })
}

fn collect_smart_attributes(table: &[Value]) -> Vec<SmartAttribute> {
    table
        .iter()
        .take(MAX_SMART_ATTRIBUTES)
        .filter_map(|attribute| {
            let id = json_u64(attribute.get("id"))?;
            let raw = attribute.get("raw");
            Some(SmartAttribute {
                id,
                name: json_string(attribute.get("name"))
                    .map(|value| bounded_label(value, MAX_MODEL_CHARS))
                    .unwrap_or_else(|| format!("attr-{id}")),
                value: json_u64(attribute.get("value")),
                worst: json_u64(attribute.get("worst")),
                threshold: json_u64(attribute.get("thresh")),
                raw_value: raw.and_then(|raw| json_u64(raw.get("value"))),
                raw_text: raw
                    .and_then(|raw| json_string(raw.get("string")))
                    .map(|value| bounded_label(value, MAX_MODEL_CHARS)),
            })
        })
        .collect()
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

    fn write_executable(path: &Path, contents: &str) {
        write(path, contents);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("fixture executable");
        }
    }

    fn add_block(
        root: &Path,
        name: &str,
        model: &str,
        serial: &str,
        sectors: u64,
        rotational: u64,
    ) {
        let dir = root.join("sys/block").join(name);
        write(&dir.join("device/model"), &format!("{model}\n"));
        write(&dir.join("device/serial"), &format!("{serial}\n"));
        write(&dir.join("size"), &format!("{sectors}\n"));
        write(&dir.join("queue/rotational"), &format!("{rotational}\n"));
        write(&root.join("dev").join(name), "");
    }

    fn smartctl_fixture(root: &Path) {
        write_executable(
            &root.join("bin/smartctl"),
            r#"#!/bin/sh
device="$4"
case "$device" in
  *nvme0n1)
    cat <<'JSON'
{"device":{"type":"nvme","protocol":"NVMe"},"model_name":"Fixture NVMe","serial_number":"SERIAL-NVME-123","smart_status":{"passed":true},"temperature":{"current":41},"nvme_smart_health_information_log":{"temperature":41,"percentage_used":7,"available_spare":100,"media_errors":0,"critical_warning":0,"power_on_hours":123},"endurance_used":{"current_percent":7}}
JSON
    ;;
  *sda)
    cat <<'JSON'
{"device":{"type":"sat","protocol":"ATA"},"model_name":"Fixture SATA","serial_number":"SERIAL-SATA-999","smart_status":{"passed":false},"temperature":{"current":45},"power_on_time":{"hours":999},"ata_smart_attributes":{"table":[{"id":5,"name":"Reallocated_Sector_Ct","value":100,"worst":100,"thresh":10,"raw":{"value":8,"string":"8"}},{"id":197,"name":"Current_Pending_Sector","value":99,"worst":99,"thresh":0,"raw":{"value":2,"string":"2"}},{"id":198,"name":"Offline_Uncorrectable","value":98,"worst":98,"thresh":0,"raw":{"value":3,"string":"3"}}]}}
JSON
    ;;
  *bad0)
    echo 'this is not json'
    ;;
  *)
    echo '{"device":{"type":"scsi"},"smart_status":{"passed":true},"temperature":{"current":30}}'
    ;;
esac
"#,
        );
    }

    fn mdstat_fixture() -> &'static str {
        "Personalities : [raid1] [raid6]\n\
md0 : active raid1 sda1[0] sdb1[1]\n\
      1953260928 blocks super 1.2 [2/2] [UU]\n\
      bitmap: 0/1 pages [0KB], 65536KB chunk\n\
\n\
md1 : active raid6 sdc1[0] sdd1[1] sde1[2] sdf1[3]\n\
      3907028992 blocks super 1.2 level 6, 512k chunk, algorithm 2 [4/3] [UUU_]\n\
      [===>....]  resync = 42.5% (123/456) finish=12.3min speed=150000K/sec\n\
\n\
unused devices: <none>\n"
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
        assert!(advertised.health.supported);
        assert_eq!(advertised.health.path, STORAGE_HEALTH_PATH);
        assert_eq!(advertised.health.limit, Some(MAX_BLOCK_DEVICES));
        assert!(advertised.smart.supported);
        assert_eq!(advertised.smart.path, STORAGE_SMART_PATH);
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
    fn smart_nvme_document_is_normalized_and_serial_is_opt_in() {
        let value: Value = serde_json::from_str(
            r#"{
                "device": {"type": "nvme", "protocol": "NVMe"},
                "model_name": "Fixture NVMe",
                "serial_number": "SERIAL-NVME-123",
                "smart_status": {"passed": true},
                "temperature": {"current": 41},
                "nvme_smart_health_information_log": {
                    "temperature": 41,
                    "percentage_used": 7,
                    "available_spare": 100,
                    "media_errors": 0,
                    "critical_warning": 0,
                    "power_on_hours": 123
                },
                "endurance_used": {"current_percent": 7}
            }"#,
        )
        .expect("fixture json");
        let smart = parse_smart_document(&value, false, false);
        assert!(smart.available);
        assert_eq!(smart.passed, Some(true));
        assert_eq!(smart.temperature_c, Some(41));
        assert_eq!(smart.percentage_used, Some(7));
        assert_eq!(smart.available_spare_percent, Some(100));
        assert_eq!(smart.media_errors, Some(0));
        assert_eq!(smart.power_on_hours, Some(123));
        assert_eq!(smart.model.as_deref(), Some("Fixture NVMe"));
        assert_eq!(smart.protocol.as_deref(), Some("NVMe"));
        assert_eq!(smart.serial, None);
        assert_eq!(smart.attributes, None);

        let with_serial = parse_smart_document(&value, true, false);
        assert_eq!(with_serial.serial.as_deref(), Some("SERIAL-NVME-123"));
    }

    #[test]
    fn smart_sata_failing_document_reports_defect_counts_and_bounded_attributes() {
        let value: Value = serde_json::from_str(
            r#"{
                "device": {"type": "sat", "protocol": "ATA"},
                "model_name": "Fixture SATA",
                "serial_number": "SERIAL-SATA-999",
                "smart_status": {"passed": false},
                "temperature": {"current": 45},
                "power_on_time": {"hours": 999},
                "ata_smart_attributes": {
                    "table": [
                        {"id": 5, "name": "Reallocated_Sector_Ct", "value": 100, "worst": 100, "thresh": 10, "raw": {"value": 8, "string": "8"}},
                        {"id": 197, "name": "Current_Pending_Sector", "value": 99, "worst": 99, "thresh": 0, "raw": {"value": 2, "string": "2"}},
                        {"id": 198, "name": "Offline_Uncorrectable", "value": 98, "worst": 98, "thresh": 0, "raw": {"value": 3, "string": "3"}}
                    ]
                }
            }"#,
        )
        .expect("fixture json");
        let smart = parse_smart_document(&value, false, true);
        assert!(smart.available);
        assert_eq!(smart.passed, Some(false));
        assert_eq!(smart.temperature_c, Some(45));
        assert_eq!(smart.reallocated_sectors, Some(8));
        assert_eq!(smart.pending_sectors, Some(2));
        assert_eq!(smart.uncorrectable_sectors, Some(3));
        assert_eq!(smart.power_on_hours, Some(999));
        let attributes = smart.attributes.expect("explicit attributes");
        assert_eq!(attributes.len(), 3);
        assert_eq!(attributes[0].id, 5);
        assert_eq!(attributes[0].raw_value, Some(8));
        assert_eq!(attributes[0].raw_text.as_deref(), Some("8"));

        let redacted = parse_smart_document(&value, false, false);
        assert_eq!(redacted.attributes, None);
    }

    #[test]
    fn smart_error_messages_become_typed_unavailability() {
        let value: Value = serde_json::from_str(
            r#"{
                "smartctl": {
                    "messages": [
                        {"string": "Smartctl open device: /dev/nvme0n1 failed: Permission denied", "severity": "error"}
                    ],
                    "exit_status": 2
                }
            }"#,
        )
        .expect("fixture json");
        let smart = parse_smart_document(&value, false, false);
        assert!(!smart.available);
        assert_eq!(smart.code, Some("storage.smart-unavailable"));
        assert!(smart
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Permission denied"));
    }

    #[test]
    fn mdstat_parses_healthy_and_degraded_arrays() {
        let arrays = parse_mdstat(mdstat_fixture());
        assert_eq!(arrays.len(), 2);

        let healthy = &arrays[0];
        assert_eq!(healthy.name, "md0");
        assert_eq!(healthy.device, "/dev/md0");
        assert_eq!(healthy.state, "active");
        assert_eq!(healthy.level, "raid1");
        assert!(!healthy.degraded);
        assert_eq!(healthy.total_devices, Some(2));
        assert_eq!(healthy.active_devices, Some(2));
        assert_eq!(healthy.size_bytes, Some(1953260928 * 1024));
        assert_eq!(healthy.slot_map.as_deref(), Some("UU"));
        assert_eq!(healthy.members.len(), 2);
        assert_eq!(healthy.members[0].device, "sda1");
        assert_eq!(healthy.members[0].slot, Some(0));
        assert_eq!(healthy.members[0].state, "active");

        let degraded = &arrays[1];
        assert_eq!(degraded.level, "raid6");
        assert!(degraded.degraded);
        assert_eq!(degraded.total_devices, Some(4));
        assert_eq!(degraded.active_devices, Some(3));
        assert_eq!(degraded.slot_map.as_deref(), Some("UUU_"));
        assert_eq!(degraded.members.len(), 4);
        let sync = degraded.sync.as_ref().expect("resync");
        assert_eq!(sync.operation, "resync");
        assert_eq!(sync.percent, Some(42.5));
        assert_eq!(sync.speed_kib, Some(150000));
        assert_eq!(sync.finish_minutes, Some(12.3));
        assert_eq!(parse_md_speed("1.5M/sec"), Some(1536));
        assert_eq!(parse_md_speed("150000K/sec"), Some(150000));
    }

    #[test]
    fn mdstat_malformed_lines_are_ignored_without_panicking() {
        let arrays = parse_mdstat("garbage\nmd\nmd0 : active\n      123 blocks\n");
        assert_eq!(arrays.len(), 1);
        assert_eq!(arrays[0].name, "md0");
        assert_eq!(arrays[0].level, "unknown");
        assert!(arrays[0].members.is_empty());
        assert_eq!(arrays[0].size_bytes, Some(123 * 1024));
    }

    #[test]
    fn mdadm_scan_parser_reads_uuid_and_metadata() {
        let arrays = parse_mdadm_scan(
            "ARRAY /dev/md0 metadata=1.2 name=host:0 UUID=1234:5678:9abc:def0\n\
             ARRAY /dev/md1 metadata=1.2 name=host:1 UUID=aaaa:bbbb:cccc:dddd\n\
             bogus\n",
        );
        assert_eq!(arrays.len(), 2);
        assert_eq!(arrays[0].device, "/dev/md0");
        assert_eq!(arrays[0].uuid.as_deref(), Some("1234:5678:9abc:def0"));
        assert_eq!(arrays[0].metadata.as_deref(), Some("1.2"));
    }

    #[test]
    fn block_device_validation_rejects_traversal_and_missing_devices() {
        let root = fixture_root("validate-device");
        add_block(&root, "nvme0n1", "Fixture", "SERIAL", 100, 0);
        let sys_block = root.join("sys/block");
        let dev = root.join("dev");

        assert_eq!(
            validate_block_device("/dev/nvme0n1", &sys_block, &dev).expect("valid"),
            "nvme0n1"
        );
        for device in [
            "/dev/../etc/passwd",
            "/dev/nvme0n1/../../etc/passwd",
            "/etc/passwd",
            "/dev/",
            "/dev/does-not-exist",
            "/dev/.hidden",
            "/dev/a b",
            "nvme0n1",
            "/dev/md/0",
        ] {
            let error = validate_block_device(device, &sys_block, &dev)
                .expect_err("invalid device must be rejected");
            assert_eq!(error.code, "storage.invalid-device", "{device}");
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{device}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn storage_health_is_bounded_and_reports_md_arrays() {
        let root = fixture_root("health-bound");
        for index in 0..MAX_BLOCK_DEVICES + 8 {
            add_block(
                &root,
                &format!("disk{index:02}"),
                "Fixture",
                "SERIAL",
                100,
                0,
            );
        }
        smartctl_fixture(&root);
        write(&root.join("proc/mdstat"), mdstat_fixture());
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state
            .storage_health(false, false)
            .await
            .expect("health report");
        assert!(report.ok);
        assert_eq!(report.matched, MAX_BLOCK_DEVICES + 8);
        assert_eq!(report.count, MAX_BLOCK_DEVICES);
        assert_eq!(report.limit, MAX_BLOCK_DEVICES);
        assert!(report.truncated);
        assert!(!report.serials_included);
        assert!(!report.attributes_included);
        assert!(report.devices.iter().all(|device| device.serial.is_none()));
        assert!(report
            .devices
            .iter()
            .all(|device| device.smart.attributes.is_none()));
        assert!(report.smart.available);
        assert!(report.md.available);
        assert_eq!(report.md.count, 2);
        assert!(!report.unavailable.contains(&"md"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn storage_health_normalizes_devices_and_redacts_serials_by_default() {
        let root = fixture_root("health-per-device");
        add_block(&root, "nvme0n1", "Fixture NVMe", "SERIAL-NVME-123", 1000, 0);
        add_block(&root, "sda", "Fixture SATA", "SERIAL-SATA-999", 2000, 1);
        add_block(&root, "bad0", "Fixture Bad", "SERIAL-BAD", 3000, 0);
        smartctl_fixture(&root);
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state
            .storage_health(false, false)
            .await
            .expect("health report");
        assert_eq!(report.count, 3);
        assert!(!report.truncated);
        assert!(!report.serials_included);
        assert!(report.devices.iter().all(|device| device.serial.is_none()));
        assert!(!report.md.available);
        assert_eq!(report.md.code, Some("storage.md-unavailable"));
        assert!(report.unavailable.contains(&"md"));

        let nvme = report
            .devices
            .iter()
            .find(|device| device.name == "nvme0n1")
            .expect("nvme");
        assert_eq!(nvme.device, "/dev/nvme0n1");
        assert_eq!(nvme.model.as_deref(), Some("Fixture NVMe"));
        assert_eq!(nvme.size_bytes, Some(1000 * 512));
        assert_eq!(nvme.rotational, Some(false));
        assert!(nvme.smart.available);
        assert_eq!(nvme.smart.temperature_c, Some(41));
        assert_eq!(nvme.smart.percentage_used, Some(7));

        let sata = report
            .devices
            .iter()
            .find(|device| device.name == "sda")
            .expect("sata");
        assert_eq!(sata.rotational, Some(true));
        assert_eq!(sata.smart.passed, Some(false));
        assert_eq!(sata.smart.reallocated_sectors, Some(8));
        assert_eq!(sata.smart.pending_sectors, Some(2));
        assert_eq!(sata.smart.uncorrectable_sectors, Some(3));
        assert_eq!(sata.smart.attributes, None);

        let bad = report
            .devices
            .iter()
            .find(|device| device.name == "bad0")
            .expect("bad");
        assert!(!bad.smart.available);
        assert_eq!(bad.smart.code, Some("storage.smart-unavailable"));

        let json = serde_json::to_string(&report).expect("serialize");
        assert!(!json.contains("SERIAL-NVME-123"));
        assert!(!json.contains("SERIAL-SATA-999"));

        let with_serials = state
            .storage_health(true, true)
            .await
            .expect("health with serials");
        assert!(with_serials.serials_included);
        assert!(with_serials.attributes_included);
        let nvme = with_serials
            .devices
            .iter()
            .find(|device| device.name == "nvme0n1")
            .expect("nvme");
        assert_eq!(nvme.serial.as_deref(), Some("SERIAL-NVME-123"));
        let sata = with_serials
            .devices
            .iter()
            .find(|device| device.name == "sda")
            .expect("sata");
        assert_eq!(sata.serial.as_deref(), Some("SERIAL-SATA-999"));
        assert_eq!(sata.smart.attributes.as_ref().map(Vec::len), Some(3));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn storage_health_names_unavailable_smart_and_md_sections() {
        let root = fixture_root("health-unavailable");
        add_block(&root, "nvme0n1", "Fixture", "SERIAL", 100, 0);
        write_executable(
            &root.join("bin/smartctl"),
            r#"#!/bin/sh
cat <<'JSON'
{"smartctl":{"messages":[{"string":"Smartctl open device failed","severity":"error"}],"exit_status":2}}
JSON
"#,
        );
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state
            .storage_health(false, false)
            .await
            .expect("health report");
        assert!(!report.smart.available);
        assert_eq!(report.smart.code, Some("storage.smart-unavailable"));
        assert!(report
            .smart
            .error
            .as_deref()
            .unwrap_or("")
            .contains("no usable SMART"));
        assert!(!report.md.available);
        assert_eq!(report.md.code, Some("storage.md-unavailable"));
        assert!(report.unavailable.contains(&"smart"));
        assert!(report.unavailable.contains(&"md"));
        assert_eq!(report.count, 1);
        assert!(!report.devices[0].smart.available);
        assert_eq!(
            report.devices[0].smart.code,
            Some("storage.smart-unavailable")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn smart_endpoint_normalizes_and_validates_devices() {
        let root = fixture_root("smart-endpoint");
        add_block(&root, "nvme0n1", "Fixture NVMe", "SERIAL-NVME-123", 1000, 0);
        add_block(&root, "sda", "Fixture SATA", "SERIAL-SATA-999", 2000, 1);
        add_block(&root, "bad0", "Fixture Bad", "SERIAL-BAD", 3000, 0);
        smartctl_fixture(&root);
        let state = HostSurfacesState::with_test_paths(&root);

        let report = state
            .smart("/dev/nvme0n1", false, false)
            .await
            .expect("smart report");
        assert_eq!(report.device, "/dev/nvme0n1");
        assert_eq!(report.model.as_deref(), Some("Fixture NVMe"));
        assert_eq!(report.serial, None);
        assert_eq!(report.size_bytes, Some(1000 * 512));
        assert_eq!(report.rotational, Some(false));
        assert_eq!(report.protocol.as_deref(), Some("NVMe"));
        assert_eq!(report.smart.temperature_c, Some(41));
        assert_eq!(report.smart.percentage_used, Some(7));

        let with_serial = state
            .smart("/dev/nvme0n1", true, false)
            .await
            .expect("smart report with serial");
        assert_eq!(with_serial.serial.as_deref(), Some("SERIAL-NVME-123"));

        let sata = state
            .smart("/dev/sda", false, true)
            .await
            .expect("sata smart report");
        assert_eq!(sata.smart.reallocated_sectors, Some(8));
        assert_eq!(sata.smart.attributes.as_ref().map(Vec::len), Some(3));

        let error = state
            .smart("/dev/../etc/passwd", false, false)
            .await
            .expect_err("traversal must be rejected");
        assert_eq!(error.code, "storage.invalid-device");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);

        let error = state
            .smart("/dev/does-not-exist", false, false)
            .await
            .expect_err("missing device must be rejected");
        assert_eq!(error.code, "storage.invalid-device");

        let error = state
            .smart("/dev/bad0", false, false)
            .await
            .expect_err("malformed smartctl output must be typed");
        assert_eq!(error.code, "storage.smart-unavailable");
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn storage_health_routes_are_bounded_and_preserve_typed_errors() {
        let root = fixture_root("health-routes");
        add_block(&root, "nvme0n1", "Fixture NVMe", "SERIAL-NVME-123", 1000, 0);
        smartctl_fixture(&root);
        let app = router(HostSurfacesState::with_test_paths(&root));

        let (status, health) = get_json(&app, STORAGE_HEALTH_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(health["ok"], true);
        assert_eq!(health["count"], 1);
        assert_eq!(health["serials_included"], false);
        assert!(health["devices"][0].get("serial").is_none());
        assert_eq!(health["devices"][0]["smart"]["percentage_used"], 7);

        let (status, health) = get_json(&app, "/storage/health?serials=true").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(health["serials_included"], true);
        assert_eq!(health["devices"][0]["serial"], "SERIAL-NVME-123");

        let (status, health) = get_json(&app, STORAGE_HEALTH_API_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(health["count"], 1);

        let (status, smart) = get_json(&app, "/storage/smart?device=/dev/nvme0n1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(smart["device"], "/dev/nvme0n1");
        assert_eq!(smart["smart"]["temperature_c"], 41);
        assert!(smart.get("serial").is_none());

        let (status, smart) =
            get_json(&app, "/storage/smart?device=/dev/nvme0n1&serials=true").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(smart["serial"], "SERIAL-NVME-123");

        let (status, error) = get_json(&app, STORAGE_SMART_PATH).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "storage.invalid-device");

        let (status, error) = get_json(&app, "/storage/smart?device=/dev/../etc/passwd").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "storage.invalid-device");

        let (status, _) = get_json(&app, STORAGE_SMART_API_PATH).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let missing = fixture_root("health-routes-no-smartctl");
        add_block(&missing, "nvme0n1", "Fixture", "SERIAL", 100, 0);
        let missing_app = router(HostSurfacesState::with_test_paths(&missing));
        let (status, error) = get_json(&missing_app, "/storage/smart?device=/dev/nvme0n1").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error["code"], "storage.smart-unavailable");

        let (status, error) = get_json(&missing_app, STORAGE_HEALTH_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(error["md"]["code"], "storage.md-unavailable");
        assert_eq!(error["smart"]["code"], "storage.smart-unavailable");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(missing);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_host_storage_health_and_smart_are_bounded_and_read_only() {
        let state = HostSurfacesState::host_with_identity("live-node", "hardware");
        let health = state
            .storage_health(false, false)
            .await
            .expect("live storage health");
        assert!(health.count <= MAX_BLOCK_DEVICES);
        assert!(health.matched >= health.count);
        assert!(!health.serials_included);
        assert!(!health.attributes_included);
        assert!(health.devices.iter().all(|device| device.serial.is_none()));
        assert!(health
            .devices
            .iter()
            .all(|device| device.smart.attributes.is_none()));
        assert!(health.md.count <= MAX_MD_ARRAYS);
        if !health.md.available {
            assert_eq!(health.md.code, Some("storage.md-unavailable"));
        }
        for device in &health.devices {
            assert_eq!(device.device, format!("/dev/{}", device.name));
            if let Some(size) = device.size_bytes {
                assert!(size > 0);
            }
        }
        if let Some(nvme) = health
            .devices
            .iter()
            .find(|device| device.name.starts_with("nvme"))
        {
            assert_eq!(nvme.rotational, Some(false));
        }

        if std::path::Path::new("/sys/block/nvme0n1").exists() {
            match state.smart("/dev/nvme0n1", false, false).await {
                Ok(report) => {
                    assert_eq!(report.device, "/dev/nvme0n1");
                    assert_eq!(report.serial, None);
                    assert!(report.smart.available);
                    assert!(
                        report.smart.temperature_c.is_some()
                            || report.smart.percentage_used.is_some()
                    );
                }
                Err(error) => {
                    assert_eq!(error.code, "storage.smart-unavailable");
                    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
                }
            }
        }
    }

    #[test]
    fn router_builds() {
        let _ = router(HostSurfacesState::host());
    }
}
