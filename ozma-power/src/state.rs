// SPDX-License-Identifier: AGPL-3.0-only
//! Power state values shared by every backend.
//!
//! The raw string is retained in [`PowerState::status`], but the constructors
//! used by backends normalize it to the stable vocabulary the Python soft node
//! exposes over HTTP and metrics:
//!
//! `running`, `blocked`, `paused`, `shutdown`, `shutoff`, `crashed`,
//! `pmsuspended`, `unknown`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::PowerError;

/// The current power state of a VM.
///
/// Serializes as the same object the Python soft node returns from
/// `query_status`: `{"status": "running"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PowerState {
    /// Stable status string (`running`, `paused`, `shutoff`, ...).
    pub status: String,
}

impl PowerState {
    /// Stable name for a running VM.
    pub const RUNNING: &'static str = "running";
    /// Stable name for a blocked (I/O wait) VM.
    pub const BLOCKED: &'static str = "blocked";
    /// Stable name for a paused VM.
    pub const PAUSED: &'static str = "paused";
    /// Stable name for a VM in the shutdown process.
    pub const SHUTDOWN: &'static str = "shutdown";
    /// Stable name for a stopped VM.
    pub const SHUTOFF: &'static str = "shutoff";
    /// Stable name for a crashed VM.
    pub const CRASHED: &'static str = "crashed";
    /// Stable name for a VM suspended by the guest (S3).
    pub const PMSUSPENDED: &'static str = "pmsuspended";
    /// Stable name for an unrecognized state.
    pub const UNKNOWN: &'static str = "unknown";

    /// Build a state from a raw status string.
    ///
    /// Surrounding whitespace is trimmed, an empty value becomes
    /// [`PowerState::UNKNOWN`], and the value is lowercased. Known aliases such
    /// as `shut off` are **not** rewritten here; use [`PowerState::from_libvirt`]
    /// or [`PowerState::from_qmp`] when consuming backend output.
    pub fn new(status: impl Into<String>) -> Self {
        let status = status.into();
        let status = status.trim();
        if status.is_empty() {
            Self {
                status: Self::UNKNOWN.to_string(),
            }
        } else {
            Self {
                status: status.to_ascii_lowercase(),
            }
        }
    }

    /// State for a running VM.
    pub fn running() -> Self {
        Self::new(Self::RUNNING)
    }

    /// State for a blocked VM.
    pub fn blocked() -> Self {
        Self::new(Self::BLOCKED)
    }

    /// State for a paused VM.
    pub fn paused() -> Self {
        Self::new(Self::PAUSED)
    }

    /// State for a VM in the shutdown process.
    pub fn shutdown() -> Self {
        Self::new(Self::SHUTDOWN)
    }

    /// State for a stopped VM.
    pub fn shutoff() -> Self {
        Self::new(Self::SHUTOFF)
    }

    /// State for a crashed VM.
    pub fn crashed() -> Self {
        Self::new(Self::CRASHED)
    }

    /// State for a guest-suspended (S3) VM.
    pub fn pmsuspended() -> Self {
        Self::new(Self::PMSUSPENDED)
    }

    /// State for an unrecognized VM state.
    pub fn unknown() -> Self {
        Self::new(Self::UNKNOWN)
    }

    /// Normalize output from `virsh domstate` to the stable vocabulary.
    ///
    /// `virsh` prints `shut off` and `in shutdown`, whereas the libvirt API and
    /// the Python HTTP surface use `shutoff` and `shutdown`. Unrecognized states
    /// become `unknown`, matching the Python libvirt API mapping for state
    /// values outside 1–7.
    pub fn from_libvirt(raw: &str) -> Self {
        Self::normalize(raw)
    }

    /// Normalize a QMP `query-status` status to the stable vocabulary.
    ///
    /// QMP's `suspended` (guest S3) maps to `pmsuspended`, the name libvirt and
    /// the Python power API use. QMP states outside the stable set
    /// (`prelaunch`, `io-error`, ...) map to `unknown`.
    pub fn from_qmp(raw: &str) -> Self {
        Self::normalize(raw)
    }

    fn normalize(raw: &str) -> Self {
        let kind = PowerStatus::from_raw(raw);
        Self::new(kind.as_str())
    }

    /// Raw status string.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Raw status string (alias for [`PowerState::status`]).
    pub fn as_str(&self) -> &str {
        &self.status
    }

    /// True only when the VM is running.
    pub fn is_running(&self) -> bool {
        self.kind() == PowerStatus::Running
    }

    /// True when the VM is stopped.
    pub fn is_shutoff(&self) -> bool {
        self.kind() == PowerStatus::Shutoff
    }

    /// True when the VM is paused.
    pub fn is_paused(&self) -> bool {
        self.kind() == PowerStatus::Paused
    }

    /// Classify the raw status into the stable state set.
    pub fn kind(&self) -> PowerStatus {
        PowerStatus::from_raw(&self.status)
    }
}

impl fmt::Display for PowerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.status)
    }
}

impl From<PowerStatus> for PowerState {
    fn from(status: PowerStatus) -> Self {
        Self::new(status.as_str())
    }
}

impl FromStr for PowerState {
    type Err = PowerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        PowerStatus::from_str(value).map(PowerState::from)
    }
}

/// The closed set of stable power states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerStatus {
    /// The VM is running.
    Running,
    /// The VM is blocked on I/O.
    Blocked,
    /// The VM is paused.
    Paused,
    /// The guest is shutting down.
    Shutdown,
    /// The VM is stopped.
    Shutoff,
    /// The VM crashed.
    Crashed,
    /// The guest is suspended (S3).
    Pmsuspended,
    /// The state is not recognized.
    Unknown,
}

impl PowerStatus {
    /// Stable string for this state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => PowerState::RUNNING,
            Self::Blocked => PowerState::BLOCKED,
            Self::Paused => PowerState::PAUSED,
            Self::Shutdown => PowerState::SHUTDOWN,
            Self::Shutoff => PowerState::SHUTOFF,
            Self::Crashed => PowerState::CRASHED,
            Self::Pmsuspended => PowerState::PMSUSPENDED,
            Self::Unknown => PowerState::UNKNOWN,
        }
    }

    /// Classify a raw backend status, accepting the `virsh`/QMP aliases and
    /// returning [`PowerStatus::Unknown`] for anything else.
    pub fn from_raw(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "running" => Self::Running,
            "blocked" => Self::Blocked,
            "paused" => Self::Paused,
            "shutdown" | "in shutdown" => Self::Shutdown,
            "shutoff" | "shut off" => Self::Shutoff,
            "crashed" => Self::Crashed,
            "pmsuspended" | "suspended" => Self::Pmsuspended,
            "unknown" => Self::Unknown,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for PowerStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PowerStatus {
    type Err = PowerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        let kind = Self::from_raw(&normalized);
        if kind == Self::Unknown && normalized != PowerState::UNKNOWN {
            Err(PowerError::InvalidState(format!(
                "unknown power state {value:?}"
            )))
        } else {
            Ok(kind)
        }
    }
}
