// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Deterministic shape and vibe extraction plus advisory coherence scoring.
//!
//! Nothing in this module uses vision, the network, or a build step. Widget
//! properties, layout, design tokens, SVG text, and CSS text are parsed by a
//! bounded parser. SVG entities are never resolved; documents that declare
//! external entities are rejected.
//!
//! Extracted shape members only fill fields that are absent. Source and user
//! values already present on [`WidgetIR::shape`] are preserved verbatim and
//! recorded in [`ShapeProvenance`]. Vibe keywords stay in three layers:
//! source, user, and derived. Derived entries carry a method and confidence,
//! and user/source values win on conflict.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::WidgetIR;
use crate::vibe::{
    CoherenceDimensions, CoherenceReport, DerivedKeyword, ShapeProvenance, ShapeSignature,
    VibeProfile, VibeProvenance,
};
use crate::{IrError, IrResult, VIBE_TAXONOMY};

/// Maximum bytes accepted by the SVG and CSS parsers.
pub const MAX_TEXT_INPUT: usize = 256 * 1024;

/// A normalized palette colour (`#rrggbb`).
pub type Color = String;

/// Parsed SVG path statistics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PathStats {
    /// Number of command letters.
    pub commands: u32,
    /// Number of parameter groups (`L` with two pairs counts twice).
    pub segments: u32,
    /// Number of elliptical arc groups.
    pub arcs: u32,
    /// Number of arc groups with `large-arc-flag = 1`.
    pub large_arc_flags: u32,
    /// Number of arc groups with `sweep-flag = 1`.
    pub sweep_flags: u32,
}

/// Shape and palette signals read from an SVG or CSS document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisualExtraction {
    /// Extracted geometry signature.
    #[serde(default)]
    pub shape: ShapeSignature,
    /// Distinct normalized colours, in first-seen order.
    #[serde(default)]
    pub colors: Vec<Color>,
    /// Font families mentioned by the document.
    #[serde(default)]
    pub fonts: Vec<String>,
    /// Gradient kinds present (`linear-gradient`, `radial-gradient`).
    #[serde(default)]
    pub gradients: Vec<String>,
    /// Whether animation declarations are present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animated: Option<bool>,
}

/// Typography signals used for vibe derivation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VibeTypography {
    /// Font family names.
    #[serde(default)]
    pub families: Vec<String>,
    /// Numeric weight (`100..=900`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// Width keyword (`condensed`, `normal`, `expanded`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<String>,
}

/// Inputs to [`derive_vibe`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VibeInput {
    /// Palette colours (hex or `rgb()`).
    #[serde(default)]
    pub colors: Vec<Color>,
    /// Font family names.
    #[serde(default)]
    pub fonts: Vec<String>,
    /// Optional typography metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typography: Option<VibeTypography>,
    /// Geometry signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeSignature>,
    /// Motion hints (`static`, `animated`, `pulse`, `sweep`, ...).
    #[serde(default)]
    pub motion: Vec<String>,
    /// Reference styles (`aviation`, `noc`, `sci_fi`, ...).
    #[serde(default)]
    pub references: Vec<String>,
    /// Source-supplied profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VibeProfile>,
    /// Human-supplied profile, highest priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<VibeProfile>,
}

/// One widget/theme/scene member considered by [`coherence_for`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoherenceItem {
    /// Stable identifier used for outlier reporting.
    #[serde(default)]
    pub id: String,
    /// Design-language profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vibe: Option<VibeProfile>,
    /// Geometry signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeSignature>,
}

/// Advisory coherence controls.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoherenceOptions {
    /// Record an intentional mix. The overall score becomes `1.0` while the
    /// dimensions remain advisory.
    #[serde(default)]
    pub intentional_mixing: bool,
    /// Specific conflict strings the user accepts, for example
    /// `"rounded vs sharp"`.
    #[serde(default)]
    pub accepted_conflicts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Shape extraction
// ---------------------------------------------------------------------------

/// Extract a shape signature from a widget's IR projection.
///
/// Existing [`WidgetIR::shape`] values always win. The returned provenance
/// records which fields came from the source shape and which were derived.
pub fn extract_shape(widget: &WidgetIR) -> ShapeSignature {
    let existing = widget.shape.clone().unwrap_or_default();
    let source_fields = shape_fields(&existing);

    let mut extracted = ShapeSignature {
        widget_kind: Some(widget.kind.clone()),
        ..ShapeSignature::default()
    };
    let mut methods = BTreeSet::new();
    methods.insert("widget_ir".to_owned());

    let mut palette: Vec<Color> = Vec::new();

    if let Some(layout) = &widget.layout {
        methods.insert("layout".to_owned());
        extracted.aspect_ratio =
            layout
                .aspect_ratio
                .or_else(|| match (layout.width, layout.height) {
                    (Some(width), Some(height)) if width > 0.0 && height > 0.0 => {
                        Some(round4(width / height))
                    }
                    _ => None,
                });
        if let (Some(width), Some(height)) = (layout.width, layout.height) {
            if width > 0.0 && height > 0.0 {
                extracted.density = Some(round4(1.0 / ((width * height) / 10_000.0)));
            }
        }
        extracted.orientation = Some(orientation_for(
            layout.width,
            layout.height,
            extracted.aspect_ratio,
        ));
    }

    if let Some(style) = &widget.style {
        methods.insert("style".to_owned());
        let reference = widget
            .layout
            .as_ref()
            .and_then(|layout| match (layout.width, layout.height) {
                (Some(width), Some(height)) => Some(width.min(height)),
                _ => None,
            })
            .filter(|value| *value > 0.0)
            .unwrap_or(100.0);

        if let Some(ratio) = token_ratio(&style.tokens, &["aspect-ratio", "aspect_ratio", "aspect"])
        {
            extracted.aspect_ratio = Some(ratio);
        }
        if let Some((value, percent)) = token_number(
            &style.tokens,
            &[
                "border-radius",
                "border_radius",
                "corner-radius",
                "corner_radius",
                "radius",
            ],
        ) {
            extracted.corner_radius_ratio = Some(if percent {
                round4(value / 100.0)
            } else {
                round4(value / reference)
            });
        }
        if let Some((value, percent)) = token_number(
            &style.tokens,
            &[
                "stroke-width",
                "stroke_width",
                "border-width",
                "border_width",
            ],
        ) {
            extracted.stroke_width_ratio = Some(if percent {
                round4(value / 100.0)
            } else {
                round4(value / reference)
            });
        }
        if let Some((value, _)) = token_number(&style.tokens, &["fill-ratio", "fill_ratio"]) {
            extracted.fill_ratio = Some(round4(value));
        }
        if let Some((value, _)) = token_number(&style.tokens, &["tick-count", "tick_count"]) {
            extracted.tick_count = Some(value.max(0.0).round() as u32);
        }
        if let Some((value, _)) = token_number(&style.tokens, &["segment-count", "segment_count"]) {
            extracted.segment_count = Some(value.max(0.0).round() as u32);
        }
        if let Some((value, _)) = token_number(&style.tokens, &["arc-start", "arc_start"]) {
            extracted.arc_start_deg = Some(round4(value));
        }
        if let Some((value, _)) = token_number(&style.tokens, &["arc-end", "arc_end"]) {
            extracted.arc_end_deg = Some(round4(value));
        }
        if let Some((value, _)) = token_number(&style.tokens, &["arc-span", "arc_span"]) {
            extracted.arc_span_deg = Some(round4(value));
        }
        if let Some((value, _)) = token_number(&style.tokens, &["needle-width", "needle_width"]) {
            if value >= 4.0 {
                extracted.needle_style = Some("modern".to_owned());
            }
        }
        for class in &style.classes {
            let class = class.to_ascii_lowercase();
            if matches!(class.as_str(), "rounded" | "pill" | "circle") {
                if extracted.corner_radius_ratio.is_none() {
                    extracted.corner_radius_ratio = Some(0.5);
                }
                if extracted.symmetry.is_none() {
                    extracted.symmetry = Some("radial".to_owned());
                }
            } else if matches!(class.as_str(), "sharp" | "square") {
                if extracted.corner_radius_ratio.is_none() {
                    extracted.corner_radius_ratio = Some(0.0);
                }
            } else if matches!(class.as_str(), "dense" | "dashboard") {
                if extracted.density.is_none() {
                    extracted.density = Some(10.0);
                }
            } else if matches!(class.as_str(), "sparse" | "minimal") && extracted.density.is_none()
            {
                extracted.density = Some(0.25);
            }
        }
        for value in style.tokens.values() {
            collect_colors_from_value(value, &mut palette);
        }
    }

    if let Some(domain) = &widget.domain {
        for threshold in &domain.thresholds {
            if let Some(color) = threshold.color.as_deref().and_then(normalize_color) {
                push_unique(&mut palette, color);
            }
        }
    }
    if !palette.is_empty() {
        extracted.palette_size = Some(palette.len() as u32);
    }

    for key in [
        "tick_count",
        "tick-count",
        "segment_count",
        "segment-count",
        "arc_start_deg",
        "arc_end_deg",
        "arc_span_deg",
    ] {
        if let Some(value) = widget.extensions.get(key) {
            apply_numeric_hint(&mut extracted, key, value);
        }
    }
    if let Some(value) = widget.extensions.get("shape") {
        if let Ok(shape) = serde_json::from_value::<ShapeSignature>(value.clone()) {
            extracted.fill_missing(&shape);
        }
    }

    let kind = widget.kind.as_str();
    if extracted.symmetry.is_none() {
        extracted.symmetry = match kind {
            "gauge" => Some("radial".to_owned()),
            "bar" => Some("horizontal".to_owned()),
            _ => None,
        };
    }
    if extracted.orientation.is_none() {
        extracted.orientation = match kind {
            "gauge" => Some("radial".to_owned()),
            "bar" => Some("horizontal".to_owned()),
            "sparkline" | "chart" => None,
            _ => None,
        };
    }
    if extracted.tick_count.is_none() && kind == "gauge" {
        if let Some(domain) = &widget.domain {
            if !domain.thresholds.is_empty() {
                extracted.tick_count = Some(domain.thresholds.len() as u32 + 1);
            }
        }
    }
    if let Some(span) = arc_span(extracted.arc_start_deg, extracted.arc_end_deg) {
        if extracted.arc_span_deg.is_none() {
            extracted.arc_span_deg = Some(span);
        }
    }
    if let Some(density) = extracted.density {
        let elements = extracted
            .segment_count
            .or(extracted.tick_count)
            .unwrap_or(1)
            .max(1);
        extracted.density = Some(round4(density * f64::from(elements)));
    }

    methods.extend(existing.provenance.methods.iter().cloned());
    let mut result = existing;
    let preserved = source_fields;
    let mut user_fields = result.provenance.user_fields.clone();
    let mut source_fields = result.provenance.source_fields.clone();
    for field in &preserved {
        if !user_fields.iter().any(|existing| existing == field) {
            source_fields.push(field.clone());
        }
    }
    source_fields.sort();
    source_fields.dedup();
    user_fields.sort();
    user_fields.dedup();
    result.fill_missing(&extracted);
    if result.arc_span_deg.is_none() {
        result.arc_span_deg = arc_span(result.arc_start_deg, result.arc_end_deg);
    }
    let mut derived_fields = shape_fields(&result);
    for field in &source_fields {
        derived_fields.remove(field);
    }
    for field in &user_fields {
        derived_fields.remove(field);
    }
    result.provenance = ShapeProvenance {
        methods: methods.into_iter().collect(),
        source_fields,
        user_fields,
        derived_fields: derived_fields.into_iter().collect(),
    };
    if result.confidence.is_none() {
        let base: f64 =
            0.55 + if result.aspect_ratio.is_some() {
                0.1
            } else {
                0.0
            } + if result.density.is_some() { 0.1 } else { 0.0 }
                + if result.palette_size.is_some() {
                    0.05
                } else {
                    0.0
                }
                + if result.corner_radius_ratio.is_some() {
                    0.05
                } else {
                    0.0
                };
        result.confidence = Some(round4(base.min(0.95)));
    }
    result
}

