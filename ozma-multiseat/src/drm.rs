// SPDX-License-Identifier: AGPL-3.0-only
//! Side-effect-free DRM connector detection from sysfs.
//!
//! This is detection, not a mode-setting or scanout backend. The planner uses
//! Xrandr for capture regions today; DRM gives an honest answer about which
//! connectors the kernel sees as connected and which modes they advertise.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::display::BackendDetection;
use crate::error::MultiseatError;

/// Default sysfs DRM class root.
pub const DEFAULT_DRM_ROOT: &str = "/sys/class/drm";

/// Kernel-reported connector status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmStatus {
    /// The connector reports `connected`.
    Connected,
    /// The connector reports `disconnected`.
    Disconnected,
    /// A status string this crate does not recognise.
    Unknown(String),
}

impl DrmStatus {
    /// Parse the sysfs `status` file.
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "connected" => Self::Connected,
            "disconnected" => Self::Disconnected,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Whether the connector is connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// One DRM connector directory (`card0-HDMI-A-1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmConnector {
    /// Card directory prefix, for example `card0`.
    pub card: String,
    /// Connector name, for example `HDMI-A-1`.
    pub connector: String,
    /// Kernel connector status.
    pub status: DrmStatus,
    /// Advertised modes, newest first as sysfs reports them.
    #[serde(default)]
    pub modes: Vec<String>,
}

/// DRM detection result for a host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmDetection {
    /// Backend name (`drm`).
    pub backend: String,
    /// Whether at least one connector is connected.
    pub available: bool,
    /// All discovered connectors.
    pub connectors: Vec<DrmConnector>,
    /// Why detection is unavailable or incomplete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DrmDetection {
    /// Connectors the kernel currently reports as connected.
    pub fn connected(&self) -> impl Iterator<Item = &DrmConnector> {
        self.connectors
            .iter()
            .filter(|connector| connector.status.is_connected())
    }

    /// Reuse the common detection shape for status documents.
    pub fn as_backend_detection(&self) -> BackendDetection {
        if self.available {
            BackendDetection::available(
                "drm",
                Some(format!(
                    "{} connected connector(s) of {} detected",
                    self.connected().count(),
                    self.connectors.len()
                )),
            )
        } else {
            BackendDetection::unavailable(
                "drm",
                self.reason
                    .clone()
                    .unwrap_or_else(|| "no connected DRM connector detected".to_owned()),
            )
        }
    }
}

/// Detect DRM connectors under the default `/sys/class/drm` root.
pub fn detect() -> DrmDetection {
    detect_at(Path::new(DEFAULT_DRM_ROOT))
}

/// Detect DRM connectors under an explicit sysfs root. Missing roots are a
/// clean unavailable result, not an error.
pub fn detect_at(root: &Path) -> DrmDetection {
    match detect_connectors(root) {
        Ok(connectors) => {
            let available = connectors
                .iter()
                .any(|connector| connector.status.is_connected());
            DrmDetection {
                backend: "drm".to_owned(),
                available,
                reason: if available {
                    None
                } else {
                    Some("no connected DRM connector detected".to_owned())
                },
                connectors,
            }
        }
        Err(error) => DrmDetection {
            backend: "drm".to_owned(),
            available: false,
            connectors: Vec::new(),
            reason: Some(error.to_string()),
        },
    }
}

/// Read all `card*-*` connector directories under `root`.
pub fn detect_connectors(root: &Path) -> Result<Vec<DrmConnector>, MultiseatError> {
    let directory = std::fs::read_dir(root).map_err(|error| MultiseatError::io(root, error))?;
    let mut connectors = Vec::new();

    for entry in directory {
        let entry = entry.map_err(|error| MultiseatError::io(root, error))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some((card, connector)) = name.split_once('-') else {
            continue;
        };
        if !card.starts_with("card") || connector.is_empty() {
            continue;
        }

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let status_path = path.join("status");
        let status = match std::fs::read_to_string(&status_path) {
            Ok(value) => DrmStatus::parse(&value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(MultiseatError::io(&status_path, error)),
        };

        let modes_path = path.join("modes");
        let modes = match std::fs::read_to_string(&modes_path) {
            Ok(value) => value
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(MultiseatError::io(&modes_path, error)),
        };

        connectors.push(DrmConnector {
            card: card.to_owned(),
            connector: connector.to_owned(),
            status,
            modes,
        });
    }

    connectors
        .sort_by(|left, right| (&left.card, &left.connector).cmp(&(&right.card, &right.connector)));
    Ok(connectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_known_values() {
        assert!(DrmStatus::parse("connected\n").is_connected());
        assert_eq!(DrmStatus::parse("disconnected"), DrmStatus::Disconnected);
        assert_eq!(
            DrmStatus::parse("writeback"),
            DrmStatus::Unknown("writeback".to_owned())
        );
    }
}
