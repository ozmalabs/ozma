// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Lossless Widget IR data model, validation, and canonical hashing.
//!
//! The model deliberately keeps every source artifact: [`WidgetIR::source`]
//! holds the complete [`SourceDocument`], [`WidgetIR::assets`] holds opaque
//! images/scripts/raw files, and [`WidgetIR::extensions`] holds format-specific
//! data that the typed projection does not model.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::{
    require_non_empty, require_non_negative, require_unit_interval, validate_sha256,
    CoherenceReport, IrError, IrResult, ShapeSignature, VibeProfile,
};

/// Largest integer that an `f64` can represent without losing precision.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_992.0;

/// Widget kinds understood by the interchange model.
pub const WIDGET_KINDS: &[&str] = &[
    "gauge",
    "bar",
    "number",
    "text",
    "sparkline",
    "chart",
    "status",
    "icon",
    "table",
    "card",
    "custom",
];

/// Asset kinds understood by the interchange model.
pub const ASSET_KINDS: &[&str] = &[
    "original", "image", "font", "script", "theme", "preview", "other",
];

/// Executable sandbox modes.
pub const SANDBOX_MODES: &[&str] = &["iframe", "wasm", "none"];

/// Asset-pack types understood by the repository.
pub const PACK_TYPES: &[&str] = &[
    "widget_pack",
    "theme_pack",
    "description_pack",
    "import_pack",
];

/// Pack confidence vocabulary shared with the device database.
pub const PACK_CONFIDENCE: &[&str] = &["exact", "approximate", "community", "template"];

/// Provenance kinds.
pub const PROVENANCE_KINDS: &[&str] = &["original", "imported", "adapted", "community"];

/// Asset sync modes.
pub const SYNC_MODES: &[&str] = &["full", "installed", "manual", "offline"];

/// A semantic widget projection plus its lossless source record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetIR {
    /// Stable widget identifier within the owning pack/scene.
    pub id: String,
    /// Interchange widget kind (see [`WIDGET_KINDS`]).
    pub kind: String,
    /// Semantic role such as `cpu_temp`, `fan_rpm`, or `threshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Data binding; reused by the display protocol's field identity.
    pub binding: DataBinding,
    /// Value range, thresholds, and severity bands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<ValueDomain>,
    /// History/sample policy for sparklines and charts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryPolicy>,
    /// Human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Icon reference, usually a named icon or an asset id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconRef>,
    /// Interaction targets attached to the widget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionRef>,
    /// Accessibility intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<Accessibility>,
    /// Target-independent layout hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutHint>,
    /// Design-token references, never raw target CSS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<StyleRef>,
    /// Font intent and typography metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typography: Option<TypographyRef>,
    /// Original or embedded font assets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fonts: Vec<FontAssetRef>,
    /// Intended design language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vibe: Option<VibeProfile>,
    /// Target-independent geometry signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeSignature>,
    /// Complete lossless source record.
    pub source: SourceDocument,
    /// Format-specific extensions retained verbatim.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extensions: Map<String, Value>,
    /// Images, fonts, scripts, previews, and raw source bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<AssetRef>,
}

impl WidgetIR {
    /// Validate the typed projection and its retained source/assets.
    pub fn validate(&self) -> IrResult<()> {
        require_non_empty(&self.id, "id")?;
        if !WIDGET_KINDS.contains(&self.kind.as_str()) {
            return Err(IrError::UnknownWidgetKind(self.kind.clone()));
        }
        require_non_empty(&self.binding.metric, "binding.metric")?;
        self.binding.validate()?;
        self.source.validate()?;
        if let Some(domain) = &self.domain {
            domain.validate()?;
        }
        if let Some(history) = &self.history {
            history.validate()?;
        }
        if let Some(layout) = &self.layout {
            layout.validate()?;
        }
        if let Some(typography) = &self.typography {
            typography.validate()?;
        }
        if let Some(vibe) = &self.vibe {
            vibe.validate()?;
        }
        if let Some(shape) = &self.shape {
            shape.validate()?;
        }
        for action in &self.actions {
            action.validate()?;
        }
        self.validate_asset_ids()?;
        Ok(())
    }