fn apply_numeric_hint(shape: &mut ShapeSignature, key: &str, value: &serde_json::Value) {
    let Some(number) = value.as_f64() else {
        return;
    };
    if !number.is_finite() || number < 0.0 {
        return;
    }
    match key {
        "tick_count" | "tick-count" if shape.tick_count.is_none() => {
            shape.tick_count = Some(number.round() as u32);
        }
        "segment_count" | "segment-count" if shape.segment_count.is_none() => {
            shape.segment_count = Some(number.round() as u32);
        }
        "arc_start_deg" if shape.arc_start_deg.is_none() => {
            shape.arc_start_deg = Some(round4(number));
        }
        "arc_end_deg" if shape.arc_end_deg.is_none() => {
            shape.arc_end_deg = Some(round4(number));
        }
        "arc_span_deg" if shape.arc_span_deg.is_none() => {
            shape.arc_span_deg = Some(round4(number));
        }
        _ => {}
    }
}

fn shape_fields(shape: &ShapeSignature) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    if shape.widget_kind.is_some() {
        fields.insert("widget_kind".to_owned());
    }
    if shape.aspect_ratio.is_some() {
        fields.insert("aspect_ratio".to_owned());
    }
    if shape.corner_radius_ratio.is_some() {
        fields.insert("corner_radius_ratio".to_owned());
    }
    if shape.stroke_width_ratio.is_some() {
        fields.insert("stroke_width_ratio".to_owned());
    }
    if shape.fill_ratio.is_some() {
        fields.insert("fill_ratio".to_owned());
    }
    if shape.symmetry.is_some() {
        fields.insert("symmetry".to_owned());
    }
    if shape.arc_start_deg.is_some() {
        fields.insert("arc_start_deg".to_owned());
    }
    if shape.arc_end_deg.is_some() {
        fields.insert("arc_end_deg".to_owned());
    }
    if shape.arc_span_deg.is_some() {
        fields.insert("arc_span_deg".to_owned());
    }
    if shape.tick_count.is_some() {
        fields.insert("tick_count".to_owned());
    }
    if shape.segment_count.is_some() {
        fields.insert("segment_count".to_owned());
    }
    if shape.density.is_some() {
        fields.insert("density".to_owned());
    }
    if shape.orientation.is_some() {
        fields.insert("orientation".to_owned());
    }
    if shape.palette_size.is_some() {
        fields.insert("palette_size".to_owned());
    }
    if shape.needle_style.is_some() {
        fields.insert("needle_style".to_owned());
    }
    if shape.chart_mark.is_some() {
        fields.insert("chart_mark".to_owned());
    }
    if shape.texture.is_some() {
        fields.insert("texture".to_owned());
    }
    if shape.elevation.is_some() {
        fields.insert("elevation".to_owned());
    }
    if shape.glow.is_some() {
        fields.insert("glow".to_owned());
    }
    if shape.animation.is_some() {
        fields.insert("animation".to_owned());
    }
    if shape.confidence.is_some() {
        fields.insert("confidence".to_owned());
    }
    fields
}

fn orientation_for(width: Option<f64>, height: Option<f64>, aspect: Option<f64>) -> String {
    if let (Some(width), Some(height)) = (width, height) {
        if (width - height).abs() <= f64::EPSILON {
            return "square".to_owned();
        }
    }
    if let Some(aspect) = aspect {
        if aspect > 1.05 {
            return "horizontal".to_owned();
        }
        if aspect < 0.95 {
            return "vertical".to_owned();
        }
    }
    "square".to_owned()
}

fn token_number(
    tokens: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<(f64, bool)> {
    for (key, value) in tokens {
        let normalized = key.to_ascii_lowercase().replace('_', "-");
        if keys
            .iter()
            .any(|candidate| *candidate == key || candidate.replace('_', "-") == normalized)
        {
            return value_number(value);
        }
    }
    None
}

fn value_number(value: &serde_json::Value) -> Option<(f64, bool)> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some((number, false));
    }
    let text = value.as_str()?.trim();
    let percent = text.ends_with('%');
    let number = parse_length(text)?;
    number.is_finite().then_some((number, percent))
}

fn token_ratio(tokens: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
    for (key, value) in tokens {
        let normalized = key.to_ascii_lowercase().replace('_', "-");
        if keys
            .iter()
            .any(|candidate| *candidate == key || candidate.replace('_', "-") == normalized)
        {
            let (number, percent) = value_number(value)?;
            return Some(if percent {
                round4(number / 100.0)
            } else {
                round4(number)
            });
        }
    }
    None
}

fn collect_colors_from_value(value: &serde_json::Value, colors: &mut Vec<Color>) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(color) = normalize_color(text) {
                push_unique(colors, color);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_colors_from_value(item, colors);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_colors_from_value(item, colors);
            }
        }
        _ => {}
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

// ---------------------------------------------------------------------------
// Bounded SVG and CSS parsing
// ---------------------------------------------------------------------------

/// Parse an SVG document and return shape, palette, and font signals.
///
/// The parser is deliberately small: it reads tags, attributes, and path data.
/// It never resolves entities, fetch URLs, or expand includes.
pub fn extract_from_svg(text: &str) -> IrResult<VisualExtraction> {
    check_input(text, "svg")?;
    let lower = text.to_ascii_lowercase();
    if lower.contains("<!entity") {
        return Err(invalid_error("svg", "XML entities are not supported"));
    }
    if lower.contains("<!doctype") && (lower.contains("system") || lower.contains("public")) {
        return Err(invalid_error(
            "svg",
            "external DTD subsets are not supported",
        ));
    }
    let cleaned = strip_comments(text, "svg")?;
    let tags = split_tags(&cleaned);
    let Some(root) = tags
        .iter()
        .copied()
        .find(|tag| tag_name(tag).eq_ignore_ascii_case("svg"))
    else {
        return Err(invalid_error("svg", "missing <svg> root element"));
    };
    let root_is_self_closing = root.trim_end().ends_with("/>");
    if !root_is_self_closing && !lower.contains("</svg") {
        return Err(invalid_error("svg", "missing </svg> closing tag"));
    }

    let root_attrs = attribute_map(root);
    let viewbox = root_attrs
        .get("viewbox")
        .and_then(|value| parse_viewbox(value));
    let width = root_attrs
        .get("width")
        .and_then(|value| parse_length(value));
    let height = root_attrs
        .get("height")
        .and_then(|value| parse_length(value));
    let (box_width, box_height) = match (viewbox, width, height) {
        (Some((x, y, w, h)), _, _) => {
            if w <= 0.0 || h <= 0.0 {
                return Err(invalid_error(
                    "svg",
                    "viewBox must have positive dimensions",
                ));
            }
            let _ = (x, y);
            (w, h)
        }
        (None, Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
        _ => {
            return Err(invalid_error(
                "svg",
                "expected a viewBox or positive width and height",
            ))
        }
    };
    let shorter = box_width.min(box_height);

    let mut shape = ShapeSignature {
        aspect_ratio: Some(round4(box_width / box_height)),
        orientation: Some(if (box_width - box_height).abs() <= f64::EPSILON {
            "square".to_owned()
        } else if box_width > box_height {
            "horizontal".to_owned()
        } else {
            "vertical".to_owned()
        }),
        ..ShapeSignature::default()
    };

    let mut colors: Vec<Color> = Vec::new();
    let mut fonts: Vec<String> = Vec::new();
    let mut gradients: BTreeSet<String> = BTreeSet::new();
    let mut animated = false;
    let mut max_rx: f64 = 0.0;
    let mut max_ry: f64 = 0.0;
    let mut max_stroke: f64 = 0.0;
    let mut segments: u32 = 0;
    let mut arcs: u32 = 0;
    let mut large_arc = false;
    let mut fill_ratio: Option<f64> = None;
    let mut has_circle = false;
    let mut has_path = false;

    for tag in &tags {
        let name = tag_name(tag);
        if name.is_empty()
            || name.starts_with('!')
            || name.starts_with('?')
            || name.starts_with('/')
        {
            continue;
        }
        let attrs = attribute_map(tag);
        if attrs
            .keys()
            .any(|key| key == "fill" || key == "stroke" || key == "stop-color")
        {
            for key in ["fill", "stroke", "stop-color"] {
                if let Some(value) = attrs.get(key).and_then(|value| normalize_color(value)) {
                    push_unique(&mut colors, value);
                }
            }
        }
        if let Some(style) = attrs.get("style") {
            for (_, value) in style_declarations(style) {
                for color in colors_in_text(&value) {
                    push_unique(&mut colors, color);
                }
                if value.to_ascii_lowercase().contains("linear-gradient(") {
                    gradients.insert("linear-gradient".to_owned());
                }
                if value.to_ascii_lowercase().contains("radial-gradient(") {
                    gradients.insert("radial-gradient".to_owned());
                }
            }
        }
        if let Some(family) = attrs.get("font-family") {
            for font in split_font_families(family) {
                push_unique(&mut fonts, font);
            }
        }
        if name.eq_ignore_ascii_case("lineargradient") {
            gradients.insert("linear-gradient".to_owned());
        }
        if name.eq_ignore_ascii_case("radialgradient") {
            gradients.insert("radial-gradient".to_owned());
        }
        if name.to_ascii_lowercase().starts_with("animate") {
            animated = true;
        }
        if name.eq_ignore_ascii_case("circle") {
            has_circle = true;
        }
        if let Some(value) = attrs.get("rx").and_then(|value| parse_length(value)) {
            max_rx = max_rx.max(value);
        }
        if let Some(value) = attrs.get("ry").and_then(|value| parse_length(value)) {
            max_ry = max_ry.max(value);
        }
        if let Some(value) = attrs
            .get("stroke-width")
            .and_then(|value| parse_length(value))
        {
            max_stroke = max_stroke.max(value);
        }
        if name.eq_ignore_ascii_case("rect") {
            if let (Some(w), Some(h)) = (
                attrs.get("width").and_then(|value| parse_length(value)),
                attrs.get("height").and_then(|value| parse_length(value)),
            ) {
                if w > 0.0 && h > 0.0 {
                    let ratio = ((w * h) / (box_width * box_height)).clamp(0.0, 1.0);
                    fill_ratio = Some(round4(fill_ratio.map_or(ratio, |old| old.max(ratio))));
                }
            }
        }
        if name.eq_ignore_ascii_case("path") {
            if let Some(data) = attrs.get("d") {
                has_path = true;
                let stats = parse_svg_path(data)?;
                segments = segments.saturating_add(stats.segments);
                arcs = arcs.saturating_add(stats.arcs);
                large_arc |= stats.large_arc_flags > 0;
            }
        }
    }

    if shorter > 0.0 {
        if max_rx > 0.0 || max_ry > 0.0 {
            shape.corner_radius_ratio = Some(round4(max_rx.max(max_ry) / shorter));
        }
        if max_stroke > 0.0 {
            shape.stroke_width_ratio = Some(round4(max_stroke / shorter));
        }
    }
    if let Some(ratio) = fill_ratio {
        shape.fill_ratio = Some(ratio);
    }
    if segments > 0 {
        shape.segment_count = Some(segments);
    }
    if arcs > 0 {
        shape.arc_span_deg = Some(if large_arc { 360.0 } else { 180.0 });
    }
    shape.symmetry = Some(
        if has_circle || shape.corner_radius_ratio.unwrap_or(0.0) >= 0.5 {
            "radial".to_owned()
        } else {
            "none".to_owned()
        },
    );
    if has_path && shape.orientation.as_deref() == Some("square") {
        shape.orientation = Some("radial".to_owned());
    }
    if !colors.is_empty() {
        shape.palette_size = Some(colors.len() as u32);
    }
    let methods = vec!["svg".to_owned()];
    shape.provenance = ShapeProvenance {
        methods,
        source_fields: Vec::new(),
        user_fields: Vec::new(),
        derived_fields: shape_fields(&shape).into_iter().collect(),
    };
    shape.confidence = Some(round4(
        (if viewbox.is_some() { 0.8_f64 } else { 0.65 }
            + if !colors.is_empty() { 0.05 } else { 0.0 })
        .min(0.95),
    ));

    fonts.sort();
    fonts.dedup();
    Ok(VisualExtraction {
        shape,
        colors,
        fonts,
        gradients: gradients.into_iter().collect(),
        animated: Some(animated),
    })
}

/// Parse a CSS stylesheet and return shape, palette, and font signals.
pub fn extract_from_css(text: &str) -> IrResult<VisualExtraction> {
    check_input(text, "css")?;
    validate_css(text)?;
    let declarations = css_declarations(text);
    let mut shape = ShapeSignature::default();
    let mut colors: Vec<Color> = Vec::new();
    let mut fonts: Vec<String> = Vec::new();
    let mut gradients: BTreeSet<String> = BTreeSet::new();
    let mut animated = false;

    let width = declaration_number(&declarations, &["width"]);
    let height = declaration_number(&declarations, &["height"]);
    let reference = match (width, height) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => w.min(h),
        _ => 100.0,
    };
    if let (Some(w), Some(h)) = (width, height) {
        if w > 0.0 && h > 0.0 {
            shape.aspect_ratio = Some(round4(w / h));
            shape.orientation = Some(orientation_for(Some(w), Some(h), shape.aspect_ratio));
        }
    }
    for (property, value) in &declarations {
        let lower_value = value.to_ascii_lowercase();
        match property.as_str() {
            "aspect-ratio" => {
                if let Some((w, h)) = parse_ratio(value) {
                    shape.aspect_ratio = Some(round4(w / h));
                }
            }
            "border-radius" | "corner-radius" | "radius" => {
                if let Some((value, percent)) = css_first_length(value) {
                    shape.corner_radius_ratio = Some(if percent {
                        round4(value / 100.0)
                    } else {
                        round4(value / reference)
                    });
                }
            }
            "border-width" | "stroke-width" => {
                if let Some((value, percent)) = css_first_length(value) {
                    shape.stroke_width_ratio = Some(if percent {
                        round4(value / 100.0)
                    } else {
                        round4(value / reference)
                    });
                }
            }
            "fill-ratio" => {
                if let Some((value, _)) = css_first_length(value) {
                    shape.fill_ratio = Some(round4(value));
                }
            }
            "tick-count" => {
                if let Some((value, _)) = css_first_length(value) {
                    shape.tick_count = Some(value.max(0.0).round() as u32);
                }
            }
            "segment-count" => {
                if let Some((value, _)) = css_first_length(value) {
                    shape.segment_count = Some(value.max(0.0).round() as u32);
                }
            }
            "font-family" => {
                for font in split_font_families(value) {
                    push_unique(&mut fonts, font);
                }
            }
            "animation" | "animation-name" | "transition" | "transition-property"
                if !lower_value.trim().is_empty()
                    && lower_value.trim() != "none"
                    && lower_value.trim() != "0s" =>
            {
                animated = true;
            }
            _ => {}
        }
        for color in colors_in_text(value) {
            push_unique(&mut colors, color);
        }
        if lower_value.contains("linear-gradient(") {
            gradients.insert("linear-gradient".to_owned());
        }
        if lower_value.contains("radial-gradient(") {
            gradients.insert("radial-gradient".to_owned());
        }
    }
    if !colors.is_empty() {
        shape.palette_size = Some(colors.len() as u32);
    }
    shape.provenance = ShapeProvenance {
        methods: vec!["css".to_owned()],
        source_fields: Vec::new(),
        user_fields: Vec::new(),
        derived_fields: shape_fields(&shape).into_iter().collect(),
    };
    shape.confidence = Some(round4(
        (0.7_f64
            + if !colors.is_empty() { 0.05 } else { 0.0 }
            + if shape.aspect_ratio.is_some() {
                0.05
            } else {
                0.0
            })
        .min(0.95),
    ));
    fonts.sort();
    fonts.dedup();
    Ok(VisualExtraction {
        shape,
        colors,
        fonts,
        gradients: gradients.into_iter().collect(),
        animated: Some(animated),
    })
}

