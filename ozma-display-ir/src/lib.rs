// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Lossless display Widget IR and translation contracts.
//!
//! This crate owns the interchange model shared by importers, the asset
//! repository, and every render adapter. The typed fields are a semantic
//! projection for rendering; the complete source document, unknown fields,
//! opaque assets, fonts, and format extensions are retained alongside them.
//!
//! Imports are lossless. Exports are the only loss boundary and must carry a
//! [`TranslationReport`](model::TranslationReport).
//!
//! The JSON shapes mirror `schemas/display/*.schema.json` and the Python
//! definitions in `controller/display_ir.py`.

#![forbid(unsafe_code)]

use thiserror::Error;

pub mod extract;
pub mod model;
pub mod translate;
pub mod vibe;

pub use extract::*;
pub use model::*;
pub use translate::*;
pub use vibe::*;

/// Version of the compact IR projection stored in `DisplayDefinition.meta`.
pub const IR_SCHEMA: &str = "org.ozma.widget-ir/1";

/// Version of the vibe keyword taxonomy documented by the research notes.
pub const VIBE_TAXONOMY: &str = "org.ozma.vibe/1";

/// Typed errors returned by IR validation and translation helpers.
#[derive(Debug, Error)]
pub enum IrError {
    /// A required member was absent from an input document.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),

    /// A required string member was empty or whitespace only.
    #[error("`{0}` must not be empty")]
    EmptyField(&'static str),

    /// A widget used a kind outside the interchange vocabulary.
    #[error("unknown widget kind `{0}`")]
    UnknownWidgetKind(String),

    /// An asset used a kind outside the interchange vocabulary.
    #[error("unknown asset kind `{0}`")]
    UnknownAssetKind(String),

    /// An asset pack used a type outside the interchange vocabulary.
    #[error("unknown asset pack type `{0}`")]
    UnknownPackType(String),

    /// A source format was unknown to this first slice.
    #[error("unknown source format `{0}`")]
    UnknownSourceFormat(String),

    /// Two assets or fonts declared the same stable identifier.
    #[error("duplicate asset id `{0}`")]
    DuplicateAssetId(String),

    /// A SHA-256 digest was not 64 hexadecimal characters.
    #[error("invalid sha256 `{0}`: expected 64 hexadecimal characters")]
    InvalidSha256(String),

    /// A value was outside the permitted range or otherwise malformed.
    #[error("invalid `{field}`: {message}")]
    InvalidValue {
        /// Field name for diagnostics.
        field: &'static str,
        /// Human-readable explanation.
        message: String,
    },

    /// A source capture report claimed a lossy import.
    #[error("source capture for `{source_format}` is lossy; imports must be lossless")]
    LossySourceCapture {
        /// Source format named by the report.
        source_format: String,
    },

    /// An export was attempted without its mandatory translation report.
    #[error("export to `{target}` requires a TranslationReport")]
    MissingExportReport {
        /// Target format named by the caller.
        target: String,
    },

    /// A translation report was not an export report.
    #[error("translation report direction must be `export`, got `{0}`")]
    InvalidDirection(String),

    /// An asset-pack sync policy used an unknown mode.
    #[error("unknown asset sync mode `{0}`")]
    UnknownSyncMode(String),

    /// A value failed to parse as the expected JSON shape.
    #[error("invalid JSON for display IR: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias used by this crate.
pub type IrResult<T> = Result<T, IrError>;

/// Validate a lower- or upper-case 64-character SHA-256 hex digest.
pub fn validate_sha256(value: &str, field: &'static str) -> IrResult<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(IrError::InvalidSha256(format!("{field}={value}")));
    }
    Ok(())
}

/// Require a non-empty, non-whitespace string.
pub fn require_non_empty(value: &str, field: &'static str) -> IrResult<()> {
    if value.trim().is_empty() {
        return Err(IrError::EmptyField(field));
    }
    Ok(())
}

/// Require a finite value in the inclusive `0.0..=1.0` range.
pub fn require_unit_interval(value: f64, field: &'static str) -> IrResult<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(IrError::InvalidValue {
            field,
            message: format!("expected a number between 0 and 1, got {value}"),
        });
    }
    Ok(())
}

