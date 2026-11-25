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
    /// Per-keyword derived signals with method and confidence. Source and user
    /// keywords never appear here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived: Vec<DerivedKeyword>,
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
            derived: Vec::new(),
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
        validate_confidence(self.confidence, "vibe.confidence")?;
        for keyword in &self.derived {
            keyword.validate()?;
        }
        Ok(())
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

/// A single keyword derived from an observable signal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DerivedKeyword {
    /// Keyword value added to the profile.
    pub keyword: String,
    /// Profile member that owns the keyword (`color`, `typography`, `form`,
    /// `density`, `motion`, `references`, or `keywords`).
    #[serde(default)]
    pub field: String,
    /// Confidence in this individual derivation, `0.0..=1.0`.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// Signal that produced the keyword (`palette`, `font`, `geometry`,
    /// `motion`, `reference`).
    #[serde(default)]
    pub method: String,
}

impl DerivedKeyword {
    /// Validate the keyword, confidence, and provenance member.
    pub fn validate(&self) -> IrResult<()> {
        crate::require_non_empty(&self.keyword, "vibe.derived.keyword")?;
        crate::require_non_empty(&self.field, "vibe.derived.field")?;
        crate::require_non_empty(&self.method, "vibe.derived.method")?;
        validate_confidence(self.confidence, "vibe.derived.confidence")
    }
}

/// Provenance for a shape signature.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShapeProvenance {
    /// Extraction methods (`widget_ir`, `layout`, `style`, `svg`, `css`, `user`).
    #[serde(default)]
    pub methods: Vec<String>,
    /// Fields that already existed on the source shape and were preserved.
    #[serde(default)]
    pub source_fields: Vec<String>,
    /// Fields supplied by a human override and preserved.
    #[serde(default)]
    pub user_fields: Vec<String>,
    /// Fields computed by this extraction pass.
    #[serde(default)]
    pub derived_fields: Vec<String>,
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
    /// Arc sweep in degrees (`arc_end_deg - arc_start_deg`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_span_deg: Option<f64>,
    /// Tick count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_count: Option<u32>,
    /// Segment count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_count: Option<u32>,
    /// Visual elements per 10,000 square layout pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<f64>,
    /// Dominant orientation (`horizontal`, `vertical`, `square`, `radial`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<String>,
    /// Number of distinct colours in the palette.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette_size: Option<u32>,
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
    /// Confidence in the signature, `0.0..=1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Field-level provenance and extraction methods.
    #[serde(default, skip_serializing_if = "ShapeProvenance::is_empty")]
    pub provenance: ShapeProvenance,
}

impl ShapeProvenance {
    /// Whether no provenance has been recorded.
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
            && self.source_fields.is_empty()
            && self.user_fields.is_empty()
            && self.derived_fields.is_empty()
    }
}

impl ShapeSignature {
    /// Validate finite, non-negative geometry ratios.
    pub fn validate(&self) -> IrResult<()> {
        for (name, value) in [
            ("shape.aspect_ratio", self.aspect_ratio),
            ("shape.corner_radius_ratio", self.corner_radius_ratio),
            ("shape.stroke_width_ratio", self.stroke_width_ratio),
            ("shape.fill_ratio", self.fill_ratio),
            ("shape.density", self.density),
        ] {
            if let Some(value) = value {
                require_non_negative(value, name)?;
            }
        }
        for (name, value) in [
            ("shape.arc_start_deg", self.arc_start_deg),
            ("shape.arc_end_deg", self.arc_end_deg),
            ("shape.arc_span_deg", self.arc_span_deg),
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
        if let Some(confidence) = self.confidence {
            validate_confidence(confidence, "shape.confidence")?;
        }
        Ok(())
    }

    /// Copy every member that is unset on `self` from `other`, preserving any
    /// existing source or user values.
    pub fn fill_missing(&mut self, other: &ShapeSignature) {
        if self.widget_kind.is_none() {
            self.widget_kind = other.widget_kind.clone();
        }
        if self.aspect_ratio.is_none() {
            self.aspect_ratio = other.aspect_ratio;
        }
        if self.corner_radius_ratio.is_none() {
            self.corner_radius_ratio = other.corner_radius_ratio;
        }
        if self.stroke_width_ratio.is_none() {
            self.stroke_width_ratio = other.stroke_width_ratio;
        }
        if self.fill_ratio.is_none() {
            self.fill_ratio = other.fill_ratio;
        }
        if self.symmetry.is_none() {
            self.symmetry = other.symmetry.clone();
        }
        if self.arc_start_deg.is_none() {
            self.arc_start_deg = other.arc_start_deg;
        }
        if self.arc_end_deg.is_none() {
            self.arc_end_deg = other.arc_end_deg;
        }
        if self.arc_span_deg.is_none() {
            self.arc_span_deg = other.arc_span_deg;
        }
        if self.tick_count.is_none() {
            self.tick_count = other.tick_count;
        }
        if self.segment_count.is_none() {
            self.segment_count = other.segment_count;
        }
        if self.density.is_none() {
            self.density = other.density;
        }
        if self.orientation.is_none() {
            self.orientation = other.orientation.clone();
        }
        if self.palette_size.is_none() {
            self.palette_size = other.palette_size;
        }
        if self.needle_style.is_none() {
            self.needle_style = other.needle_style.clone();
        }
        if self.chart_mark.is_none() {
            self.chart_mark = other.chart_mark.clone();
        }
        if self.texture.is_none() {
            self.texture = other.texture.clone();
        }
        if self.elevation.is_none() {
            self.elevation = other.elevation.clone();
        }
        if self.glow.is_none() {
            self.glow = other.glow;
        }
        if self.animation.is_none() {
            self.animation = other.animation.clone();
        }
        if self.confidence.is_none() {
            self.confidence = other.confidence;
        }
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
    /// Form/geometry coherence. Serialized as `form` for compatibility with
    /// the versioned schema; see [`CoherenceDimensions::geometry`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<f64>,
    /// Material coherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<f64>,
    /// Colour/palette coherence. Serialized as `color` for compatibility with
    /// the versioned schema; see [`CoherenceDimensions::palette`].
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
    /// Palette/colour coherence alias for the serialized `color` member.
    pub fn palette(&self) -> Option<f64> {
        self.color
    }

    /// Geometry/form coherence alias for the serialized `form` member.
    pub fn geometry(&self) -> Option<f64> {
        self.form
    }

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