fn check_input(text: &str, field: &'static str) -> IrResult<()> {
    if text.trim().is_empty() {
        return Err(IrError::EmptyField(field));
    }
    if text.len() > MAX_TEXT_INPUT {
        return Err(invalid_error(
            field,
            format!("input exceeds {MAX_TEXT_INPUT} bytes"),
        ));
    }
    Ok(())
}

fn strip_comments(text: &str, field: &'static str) -> IrResult<String> {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        let Some(end) = after.find("-->") else {
            return Err(invalid_error(field, "unterminated comment"));
        };
        rest = &after[end + 3..];
    }
    output.push_str(rest);
    Ok(output)
}

fn split_tags(text: &str) -> Vec<&str> {
    let mut tags = Vec::new();
    let mut position = 0;
    while let Some(relative) = text[position..].find('<') {
        let start = position + relative;
        let Some(relative_end) = find_tag_end(&text[start..]) else {
            break;
        };
        let end = start + relative_end;
        tags.push(&text[start..=end]);
        position = end + 1;
    }
    tags
}

fn find_tag_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, byte) in bytes.iter().enumerate().skip(1) {
        match quote {
            Some(active) => {
                if *byte == active {
                    quote = None;
                }
            }
            None => {
                if *byte == b'"' || *byte == b'\'' {
                    quote = Some(*byte);
                } else if *byte == b'>' {
                    return Some(index);
                }
            }
        }
    }
    None
}

fn tag_name(tag: &str) -> &str {
    let bytes = tag.as_bytes();
    if bytes.first() != Some(&b'<') {
        return "";
    }
    let start = 1;
    let mut end = start;
    while end < bytes.len()
        && !bytes[end].is_ascii_whitespace()
        && bytes[end] != b'>'
        && bytes[end] != b'/'
    {
        end += 1;
    }
    tag.get(start..end).unwrap_or("")
}

fn attribute_map(tag: &str) -> BTreeMap<String, String> {
    let bytes = tag.as_bytes();
    let mut index = 1;
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && bytes[index] != b'>'
        && bytes[index] != b'/'
    {
        index += 1;
    }
    let mut attrs = BTreeMap::new();
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] == b'>' {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'>'
            && bytes[index] != b'/'
        {
            index += 1;
        }
        let name = tag
            .get(name_start..index)
            .unwrap_or("")
            .to_ascii_lowercase();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
                let quote = bytes[index];
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                let value = tag.get(value_start..index).unwrap_or("").to_owned();
                if index < bytes.len() {
                    index += 1;
                }
                if !name.is_empty() {
                    attrs.insert(name, value);
                }
            } else {
                let value_start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && bytes[index] != b'>'
                {
                    index += 1;
                }
                let value = tag.get(value_start..index).unwrap_or("").to_owned();
                if !name.is_empty() {
                    attrs.insert(name, value);
                }
            }
        } else if !name.is_empty() {
            attrs.insert(name, String::new());
        }
    }
    attrs
}

fn parse_viewbox(value: &str) -> Option<(f64, f64, f64, f64)> {
    let numbers: Vec<f64> = value
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f64>().ok())
        .collect();
    match numbers.as_slice() {
        [x, y, width, height] => Some((*x, *y, *width, *height)),
        _ => None,
    }
}

fn parse_length(value: &str) -> Option<f64> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    let end = text
        .find(|character: char| character.is_ascii_alphabetic() || character == '%')
        .unwrap_or(text.len());
    let number = text.get(..end)?.trim().parse::<f64>().ok()?;
    if number.is_finite() {
        Some(number)
    } else {
        None
    }
}

/// Parse SVG path data, validating command syntax and arc flags.
pub fn parse_svg_path(data: &str) -> IrResult<PathStats> {
    let tokens = path_tokens(data)?;
    let mut stats = PathStats::default();
    let mut index = 0;
    while index < tokens.len() {
        let Token::Command(command) = tokens[index] else {
            return Err(invalid_error("svg.path", "path must start with a command"));
        };
        index += 1;
        stats.commands += 1;
        let arity = match command.to_ascii_uppercase() {
            'Z' => 0,
            'M' | 'L' | 'T' => 2,
            'H' | 'V' => 1,
            'C' => 6,
            'S' | 'Q' => 4,
            'A' => 7,
            _ => {
                return Err(invalid_error(
                    "svg.path",
                    format!("unknown path command `{command}`"),
                ))
            }
        };
        if arity == 0 {
            continue;
        }
        let group_start = index;
        while index < tokens.len() {
            if matches!(tokens[index], Token::Command(_)) {
                break;
            }
            let group_end = index + arity;
            if group_end > tokens.len() {
                return Err(invalid_error("svg.path", "incomplete path command"));
            }
            let mut values = Vec::with_capacity(arity);
            for token in &tokens[index..group_end] {
                if let Token::Number(number) = token {
                    values.push(*number);
                }
            }
            if values.len() != arity {
                return Err(invalid_error(
                    "svg.path",
                    "expected numeric path parameters",
                ));
            }
            if command.eq_ignore_ascii_case(&'a') {
                if !is_flag(values[3]) || !is_flag(values[4]) {
                    return Err(invalid_error("svg.path", "arc flags must be 0 or 1"));
                }
                stats.arcs += 1;
                if values[3] == 1.0 {
                    stats.large_arc_flags += 1;
                }
                if values[4] == 1.0 {
                    stats.sweep_flags += 1;
                }
            }
            if !matches!(command.to_ascii_uppercase(), 'M' | 'Z') {
                stats.segments += 1;
            }
            index = group_end;
        }
        if index == group_start {
            return Err(invalid_error("svg.path", "path command has no parameters"));
        }
    }
    Ok(stats)
}

fn is_flag(value: f64) -> bool {
    value == 0.0 || value == 1.0
}

enum Token {
    Command(char),
    Number(f64),
}

