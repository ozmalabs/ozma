// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Vibe profile, shape signature, and advisory coherence scoring.
//!
//! Keywords are design-language intent, not a quality judgement. Source and
//! user keywords are preserved verbatim; derived keywords carry provenance and
//! confidence. Unknown taxonomy values remain keywords rather than being
//! dropped.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::validate_confidence;
use crate::{require_non_negative, require_unit_interval, IrResult, VIBE_TAXONOMY};

/// Versioned design-language intent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VibeProfile {
    /// Taxonomy identifier, normally [`VIBE_TAXONOMY`].
    #[serde(default = "default_taxonomy")]
    pub taxonomy: String,
    /// Free-form keywords, preserved exactly.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Mood keywords (`calm`, `alert`, `urgent`, ...).
    #[serde(default)]
    pub mood: Vec<String>,
    /// Optional era (`aviation_1950s`, `synth_1980s`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub era: Option<String>,
    /// Form keywords (`circular`, `segmented`, `dot_matrix`, ...).
    #[serde(default)]
    pub form: Vec<String>,
    /// Material keywords (`brushed_metal`, `crt`, `eink`, ...).
    #[serde(default)]
    pub material: Vec<String>,
    /// Optional density (`sparse`, `balanced`, `dense`, `dashboard`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<String>,
    /// Colour-language keywords.
    #[serde(default)]
    pub color: Vec<String>,
    /// Typography-language keywords.
    #[serde(default)]
    pub typography: Vec<String>,
    /// Motion-language keywords.
    #[serde(default)]
    pub motion: Vec<String>,
    /// Reference styles (`aviation`, `noc`, `sci_fi`, ...).
    #[serde(default)]
    pub references: Vec<String>,
    /// Ornament language (`minimal`, `skeuomorphic`, `brutalist`, ...).
    #[serde(default)]
    pub ornament: Vec<String>,
    /// Confidence in the profile, `0.0..=1.0`.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// Keyword provenance.
    #[serde(default)]
    pub provenance: VibeProvenance,
    /// Unknown taxonomy fields, retained verbatim so newer producers do not
    /// lose information on an older reader.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

fn default_taxonomy() -> String {
    VIBE_TAXONOMY.to_owned()
}

fn default_confidence() -> f64 {
    1.0
}

impl Default for VibeProfile {
    fn default() -> Self {
        Self {
            taxonomy: default_taxonomy(),
            keywords: Vec::new(),
            mood: Vec::new(),
            era: None,
            form: Vec::new(),
            material: Vec::new(),
            density: None,
            color: Vec::new(),
            typography: Vec::new(),
            motion: Vec::new(),
            references: Vec::new(),
            ornament: Vec::new(),
            confidence: default_confidence(),
            provenance: VibeProvenance::default(),
            extra: Map::new(),
        }
    }
}

impl VibeProfile {
    /// Validate confidence and taxonomy identity.
    pub fn validate(&self) -> IrResult<()> {
        if self.taxonomy.trim().is_empty() {
            return Err(crate::IrError::EmptyField("vibe.taxonomy"));
        }
        validate_confidence(self.confidence, "vibe.confidence")
    }

    /// Every free-form keyword, including values outside the taxonomy.
    pub fn all_keywords(&self) -> Vec<&str> {
        self.keywords.iter().map(String::as_str).collect()
    }
}

/// Provenance for vibe keywords.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VibeProvenance {
    /// Keywords exactly as supplied by the source.
    #[serde(default)]
    pub source_keywords: Vec<String>,
    /// Keywords derived from geometry, palette, fonts, or vision.
    #[serde(default)]
    pub derived_keywords: Vec<String>,
    /// Human overrides, highest priority.
    #[serde(default)]
    pub user_keywords: Vec<String>,
    /// Extraction methods (`manifest`, `geometry`, `palette`, `font`, `vision`, `human`).
    #[serde(default)]
    pub methods: Vec<String>,
}