    /// Validate unique asset and font identifiers and each asset reference.
    pub fn validate_asset_ids(&self) -> IrResult<()> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for asset in &self.assets {
            asset.validate()?;
            if !seen.insert(asset.id.as_str()) {
                return Err(IrError::DuplicateAssetId(asset.id.clone()));
            }
        }
        for font in &self.fonts {
            font.validate()?;
            if !seen.insert(font.id.as_str()) {
                return Err(IrError::DuplicateAssetId(font.id.clone()));
            }
        }
        Ok(())
    }

    /// The bound metric key.
    pub fn metric_key(&self) -> &str {
        &self.binding.metric
    }
}

/// Data binding for a widget value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataBinding {
    /// Namespaced metric key, for example `org.ozma/node.cpu_temp`.
    pub metric: String,
    /// Optional source family (`node`, `mqtt`, `home_assistant`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Unit declared by the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Scaled-integer multiplier (the display protocol's `scale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Display formatter, for example `fixed(1)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Optional compiled field identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_id: Option<u32>,
    /// Display-protocol value type (`sint`, `uint`, `real`, `bool`, `string`,
    /// `enum`, `bytes`, or `timestamp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    /// Scene property populated by the binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
}

impl DataBinding {
    fn validate(&self) -> IrResult<()> {
        if let Some(scale) = self.scale {
            if !scale.is_finite() {
                return Err(IrError::InvalidValue {
                    field: "binding.scale",
                    message: "must be finite".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Value range and severity vocabulary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValueDomain {
    /// Minimum expected value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Maximum expected value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Nominal/expected value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nominal: Option<f64>,
    /// Point thresholds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thresholds: Vec<Threshold>,
    /// Ranged severity bands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub severity_bands: Vec<SeverityBand>,
}

impl ValueDomain {
    /// Validate coherent bounds and finite threshold values.
    pub fn validate(&self) -> IrResult<()> {
        for (name, value) in [
            ("domain.min", self.min),
            ("domain.max", self.max),
            ("domain.nominal", self.nominal),
        ] {
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(IrError::InvalidValue {
                        field: name,
                        message: "must be finite".to_owned(),
                    });
                }
            }
        }
        if let (Some(min), Some(max)) = (self.min, self.max) {
            if min > max {
                return Err(IrError::InvalidValue {
                    field: "domain",
                    message: format!("min {min} is greater than max {max}"),
                });
            }
        }
        for threshold in &self.thresholds {
            threshold.validate()?;
        }
        for band in &self.severity_bands {
            band.validate()?;
        }
        Ok(())
    }
}

/// A threshold at one value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Threshold {
    /// Threshold value.
    pub at: f64,
    /// Severity name (`info`, `warning`, `critical`, ...).
    pub severity: String,
    /// Optional label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional design-token or colour reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl Threshold {
    fn validate(&self) -> IrResult<()> {
        if !self.at.is_finite() {
            return Err(IrError::InvalidValue {
                field: "domain.thresholds[].at",
                message: "must be finite".to_owned(),
            });
        }
        require_non_empty(&self.severity, "domain.thresholds[].severity")?;
        Ok(())
    }
}

/// A ranged severity band.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeverityBand {
    /// Inclusive lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Inclusive upper bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Severity name.
    pub severity: String,
    /// Optional label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl SeverityBand {
    fn validate(&self) -> IrResult<()> {
        for value in [self.min, self.max].into_iter().flatten() {
            if !value.is_finite() {
                return Err(IrError::InvalidValue {
                    field: "domain.severity_bands[]",
                    message: "bounds must be finite".to_owned(),
                });
            }
        }
        if let (Some(min), Some(max)) = (self.min, self.max) {
            if min > max {
                return Err(IrError::InvalidValue {
                    field: "domain.severity_bands[]",
                    message: format!("min {min} is greater than max {max}"),
                });
            }
        }
        require_non_empty(&self.severity, "domain.severity_bands[].severity")?;
        Ok(())
    }
}

/// History sampling policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryPolicy {
    /// Rolling window in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<f64>,
    /// Sample rate in hertz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<f64>,
    /// Aggregation name (`mean`, `min`, `max`, `last`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation: Option<String>,
    /// Optional retained point count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_points: Option<u32>,
}

impl HistoryPolicy {
    fn validate(&self) -> IrResult<()> {
        if let Some(window) = self.window_seconds {
            require_non_negative(window, "history.window_seconds")?;
        }
        if let Some(rate) = self.sample_rate_hz {
            if !rate.is_finite() || rate <= 0.0 {
                return Err(IrError::InvalidValue {
                    field: "history.sample_rate_hz",
                    message: "must be finite and greater than zero".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Icon reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IconRef {
    /// Named icon from the active icon set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Referenced asset identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// Optional icon-set identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<String>,
}

/// Interaction target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionRef {
    /// Namespaced action key.
    pub key: String,
    /// Optional compiled action identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<u32>,
    /// Optional display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Static parameters sent with the invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Declared permissions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
}

impl ActionRef {
    fn validate(&self) -> IrResult<()> {
        require_non_empty(&self.key, "actions[].key")
    }
}

/// Accessibility intent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Accessibility {
    /// Accessible name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Longer description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// ARIA-style role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Keyboard shortcut hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_shortcut: Option<String>,
    /// Whether the widget respects reduced-motion preferences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduced_motion: Option<bool>,
}

/// Target-independent layout hint.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LayoutHint {
    /// Logical x coordinate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    /// Logical y coordinate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// Logical width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Logical height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Layout zone or region name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// Responsive priority; larger values are retained first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responsive_priority: Option<i32>,
    /// Declared aspect ratio (width / height).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<f64>,
}

impl LayoutHint {
    /// Validate positive sizes and finite coordinates.
    pub fn validate(&self) -> IrResult<()> {
        for (name, value) in [
            ("layout.x", self.x),
            ("layout.y", self.y),
            ("layout.width", self.width),
            ("layout.height", self.height),
        ] {
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(IrError::InvalidValue {
                        field: name,
                        message: "must be finite".to_owned(),
                    });
                }
            }
        }
        for (name, value) in [("layout.width", self.width), ("layout.height", self.height)] {
            if let Some(value) = value {
                if value <= 0.0 {
                    return Err(IrError::InvalidValue {
                        field: name,
                        message: "must be greater than zero".to_owned(),
                    });
                }
            }
        }
        if let Some(ratio) = self.aspect_ratio {
            if !ratio.is_finite() || ratio <= 0.0 {
                return Err(IrError::InvalidValue {
                    field: "layout.aspect_ratio",
                    message: "must be finite and greater than zero".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Design-token references.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StyleRef {
    /// Theme identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Token name to token value/reference.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub tokens: Map<String, Value>,
    /// Style class references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<String>,
}

/// Typography intent and metrics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TypographyRef {
    /// Font-intent identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Size in CSS pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_px: Option<f64>,
    /// Size in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_pt: Option<f64>,
    /// Line height multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f64>,
    /// Tracking in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking: Option<f64>,
    /// Numeric weight (100..900).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// OpenType feature tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl TypographyRef {
    fn validate(&self) -> IrResult<()> {
        for (name, value) in [
            ("typography.size_px", self.size_px),
            ("typography.size_pt", self.size_pt),
        ] {
            if let Some(value) = value {
                if !value.is_finite() || value <= 0.0 {
                    return Err(IrError::InvalidValue {
                        field: name,
                        message: "must be finite and greater than zero".to_owned(),
                    });
                }
            }
        }
        for (name, value) in [
            ("typography.line_height", self.line_height),
            ("typography.tracking", self.tracking),
        ] {
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(IrError::InvalidValue {
                        field: name,
                        message: "must be finite".to_owned(),
                    });
                }
            }
        }
        if let Some(weight) = self.weight {
            if !(1..=1000).contains(&weight) {
                return Err(IrError::InvalidValue {
                    field: "typography.weight",
                    message: format!("expected 1..=1000, got {weight}"),
                });
            }
        }
        Ok(())
    }
}

/// A retained image, font, script, preview, or raw source file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssetRef {
    /// Stable asset identifier within the pack/widget.
    pub id: String,
    /// Relative path inside the pack.
    pub path: String,
    /// MIME type.
    pub mime: String,
    /// SHA-256 digest of the bytes.
    pub sha256: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Asset kind (see [`ASSET_KINDS`]).
    pub kind: String,
    /// Whether the asset is executable code.
    #[serde(default)]
    pub executable: bool,
    /// Sandbox required for executable assets (`iframe`, `wasm`, or `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

impl AssetRef {
    /// Validate identifiers, digest shape, kind, and sandbox policy.
    pub fn validate(&self) -> IrResult<()> {
        require_non_empty(&self.id, "assets[].id")?;
        require_non_empty(&self.path, "assets[].path")?;
        require_non_empty(&self.mime, "assets[].mime")?;
        validate_sha256(&self.sha256, "assets[].sha256")?;
        if !ASSET_KINDS.contains(&self.kind.as_str()) {
            return Err(IrError::UnknownAssetKind(self.kind.clone()));
        }
        if let Some(sandbox) = &self.sandbox {
            if !SANDBOX_MODES.contains(&sandbox.as_str()) {
                return Err(IrError::InvalidValue {
                    field: "assets[].sandbox",
                    message: format!("expected one of {SANDBOX_MODES:?}, got `{sandbox}`"),
                });
            }
        }
        if self.executable {
            match self.sandbox.as_deref() {
                Some("iframe") | Some("wasm") => {}
                _ => {
                    return Err(IrError::InvalidValue {
                        field: "assets[].sandbox",
                        message: "executable assets require an iframe or wasm sandbox".to_owned(),
                    })
                }
            }
        }
        Ok(())
    }
}

/// A retained font asset with optional metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FontAssetRef {
    /// Stable font asset identifier.
    pub id: String,
    /// Relative path inside the pack.
    pub path: String,
    /// MIME type.
    pub mime: String,
    /// SHA-256 digest of the bytes.
    pub sha256: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Asset kind, normally `font`.
    pub kind: String,
    /// Whether the asset is executable code.
    #[serde(default)]
    pub executable: bool,
    /// Sandbox required for executable assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Family name from the font name table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Style name (`Regular`, `Bold`, `Italic`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Container format (`ttf`, `otf`, `woff2`, `bdf`, `pcf`, `bitmap`, `lvgl_c`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// SPDX licence expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Whether embedding is permitted by the font licence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_allowed: Option<bool>,
    /// Imported fonts stay private unless explicitly published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

impl FontAssetRef {
    /// Validate the underlying asset and font metadata.
    pub fn validate(&self) -> IrResult<()> {
        let asset = AssetRef {
            id: self.id.clone(),
            path: self.path.clone(),
            mime: self.mime.clone(),
            sha256: self.sha256.clone(),
            size: self.size,
            kind: self.kind.clone(),
            executable: self.executable,
            sandbox: self.sandbox.clone(),
        };
        asset.validate()
    }
}

/// Complete source document retained by an import.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceDocument {
    /// Source format identifier.
    pub format: String,
    /// Optional source format version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// SHA-256 digest of the complete source payload.
    pub source_hash: String,
    /// Original bytes retained as a private asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<AssetRef>,
    /// Full format-specific parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parsed: Option<Value>,
    /// Every field the typed IR does not model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<Value>,
}

impl SourceDocument {
    /// Validate the source format and retained digest/assets.
    pub fn validate(&self) -> IrResult<()> {
        require_non_empty(&self.format, "source.format")?;
        validate_sha256(&self.source_hash, "source.source_hash")?;
        if let Some(original) = &self.original {
            original.validate()?;
        }
        Ok(())
    }
}

/// Lossless import report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceCaptureReport {
    /// Format that was imported.
    pub source_format: String,
    /// Optional source format version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version: Option<String>,
    /// SHA-256 digest of the captured source.
    pub source_hash: String,
    /// Source features represented in the typed IR.
    #[serde(default)]
    pub captured: Vec<String>,
    /// Source features retained but not interpreted.
    #[serde(default)]
    pub opaque: Vec<String>,
    /// Retained asset identifiers.
    #[serde(default)]
    pub assets: Vec<String>,
    /// Must be `true` for imports.
    pub lossless: bool,
}

impl SourceCaptureReport {
    /// Validate an import capture report; lossy imports are rejected.
    pub fn validate_import(&self) -> IrResult<()> {
        require_non_empty(&self.source_format, "source_capture.source_format")?;
        validate_sha256(&self.source_hash, "source_capture.source_hash")?;
        if !self.lossless {
            return Err(IrError::LossySourceCapture {
                source_format: self.source_format.clone(),
            });
        }
        Ok(())
    }
}

/// Translation direction. Only exports are representable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationDirection {
    /// Data leaves the IR for a render target.
    #[default]
    Export,
}

/// Export fidelity vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Target represents the same semantic widget and properties.
    Exact,
    /// Same meaning and data binding; target-specific styling differs.
    Semantic,
    /// Meaning preserved but some properties inferred, simplified, or dropped.
    Approximate,
    /// Geometry/text preserved; behaviour or data binding lost.
    VisualOnly,
}

/// Mandatory report emitted by every export adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranslationReport {
    /// Always `export`.
    pub direction: TranslationDirection,
    /// Format the IR was translated from.
    pub source_format: String,
    /// Target format.
    pub target_format: String,
    /// Translator name.
    pub translator: String,
    /// Translator version.
    pub translator_version: String,
    /// Export fidelity.
    pub fidelity: Fidelity,
    /// Features represented exactly.
    #[serde(default)]
    pub converted: Vec<String>,
    /// Features represented approximately.
    #[serde(default)]
    pub approximated: Vec<String>,
    /// Features dropped by the target.
    #[serde(default)]
    pub dropped: Vec<String>,
    /// Adapter warnings.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl TranslationReport {
    /// Validate an export report.
    pub fn validate_export(&self) -> IrResult<()> {
        if self.direction != TranslationDirection::Export {
            return Err(IrError::InvalidDirection("non-export".to_owned()));
        }
        require_non_empty(&self.source_format, "translation_report.source_format")?;
        require_non_empty(&self.target_format, "translation_report.target_format")?;
        require_non_empty(&self.translator, "translation_report.translator")?;
        require_non_empty(
            &self.translator_version,
            "translation_report.translator_version",
        )?;
        Ok(())
    }
}