fn path_tokens(data: &str) -> IrResult<Vec<Token>> {
    let mut tokens = Vec::new();
    let bytes = data.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() || byte == b',' {
            index += 1;
            continue;
        }
        if byte.is_ascii_alphabetic() {
            let command = byte as char;
            if !"MmLlHhVvCcSsQqTtAaZz".contains(command) {
                return Err(invalid_error(
                    "svg.path",
                    format!("unknown path command `{command}`"),
                ));
            }
            tokens.push(Token::Command(command));
            index += 1;
            continue;
        }
        let start = index;
        if byte == b'-' || byte == b'+' {
            index += 1;
        }
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
        }
        if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
            index += 1;
            if index < bytes.len() && (bytes[index] == b'-' || bytes[index] == b'+') {
                index += 1;
            }
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
        }
        if start == index {
            return Err(invalid_error("svg.path", "invalid path token"));
        }
        let number = data
            .get(start..index)
            .unwrap_or("")
            .parse::<f64>()
            .map_err(|_| invalid_error("svg.path", "invalid path number"))?;
        if !number.is_finite() {
            return Err(invalid_error("svg.path", "path numbers must be finite"));
        }
        tokens.push(Token::Number(number));
    }
    Ok(tokens)
}

fn validate_css(text: &str) -> IrResult<()> {
    let cleaned = strip_comments(text, "css")?;
    let mut braces: i32 = 0;
    let mut quote: Option<char> = None;
    for character in cleaned.chars() {
        match quote {
            Some(active) => {
                if character == active {
                    quote = None;
                }
            }
            None => match character {
                '"' | '\'' => quote = Some(character),
                '{' => braces += 1,
                '}' => {
                    braces -= 1;
                    if braces < 0 {
                        return Err(invalid_error("css", "unbalanced closing brace"));
                    }
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err(invalid_error("css", "unterminated string"));
    }
    if braces != 0 {
        return Err(invalid_error("css", "unbalanced braces"));
    }
    Ok(())
}

fn css_declarations(text: &str) -> Vec<(String, String)> {
    let cleaned = strip_comments(text, "css").unwrap_or_else(|_| text.to_owned());
    let mut declarations = Vec::new();
    let mut depth = 0usize;
    let mut buffer = String::new();
    for character in cleaned.chars() {
        match character {
            '{' => {
                depth += 1;
                buffer.clear();
            }
            '}' => {
                if depth > 0 {
                    push_declaration(&mut declarations, &buffer);
                    depth -= 1;
                }
                buffer.clear();
            }
            ';' if depth > 0 => {
                push_declaration(&mut declarations, &buffer);
                buffer.clear();
            }
            _ if depth > 0 => buffer.push(character),
            _ => {}
        }
    }
    if depth > 0 {
        push_declaration(&mut declarations, &buffer);
    }
    declarations
}

fn style_declarations(style: &str) -> Vec<(String, String)> {
    style
        .split(';')
        .filter_map(|part| {
            let (property, value) = part.split_once(':')?;
            let property = property.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            (!property.is_empty() && !value.is_empty()).then_some((property, value))
        })
        .collect()
}

fn push_declaration(declarations: &mut Vec<(String, String)>, raw: &str) {
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let Some((property, value)) = raw.split_once(':') else {
        return;
    };
    let property = property.trim().to_ascii_lowercase();
    let value = value.trim().to_owned();
    if !property.is_empty() && !value.is_empty() && !property.contains('{') {
        declarations.push((property, value));
    }
}

fn declaration_length(declarations: &[(String, String)], names: &[&str]) -> Option<(f64, bool)> {
    for (property, value) in declarations {
        if names.contains(&property.as_str()) {
            return css_first_length(value);
        }
    }
    None
}

fn declaration_number(declarations: &[(String, String)], names: &[&str]) -> Option<f64> {
    declaration_length(declarations, names).map(|(value, _)| value)
}

fn css_first_length(value: &str) -> Option<(f64, bool)> {
    let token = value
        .split(|character: char| character.is_whitespace() || character == ',')
        .find(|part| !part.is_empty())?;
    let percent = token.ends_with('%');
    let number = parse_length(token)?;
    Some((number, percent))
}

fn parse_ratio(value: &str) -> Option<(f64, f64)> {
    let mut parts = value.split('/');
    let width = parse_length(parts.next()?)?;
    let height = parse_length(parts.next()?)?;
    if width > 0.0 && height > 0.0 {
        Some((width, height))
    } else {
        None
    }
}

fn split_font_families(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| {
            part.trim()
                .trim_matches(|character| character == '"' || character == '\'')
                .to_owned()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

fn colors_in_text(text: &str) -> Vec<Color> {
    let mut colors = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if matches!(end - start, 3 | 4 | 6 | 8) {
                if let Some(color) = normalize_color(text.get(index..end).unwrap_or("")) {
                    push_unique(&mut colors, color);
                }
            }
            index = end;
            continue;
        }
        if starts_with_ignore_case(text, index, "rgb(")
            || starts_with_ignore_case(text, index, "rgba(")
        {
            let open = index;
            let Some(close) = text[open..].find(')') else {
                index += 1;
                continue;
            };
            let end = open + close + 1;
            if let Some(color) = normalize_color(text.get(open..end).unwrap_or("")) {
                push_unique(&mut colors, color);
            }
            index = end;
            continue;
        }
        index += 1;
    }
    colors
}

fn starts_with_ignore_case(text: &str, position: usize, prefix: &str) -> bool {
    text.len() >= position + prefix.len()
        && text.as_bytes()[position..position + prefix.len()]
            .eq_ignore_ascii_case(prefix.as_bytes())
}

fn normalize_color(value: &str) -> Option<Color> {
    let text = value.trim().to_ascii_lowercase();
    if text.is_empty()
        || text == "none"
        || text == "transparent"
        || text == "currentcolor"
        || text.starts_with("url(")
    {
        return None;
    }
    if text.starts_with('#') {
        let (red, green, blue) = parse_hex_color(&text)?;
        return Some(format!("#{red:02x}{green:02x}{blue:02x}"));
    }
    for prefix in ["rgba(", "rgb("] {
        if let Some(inner) = text
            .strip_prefix(prefix)
            .and_then(|part| part.strip_suffix(')'))
        {
            let parts: Vec<&str> = inner
                .split(|character: char| {
                    character == ',' || character == '/' || character.is_whitespace()
                })
                .filter(|part| !part.is_empty())
                .collect();
            if parts.len() >= 3 {
                let red = parse_color_component(parts[0])?;
                let green = parse_color_component(parts[1])?;
                let blue = parse_color_component(parts[2])?;
                return Some(format!("#{red:02x}{green:02x}{blue:02x}"));
            }
        }
    }
    named_color(&text).map(ToOwned::to_owned)
}

fn parse_hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.strip_prefix('#')?;
    if !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let component = |part: &str| u8::from_str_radix(part, 16).ok();
    match hex.len() {
        3 => Some((
            component(&hex[0..1])? * 17,
            component(&hex[1..2])? * 17,
            component(&hex[2..3])? * 17,
        )),
        4 => Some((
            component(&hex[0..1])? * 17,
            component(&hex[1..2])? * 17,
            component(&hex[2..3])? * 17,
        )),
        6 => Some((
            component(&hex[0..2])?,
            component(&hex[2..4])?,
            component(&hex[4..6])?,
        )),
        8 => Some((
            component(&hex[0..2])?,
            component(&hex[2..4])?,
            component(&hex[4..6])?,
        )),
        _ => None,
    }
}

fn parse_color_component(value: &str) -> Option<u8> {
    if let Some(percent) = value.strip_suffix('%') {
        let number = percent.parse::<f64>().ok()?;
        if !number.is_finite() {
            return None;
        }
        return Some((number.clamp(0.0, 100.0) * 2.55).round() as u8);
    }
    let number = value.parse::<f64>().ok()?;
    if !number.is_finite() {
        return None;
    }
    Some(number.clamp(0.0, 255.0).round() as u8)
}

fn named_color(name: &str) -> Option<&'static str> {
    Some(match name {
        "black" => "#000000",
        "white" => "#ffffff",
        "red" => "#ff0000",
        "green" => "#008000",
        "blue" => "#0000ff",
        "yellow" => "#ffff00",
        "orange" => "#ffa500",
        "purple" => "#800080",
        "pink" => "#ffc0cb",
        "cyan" | "aqua" => "#00ffff",
        "magenta" | "fuchsia" => "#ff00ff",
        "gray" | "grey" => "#808080",
        "silver" => "#c0c0c0",
        "maroon" => "#800000",
        "olive" => "#808000",
        "navy" => "#000080",
        "teal" => "#008080",
        "lime" => "#00ff00",
        _ => return None,
    })
}

fn invalid_error(field: &'static str, message: impl Into<String>) -> IrError {
    IrError::InvalidValue {
        field,
        message: message.into(),
    }
}

fn round4(value: f64) -> f64 {
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

fn arc_span(start: Option<f64>, end: Option<f64>) -> Option<f64> {
    match (start, end) {
        (Some(start), Some(end)) if end > start => Some(round4(end - start)),
        (Some(start), Some(end)) => Some(round4((end - start).abs())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Vibe derivation and keyword merging
// ---------------------------------------------------------------------------

/// Derive a [`VibeProfile`] from palette, typography, geometry, motion, and
/// reference signals.
pub fn derive_vibe(input: &VibeInput) -> VibeProfile {
    let derived = derive_keywords(input);
    merge_vibe_keywords(input.source.as_ref(), input.user.as_ref(), &derived)
}

/// Merge source, user, and derived layers.
///
/// Source and user keywords are preserved verbatim. Derived keywords are
/// appended only when they do not duplicate or contradict an existing
/// keyword. The strongest layer wins on a conflict: user over source over
/// derived.
pub fn merge_vibe_keywords(
    source: Option<&VibeProfile>,
    user: Option<&VibeProfile>,
    derived: &[DerivedKeyword],
) -> VibeProfile {
    let mut profile = VibeProfile::default();
    if let Some(source) = source {
        profile = source.clone();
    }
    if let Some(user) = user {
        merge_user_layer(&mut profile, user);
    }
    if profile.taxonomy.trim().is_empty() {
        profile.taxonomy = VIBE_TAXONOMY.to_owned();
    }

    let mut accepted: Vec<DerivedKeyword> = Vec::new();
    let normalized: Vec<DerivedKeyword> = derived
        .iter()
        .filter_map(|candidate| {
            let keyword = candidate.keyword.trim();
            if keyword.is_empty() {
                return None;
            }
            Some(DerivedKeyword {
                keyword: keyword.to_owned(),
                field: if candidate.field.trim().is_empty() {
                    "keywords".to_owned()
                } else {
                    candidate.field.trim().to_owned()
                },
                confidence: if candidate.confidence.is_finite() {
                    candidate.confidence.clamp(0.0, 1.0)
                } else {
                    0.0
                },
                method: if candidate.method.trim().is_empty() {
                    "derived".to_owned()
                } else {
                    candidate.method.trim().to_owned()
                },
            })
        })
        .collect();
    for candidate in &normalized {
        let conflicts = all_profile_keywords(&profile)
            .into_iter()
            .any(|existing| keywords_conflict(&existing, &candidate.keyword));
        let duplicate = all_profile_keywords(&profile)
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&candidate.keyword));
        if conflicts || duplicate {
            continue;
        }
        if !accepted
            .iter()
            .any(|existing| existing.keyword.eq_ignore_ascii_case(&candidate.keyword))
        {
            accepted.push(candidate.clone());
        }
    }
    accepted.sort_by(|left, right| {
        (left.field.as_str(), left.keyword.as_str())
            .cmp(&(right.field.as_str(), right.keyword.as_str()))
    });
    for candidate in &accepted {
        add_keyword(&mut profile, &candidate.field, &candidate.keyword);
    }
    profile.derived = accepted.clone();

    let source_keywords = source.map(source_layer_keywords).unwrap_or_default();
    let user_keywords = user.map(user_layer_keywords).unwrap_or_default();
    let mut methods = BTreeSet::new();
    if let Some(source) = source {
        methods.extend(source.provenance.methods.iter().cloned());
    }
    if let Some(user) = user {
        methods.extend(user.provenance.methods.iter().cloned());
    }
    for candidate in &normalized {
        if !candidate.method.is_empty() {
            methods.insert(candidate.method.clone());
        }
    }
    profile.provenance = VibeProvenance {
        source_keywords,
        derived_keywords: accepted.iter().map(|entry| entry.keyword.clone()).collect(),
        user_keywords,
        methods: methods.into_iter().collect(),
    };

    profile.confidence = if let Some(user) = user {
        user.confidence
    } else if let Some(source) = source {
        source.confidence
    } else if accepted.is_empty() {
        0.0
    } else {
        let total: f64 = accepted.iter().map(|entry| entry.confidence).sum();
        round4(total / accepted.len() as f64)
    };
    profile
}

fn merge_user_layer(profile: &mut VibeProfile, user: &VibeProfile) {
    let user_keywords = user_layer_keywords(user);
    for keyword in &user_keywords {
        remove_conflicting(profile, keyword);
    }
    copy_layer(profile, user);
    if user.density.is_some() {
        profile.density = user.density.clone();
    }
    if user.era.is_some() {
        profile.era = user.era.clone();
    }
    for (key, value) in &user.extra {
        profile.extra.insert(key.clone(), value.clone());
    }
    if !user.taxonomy.trim().is_empty() {
        profile.taxonomy = user.taxonomy.clone();
    }
}

fn remove_conflicting(profile: &mut VibeProfile, keyword: &str) {
    remove_conflicts(&mut profile.keywords, keyword);
    remove_conflicts(&mut profile.mood, keyword);
    remove_conflicts(&mut profile.form, keyword);
    remove_conflicts(&mut profile.material, keyword);
    remove_conflicts(&mut profile.color, keyword);
    remove_conflicts(&mut profile.typography, keyword);
    remove_conflicts(&mut profile.motion, keyword);
    remove_conflicts(&mut profile.references, keyword);
    remove_conflicts(&mut profile.ornament, keyword);
    if let Some(density) = &profile.density {
        if keywords_conflict(density, keyword) {
            profile.density = None;
        }
    }
}

fn remove_conflicts(values: &mut Vec<String>, keyword: &str) {
    values.retain(|value| !keywords_conflict(value, keyword));
}

fn copy_layer(profile: &mut VibeProfile, layer: &VibeProfile) {
    append_unique(&mut profile.keywords, &layer.keywords);
    append_unique(&mut profile.mood, &layer.mood);
    append_unique(&mut profile.form, &layer.form);
    append_unique(&mut profile.material, &layer.material);
    append_unique(&mut profile.color, &layer.color);
    append_unique(&mut profile.typography, &layer.typography);
    append_unique(&mut profile.motion, &layer.motion);
    append_unique(&mut profile.references, &layer.references);
    append_unique(&mut profile.ornament, &layer.ornament);
}

fn append_unique(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        if !target
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(value))
        {
            target.push(value.clone());
        }
    }
}

fn add_keyword(profile: &mut VibeProfile, field: &str, keyword: &str) {
    let target = match field {
        "keywords" => &mut profile.keywords,
        "mood" => &mut profile.mood,
        "form" => &mut profile.form,
        "material" => &mut profile.material,
        "color" => &mut profile.color,
        "typography" => &mut profile.typography,
        "motion" => &mut profile.motion,
        "references" => &mut profile.references,
        "ornament" => &mut profile.ornament,
        _ => &mut profile.keywords,
    };
    if !target
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(keyword))
    {
        target.push(keyword.to_owned());
    }
    if field == "density" {
        profile.density = Some(keyword.to_owned());
    } else if !profile
        .keywords
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(keyword))
    {
        profile.keywords.push(keyword.to_owned());
    }
}