/// Target-independent geometry signature.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShapeSignature {
    /// Widget kind this signature describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget_kind: Option<String>,
    /// Width / height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<f64>,
    /// Corner radius relative to the shorter side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner_radius_ratio: Option<f64>,
    /// Stroke width relative to the shorter side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width_ratio: Option<f64>,
    /// Filled area fraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_ratio: Option<f64>,
    /// Symmetry (`none`, `vertical`, `horizontal`, `radial`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symmetry: Option<String>,
    /// Arc start angle in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_start_deg: Option<f64>,
    /// Arc end angle in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_end_deg: Option<f64>,
    /// Tick count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_count: Option<u32>,
    /// Segment count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_count: Option<u32>,
    /// Needle style (`classic`, `modern`, `minimal`, `sword`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needle_style: Option<String>,
    /// Chart mark (`line`, `area`, `bar`, `dot`, `step`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart_mark: Option<String>,
    /// Texture (`flat`, `noise`, `scanline`, `grid`, `glossy`, `matte`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture: Option<String>,
    /// Elevation (`flat`, `inset`, `raised`, `floating`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation: Option<String>,
    /// Whether the shape glows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glow: Option<bool>,
    /// Animation style (`none`, `needle`, `pulse`, `sweep`, `shimmer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animation: Option<String>,
}

impl ShapeSignature {
    /// Validate finite, non-negative geometry ratios.
    pub fn validate(&self) -> IrResult<()> {
        for (name, value) in [
            ("shape.aspect_ratio", self.aspect_ratio),
            ("shape.corner_radius_ratio", self.corner_radius_ratio),
            ("shape.stroke_width_ratio", self.stroke_width_ratio),
            ("shape.fill_ratio", self.fill_ratio),
        ] {
            if let Some(value) = value {
                require_non_negative(value, name)?;
            }
        }
        for (name, value) in [
            ("shape.arc_start_deg", self.arc_start_deg),
            ("shape.arc_end_deg", self.arc_end_deg),
        ] {
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(crate::IrError::InvalidValue {
                        field: name,
                        message: "must be finite".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Per-dimension coherence scores.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoherenceDimensions {
    /// Mood coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<f64>,
    /// Era coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub era: Option<f64>,
    /// Form coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<f64>,
    /// Material coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<f64>,
    /// Colour coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<f64>,
    /// Typography coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typography: Option<f64>,
    /// Density coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<f64>,
    /// Motion coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion: Option<f64>,
}

impl CoherenceDimensions {
    fn validate(&self) -> IrResult<()> {
        for (name, value) in [
            ("coherence.dimensions.mood", self.mood),
            ("coherence.dimensions.era", self.era),
            ("coherence.dimensions.form", self.form),
            ("coherence.dimensions.material", self.material),
            ("coherence.dimensions.color", self.color),
            ("coherence.dimensions.typography", self.typography),
            ("coherence.dimensions.density", self.density),
            ("coherence.dimensions.motion", self.motion),
        ] {
            if let Some(value) = value {
                require_unit_interval(value, name)?;
            }
        }
        Ok(())
    }
}

/// Advisory pack/scene coherence report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoherenceReport {
    /// Overall score, `0.0..=1.0`.
    pub score: f64,
    /// Per-dimension scores.
    #[serde(default)]
    pub dimensions: CoherenceDimensions,
    /// Widget/theme identifiers that diverge most.
    #[serde(default)]
    pub outliers: Vec<String>,
    /// Design-language conflicts.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Advisory suggestions.
    #[serde(default)]
    pub suggestions: Vec<String>,
    /// Confidence in the score, `0.0..=1.0`.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

impl CoherenceReport {
    /// Build a minimal report, useful for fixtures and importers.
    pub fn new(score: f64, confidence: f64) -> Self {
        Self {
            score,
            dimensions: CoherenceDimensions::default(),
            outliers: Vec::new(),
            conflicts: Vec::new(),
            suggestions: Vec::new(),
            confidence,
        }
    }

    /// Validate score and confidence bounds.
    pub fn validate(&self) -> IrResult<()> {
        require_unit_interval(self.score, "coherence.score")?;
        require_unit_interval(self.confidence, "coherence.confidence")?;
        self.dimensions.validate()
    }
}