/// Require a finite, non-negative value.
pub fn require_non_negative(value: f64, field: &'static str) -> IrResult<()> {
    if !value.is_finite() || value < 0.0 {
        return Err(IrError::InvalidValue {
            field,
            message: format!("expected a finite non-negative number, got {value}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/display_ir")
            .join(name)
    }

    fn fixture_text(name: &str) -> String {
        std::fs::read_to_string(fixture(name)).expect("fixture is readable")
    }

    fn widget() -> WidgetIR {
        serde_json::from_str(&fixture_text("widget_ir.json")).expect("valid widget fixture")
    }

    #[test]
    fn widget_ir_round_trips_through_serde() {
        let original = widget();
        original.validate().expect("fixture validates");
        let encoded = serde_json::to_string(&original).expect("serializes");
        let decoded: WidgetIR = serde_json::from_str(&encoded).expect("deserializes");
        assert_eq!(original, decoded);
        assert_eq!(decoded.metric_key(), "org.ozma/node.cpu_temp");
        assert_eq!(
            decoded.extensions["org.example.widget"]["vendor"],
            "example"
        );
    }

    #[test]
    fn canonical_hash_is_stable_and_order_independent() {
        let left = json!({
            "z": 1,
            "a": [true, null, {"y": 1.0, "x": "Grüße"}],
        });
        let right = json!({
            "a": [true, null, {"x": "Grüße", "y": 1}],
            "z": 1,
        });
        assert_eq!(canonical_json(&left), canonical_json(&right));
        assert_eq!(canonical_hash(&left), canonical_hash(&right));
        assert_eq!(canonical_hash(&left).len(), 64);
        assert_ne!(
            canonical_hash(&left),
            canonical_hash(&json!({"z": 2, "a": []}))
        );
        assert_eq!(
            canonical_json(&json!({"b": 1.0, "a": -0.0})),
            r#"{"a":0,"b":1}"#
        );
    }

    #[test]
    fn validation_rejects_missing_source_and_lossy_imports() {
        let mut value: Value =
            serde_json::from_str(&fixture_text("widget_ir.json")).expect("valid JSON");
        value.as_object_mut().expect("object").remove("source");
        assert!(serde_json::from_value::<WidgetIR>(value).is_err());

        let mut capture: SourceCaptureReport =
            serde_json::from_str(&fixture_text("source_capture.json")).expect("valid capture");
        capture.validate_import().expect("fixture is lossless");
        capture.lossless = false;
        assert!(matches!(
            capture.validate_import(),
            Err(IrError::LossySourceCapture { .. })
        ));

        assert!(matches!(
            ensure_export_report(None, "browser_svg"),
            Err(IrError::MissingExportReport { .. })
        ));

        let mut missing_direction: Value =
            serde_json::from_str(&fixture_text("translation_report.json")).expect("valid JSON");
        missing_direction
            .as_object_mut()
            .expect("object")
            .remove("direction");
        assert!(serde_json::from_value::<TranslationReport>(missing_direction).is_err());
    }

    #[test]
    fn unknown_vibe_keywords_are_retained() {
        let profile: VibeProfile =
            serde_json::from_str(&fixture_text("vibe.json")).expect("valid vibe");
        profile.validate().expect("valid confidence");
        assert!(profile.all_keywords().contains(&"not_in_taxonomy"));
        assert_eq!(profile.extra["custom_axis"], json!(["retro"]));
        let encoded = serde_json::to_string(&profile).expect("serializes");
        assert!(encoded.contains("not_in_taxonomy"));
        assert!(encoded.contains("custom_axis"));
    }

    #[test]
    fn shape_and_coherence_bounds_are_enforced() {
        let parsed = widget();
        parsed
            .shape
            .expect("fixture shape")
            .validate()
            .expect("valid shape");

        let report: CoherenceReport = serde_json::from_value(json!({
            "score": 0.93,
            "dimensions": {"mood": 0.9, "color": 0.95},
            "confidence": 0.8
        }))
        .expect("valid coherence");
        report.validate().expect("in range");

        let bad_score: CoherenceReport = serde_json::from_value(json!({
            "score": 1.2,
            "confidence": 0.8
        }))
        .expect("parses");
        assert!(bad_score.validate().is_err());

        let bad_dimension: CoherenceReport = serde_json::from_value(json!({
            "score": 0.9,
            "dimensions": {"mood": 1.5},
            "confidence": 0.8
        }))
        .expect("parses");
        assert!(bad_dimension.validate().is_err());
    }

    #[test]
    fn ir_maps_to_screen_layout_and_definition_json() {
        let parsed = widget();
        let layout = parsed.to_screen_layout();
        assert_eq!(layout["id"], "org.ozma.widget.cpu_temp");
        assert_eq!(layout["widgets"][0]["type"], "gauge");
        assert_eq!(layout["widgets"][0]["metric"], "org.ozma/node.cpu_temp");
        assert_eq!(layout["widgets"][0]["field"], 1);
        assert_eq!(layout["widgets"][0]["custom_badge"], "new");
        assert_eq!(layout["fields"][0]["key"], "org.ozma/node.cpu_temp");
        assert_eq!(layout["fields"][0]["type"], "real");

        let definition = parsed.to_definition_json(3);
        assert_eq!(definition["id"], "org.ozma.widget.cpu_temp");
        assert_eq!(definition["version"], 3);
        assert_eq!(definition["meta"]["ozma_ir"]["schema"], IR_SCHEMA);
        assert_eq!(
            definition["meta"]["ozma_ir"]["widgets"][0]["source_hash"],
            parsed.source.source_hash
        );

        let bytes = serde_json::to_vec(&definition).expect("serializes");
        let document = ozma_display_definition::DisplayDefinitionDocument::parse(&bytes)
            .expect("definition schema remains compatible");
        assert_eq!(document.id(), "org.ozma.widget.cpu_temp");
        assert_eq!(document.version(), 3);
    }

    #[test]
    fn text_widget_maps_to_label_and_exports_have_reports() {
        let mut parsed = widget();
        parsed.kind = "text".to_owned();
        parsed.binding.value_type = None;
        let layout = parsed.to_screen_layout();
        assert_eq!(layout["widgets"][0]["type"], "label");
        assert_eq!(layout["fields"][0]["type"], "string");

        let report = export_report(
            &parsed,
            "browser_svg",
            "test-adapter",
            "0.1.0",
            Fidelity::Semantic,
        );
        report.validate_export().expect("export report validates");
        assert_eq!(
            ensure_export_report(Some(&report), "browser_svg")
                .expect("report present")
                .target_format,
            "browser_svg"
        );
    }

    #[test]
    fn asset_pack_and_sync_policy_validate() {
        let entry: AssetPackEntry =
            serde_json::from_str(&fixture_text("asset_pack.json")).expect("valid pack");
        entry.validate().expect("pack validates");
        entry
            .require_export_report("browser_svg")
            .expect("export report present");
        assert!(entry.require_export_report("lvgl").is_err());

        let mut lossy = entry.clone();
        lossy.source_capture.as_mut().expect("capture").lossless = false;
        assert!(lossy.validate().is_err());

        let mut no_reports = entry.clone();
        no_reports.translation_reports.clear();
        assert!(no_reports.validate().is_err());

        let policy: AssetSyncPolicy =
            serde_json::from_value(json!({"mode": "offline"})).expect("defaults");
        policy.validate().expect("defaults are valid");
        assert_eq!(policy.sync_interval_hours, 24);
        assert!(policy.include_previews);
        assert!(!policy.verify_signatures);

        let invalid: AssetSyncPolicy =
            serde_json::from_value(json!({"mode": "sometimes"})).expect("parses");
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn report_fixtures_parse_and_validate() {
        let capture: SourceCaptureReport =
            serde_json::from_str(&fixture_text("source_capture.json")).expect("capture");
        capture.validate_import().expect("lossless");
        let report: TranslationReport =
            serde_json::from_str(&fixture_text("translation_report.json")).expect("report");
        report.validate_export().expect("export");
        assert_eq!(report.direction, TranslationDirection::Export);
        assert_eq!(report.fidelity, Fidelity::Semantic);
    }
}