fn source_layer_keywords(profile: &VibeProfile) -> Vec<String> {
    if profile.provenance.source_keywords.is_empty() {
        all_profile_keywords(profile)
    } else {
        dedupe_preserving_order(&profile.provenance.source_keywords)
    }
}

fn user_layer_keywords(profile: &VibeProfile) -> Vec<String> {
    if profile.provenance.user_keywords.is_empty() {
        all_profile_keywords(profile)
    } else {
        dedupe_preserving_order(&profile.provenance.user_keywords)
    }
}

fn dedupe_preserving_order(values: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for value in values {
        if !result
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(value))
        {
            result.push(value.clone());
        }
    }
    result
}

fn all_profile_keywords(profile: &VibeProfile) -> Vec<String> {
    let mut values = Vec::new();
    append_unique(&mut values, &profile.keywords);
    append_unique(&mut values, &profile.mood);
    append_unique(&mut values, &profile.form);
    append_unique(&mut values, &profile.material);
    append_unique(&mut values, &profile.color);
    append_unique(&mut values, &profile.typography);
    append_unique(&mut values, &profile.motion);
    append_unique(&mut values, &profile.references);
    append_unique(&mut values, &profile.ornament);
    if let Some(era) = &profile.era {
        if !values.iter().any(|value| value.eq_ignore_ascii_case(era)) {
            values.push(era.clone());
        }
    }
    if let Some(density) = &profile.density {
        if !values
            .iter()
            .any(|value| value.eq_ignore_ascii_case(density))
        {
            values.push(density.clone());
        }
    }
    values
}

fn keywords_conflict(left: &str, right: &str) -> bool {
    match (keyword_axis(left), keyword_axis(right)) {
        (Some((left_axis, left_side)), Some((right_axis, right_side))) => {
            left_axis == right_axis && left_side != right_side
        }
        _ => false,
    }
}

fn keyword_axis(keyword: &str) -> Option<(&'static str, &'static str)> {
    Some(match keyword.to_ascii_lowercase().as_str() {
        "rounded" | "soft" => ("corner", "rounded"),
        "sharp" | "square" => ("corner", "sharp"),
        "circular" => ("form", "circular"),
        "symmetric" => ("symmetry", "symmetric"),
        "asymmetric" => ("symmetry", "asymmetric"),
        "sparse" => ("density", "sparse"),
        "balanced" => ("density", "balanced"),
        "dense" | "dashboard" => ("density", "dense"),
        "static" => ("motion", "static"),
        "animated" => ("motion", "animated"),
        "warm" => ("temperature", "warm"),
        "cool" => ("temperature", "cool"),
        "neutral" => ("temperature", "neutral"),
        "monochrome" => ("vibrancy", "monochrome"),
        "pastel" => ("vibrancy", "pastel"),
        "neon" => ("vibrancy", "neon"),
        "high_contrast" => ("contrast", "high"),
        "low_contrast" => ("contrast", "low"),
        "serif" => ("type_style", "serif"),
        "geometric_sans" | "humanist_sans" | "sans_serif" => ("type_style", "sans"),
        "condensed" => ("type_width", "condensed"),
        "expanded" => ("type_width", "expanded"),
        "light" => ("weight", "light"),
        "bold" => ("weight", "bold"),
        "minimal" => ("ornament", "minimal"),
        "decorative" | "skeuomorphic" | "brutalist" => ("ornament", "decorative"),
        _ => return None,
    })
}

fn derive_keywords(input: &VibeInput) -> Vec<DerivedKeyword> {
    let mut derived = Vec::new();
    derive_palette_keywords(&input.colors, &mut derived);
    derive_typography_keywords(input, &mut derived);
    if let Some(shape) = &input.shape {
        derive_geometry_keywords(shape, &mut derived);
        derive_shape_motion_keywords(shape, &mut derived);
    }
    derive_motion_keywords(&input.motion, &mut derived);
    for reference in &input.references {
        let keyword = reference.trim().to_ascii_lowercase();
        if !keyword.is_empty() {
            push_derived(&mut derived, keyword, "references", 0.8, "reference");
        }
    }
    derived.sort_by(|left, right| {
        (left.field.as_str(), left.keyword.as_str())
            .cmp(&(right.field.as_str(), right.keyword.as_str()))
    });
    derived
}

fn derive_palette_keywords(colors: &[Color], derived: &mut Vec<DerivedKeyword>) {
    let parsed: Vec<(f64, f64, f64)> = colors
        .iter()
        .filter_map(|color| normalize_color(color))
        .filter_map(|color| {
            let (red, green, blue) = parse_hex_color(&color)?;
            Some(rgb_to_hsl(red, green, blue))
        })
        .collect();
    if parsed.is_empty() {
        return;
    }
    let count = parsed.len() as f64;
    let mut warm = 0usize;
    let mut cool = 0usize;
    let mut neutral = 0usize;
    let mut saturation_total = 0.0;
    let mut lightness_total = 0.0;
    let mut lightness_min = 1.0_f64;
    let mut lightness_max = 0.0_f64;
    let mut phosphor = false;
    let mut amber = false;
    for (hue, saturation, lightness) in &parsed {
        saturation_total += saturation;
        lightness_total += lightness;
        lightness_min = lightness_min.min(*lightness);
        lightness_max = lightness_max.max(*lightness);
        if *saturation < 0.12 {
            neutral += 1;
        } else if *hue < 70.0 || *hue >= 300.0 {
            warm += 1;
        } else {
            cool += 1;
        }
        if (90.0..=160.0).contains(hue) && *saturation > 0.5 {
            phosphor = true;
        }
        if (25.0..=65.0).contains(hue) && *saturation > 0.5 {
            amber = true;
        }
    }
    let dominant = if warm >= cool && warm >= neutral {
        Some(("warm", warm))
    } else if cool >= neutral {
        Some(("cool", cool))
    } else {
        Some(("neutral", neutral))
    };
    if let Some((keyword, matches)) = dominant {
        let confidence = round4(0.5 + 0.4 * (matches as f64 / count));
        push_derived(derived, keyword.to_owned(), "color", confidence, "palette");
    }
    let saturation_average = saturation_total / count;
    let lightness_average = lightness_total / count;
    if saturation_average > 0.7 && lightness_average > 0.4 {
        push_derived(derived, "neon".to_owned(), "color", 0.75, "palette");
    }
    if saturation_average < 0.25 && lightness_average > 0.7 {
        push_derived(derived, "pastel".to_owned(), "color", 0.7, "palette");
    }
    if neutral as f64 / count >= 0.75 {
        push_derived(derived, "monochrome".to_owned(), "color", 0.75, "palette");
    }
    if lightness_max - lightness_min > 0.55 {
        push_derived(derived, "high_contrast".to_owned(), "color", 0.7, "palette");
    }
    if phosphor {
        push_derived(
            derived,
            "phosphor_green".to_owned(),
            "color",
            0.7,
            "palette",
        );
    }
    if amber {
        push_derived(derived, "amber".to_owned(), "color", 0.65, "palette");
    }
}

