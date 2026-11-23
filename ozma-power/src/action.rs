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
/// | [`PowerAction::Start`] | `on` | `start`, `cont`, `resume` |
/// | [`PowerAction::Shutdown`] | `off` | `shutdown`, `powerdown`, `power-off` |
/// | [`PowerAction::Reset`] | `reset` | — |
/// | [`PowerAction::Stop`] | `force-off` | `stop`, `destroy`, `force_off` |
///
/// [`PowerAction::Start`] means "resume a paused VM or start a stopped one",
/// matching [`crate::PowerBackend::cont`]. [`PowerAction::Stop`] means a hard
/// stop, matching [`crate::PowerBackend::stop`].
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
    /// Force the VM off (`force-off`).
    #[serde(rename = "force-off")]
    Stop,
}

impl PowerAction {
    /// Every action, in HTTP-surface order.
    pub const ALL: [PowerAction; 4] = [
        PowerAction::Start,
        PowerAction::Shutdown,
        PowerAction::Reset,
        PowerAction::Stop,
    ];

    /// Canonical HTTP name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "on",
            Self::Shutdown => "off",
            Self::Reset => "reset",
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
            "on" | "start" | "cont" | "resume" => Ok(Self::Start),
            "off" | "shutdown" | "powerdown" | "power-off" => Ok(Self::Shutdown),
            "reset" => Ok(Self::Reset),
            "force-off" | "force_off" | "stop" | "destroy" => Ok(Self::Stop),
            _ => Err(PowerError::InvalidState(format!(
                "unknown power action {value:?}; expected one of: on, off, reset, force-off"
            ))),
        }
    }
}