/// Require the mandatory export report for a target.
pub fn ensure_export_report<'a>(
    report: Option<&'a TranslationReport>,
    target: &str,
) -> IrResult<&'a TranslationReport> {
    match report {
        Some(report) => {
            report.validate_export()?;
            Ok(report)
        }
        None => Err(IrError::MissingExportReport {
            target: target.to_owned(),
        }),
    }
}

/// Provenance record for a pack or imported asset.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// One of [`PROVENANCE_KINDS`].
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional source URL or description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Extraction/import method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Free-form notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Provenance {
    fn validate(&self) -> IrResult<()> {
        if !PROVENANCE_KINDS.contains(&self.kind.as_str()) {
            return Err(IrError::InvalidValue {
                field: "provenance.type",
                message: format!("expected one of {PROVENANCE_KINDS:?}, got `{}`", self.kind),
            });
        }
        Ok(())
    }
}

/// Searchable asset-pack manifest entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssetPackEntry {
    /// Pack identifier.
    pub id: String,
    /// One of [`PACK_TYPES`].
    #[serde(rename = "type")]
    pub pack_type: String,
    /// Human-readable name.
    pub name: String,
    /// Semantic version.
    pub version: String,
    /// Asset-pack schema identifier (`org.ozma.asset-pack/1`).
    pub schema: String,
    /// Author or maintainer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// SPDX licence expression.
    pub license: String,
    /// Upstream URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Search tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Capabilities (`rendered_widget`, `data_provider`, ...).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Permissions (`internet`, `ha_read`, ...).
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Minimum Ozma version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ozma_version: Option<String>,
    /// Minimum display schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_display_schema: Option<String>,
    /// Pack files.
    #[serde(default)]
    pub assets: Vec<AssetRef>,
    /// Optional preview asset path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Provenance record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Pack confidence vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Design-language intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vibe: Option<VibeProfile>,
    /// Geometry signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeSignature>,
    /// Pack-level coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coherence: Option<CoherenceReport>,
    /// Detached signature over manifest and checksums.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Publication timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Yanked versions are never installed automatically.
    #[serde(default)]
    pub yanked: bool,
    /// Lossless import report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_capture: Option<SourceCaptureReport>,
    /// Reports for every produced render target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub translation_reports: Vec<TranslationReport>,
}