fn derive_typography_keywords(input: &VibeInput, derived: &mut Vec<DerivedKeyword>) {
    let mut families = input.fonts.clone();
    let mut weight = None;
    let mut width = None;
    if let Some(typography) = &input.typography {
        families.extend(typography.families.iter().cloned());
        weight = typography.weight;
        width = typography.width.clone();
    }
    for family in &families {
        let lower = family.to_ascii_lowercase();
        if lower.contains("dot") && lower.contains("matrix") {
            push_derived(derived, "dot_matrix".to_owned(), "typography", 0.8, "font");
        } else if lower.contains("mono")
            || lower.contains("courier")
            || lower.contains("console")
            || lower.contains("code")
        {
            push_derived(derived, "monospace".to_owned(), "typography", 0.9, "font");
        } else if lower.contains("serif") && !lower.contains("sans") {
            push_derived(derived, "serif".to_owned(), "typography", 0.85, "font");
        } else if lower.contains("sans") || is_known_font(&lower) {
            let geometric = lower.contains("geometric")
                || [
                    "futura",
                    "century gothic",
                    "montserrat",
                    "poppins",
                    "jost",
                    "outfit",
                ]
                .iter()
                .any(|name| lower.contains(name));
            push_derived(
                derived,
                if geometric {
                    "geometric_sans".to_owned()
                } else {
                    "humanist_sans".to_owned()
                },
                "typography",
                0.75,
                "font",
            );
        }
        if lower.contains("condensed") || lower.contains("narrow") {
            push_derived(derived, "condensed".to_owned(), "typography", 0.7, "font");
        }
        if lower.contains("expanded") || lower.contains("extended") {
            push_derived(derived, "expanded".to_owned(), "typography", 0.7, "font");
        }
    }
    if let Some(weight) = weight {
        if weight >= 700 {
            push_derived(derived, "bold".to_owned(), "typography", 0.7, "font");
        } else if weight <= 300 {
            push_derived(derived, "light".to_owned(), "typography", 0.65, "font");
        }
    }
    if let Some(width) = width {
        let width = width.to_ascii_lowercase();
        if width == "condensed" || width == "expanded" {
            push_derived(derived, width, "typography", 0.7, "font");
        }
    }
}

fn is_known_font(family: &str) -> bool {
    [
        "inter",
        "helvetica",
        "arial",
        "roboto",
        "lato",
        "open sans",
        "ibm plex sans",
        "source sans",
        "noto sans",
    ]
    .iter()
    .any(|name| family.contains(name))
}

fn derive_geometry_keywords(shape: &ShapeSignature, derived: &mut Vec<DerivedKeyword>) {
    if let Some(radius) = shape.corner_radius_ratio {
        if radius >= 0.35 {
            push_derived(derived, "rounded".to_owned(), "form", 0.8, "geometry");
        } else if radius <= 0.08 {
            push_derived(derived, "sharp".to_owned(), "form", 0.8, "geometry");
        }
    }
    if let Some(symmetry) = &shape.symmetry {
        match symmetry.to_ascii_lowercase().as_str() {
            "radial" => push_derived(derived, "circular".to_owned(), "form", 0.8, "geometry"),
            "vertical" | "horizontal" => {
                push_derived(derived, "symmetric".to_owned(), "form", 0.65, "geometry");
            }
            _ => {}
        }
    }
    if shape.tick_count.unwrap_or(0) > 0 || shape.segment_count.unwrap_or(0) > 0 {
        push_derived(derived, "segmented".to_owned(), "form", 0.6, "geometry");
    }
    if let Some(density) = shape.density {
        let keyword = density_keyword(density);
        push_derived(derived, keyword.to_owned(), "density", 0.7, "geometry");
    }
    if let Some(texture) = &shape.texture {
        let texture = texture.to_ascii_lowercase();
        if matches!(
            texture.as_str(),
            "flat" | "noise" | "scanline" | "grid" | "glossy" | "matte"
        ) {
            push_derived(derived, texture, "material", 0.65, "geometry");
        }
    }
    if let Some(elevation) = &shape.elevation {
        match elevation.to_ascii_lowercase().as_str() {
            "raised" | "floating" => {
                push_derived(
                    derived,
                    "skeuomorphic".to_owned(),
                    "ornament",
                    0.6,
                    "geometry",
                );
            }
            "flat" | "inset" => {
                push_derived(derived, "minimal".to_owned(), "ornament", 0.55, "geometry");
            }
            _ => {}
        }
    }
}

fn derive_shape_motion_keywords(shape: &ShapeSignature, derived: &mut Vec<DerivedKeyword>) {
    if let Some(animation) = &shape.animation {
        let animation = animation.to_ascii_lowercase();
        if animation == "none" || animation == "static" {
            push_derived(derived, "static".to_owned(), "motion", 0.8, "motion");
        } else {
            push_derived(derived, "animated".to_owned(), "motion", 0.8, "motion");
            if matches!(animation.as_str(), "needle" | "pulse" | "sweep" | "shimmer") {
                push_derived(derived, animation, "motion", 0.75, "motion");
            }
        }
    }
}

fn derive_motion_keywords(motion: &[String], derived: &mut Vec<DerivedKeyword>) {
    if motion.is_empty() {
        return;
    }
    let mut animated = false;
    let mut static_motion = false;
    for hint in motion {
        let hint = hint.to_ascii_lowercase();
        match hint.as_str() {
            "static" | "none" | "reduced" | "reduced_motion" => static_motion = true,
            "animated" | "animation" => animated = true,
            "needle" | "pulse" | "sweep" | "shimmer" | "glow" => {
                animated = true;
                push_derived(derived, hint, "motion", 0.7, "motion");
            }
            _ => {}
        }
    }
    if animated {
        push_derived(derived, "animated".to_owned(), "motion", 0.8, "motion");
    } else if static_motion {
        push_derived(derived, "static".to_owned(), "motion", 0.8, "motion");
    }
}

fn density_keyword(density: f64) -> &'static str {
    if density <= 0.5 {
        "sparse"
    } else if density >= 5.0 {
        "dense"
    } else {
        "balanced"
    }
}

fn push_derived(
    derived: &mut Vec<DerivedKeyword>,
    keyword: String,
    field: &str,
    confidence: f64,
    method: &str,
) {
    if derived
        .iter()
        .any(|entry| entry.keyword.eq_ignore_ascii_case(&keyword) && entry.field == field)
    {
        return;
    }
    derived.push(DerivedKeyword {
        keyword,
        field: field.to_owned(),
        confidence: round4(confidence),
        method: method.to_owned(),
    });
}

fn rgb_to_hsl(red: u8, green: u8, blue: u8) -> (f64, f64, f64) {
    let red = f64::from(red) / 255.0;
    let green = f64::from(green) / 255.0;
    let blue = f64::from(blue) / 255.0;
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let lightness = (maximum + minimum) / 2.0;
    let delta = maximum - minimum;
    if delta <= f64::EPSILON {
        return (0.0, 0.0, round4(lightness));
    }
    let saturation = if lightness > 0.5 {
        delta / (2.0 - maximum - minimum)
    } else {
        delta / (maximum + minimum)
    };
    let hue = if maximum == red {
        ((green - blue) / delta).rem_euclid(6.0) * 60.0
    } else if maximum == green {
        ((blue - red) / delta + 2.0) * 60.0
    } else {
        ((red - green) / delta + 4.0) * 60.0
    };
    (round4(hue), round4(saturation), round4(lightness))
}

// ---------------------------------------------------------------------------
// Coherence scoring
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Dimension {
    Color,
    Typography,
    Form,
    Motion,
    Density,
}

impl Dimension {
    const ALL: [Dimension; 5] = [
        Dimension::Color,
        Dimension::Typography,
        Dimension::Form,
        Dimension::Motion,
        Dimension::Density,
    ];

    fn key(self) -> &'static str {
        match self {
            Dimension::Color => "color",
            Dimension::Typography => "typography",
            Dimension::Form => "form",
            Dimension::Motion => "motion",
            Dimension::Density => "density",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Dimension::Color => "palette",
            Dimension::Typography => "typography",
            Dimension::Form => "geometry",
            Dimension::Motion => "motion",
            Dimension::Density => "density",
        }
    }

    fn weight(self) -> f64 {
        match self {
            Dimension::Color => 0.25,
            Dimension::Typography => 0.2,
            Dimension::Form => 0.2,
            Dimension::Motion => 0.2,
            Dimension::Density => 0.15,
        }
    }
}

/// Score coherence across widgets, themes, or a scene.
///
/// The report is advisory. A single item returns `score = 1.0` with low
/// confidence. Call [`coherence_for_with`] to record intentional mixing.
pub fn coherence_for(items: &[CoherenceItem]) -> CoherenceReport {
    coherence_for_with(items, &CoherenceOptions::default())
}

/// Score coherence with explicit advisory controls.
pub fn coherence_for_with(items: &[CoherenceItem], options: &CoherenceOptions) -> CoherenceReport {
    if items.is_empty() {
        return CoherenceReport {
            score: 1.0,
            dimensions: CoherenceDimensions::default(),
            outliers: Vec::new(),
            conflicts: Vec::new(),
            suggestions: Vec::new(),
            confidence: 0.0,
        };
    }

    let item_keywords: Vec<BTreeMap<&'static str, BTreeSet<String>>> =
        items.iter().map(|item| item_feature_map(item)).collect();
    let item_count = items.len();

    let mut dimensions = CoherenceDimensions::default();
    let mut dimension_scores: BTreeMap<&'static str, f64> = BTreeMap::new();
    let mut data_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut conflicts: BTreeSet<String> = BTreeSet::new();

    for dimension in Dimension::ALL {
        let key = dimension.key();
        let sets: Vec<(usize, &BTreeSet<String>)> = item_keywords
            .iter()
            .enumerate()
            .filter_map(|(index, features)| features.get(key).map(|set| (index, set)))
            .filter(|(_, set)| !set.is_empty())
            .collect();
        data_counts.insert(key, sets.len());
        if item_count == 1 && !sets.is_empty() {
            dimension_scores.insert(key, 1.0);
        } else if let Some(score) = pairwise_score(&sets) {
            dimension_scores.insert(key, score);
        }
        for keyword in sets.iter().flat_map(|(_, set)| set.iter()) {
            for other in sets.iter().flat_map(|(_, set)| set.iter()) {
                if keyword < other && keywords_conflict(keyword, other) {
                    let mut pair = [keyword.as_str(), other.as_str()];
                    pair.sort_unstable();
                    conflicts.insert(format!("{} vs {}", pair[0], pair[1]));
                }
            }
        }
        let value = match dimension {
            Dimension::Color => &mut dimensions.color,
            Dimension::Typography => &mut dimensions.typography,
            Dimension::Form => &mut dimensions.form,
            Dimension::Motion => &mut dimensions.motion,
            Dimension::Density => &mut dimensions.density,
        };
        *value = dimension_scores.get(key).copied();
    }

    let mut total_weight = 0.0;
    let mut weighted = 0.0;
    for dimension in Dimension::ALL {
        if let Some(score) = dimension_scores.get(dimension.key()) {
            let weight = dimension.weight();
            total_weight += weight;
            weighted += score * weight;
        }
    }
    let mut score = if item_count == 1 || total_weight <= f64::EPSILON {
        1.0
    } else {
        round4(weighted / total_weight)
    };

    let coverage: f64 = Dimension::ALL
        .iter()
        .map(|dimension| {
            data_counts.get(dimension.key()).copied().unwrap_or(0) as f64 / item_count as f64
        })
        .sum::<f64>()
        / Dimension::ALL.len() as f64;
    let mut confidence = if item_count == 1 {
        0.25
    } else {
        round4(0.5 + 0.5 * coverage)
    };

    let mut outliers = Vec::new();
    if item_count >= 3 {
        let similarities: Vec<f64> = item_keywords
            .iter()
            .enumerate()
            .map(|(index, features)| {
                let mut best = 0.0_f64;
                for (other_index, other) in item_keywords.iter().enumerate() {
                    if index == other_index {
                        continue;
                    }
                    best = best.max(item_similarity(features, other));
                }
                best
            })
            .collect();
        let best = similarities.iter().copied().fold(0.0_f64, f64::max);
        for (index, similarity) in similarities.iter().enumerate() {
            if *similarity < 0.5 && *similarity < best - 0.25 {
                outliers.push(item_identifier(items, index));
            }
        }
        outliers.sort();
        outliers.dedup();
    }

    let intentional = options.intentional_mixing
        || items.iter().any(|item| {
            item.vibe
                .as_ref()
                .map(all_profile_keywords)
                .unwrap_or_default()
                .iter()
                .any(|keyword| keyword.eq_ignore_ascii_case("intentional_mixing"))
        });
    if intentional {
        score = 1.0;
        outliers.clear();
        conflicts.clear();
        confidence = confidence.max(0.25);
    }
    let accepted: BTreeSet<&str> = options
        .accepted_conflicts
        .iter()
        .map(String::as_str)
        .collect();
    conflicts.retain(|conflict| !accepted.contains(conflict.as_str()));

    let mut suggestions = Vec::new();
    for dimension in Dimension::ALL {
        if let Some(value) = dimension_scores.get(dimension.key()) {
            if *value < 0.8 {
                suggestions.push(format!("review {}", dimension.label()));
            }
        }
    }
    if intentional {
        suggestions.push("intentional mixing accepted".to_owned());
    } else if !conflicts.is_empty() {
        suggestions.push("resolve conflicting keywords".to_owned());
    }
    suggestions.sort();
    suggestions.dedup();

    CoherenceReport {
        score,
        dimensions,
        outliers,
        conflicts: conflicts.into_iter().collect(),
        suggestions,
        confidence,
    }
}

