// SPDX-License-Identifier: AGPL-3.0-only
//! Protocol version negotiation.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::error::ApiError;

/// A `major.minor` protocol version. Compatibility is decided by the major
/// number; a newer minor is allowed unless a feature flag says otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

/// The version emitted by this build.
pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Same major and the peer does not require a newer minor than we know.
    pub fn is_compatible_with(self, peer: Self) -> bool {
        self.major == peer.major && peer.minor <= self.minor
    }

    /// Pick the highest common version, or explain why there is none.
    pub fn negotiate(local: Self, remote: Self) -> crate::Result<Self> {
        if local.major != remote.major {
            return Err(ApiError::new(
                crate::error::codes::PROTOCOL_VERSION_MISMATCH,
                format!(
                    "Protocol major version {} is not compatible with {}",
                    remote.major, local.major
                ),
            )
            .with_remediation("Upgrade the node, controller, or client to the same major version."));
        }
        Ok(Self {
            major: local.major,
            minor: local.minor.min(remote.minor),
        })
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        CURRENT_PROTOCOL_VERSION
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl FromStr for ProtocolVersion {
    type Err = ApiError;

    fn from_str(value: &str) -> crate::Result<Self> {
        let value = value.trim().trim_start_matches("ozma/");
        let (major, minor) = value.split_once('.').ok_or_else(|| {
            ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                format!("Invalid protocol version '{value}'; expected major.minor"),
            )
        })?;
        Ok(Self {
            major: major.parse().map_err(|_| {
                ApiError::new(
                    crate::error::codes::INVALID_REQUEST,
                    format!("Invalid protocol major version '{major}'"),
                )
            })?,
            minor: minor.parse().map_err(|_| {
                ApiError::new(
                    crate::error::codes::INVALID_REQUEST,
                    format!("Invalid protocol minor version '{minor}'"),
                )
            })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        let v: ProtocolVersion = "ozma/1.3".parse().unwrap();
        assert_eq!(v, ProtocolVersion::new(1, 3));
        assert_eq!(v.to_string(), "1.3");
    }

    #[test]
    fn negotiates_down_to_the_older_minor() {
        assert_eq!(
            ProtocolVersion::negotiate(ProtocolVersion::new(1, 4), ProtocolVersion::new(1, 2))
                .unwrap(),
            ProtocolVersion::new(1, 2)
        );
    }

    #[test]
    fn rejects_different_major() {
        assert!(
            ProtocolVersion::negotiate(ProtocolVersion::new(2, 0), ProtocolVersion::new(1, 9))
                .is_err()
        );
    }
}