impl AssetPackEntry {
    /// Validate the manifest, lossless import capture, and export reports.
    pub fn validate(&self) -> IrResult<()> {
        require_non_empty(&self.id, "id")?;
        if !PACK_TYPES.contains(&self.pack_type.as_str()) {
            return Err(IrError::UnknownPackType(self.pack_type.clone()));
        }
        require_non_empty(&self.name, "name")?;
        require_non_empty(&self.version, "version")?;
        require_non_empty(&self.schema, "schema")?;
        require_non_empty(&self.license, "license")?;
        if let Some(confidence) = &self.confidence {
            if !PACK_CONFIDENCE.contains(&confidence.as_str()) {
                return Err(IrError::InvalidValue {
                    field: "confidence",
                    message: format!("expected one of {PACK_CONFIDENCE:?}, got `{confidence}`"),
                });
            }
        }
        if let Some(provenance) = &self.provenance {
            provenance.validate()?;
        }
        let mut seen = BTreeSet::new();
        for asset in &self.assets {
            asset.validate()?;
            if !seen.insert(asset.id.as_str()) {
                return Err(IrError::DuplicateAssetId(asset.id.clone()));
            }
        }
        if let Some(vibe) = &self.vibe {
            vibe.validate()?;
        }
        if let Some(shape) = &self.shape {
            shape.validate()?;
        }
        if let Some(coherence) = &self.coherence {
            coherence.validate()?;
        }
        if let Some(capture) = &self.source_capture {
            capture.validate_import()?;
            if self.translation_reports.is_empty() {
                return Err(IrError::InvalidValue {
                    field: "translation_reports",
                    message: "an imported pack requires at least one export report".to_owned(),
                });
            }
        }
        for report in &self.translation_reports {
            report.validate_export()?;
        }
        Ok(())
    }