fn pairwise_score(sets: &[(usize, &BTreeSet<String>)]) -> Option<f64> {
    if sets.len() < 2 {
        return None;
    }
    let mut total = 0.0;
    let mut comparisons = 0usize;
    for (index, (_, left)) in sets.iter().enumerate() {
        for (_, right) in sets.iter().skip(index + 1) {
            total += jaccard(left, right);
            comparisons += 1;
        }
    }
    if comparisons == 0 {
        None
    } else {
        Some(round4(total / comparisons as f64))
    }
}

fn jaccard(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    let intersection = left.intersection(right).count() as f64;
    let union = left.union(right).count() as f64;
    if union == 0.0 {
        1.0
    } else {
        intersection / union
    }
}

fn item_similarity(
    left: &BTreeMap<&'static str, BTreeSet<String>>,
    right: &BTreeMap<&'static str, BTreeSet<String>>,
) -> f64 {
    let mut total = 0.0;
    let mut comparisons = 0usize;
    for dimension in Dimension::ALL {
        let key = dimension.key();
        let (Some(left), Some(right)) = (left.get(key), right.get(key)) else {
            continue;
        };
        if left.is_empty() || right.is_empty() {
            continue;
        }
        total += jaccard(left, right);
        comparisons += 1;
    }
    if comparisons == 0 {
        0.0
    } else {
        total / comparisons as f64
    }
}

fn item_identifier(items: &[CoherenceItem], index: usize) -> String {
    let id = items[index].id.trim();
    if id.is_empty() {
        format!("item_{}", index + 1)
    } else {
        id.to_owned()
    }
}

fn item_feature_map(item: &CoherenceItem) -> BTreeMap<&'static str, BTreeSet<String>> {
    let mut features: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    if let Some(vibe) = &item.vibe {
        insert_keywords(&mut features, "color", &vibe.color);
        insert_keywords(&mut features, "typography", &vibe.typography);
        insert_keywords(&mut features, "form", &vibe.form);
        insert_keywords(&mut features, "motion", &vibe.motion);
        if let Some(density) = &vibe.density {
            insert_keywords(&mut features, "density", std::slice::from_ref(density));
        }
        for derived in &vibe.derived {
            insert_keywords(
                &mut features,
                derived_field(derived),
                std::slice::from_ref(&derived.keyword),
            );
        }
    }
    if let Some(shape) = &item.shape {
        for keyword in shape_form_keywords(shape) {
            insert_keywords(&mut features, "form", &[keyword]);
        }
        if let Some(density) = shape.density {
            insert_keywords(
                &mut features,
                "density",
                &[density_keyword(density).to_owned()],
            );
        }
        if let Some(animation) = &shape.animation {
            let animation = animation.to_ascii_lowercase();
            if animation == "none" || animation == "static" {
                insert_keywords(&mut features, "motion", &["static".to_owned()]);
            } else {
                insert_keywords(&mut features, "motion", &["animated".to_owned()]);
                insert_keywords(&mut features, "motion", &[animation]);
            }
        }
    }
    features
}

fn derived_field(derived: &DerivedKeyword) -> &'static str {
    match derived.field.as_str() {
        "color" => "color",
        "typography" => "typography",
        "form" => "form",
        "motion" => "motion",
        "density" => "density",
        _ => "form",
    }
}

fn insert_keywords(
    features: &mut BTreeMap<&'static str, BTreeSet<String>>,
    key: &'static str,
    values: &[String],
) {
    let entry = features.entry(key).or_default();
    for value in values {
        let value = value.trim().to_ascii_lowercase();
        if !value.is_empty() {
            entry.insert(value);
        }
    }
}

