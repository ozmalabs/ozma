// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Font intent, typography, target profiles, and translation reports.
//!
//! Intent is independent of any particular font file. It records what the
//! source design meant (role, classification, shape traits, scripts, features)
//! and what a specific export target can accept.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A reference to a preferred or fallback font.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FontRef {
    /// Asset id, when the reference resolves to a captured asset.
    pub id: Option<String>,
    /// Family name.
    pub family: String,
    /// Style/subfamily name.
    pub style: Option<String>,
    /// Content hash, when the reference is pinned.
    pub sha256: Option<String>,
    /// Provenance: `embedded`, `device`, `system`, `web`, `generated`, or
    /// `imported`.
    pub source: Option<String>,
}

/// Shape traits used to score font substitutes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FontTraits {
    /// Desired weight class (100..900).
    pub weight: Option<u16>,
    /// Desired width class (`condensed`, `normal`, `expanded`, ...).
    pub width: Option<String>,
    /// Desired slant (`upright`, `italic`, `oblique`).
    pub slant: Option<String>,
    /// Desired x-height as a fraction of the em square.
    pub x_height: Option<f32>,
    /// Desired cap height as a fraction of the em square.
    pub cap_height: Option<f32>,
    /// Desired stroke contrast (`low`, `medium`, `high`).
    pub contrast: Option<String>,
    /// Desired roundness (`geometric`, `humanist`, `grotesque`).
    pub roundness: Option<String>,
    /// Desired aperture (`open`, `closed`).
    pub aperture: Option<String>,
    /// Explicit PANOSE target, for sources that captured one.
    pub panose: Option<String>,
    /// Source units per em, when preserving exact metrics.
    pub units_per_em: Option<u16>,
    /// Source ascender in font units.
    pub ascent: Option<i16>,
    /// Source descender in font units.
    pub descent: Option<i16>,
    /// Source mean advance in font units.
    pub average_advance: Option<f32>,
}

/// What a source design meant by a font choice.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FontIntent {
    /// Stable intent id, referenced by [`TypographyRef::intent`].
    pub id: String,
    /// Usage role: `body`, `label`, `numeric`, `display`, `caption`, `icon`,
    /// `brand`, or `code`.
    pub role: String,
    /// Classification: `sans`, `serif`, `mono`, `display`, `script`, or `icon`.
    pub classification: String,
    /// Shape traits used for matching.
    pub traits: FontTraits,
    /// Required scripts: `latin`, `latin-ext`, `cyrillic`, `greek`, `cjk`,
    /// `arabic`, and so on.
    pub scripts: Vec<String>,
    /// Required OpenType features, using canonical names such as
    /// `tabular_nums` or `slashed_zero`.
    pub features: Vec<String>,
    /// Target constraints, when the intent is bound to one target.
    pub target: Option<TargetFontProfile>,
    /// Preferred fallback chain, in priority order.
    pub fallbacks: Vec<FontRef>,
}

impl FontIntent {
    /// Create an intent with a role and classification.
    pub fn new(role: impl Into<String>, classification: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            classification: classification.into(),
            ..Self::default()
        }
    }
}

/// Typography applied to one widget or text run.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TypographyRef {
    /// The [`FontIntent`] id.
    pub intent: String,
    /// Size in CSS pixels.
    pub size_px: Option<f32>,
    /// Size in points.
    pub size_pt: Option<f32>,
    /// Line height as a multiple of the font size.
    pub line_height: Option<f32>,
    /// Tracking/letter spacing in pixels.
    pub tracking: Option<f32>,
    /// Weight override.
    pub weight: Option<u16>,
    /// OpenType feature overrides.
    pub features: Vec<String>,
}

/// Constraints declared by an export target.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetFontProfile {
    /// Target name: `browser`, `controller_png`, `lvgl`, `u8g2`,
    /// `streamdeck`, `corsair_lcd`, `eink`, `aida64_html`, or
    /// `adaptive_card`.
    pub target: String,
    /// Accepted embedded formats (`ttf`, `otf`, `woff2`, `bdf`, `lvgl_c`, ...).
    pub formats: Vec<String>,
    /// Device dots per inch.
    pub dpi: Option<u32>,
    /// Colour depth in bits per pixel.
    pub color_depth: Option<u32>,
    /// Embedded font bit depth (1, 2, 4, or 8).
    pub bpp: Option<u8>,
    /// Fixed cell width in pixels.
    pub cell_width: Option<u32>,
    /// Fixed cell height in pixels.
    pub cell_height: Option<u32>,
    /// Whether antialiasing is available.
    pub antialias: Option<bool>,
    /// Whether hinting is available.
    pub hinting: Option<bool>,
    /// Maximum embedded font size in bytes.
    pub max_bytes: Option<u64>,
    /// Scripts the target can render.
    pub scripts: Vec<String>,
    /// Family to use when no font can be embedded.
    pub fallback_family: Option<String>,
}

