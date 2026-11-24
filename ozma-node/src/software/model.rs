// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Normalized software inventory records shared by source adapters.
//!
//! The JSON shape mirrors the controller's `/api/v1/software` inventory API and
//! the Python model that preceded this module. Sources identify themselves with
//! a stable `source` plus a source-local `source_id` (Steam AppID, MSIX
//! PackageFamilyName, winget ID, package name, ...).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where an installation lives from the operating system's perspective.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftwareScope {
    /// Installed for the whole machine.
    System,
    /// Installed for one user/profile.
    User,
    /// Provisioned for future users but not installed for a current user.
    Provisioned,
    /// The source does not expose scope information.
    #[default]
    Unknown,
}

/// Whether the source or Ozma owns updating this title.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// Installed version matches the latest known version.
    Current,
    /// A newer version is known to exist.
    Outdated,
    /// The vendor owns updates; Ozma reports but does not patch in place.
    ExternalManaged,
    /// The source does not expose enough information to decide.
    #[default]
    Unknown,
    /// The source has no update mechanism Ozma can use.
    Unsupported,
}

/// One installed or provisioned software title.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SoftwareRecord {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub source_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub latest_version: String,
    #[serde(default)]
    pub install_path: String,
    #[serde(default)]
    pub scope: SoftwareScope,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub managed_by: String,
    #[serde(default)]
    pub update_state: UpdateState,
    #[serde(default)]
    pub license_state: String,
    #[serde(default)]
    pub identifiers: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl SoftwareRecord {
    /// Build a record with the fields every source must provide.
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            ..Self::default()
        }
    }

    /// Stable `(source, source_id)` identity used for deduplication.
    ///
    /// Falls back to the display name when a source cannot supply an id.
    pub fn identity(&self) -> (String, String) {
        let id = if self.source_id.is_empty() {
            self.name.clone()
        } else {
            self.source_id.clone()
        };
        (self.source.clone(), id)
    }

    /// Add a namespaced identifier.
    pub fn with_identifier(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.identifiers.insert(key.into(), value.into());
        self
    }

    /// Add source-specific metadata.
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// Deduplicate records by `(source, source_id)`, keeping the first occurrence.
pub fn deduplicate(records: impl IntoIterator<Item = SoftwareRecord>) -> Vec<SoftwareRecord> {
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();
    for record in records {
        if seen.insert(record.identity()) {
            result.push(record);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_and_update_state_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&UpdateState::ExternalManaged).unwrap(),
            "\"external_managed\""
        );
        assert_eq!(
            serde_json::to_string(&SoftwareScope::Provisioned).unwrap(),
            "\"provisioned\""
        );
    }

    #[test]
    fn record_serializes_controller_shape() {
        let record = SoftwareRecord::new("Half-Life", "steam")
            .with_identifier("steam_appid", "70")
            .with_metadata("buildid", serde_json::json!("123"));
        let value = serde_json::to_value(&record).unwrap();

        assert_eq!(value["name"], "Half-Life");
        assert_eq!(value["source"], "steam");
        assert_eq!(value["source_id"], "");
        assert_eq!(value["scope"], "unknown");
        assert_eq!(value["update_state"], "unknown");
        assert_eq!(value["identifiers"]["steam_appid"], "70");
        assert_eq!(value["metadata"]["buildid"], "123");
        assert_eq!(value["license_state"], "");
    }

    #[test]
    fn deduplicate_keeps_first_source_qualified_record() {
        let first = SoftwareRecord {
            name: "First".into(),
            source: "steam".into(),
            source_id: "70".into(),
            ..Default::default()
        };
        let duplicate = SoftwareRecord {
            name: "Duplicate".into(),
            source: "steam".into(),
            source_id: "70".into(),
            ..Default::default()
        };
        let other_source = SoftwareRecord {
            name: "Same id, different source".into(),
            source: "winget".into(),
            source_id: "70".into(),
            ..Default::default()
        };

        let records = deduplicate(vec![first, duplicate, other_source]);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "First");
        assert_eq!(records[1].source, "winget");
    }

    #[test]
    fn identity_falls_back_to_name_without_source_id() {
        let record = SoftwareRecord::new("Example", "snap");
        assert_eq!(
            record.identity(),
            ("snap".to_string(), "Example".to_string())
        );
    }
}