    /// Require a valid export report for one target format.
    pub fn require_export_report(&self, target_format: &str) -> IrResult<&TranslationReport> {
        let report = self
            .translation_reports
            .iter()
            .find(|report| report.target_format == target_format);
        ensure_export_report(report, target_format)
    }
}

/// Local cache/sync policy for the asset repository.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssetSyncPolicy {
    /// One of [`SYNC_MODES`].
    pub mode: String,
    /// Sync interval in hours.
    #[serde(default = "default_sync_interval")]
    pub sync_interval_hours: u32,
    /// Optional cache size limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_size_mb: Option<u32>,
    /// Whether previews are downloaded.
    #[serde(default = "default_true")]
    pub include_previews: bool,
    /// Whether private external imports may be downloaded. Never uploaded.
    #[serde(default)]
    pub include_external_imports: bool,
    /// Whether signatures are verified before installation.
    #[serde(default)]
    pub verify_signatures: bool,
}

fn default_sync_interval() -> u32 {
    24
}

fn default_true() -> bool {
    true
}

impl Default for AssetSyncPolicy {
    fn default() -> Self {
        Self {
            mode: "installed".to_owned(),
            sync_interval_hours: default_sync_interval(),
            cache_size_mb: None,
            include_previews: true,
            include_external_imports: false,
            verify_signatures: false,
        }
    }
}

