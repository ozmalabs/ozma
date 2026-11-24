// SPDX-License-Identifier: AGPL-3.0-only
//! Power actions exposed by the future HTTP `/power/{action}` surface.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::PowerError;

/// A power action a caller can request.
///
/// The canonical names match the Python soft node's HTTP endpoints so the
/// controller can use the same URLs during migration:
///
/// | Action | Canonical name | Accepted aliases |
/// |---|---|---|
/// | [`PowerAction::Start`] | `on` | `start`, `cont` |
/// | [`PowerAction::Shutdown`] | `off` | `shutdown`, `powerdown`, `power-off` |
/// | [`PowerAction::Reset`] | `reset` | — |
/// | [`PowerAction::Pause`] | `pause` | `suspend` |
/// | [`PowerAction::Resume`] | `resume` | — |
/// | [`PowerAction::Stop`] | `force-off` | `stop`, `destroy`, `force_off` |
///
/// [`PowerAction::Start`] calls [`crate::PowerBackend::cont`] (resume or start);
/// [`PowerAction::Resume`] calls [`crate::PowerBackend::resume`] (QMP `cont`,
/// libvirt `virsh resume`), the explicit pause counterpart. [`PowerAction::Pause`]
/// calls [`crate::PowerBackend::pause`] (QMP `stop`, libvirt `suspend`) and is
/// deliberately distinct from [`PowerAction::Stop`], the hard force-off that
/// calls [`crate::PowerBackend::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PowerAction {
    /// Resume a paused VM or start a stopped VM (`on`).
    #[serde(rename = "on")]
    Start,
    /// Request an ACPI shutdown (`off`).
    #[serde(rename = "off")]
    Shutdown,
    /// Reset the VM (`reset`).
    #[serde(rename = "reset")]
    Reset,
    /// Pause a running VM without powering it off (`pause`).
    #[serde(rename = "pause")]
    Pause,
    /// Resume a paused VM (`resume`).
    #[serde(rename = "resume")]
    Resume,
    /// Force the VM off (`force-off`).
    #[serde(rename = "force-off")]
    Stop,
}

impl PowerAction {
    /// Every action, in HTTP-surface order.
    pub const ALL: [PowerAction; 6] = [
        PowerAction::Start,
        PowerAction::Shutdown,
        PowerAction::Reset,
        PowerAction::Pause,
        PowerAction::Resume,
        PowerAction::Stop,
    ];

    /// Canonical HTTP name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "on",
            Self::Shutdown => "off",
            Self::Reset => "reset",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "force-off",
        }
    }
}

impl fmt::Display for PowerAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PowerAction {
    type Err = PowerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "on" | "start" | "cont" => Ok(Self::Start),
            "off" | "shutdown" | "powerdown" | "power-off" => Ok(Self::Shutdown),
            "reset" => Ok(Self::Reset),
            "pause" | "suspend" => Ok(Self::Pause),
            "resume" => Ok(Self::Resume),
            "force-off" | "force_off" | "stop" | "destroy" => Ok(Self::Stop),
            _ => Err(PowerError::InvalidState(format!(
                "unknown power action {value:?}; expected one of: on, off, reset, pause, resume, force-off"
            ))),
        }
    }
}