impl TargetFontProfile {
    /// Create a target profile with a name and accepted formats.
    pub fn new<I, S>(target: impl Into<String>, formats: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            target: target.into(),
            formats: formats.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// Whether the target accepts `format`.
    ///
    /// An empty format list means "no format constraint" for planning purposes.
    pub fn supports_format(&self, format: &str) -> bool {
        self.formats.is_empty()
            || self
                .formats
                .iter()
                .any(|candidate| formats_compatible(candidate, format))
    }

    /// Whether the target only accepts raster/bitmap font containers.
    pub fn raster_only(&self) -> bool {
        if self.formats.is_empty() {
            return self.bpp.is_some();
        }
        self.formats.iter().all(|format| is_bitmap_format(format))
    }

    /// Whether an asset of `size_bytes` fits the declared budget.
    pub fn fits_bytes(&self, size_bytes: u64) -> bool {
        self.max_bytes.is_none_or(|maximum| size_bytes <= maximum)
    }
}

/// Whether two format names identify the same container, including common
/// aliases such as `truetype` for `ttf`.
pub fn formats_compatible(left: &str, right: &str) -> bool {
    normalize_format(left) == normalize_format(right)
}

/// Normalize a font format name.
pub fn normalize_format(format: &str) -> String {
    match format.trim().to_ascii_lowercase().as_str() {
        "truetype" | "true" => "ttf".to_owned(),
        "opentype" | "cff" => "otf".to_owned(),
        "woff" => "woff".to_owned(),
        "woff2" => "woff2".to_owned(),
        "ttc" | "collection" => "ttc".to_owned(),
        "lvgl" | "lvgl-c" | "lvgl_c" => "lvgl_c".to_owned(),
        "bdf" => "bdf".to_owned(),
        "pcf" => "pcf".to_owned(),
        "bitmap" | "raster" | "atlas" | "pbm" => "bitmap".to_owned(),
        _ => format.trim().to_ascii_lowercase(),
    }
}

/// Whether a format is a raster/bitmap container rather than an outline.
pub fn is_bitmap_format(format: &str) -> bool {
    matches!(
        normalize_format(format).as_str(),
        "bdf" | "pcf" | "bitmap" | "lvgl_c"
    )
}

/// The concrete action taken when translating a font for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FontTranslationAction {
    /// The original font can be used as-is.
    Exact,
    /// The original font is subset and emitted.
    Subset,
    /// A shape-compatible substitute is used.
    Substituted,
    /// Glyphs are rasterised for a bitmap target.
    Rasterized,
    /// No legal, compatible font is available.
    #[default]
    Unavailable,
}

impl FontTranslationAction {
    /// Stable string form used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Subset => "subset",
            Self::Substituted => "substituted",
            Self::Rasterized => "rasterized",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for FontTranslationAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The export-only font loss report.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FontTranslationReport {
    /// Original family/name, or empty when no original was supplied.
    pub source_font: String,
    /// Chosen family/name, or empty when the action is `unavailable`.
    pub target_font: String,
    /// What happened to the font.
    pub action: FontTranslationAction,
    /// Match score for a substitute, in `0..=1`.
    pub shape_score: Option<f32>,
    /// Metric comparison and size-fit details.
    pub metrics: Option<BTreeMap<String, serde_json::Value>>,
    /// Required glyph/script coverage that the chosen font lacks.
    pub dropped_glyphs: Vec<String>,
    /// Required OpenType features that the chosen font lacks.
    pub dropped_features: Vec<String>,
    /// Whether the chosen source may legally be used for this export.
    pub license_ok: bool,
    /// Human-readable warnings; optional machine fields stay typed above.
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_format_aliases_and_raster_detection() {
        let browser = TargetFontProfile::new("browser", ["ttf", "woff2"]);
        assert!(browser.supports_format("truetype"));
        assert!(browser.supports_format("TTF"));
        assert!(!browser.supports_format("bdf"));
        assert!(!browser.raster_only());

        let lvgl = TargetFontProfile::new("lvgl", ["lvgl_c"]);
        assert!(lvgl.supports_format("lvgl"));
        assert!(lvgl.raster_only());

        let unconstrained = TargetFontProfile::new("custom", Vec::<String>::new());
        assert!(unconstrained.supports_format("anything"));
        assert!(!unconstrained.raster_only());

        let bitmap_without_formats = TargetFontProfile {
            bpp: Some(1),
            ..TargetFontProfile::new("eink", Vec::<String>::new())
        };
        assert!(bitmap_without_formats.raster_only());
    }

    #[test]
    fn report_round_trips_and_defaults_to_unavailable() {
        let mut report = FontTranslationReport {
            source_font: "Source Sans".to_owned(),
            target_font: "Target Sans".to_owned(),
            action: FontTranslationAction::Substituted,
            shape_score: Some(0.875),
            dropped_glyphs: vec!["script:arabic".to_owned()],
            dropped_features: vec!["tabular_nums".to_owned()],
            license_ok: true,
            warnings: vec!["sample".to_owned()],
            ..FontTranslationReport::default()
        };
        report.metrics = Some(BTreeMap::new());
        let json = serde_json::to_string(&report).expect("serialize report");
        let restored: FontTranslationReport =
            serde_json::from_str(&json).expect("deserialize report");
        assert_eq!(report, restored);
        assert_eq!(
            FontTranslationReport::default().action,
            FontTranslationAction::Unavailable
        );
        assert_eq!(FontTranslationAction::Rasterized.as_str(), "rasterized");
    }

    #[test]
    fn typography_ref_round_trips() {
        let typography = TypographyRef {
            intent: "body".to_owned(),
            size_px: Some(14.0),
            size_pt: Some(10.5),
            line_height: Some(1.4),
            tracking: Some(0.2),
            weight: Some(500),
            features: vec!["tabular_nums".to_owned()],
        };
        let json = serde_json::to_string(&typography).expect("serialize typography");
        let restored: TypographyRef = serde_json::from_str(&json).expect("deserialize typography");
        assert_eq!(typography, restored);
    }
}