impl AssetSyncPolicy {
    /// Validate the sync mode and interval.
    pub fn validate(&self) -> IrResult<()> {
        if !SYNC_MODES.contains(&self.mode.as_str()) {
            return Err(IrError::UnknownSyncMode(self.mode.clone()));
        }
        if self.sync_interval_hours == 0 {
            return Err(IrError::InvalidValue {
                field: "sync_interval_hours",
                message: "must be greater than zero".to_owned(),
            });
        }
        if let Some(size) = self.cache_size_mb {
            if size == 0 {
                return Err(IrError::InvalidValue {
                    field: "cache_size_mb",
                    message: "must be greater than zero when present".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Canonical, deterministic JSON for a serde value.
///
/// Object keys are sorted, arrays keep their order, and equivalent numeric
/// spellings are normalised (`1.0` becomes `1`, `-0.0` becomes `0`).
pub fn canonical_json(value: &Value) -> String {
    let mut encoded = Vec::new();
    write_canonical(value, &mut encoded);
    String::from_utf8(encoded).expect("canonical JSON is always UTF-8")
}

/// BLAKE3 digest of [`canonical_json`], as lowercase hexadecimal.
pub fn canonical_hash(value: &Value) -> String {
    canonical_hash_bytes(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// BLAKE3 digest of [`canonical_json`].
pub fn canonical_hash_bytes(value: &Value) -> [u8; 32] {
    *blake3::hash(canonical_json(value).as_bytes()).as_bytes()
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => write_canonical_number(number, out),
        Value::String(text) => {
            let encoded =
                serde_json::to_string(text).expect("serializing a JSON string cannot fail");
            out.extend_from_slice(encoded.as_bytes());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(object) => {
            out.push(b'{');
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let encoded =
                    serde_json::to_string(key).expect("serializing a JSON key cannot fail");
                out.extend_from_slice(encoded.as_bytes());
                out.push(b':');
                write_canonical(&object[key], out);
            }
            out.push(b'}');
        }
    }
}

fn write_canonical_number(number: &Number, out: &mut Vec<u8>) {
    if let Some(integer) = number.as_i64() {
        out.extend_from_slice(integer.to_string().as_bytes());
        return;
    }
    if let Some(integer) = number.as_u64() {
        out.extend_from_slice(integer.to_string().as_bytes());
        return;
    }
    if let Some(real) = number.as_f64() {
        if real == 0.0 {
            out.push(b'0');
            return;
        }
        if real.is_finite() && real.fract() == 0.0 && real.abs() <= MAX_SAFE_INTEGER {
            out.extend_from_slice((real as i64).to_string().as_bytes());
            return;
        }
    }

    let text = number.to_string();
    if let Some((mantissa, exponent)) = text.split_once(['e', 'E']) {
        let (sign, digits) = match exponent.strip_prefix('-') {
            Some(digits) => ("-", digits),
            None => ("", exponent.strip_prefix('+').unwrap_or(exponent)),
        };
        let digits = digits.trim_start_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        out.extend_from_slice(format!("{mantissa}e{sign}{digits}").as_bytes());
    } else {
        out.extend_from_slice(text.as_bytes());
    }
}

/// Validate a vibe confidence in addition to the shared unit-interval rule.
pub(crate) fn validate_confidence(value: f64, field: &'static str) -> IrResult<()> {
    require_unit_interval(value, field)
}