fn shape_form_keywords(shape: &ShapeSignature) -> Vec<String> {
    let mut keywords = Vec::new();
    if let Some(radius) = shape.corner_radius_ratio {
        if radius >= 0.35 {
            keywords.push("rounded".to_owned());
        } else if radius <= 0.08 {
            keywords.push("sharp".to_owned());
        }
    }
    if let Some(symmetry) = &shape.symmetry {
        match symmetry.to_ascii_lowercase().as_str() {
            "radial" => keywords.push("circular".to_owned()),
            "vertical" | "horizontal" => keywords.push("symmetric".to_owned()),
            _ => {}
        }
    }
    if shape.tick_count.unwrap_or(0) > 0 || shape.segment_count.unwrap_or(0) > 0 {
        keywords.push("segmented".to_owned());
    }
    keywords
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn widget() -> WidgetIR {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/display_ir/widget_ir.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("widget")
    }

    fn profile(value: serde_json::Value) -> VibeProfile {
        serde_json::from_value(value).expect("profile")
    }

    fn item(id: &str, value: serde_json::Value, shape: Option<ShapeSignature>) -> CoherenceItem {
        CoherenceItem {
            id: id.to_owned(),
            vibe: Some(profile(value)),
            shape,
        }
    }

    #[test]
    fn widget_geometry_is_extracted_without_overwriting_source_fields() {
        let mut source = widget();
        let shape = extract_shape(&source);
        assert_eq!(shape.widget_kind.as_deref(), Some("gauge"));
        assert_eq!(shape.aspect_ratio, Some(1.0));
        assert_eq!(shape.corner_radius_ratio, Some(0.5));
        assert_eq!(shape.stroke_width_ratio, Some(0.04));
        assert_eq!(shape.tick_count, Some(11));
        assert_eq!(shape.segment_count, Some(20));
        assert_eq!(shape.symmetry.as_deref(), Some("radial"));
        assert_eq!(shape.arc_start_deg, Some(135.0));
        assert_eq!(shape.arc_end_deg, Some(405.0));
        assert_eq!(shape.arc_span_deg, Some(270.0));
        assert_eq!(shape.palette_size, Some(3));
        assert!(shape.confidence.unwrap() > 0.0);
        assert!(shape
            .provenance
            .methods
            .iter()
            .any(|method| method == "widget_ir"));
        assert!(shape
            .provenance
            .derived_fields
            .iter()
            .any(|field| field == "density"));

        source.shape = Some(ShapeSignature {
            aspect_ratio: Some(2.0),
            corner_radius_ratio: Some(0.1),
            ..ShapeSignature::default()
        });
        let merged = extract_shape(&source);
        assert_eq!(merged.aspect_ratio, Some(2.0));
        assert_eq!(merged.corner_radius_ratio, Some(0.1));
        assert!(merged
            .provenance
            .source_fields
            .iter()
            .any(|field| field == "aspect_ratio"));
    }

    #[test]
    fn svg_shape_and_palette_are_parsed() {
        let svg = r##"<svg viewBox="0 0 200 100" width="200" height="100">
            <rect x="0" y="0" width="100" height="50" rx="10" ry="10" fill="#ff0000"
                  stroke="rgb(0, 128, 255)" stroke-width="4" />
            <path d="M 0 0 L 10 10 A 5 5 0 1 0 20 20 A 5 5 0 0 1 30 30 Z" />
            <circle cx="50" cy="50" r="20" fill="#abc" />
            <text font-family="'Inter', sans-serif" fill="rgba(0,0,0,0.5)">x</text>
            <animate attributeName="opacity" />
        </svg>"##;
        let extracted = extract_from_svg(svg).expect("valid svg");
        assert_eq!(extracted.shape.aspect_ratio, Some(2.0));
        assert_eq!(extracted.shape.orientation.as_deref(), Some("horizontal"));
        assert_eq!(extracted.shape.corner_radius_ratio, Some(0.1));
        assert_eq!(extracted.shape.stroke_width_ratio, Some(0.04));
        assert_eq!(extracted.shape.fill_ratio, Some(0.25));
        assert_eq!(extracted.shape.segment_count, Some(3));
        assert_eq!(extracted.shape.arc_span_deg, Some(360.0));
        assert_eq!(extracted.shape.symmetry.as_deref(), Some("radial"));
        assert_eq!(extracted.animated, Some(true));
        assert!(extracted.colors.contains(&"#ff0000".to_owned()));
        assert!(extracted.colors.contains(&"#0080ff".to_owned()));
        assert!(extracted.fonts.iter().any(|font| font == "Inter"));
    }

    #[test]
    fn css_shape_and_palette_are_parsed() {
        let css = r#"
            .gauge {
                width: 120px;
                height: 60px;
                border-radius: 50%;
                border-width: 2px;
                background: linear-gradient(#ff0000, #0000ff);
                color: rgb(0, 128, 0);
                font-family: "JetBrains Mono", monospace;
                animation: sweep 1s linear infinite;
            }
        "#;
        let extracted = extract_from_css(css).expect("valid css");
        assert_eq!(extracted.shape.aspect_ratio, Some(2.0));
        assert_eq!(extracted.shape.corner_radius_ratio, Some(0.5));
        assert_eq!(extracted.shape.stroke_width_ratio, Some(0.0333));
        assert_eq!(extracted.animated, Some(true));
        assert!(extracted.colors.contains(&"#ff0000".to_owned()));
        assert!(extracted.colors.contains(&"#0000ff".to_owned()));
        assert!(extracted.colors.contains(&"#008000".to_owned()));
        assert_eq!(extracted.gradients, vec!["linear-gradient"]);
        assert!(extracted.fonts.iter().any(|font| font == "JetBrains Mono"));
    }

    #[test]
    fn malformed_and_oversized_inputs_are_rejected() {
        assert!(matches!(
            extract_from_svg(""),
            Err(IrError::EmptyField("svg"))
        ));
        assert!(extract_from_svg("<svg viewBox='0 0 10 10'>").is_err());
        assert!(
            extract_from_svg("<svg viewBox='0 0 10 10'><path d='A 1 1 0 2 0 1 1'/></svg>").is_err()
        );
        assert!(extract_from_svg("<!ENTITY xxe SYSTEM 'file:///etc/passwd'><svg/>").is_err());
        assert!(matches!(
            extract_from_css(""),
            Err(IrError::EmptyField("css"))
        ));
        assert!(extract_from_css(".a { color: red; ").is_err());
        assert!(extract_from_css(".a { color: \"red; }").is_err());
        let oversized = "x".repeat(MAX_TEXT_INPUT + 1);
        assert!(extract_from_css(&oversized).is_err());
    }

    #[test]
    fn path_arc_flags_are_validated_and_counted() {
        let stats = parse_svg_path("M0 0 A 5 5 0 1 0 10 10 A 5 5 0 0 1 20 20").expect("path");
        assert_eq!(stats.commands, 3);
        assert_eq!(stats.segments, 2);
        assert_eq!(stats.arcs, 2);
        assert_eq!(stats.large_arc_flags, 1);
        assert_eq!(stats.sweep_flags, 1);
        assert!(parse_svg_path("M0 0 A 5 5 0 2 0 10 10").is_err());
        assert!(parse_svg_path("M0 0 L").is_err());
    }

    #[test]
    fn vibe_layers_are_separate_and_user_wins_conflicts() {
        let source = profile(json!({
            "taxonomy": "org.ozma.vibe/1",
            "keywords": ["rounded"],
            "form": ["rounded"],
            "provenance": {"source_keywords": ["rounded"], "methods": ["manifest"]}
        }));
        let user = profile(json!({
            "keywords": ["sharp", "not_in_taxonomy"],
            "form": ["sharp"],
            "provenance": {"user_keywords": ["sharp", "not_in_taxonomy"], "methods": ["human"]}
        }));
        let derived = vec![
            DerivedKeyword {
                keyword: "rounded".to_owned(),
                field: "form".to_owned(),
                confidence: 0.8,
                method: "geometry".to_owned(),
            },
            DerivedKeyword {
                keyword: "warm".to_owned(),
                field: "color".to_owned(),
                confidence: 0.7,
                method: "palette".to_owned(),
            },
        ];
        let merged = merge_vibe_keywords(Some(&source), Some(&user), &derived);
        assert!(merged.keywords.contains(&"sharp".to_owned()));
        assert!(merged.keywords.contains(&"not_in_taxonomy".to_owned()));
        assert!(!merged.keywords.contains(&"rounded".to_owned()));
        assert!(!merged.form.contains(&"rounded".to_owned()));
        assert_eq!(merged.provenance.source_keywords, vec!["rounded"]);
        assert_eq!(
            merged.provenance.user_keywords,
            vec!["sharp", "not_in_taxonomy"]
        );
        assert_eq!(merged.provenance.derived_keywords, vec!["warm"]);
        assert!(merged.provenance.methods.contains(&"geometry".to_owned()));
        assert_eq!(merged.derived.len(), 1);
    }

    #[test]
    fn derive_vibe_uses_palette_typography_geometry_and_motion() {
        let input = VibeInput {
            colors: vec!["#ff5500".to_owned(), "#ffcc00".to_owned()],
            fonts: vec!["JetBrains Mono".to_owned()],
            typography: Some(VibeTypography {
                families: Vec::new(),
                weight: Some(700),
                width: Some("condensed".to_owned()),
            }),
            shape: Some(ShapeSignature {
                corner_radius_ratio: Some(0.5),
                symmetry: Some("radial".to_owned()),
                tick_count: Some(12),
                density: Some(8.0),
                animation: Some("sweep".to_owned()),
                ..ShapeSignature::default()
            }),
            motion: Vec::new(),
            references: vec!["aviation".to_owned()],
            source: None,
            user: None,
        };
        let vibe = derive_vibe(&input);
        assert_eq!(vibe.taxonomy, VIBE_TAXONOMY);
        assert!(vibe.color.contains(&"warm".to_owned()));
        assert!(vibe.typography.contains(&"monospace".to_owned()));
        assert!(vibe.typography.contains(&"bold".to_owned()));
        assert!(vibe.typography.contains(&"condensed".to_owned()));
        assert!(vibe.form.contains(&"rounded".to_owned()));
        assert!(vibe.form.contains(&"circular".to_owned()));
        assert!(vibe.density.as_deref() == Some("dense"));
        assert!(vibe.motion.contains(&"animated".to_owned()));
        assert!(vibe.motion.contains(&"sweep".to_owned()));
        assert!(vibe.references.contains(&"aviation".to_owned()));
        assert!(vibe.provenance.source_keywords.is_empty());
        assert!(vibe.provenance.user_keywords.is_empty());
        assert!(!vibe.provenance.derived_keywords.is_empty());
        assert!(vibe.derived.iter().all(|entry| entry.confidence > 0.0));
        vibe.validate().expect("valid vibe");
    }

    #[test]
    fn coherence_single_item_is_neutral_and_bounds_hold() {
        let single = coherence_for(&[item(
            "only",
            json!({"keywords": ["calm"], "color": ["warm"]}),
            None,
        )]);
        assert_eq!(single.score, 1.0);
        assert_eq!(single.confidence, 0.25);
        assert_eq!(single.dimensions.color, Some(1.0));
        single.validate().expect("valid");

        let empty = coherence_for(&[]);
        assert_eq!(empty.score, 1.0);
        assert_eq!(empty.confidence, 0.0);
    }

    #[test]
    fn coherence_marks_outliers_and_allows_intentional_mixing() {
        let items = vec![
            item(
                "a",
                json!({"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"}),
                None,
            ),
            item(
                "b",
                json!({"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"}),
                None,
            ),
            item(
                "c",
                json!({"color": ["cool"], "form": ["sharp"], "motion": ["animated"], "density": "dense"}),
                None,
            ),
        ];
        let report = coherence_for(&items);
        assert!(report.score < 1.0);
        assert!(report.dimensions.color.unwrap() < 1.0);
        assert!(report.outliers.contains(&"c".to_owned()));
        assert!(report
            .conflicts
            .iter()
            .any(|conflict| conflict == "rounded vs sharp"));
        assert!(report
            .conflicts
            .iter()
            .any(|conflict| conflict == "animated vs static"));

        let options = CoherenceOptions {
            intentional_mixing: true,
            accepted_conflicts: Vec::new(),
        };
        let overridden = coherence_for_with(&items, &options);
        assert_eq!(overridden.score, 1.0);
        assert!(overridden.conflicts.is_empty());
        assert!(overridden.outliers.is_empty());
        assert!(overridden
            .suggestions
            .contains(&"intentional mixing accepted".to_owned()));

        let accepted = CoherenceOptions {
            intentional_mixing: false,
            accepted_conflicts: vec!["rounded vs sharp".to_owned()],
        };
        let filtered = coherence_for_with(&items, &accepted);
        assert!(!filtered.conflicts.contains(&"rounded vs sharp".to_owned()));
    }

    #[test]
    fn output_is_deterministic_and_round_trips_through_serde() {
        let items = vec![
            item("a", json!({"color": ["warm"], "form": ["rounded"]}), None),
            item("b", json!({"color": ["cool"], "form": ["sharp"]}), None),
        ];
        let first = serde_json::to_string(&coherence_for(&items)).expect("serializes");
        let second = serde_json::to_string(&coherence_for(&items)).expect("serializes");
        assert_eq!(first, second);

        let report: CoherenceReport = serde_json::from_str(&first).expect("round trip");
        report.validate().expect("valid");
        assert_eq!(first, serde_json::to_string(&report).expect("stable"));

        let shape =
            extract_from_svg("<svg viewBox='0 0 10 20'><rect width='5' height='5' rx='2'/></svg>")
                .expect("svg")
                .shape;
        let encoded = serde_json::to_string(&shape).expect("shape serializes");
        let decoded: ShapeSignature = serde_json::from_str(&encoded).expect("shape parses");
        assert_eq!(shape, decoded);

        let vibe = profile(json!({"keywords": ["x"]}));
        let encoded = serde_json::to_string(&vibe).expect("vibe serializes");
        let decoded: VibeProfile = serde_json::from_str(&encoded).expect("vibe parses");
        assert_eq!(vibe, decoded);
    }

    #[test]
    fn golden_json_matches_python_fixtures() {
        let vibe = derive_vibe(&VibeInput {
            colors: vec!["#ff0000".to_owned()],
            shape: Some(ShapeSignature {
                corner_radius_ratio: Some(0.0),
                ..ShapeSignature::default()
            }),
            ..VibeInput::default()
        });
        assert_eq!(
            serde_json::to_value(&vibe).expect("json"),
            json!({
                "taxonomy": "org.ozma.vibe/1",
                "keywords": ["neon", "warm", "sharp"],
                "mood": [],
                "form": ["sharp"],
                "material": [],
                "color": ["neon", "warm"],
                "typography": [],
                "motion": [],
                "references": [],
                "ornament": [],
                "confidence": 0.8167,
                "provenance": {
                    "source_keywords": [],
                    "derived_keywords": ["neon", "warm", "sharp"],
                    "user_keywords": [],
                    "methods": ["geometry", "palette"]
                },
                "derived": [
                    {"keyword": "neon", "field": "color", "confidence": 0.75, "method": "palette"},
                    {"keyword": "warm", "field": "color", "confidence": 0.9, "method": "palette"},
                    {"keyword": "sharp", "field": "form", "confidence": 0.8, "method": "geometry"}
                ]
            })
        );

        let items = vec![
            item(
                "a",
                json!({"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"}),
                None,
            ),
            item(
                "b",
                json!({"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"}),
                None,
            ),
            item(
                "c",
                json!({"color": ["cool"], "form": ["sharp"], "motion": ["animated"], "density": "dense"}),
                None,
            ),
        ];
        assert_eq!(
            serde_json::to_value(coherence_for(&items)).expect("json"),
            json!({
                "score": 0.3333,
                "dimensions": {
                    "form": 0.3333,
                    "color": 0.3333,
                    "density": 0.3333,
                    "motion": 0.3333
                },
                "outliers": ["c"],
                "conflicts": [
                    "animated vs static",
                    "cool vs warm",
                    "dense vs sparse",
                    "rounded vs sharp"
                ],
                "suggestions": [
                    "resolve conflicting keywords",
                    "review density",
                    "review geometry",
                    "review motion",
                    "review palette"
                ],
                "confidence": 0.9
            })
        );
    }

    #[test]
    fn derived_golden_json_matches_expected_shape() {
        let input = VibeInput {
            colors: vec!["#ff0000".to_owned()],
            shape: Some(ShapeSignature {
                corner_radius_ratio: Some(0.0),
                ..ShapeSignature::default()
            }),
            ..VibeInput::default()
        };
        let vibe = derive_vibe(&input);
        let value = serde_json::to_value(&vibe).expect("json");
        assert_eq!(value["taxonomy"], VIBE_TAXONOMY);
        assert_eq!(value["color"], json!(["neon", "warm"]));
        assert_eq!(value["form"], json!(["sharp"]));
        assert_eq!(
            value["provenance"]["derived_keywords"],
            json!(["neon", "warm", "sharp"])
        );
        assert_eq!(value["derived"][0]["method"], "palette");
        assert_eq!(value["derived"][1]["method"], "palette");
        assert_eq!(value["derived"][2]["method"], "geometry");

        let report = coherence_for(&[
            item("a", json!({"color": ["warm"]}), None),
            item("b", json!({"color": ["warm"]}), None),
        ]);
        let report_value = serde_json::to_value(&report).expect("json");
        assert_eq!(report_value["score"], 1.0);
        assert_eq!(report_value["dimensions"]["color"], 1.0);
        assert_eq!(report_value["conflicts"], json!([]));
    }
}
