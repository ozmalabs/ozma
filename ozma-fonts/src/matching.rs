// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Deterministic, shape-aware font matching and export planning.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::intent::{
    FontIntent, FontTraits, FontTranslationAction, FontTranslationReport, TargetFontProfile,
};
use crate::metadata::{normalize_feature, normalize_script, FontMetadata, Panose};

const WEIGHT_PANOSE: f32 = 0.22;
const WEIGHT_METRICS: f32 = 0.30;
const WEIGHT_COVERAGE: f32 = 0.26;
const WEIGHT_FEATURES: f32 = 0.10;
const WEIGHT_ROLE: f32 = 0.07;
const WEIGHT_FORMAT: f32 = 0.05;

/// Component scores for one candidate. Every component is in `0..=1`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoreBreakdown {
    /// Role/classification fit.
    pub role: f32,
    /// PANOSE or trait-based shape fit.
    pub panose: f32,
    /// Metric compatibility.
    pub metrics: f32,
    /// Script/glyph coverage.
    pub coverage: f32,
    /// OpenType feature coverage.
    pub features: f32,
    /// Target format compatibility.
    pub format: f32,
    /// Weighted total.
    pub total: f32,
}

/// A scored candidate font.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FontMatch {
    /// The complete candidate metadata, retained so reports stay lossless.
    pub font: FontMetadata,
    /// Weighted score in `0..=1`.
    pub score: f32,
    /// Component scores.
    pub breakdown: ScoreBreakdown,
    /// Required scripts the candidate does not cover.
    pub missing_scripts: Vec<String>,
    /// Required features the candidate does not expose.
    pub missing_features: Vec<String>,
}

/// Score all candidates against an intent.
///
/// Candidates that fail the role/classification hard filters are omitted.
/// Results are sorted by score descending, then by family, style, and SHA-256 so
/// equal-scoring candidates have a stable order.
pub fn match_font(intent: &FontIntent, candidates: &[FontMetadata]) -> Vec<FontMatch> {
    let mut matches: Vec<FontMatch> = candidates
        .iter()
        .filter(|candidate| passes_hard_filters(intent, candidate))
        .filter_map(|candidate| {
            let breakdown = score_candidate(intent, candidate)?;
            Some(FontMatch {
                font: candidate.clone(),
                score: breakdown.total,
                breakdown,
                missing_scripts: missing_scripts(intent, candidate),
                missing_features: missing_features(intent, candidate),
            })
        })
        .collect();

    matches.sort_by(compare_matches);
    matches
}

/// Choose the highest-scoring candidate, if any pass the hard filters.
pub fn choose_font(intent: &FontIntent, candidates: &[FontMetadata]) -> Option<FontMatch> {
    match_font(intent, candidates).into_iter().next()
}

/// Score a single candidate, ignoring the hard filters.
pub fn score_candidate(intent: &FontIntent, candidate: &FontMetadata) -> Option<ScoreBreakdown> {
    let role = role_score(intent, candidate);
    let shape = if let Some(panose) = intent.traits.panose.as_deref() {
        candidate
            .panose
            .as_deref()
            .and_then(|candidate_panose| panose_distance(panose, candidate_panose))
            .map_or_else(
                || trait_shape_score(&intent.traits, candidate),
                |distance| 1.0 - distance,
            )
    } else {
        trait_shape_score(&intent.traits, candidate)
    };
    let metrics = metric_score(&intent.traits, candidate);
    let coverage = script_coverage(&intent.scripts, candidate);
    let features = feature_coverage(&intent.features, candidate);
    let format = format_score(intent.target.as_ref(), candidate);

    let total = role * WEIGHT_ROLE
        + shape * WEIGHT_PANOSE
        + metrics * WEIGHT_METRICS
        + coverage * WEIGHT_COVERAGE
        + features * WEIGHT_FEATURES
        + format * WEIGHT_FORMAT;

    Some(ScoreBreakdown {
        role: round_score(role),
        panose: round_score(shape),
        metrics: round_score(metrics),
        coverage: round_score(coverage),
        features: round_score(features),
        format: round_score(format),
        total: round_score(total),
    })
}

/// PANOSE distance in `0..=1`; `0` is identical and a different PANOSE family
/// kind is the maximum distance.
pub fn panose_distance(left: &str, right: &str) -> Option<f32> {
    let left = Panose::parse(left)?;
    let right = Panose::parse(right)?;
    if left.family_kind() != right.family_kind() {
        return Some(1.0);
    }
    if left.is_empty() || right.is_empty() {
        return Some(0.5);
    }
    const WEIGHTS: [f32; 10] = [1.0, 0.18, 0.18, 0.12, 0.10, 0.12, 0.08, 0.08, 0.07, 0.07];
    let total_weight: f32 = WEIGHTS.iter().sum();
    let mut distance = 0.0;
    for (index, weight) in WEIGHTS.iter().enumerate() {
        let difference = (f32::from(left.0[index]) - f32::from(right.0[index])).abs() / 15.0;
        distance += difference * weight;
    }
    Some((distance / total_weight).clamp(0.0, 1.0))
}

/// Plan an export for `original` and a candidate corpus.
///
/// The decision order is:
/// 1. use the original exactly when its licence, embedding bits, format, and
///    size allow it;
/// 2. subset the original when it exceeds the target's size budget and
///    subsetting is permitted;
/// 3. rasterise an original or substitute for bitmap-only targets;
/// 4. substitute a shape-compatible, legally embeddable candidate;
/// 5. otherwise report `unavailable`.
///
/// Every path returns a report; export is the only loss boundary.
pub fn plan_export(
    intent: &FontIntent,
    original: Option<&FontMetadata>,
    candidates: &[FontMetadata],
    target: &TargetFontProfile,
) -> FontTranslationReport {
    let mut warnings = Vec::new();
    let mut report = FontTranslationReport {
        source_font: original
            .map(|font| font.asset.family.clone())
            .unwrap_or_default(),
        ..FontTranslationReport::default()
    };

    let source_license_ok = original.is_some_and(|font| font.asset.license_allowed());
    let source_embed_ok = original.is_some_and(|font| font.asset.embedding_allowed);
    let source_bitmap_ok = original.is_some_and(|font| font.asset.bitmap_only);

    if let Some(font) = original {
        if !source_license_ok {
            warnings.push(format!(
                "source font '{}' has a non-redistributable or unknown licence",
                font.asset.family
            ));
        }
        if !source_embed_ok && !source_bitmap_ok {
            warnings.push(format!(
                "source font '{}' does not permit embedding (OS/2 fsType)",
                font.asset.family
            ));
        }
    }

    if let Some(font) = original {
        let format_ok = target.supports_format(&font.asset.format);
        if source_license_ok
            && target.raster_only()
            && !format_ok
            && (source_embed_ok || source_bitmap_ok)
        {
            return direct_report(
                intent,
                report,
                FontTranslationAction::Rasterized,
                font,
                original,
                warnings,
            );
        }
        if source_license_ok && source_embed_ok && format_ok {
            let oversized = !target.fits_bytes(font.asset.size_bytes);
            if !oversized {
                return direct_report(
                    intent,
                    report,
                    FontTranslationAction::Exact,
                    font,
                    original,
                    warnings,
                );
            }
            if font.asset.subset_allowed {
                return direct_report(
                    intent,
                    report,
                    FontTranslationAction::Subset,
                    font,
                    original,
                    warnings,
                );
            }
            warnings.push(format!(
                "source font '{}' exceeds the target size budget and subsetting is not permitted",
                font.asset.family
            ));
        }
    }

    let mut match_intent = intent.clone();
    match_intent.target = Some(target.clone());
    if let Some(font) = original {
        preserve_source_metrics(&mut match_intent.traits, font);
    }

    let matches = match_font(&match_intent, candidates);
    let mut rejected_oversize: Option<String> = None;
    let mut chosen: Option<(FontMatch, bool)> = None;
    for candidate in matches {
        let font = &candidate.font;
        let license_ok = font.asset.license_allowed();
        let embedding_ok =
            font.asset.embedding_allowed || (target.raster_only() && font.asset.bitmap_only);
        if !license_ok || !embedding_ok {
            continue;
        }
        if !target.raster_only() && !target.supports_format(&font.asset.format) {
            continue;
        }
        let oversized = !target.raster_only() && !target.fits_bytes(font.asset.size_bytes);
        if oversized && !font.asset.subset_allowed {
            rejected_oversize = Some(font.asset.family.clone());
            continue;
        }
        chosen = Some((candidate, oversized));
        break;
    }

    let Some((chosen, oversized)) = chosen else {
        if let Some(family) = rejected_oversize {
            warnings.push(format!(
                "candidate '{family}' exceeds the target size budget and cannot be subset"
            ));
        }
        let missing = required_scripts(intent);
        report.action = FontTranslationAction::Unavailable;
        // There is no chosen output font, so the licence gate did not pass for
        // an exportable asset even when the original itself is licence-clean.
        report.license_ok = false;
        report.dropped_glyphs = missing
            .iter()
            .map(|script| format!("script:{script}"))
            .collect();
        report.dropped_features = normalized_features(&intent.features);
        report.warnings = warnings;
        return report;
    };

    let font = &chosen.font;
    let rasterized = target.raster_only() && !target.supports_format(&font.asset.format);
    let action = if rasterized {
        FontTranslationAction::Rasterized
    } else if oversized {
        FontTranslationAction::Subset
    } else {
        FontTranslationAction::Substituted
    };

    if action == FontTranslationAction::Substituted {
        warnings.push(format!(
            "substituted '{}' with '{}' (score {:.4})",
            report.source_font, font.asset.family, chosen.score
        ));
    }

    report.action = action;
    report.target_font = font.asset.family.clone();
    report.shape_score = Some(chosen.score);
    report.metrics = Some(metric_map(original, Some(font)));
    report.dropped_glyphs = missing_scripts(intent, font)
        .iter()
        .map(|script| format!("script:{script}"))
        .collect();
    report.dropped_features = missing_features(intent, font);
    report.license_ok = font.asset.can_embed() || font.asset.bitmap_only;
    report.warnings = warnings;
    report
}

fn direct_report(
    intent: &FontIntent,
    mut report: FontTranslationReport,
    action: FontTranslationAction,
    font: &FontMetadata,
    original: Option<&FontMetadata>,
    warnings: Vec<String>,
) -> FontTranslationReport {
    report.action = action;
    report.target_font = font.asset.family.clone();
    report.metrics = Some(metric_map(original, Some(font)));
    report.dropped_glyphs = missing_scripts(intent, font)
        .iter()
        .map(|script| format!("script:{script}"))
        .collect();
    report.dropped_features = missing_features(intent, font);
    report.license_ok = true;
    report.warnings = warnings;
    report
}

fn compare_matches(left: &FontMatch, right: &FontMatch) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| {
            left.font
                .asset
                .family
                .to_ascii_lowercase()
                .cmp(&right.font.asset.family.to_ascii_lowercase())
        })
        .then_with(|| left.font.asset.style.cmp(&right.font.asset.style))
        .then_with(|| left.font.asset.sha256.cmp(&right.font.asset.sha256))
        .then_with(|| left.font.asset.id.cmp(&right.font.asset.id))
}

fn passes_hard_filters(intent: &FontIntent, candidate: &FontMetadata) -> bool {
    let intended = normalize_classification(&intent.classification);
    let actual = normalize_classification(&candidate.classification);
    if !intended.is_empty() && !actual.is_empty() && intended != actual {
        return false;
    }
    match intent.role.trim().to_ascii_lowercase().as_str() {
        "icon" => actual.is_empty() || actual == "icon",
        "code" => actual.is_empty() || actual == "mono" || candidate.is_monospaced,
        _ => true,
    }
}

fn normalize_classification(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "sans-serif" | "sans serif" | "grotesque" => "sans".to_owned(),
        "monospaced" | "monospace" => "mono".to_owned(),
        "cursive" => "script".to_owned(),
        other => other.to_owned(),
    }
}

fn role_score(intent: &FontIntent, candidate: &FontMetadata) -> f32 {
    let classification = normalize_classification(&candidate.classification);
    let role = intent.role.trim().to_ascii_lowercase();
    match role.as_str() {
        "" => 0.75,
        "icon" => f32::from(classification == "icon"),
        "code" => {
            if classification == "mono" || candidate.is_monospaced {
                1.0
            } else {
                0.0
            }
        }
        "numeric" => match classification.as_str() {
            "mono" => 1.0,
            "sans" | "serif" => 0.8,
            "display" => 0.6,
            _ => 0.4,
        },
        "display" | "brand" => match classification.as_str() {
            "display" => 1.0,
            "sans" => 0.75,
            "serif" => 0.65,
            _ => 0.4,
        },
        "body" | "label" | "caption" => match classification.as_str() {
            "sans" => 1.0,
            "serif" => 0.85,
            "mono" => 0.6,
            "display" => 0.7,
            _ => 0.5,
        },
        _ => 0.75,
    }
}

fn trait_shape_score(traits: &FontTraits, candidate: &FontMetadata) -> f32 {
    let mut score = 0.0;
    let mut weight = 0.0;

    if let Some(target_weight) = traits.weight {
        let difference = (f32::from(candidate.weight) - f32::from(target_weight)).abs() / 800.0;
        score += (1.0 - difference).clamp(0.0, 1.0) * 0.25;
        weight += 0.25;
    }
    if let Some(target_width) = traits.width.as_deref().and_then(width_number) {
        let difference = (f32::from(candidate.width) - target_width).abs() / 8.0;
        score += (1.0 - difference).clamp(0.0, 1.0) * 0.20;
        weight += 0.20;
    }
    if let Some(target_slant) = traits.slant.as_deref() {
        score += slant_score(target_slant, &candidate.slant) * 0.15;
        weight += 0.15;
    }
    if let Some(target_contrast) = traits.contrast.as_deref() {
        if let Some(candidate_contrast) = candidate.panose_value().and_then(panose_contrast) {
            score += normalized_eq_score(target_contrast, candidate_contrast) * 0.15;
            weight += 0.15;
        }
    }
    if let Some(target_roundness) = traits.roundness.as_deref() {
        if let Some(candidate_roundness) = candidate.panose_value().and_then(panose_roundness) {
            score += normalized_eq_score(target_roundness, candidate_roundness) * 0.10;
            weight += 0.10;
        }
    }
    if let Some(target_x_height) = traits.x_height {
        if let Some(actual) = candidate.x_height_ratio() {
            score += closeness(
                normalize_intent_ratio(target_x_height, traits.units_per_em),
                actual,
            ) * 0.075;
            weight += 0.075;
        }
    }
    if let Some(target_cap_height) = traits.cap_height {
        if let Some(actual) = candidate.cap_height_ratio() {
            score += closeness(
                normalize_intent_ratio(target_cap_height, traits.units_per_em),
                actual,
            ) * 0.075;
            weight += 0.075;
        }
    }

    if weight == 0.0 {
        1.0
    } else {
        (score / weight).clamp(0.0, 1.0)
    }
}

fn width_number(width: &str) -> Option<f32> {
    let normalized = width.trim().to_ascii_lowercase().replace('_', "-");
    let number = match normalized.as_str() {
        "ultra-condensed" => 1.0,
        "extra-condensed" => 2.0,
        "condensed" => 3.0,
        "semi-condensed" => 4.0,
        "normal" | "regular" => 5.0,
        "semi-expanded" => 6.0,
        "expanded" => 7.0,
        "extra-expanded" => 8.0,
        "ultra-expanded" => 9.0,
        other => other.parse::<f32>().ok()?,
    };
    Some(number)
}

fn slant_score(target: &str, actual: &str) -> f32 {
    let target = target.trim().to_ascii_lowercase();
    let actual = actual.trim().to_ascii_lowercase();
    if target == actual {
        1.0
    } else if matches!(target.as_str(), "italic" | "oblique")
        && matches!(actual.as_str(), "italic" | "oblique")
    {
        0.8
    } else if target == "upright" || actual == "upright" {
        0.15
    } else {
        0.5
    }
}

fn panose_contrast(panose: Panose) -> Option<&'static str> {
    match panose.stroke_variation() {
        0 => None,
        1..=3 => Some("low"),
        4..=6 => Some("medium"),
        _ => Some("high"),
    }
}

fn panose_roundness(panose: Panose) -> Option<&'static str> {
    // Heuristic mapping of Latin sans serif-style values. PANOSE has no direct
    // "roundness" axis, so this is advisory only and is only used when both the
    // intent and the candidate declare it.
    if panose.family_kind() != 2 {
        return None;
    }
    match panose.serif_style() {
        11 => Some("grotesque"),
        12 | 14 => Some("humanist"),
        13 | 15 => Some("geometric"),
        _ => None,
    }
}

fn normalized_eq_score(left: &str, right: &str) -> f32 {
    if left.trim().eq_ignore_ascii_case(right.trim()) {
        1.0
    } else {
        0.25
    }
}

fn metric_score(traits: &FontTraits, candidate: &FontMetadata) -> f32 {
    let mut score = 0.0;
    let mut weight = 0.0;

    if let Some(target) = traits.units_per_em {
        if candidate.units_per_em > 0 {
            let actual = f32::from(candidate.units_per_em);
            score += closeness(f32::from(target), actual) * 0.05;
            weight += 0.05;
        }
    }
    if let Some(target) = traits.x_height {
        if let Some(actual) = candidate.x_height_ratio() {
            score += closeness(normalize_intent_ratio(target, traits.units_per_em), actual) * 0.25;
            weight += 0.25;
        }
    }
    if let Some(target) = traits.cap_height {
        if let Some(actual) = candidate.cap_height_ratio() {
            score += closeness(normalize_intent_ratio(target, traits.units_per_em), actual) * 0.20;
            weight += 0.20;
        }
    }
    if let Some(target) = traits.ascent {
        if let Some(actual) = candidate.ascent_ratio() {
            score += closeness(
                normalize_intent_ratio(f32::from(target), traits.units_per_em),
                actual,
            ) * 0.20;
            weight += 0.20;
        }
    }
    if let Some(target) = traits.descent {
        if let Some(actual) = candidate.descent_ratio() {
            score += closeness(
                normalize_intent_ratio(f32::from(target), traits.units_per_em),
                actual,
            ) * 0.15;
            weight += 0.15;
        }
    }
    if let Some(target) = traits.average_advance {
        if let Some(actual) = candidate.average_advance_ratio() {
            score += closeness(normalize_intent_ratio(target, traits.units_per_em), actual) * 0.20;
            weight += 0.20;
        }
    }

    if weight == 0.0 {
        1.0
    } else {
        (score / weight).clamp(0.0, 1.0)
    }
}

fn normalize_intent_ratio(value: f32, units_per_em: Option<u16>) -> f32 {
    match units_per_em {
        Some(units) if units > 0 && value.abs() > 2.0 => value / f32::from(units),
        _ => value,
    }
}

fn closeness(left: f32, right: f32) -> f32 {
    let span = left.abs().max(right.abs()).max(0.001);
    (1.0 - (left - right).abs() / span).clamp(0.0, 1.0)
}

fn script_coverage(required: &[String], candidate: &FontMetadata) -> f32 {
    let required = normalized_scripts(required);
    if required.is_empty() {
        return 1.0;
    }
    let matched = required
        .iter()
        .filter(|script| candidate.covers_script(script))
        .count();
    matched as f32 / required.len() as f32
}

fn feature_coverage(required: &[String], candidate: &FontMetadata) -> f32 {
    let required = normalized_features(required);
    if required.is_empty() {
        return 1.0;
    }
    let matched = required
        .iter()
        .filter(|feature| candidate.supports_feature(feature))
        .count();
    matched as f32 / required.len() as f32
}

fn format_score(target: Option<&TargetFontProfile>, candidate: &FontMetadata) -> f32 {
    let Some(target) = target else {
        return 1.0;
    };
    if target.supports_format(&candidate.asset.format) {
        1.0
    } else if target.raster_only() {
        0.9
    } else {
        0.0
    }
}

fn missing_scripts(intent: &FontIntent, candidate: &FontMetadata) -> Vec<String> {
    normalized_scripts(&intent.scripts)
        .into_iter()
        .filter(|script| !candidate.covers_script(script))
        .collect()
}

fn missing_features(intent: &FontIntent, candidate: &FontMetadata) -> Vec<String> {
    normalized_features(&intent.features)
        .into_iter()
        .filter(|feature| !candidate.supports_feature(feature))
        .collect()
}

fn required_scripts(intent: &FontIntent) -> Vec<String> {
    normalized_scripts(&intent.scripts)
}

fn normalized_scripts(scripts: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = scripts
        .iter()
        .map(|script| normalize_script(script))
        .filter(|script| !script.is_empty())
        .collect();
    set.into_iter().collect()
}

fn normalized_features(features: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = features
        .iter()
        .map(|feature| normalize_feature(feature))
        .filter(|feature| !feature.is_empty())
        .collect();
    set.into_iter().collect()
}

fn preserve_source_metrics(traits: &mut FontTraits, source: &FontMetadata) {
    if traits.units_per_em.is_none() {
        traits.units_per_em = Some(source.units_per_em);
    }
    if traits.x_height.is_none() {
        traits.x_height = source.x_height_ratio();
    }
    if traits.cap_height.is_none() {
        traits.cap_height = source.cap_height_ratio();
    }
    if traits.ascent.is_none() {
        traits.ascent = Some(source.ascent);
    }
    if traits.descent.is_none() {
        traits.descent = Some(source.descent);
    }
    if traits.average_advance.is_none() {
        traits.average_advance = source.average_advance_ratio();
    }
}

fn metric_map(
    source: Option<&FontMetadata>,
    target: Option<&FontMetadata>,
) -> BTreeMap<String, Value> {
    let mut metrics = BTreeMap::new();
    if let Some(source) = source {
        metrics.insert("source_units_per_em".to_owned(), json!(source.units_per_em));
        if let Some(value) = source.x_height_ratio() {
            metrics.insert("source_x_height_ratio".to_owned(), json!(value));
        }
        if let Some(value) = source.cap_height_ratio() {
            metrics.insert("source_cap_height_ratio".to_owned(), json!(value));
        }
        if let Some(value) = source.ascent_ratio() {
            metrics.insert("source_ascent_ratio".to_owned(), json!(value));
        }
        if let Some(value) = source.descent_ratio() {
            metrics.insert("source_descent_ratio".to_owned(), json!(value));
        }
        if let Some(value) = source.average_advance_ratio() {
            metrics.insert("source_average_advance_ratio".to_owned(), json!(value));
        }
    }
    if let Some(target) = target {
        metrics.insert("target_units_per_em".to_owned(), json!(target.units_per_em));
        if let Some(value) = target.x_height_ratio() {
            metrics.insert("target_x_height_ratio".to_owned(), json!(value));
        }
        if let Some(value) = target.cap_height_ratio() {
            metrics.insert("target_cap_height_ratio".to_owned(), json!(value));
        }
        if let Some(value) = target.ascent_ratio() {
            metrics.insert("target_ascent_ratio".to_owned(), json!(value));
        }
        if let Some(value) = target.descent_ratio() {
            metrics.insert("target_descent_ratio".to_owned(), json!(value));
        }
        if let Some(value) = target.average_advance_ratio() {
            metrics.insert("target_average_advance_ratio".to_owned(), json!(value));
        }
        if let Some(source) = source {
            if source.units_per_em > 0 {
                metrics.insert(
                    "size_scale".to_owned(),
                    json!(f32::from(target.units_per_em) / f32::from(source.units_per_em)),
                );
            }
        }
    }
    metrics
}

fn round_score(value: f32) -> f32 {
    let clamped = value.clamp(0.0, 1.0);
    (clamped * 10_000.0).round() / 10_000.0
}

/// Whether the target accepts a candidate's format.
pub fn candidate_supports_target(target: &TargetFontProfile, candidate: &FontMetadata) -> bool {
    target.supports_format(&candidate.asset.format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{FontAsset, FontMetadata};

    fn synthetic_font(family: &str, classification: &str) -> FontMetadata {
        FontMetadata {
            asset: FontAsset {
                id: format!("font-{family}"),
                family: family.to_owned(),
                style: "Regular".to_owned(),
                version: Some("1.000".to_owned()),
                format: "ttf".to_owned(),
                sha256: format!("{family:0<64}").to_ascii_lowercase(),
                size_bytes: 1_000,
                source: "web".to_owned(),
                license: Some("OFL-1.1".to_owned()),
                embedding_allowed: true,
                subset_allowed: true,
                ..FontAsset::default()
            },
            classification: classification.to_owned(),
            weight: 400,
            width: 5,
            width_name: "normal".to_owned(),
            slant: "upright".to_owned(),
            units_per_em: 1_000,
            ascent: 800,
            descent: -200,
            line_gap: 0,
            x_height: Some(500),
            cap_height: Some(700),
            average_advance: Some(500.0),
            glyph_count: 500,
            scripts: vec!["latin".to_owned()],
            features: vec!["tabular_nums".to_owned()],
            ..FontMetadata::default()
        }
    }

    fn score(intent: &FontIntent, font: &FontMetadata) -> ScoreBreakdown {
        score_candidate(intent, font).expect("candidate scores")
    }

    #[test]
    fn panose_distance_is_bounded_symmetric_and_family_aware() {
        let base = "2 11 5 3 2 2 3 2 2 4";
        assert_eq!(panose_distance(base, base), Some(0.0));

        let heavier = "2 11 7 3 2 2 3 2 2 4";
        let distance = panose_distance(base, heavier).expect("distance");
        assert!(distance > 0.0 && distance < 1.0);
        assert_eq!(
            panose_distance(base, heavier),
            panose_distance(heavier, base)
        );

        let script = "3 2 5 3 2 2 3 2 2 4";
        assert_eq!(panose_distance(base, script), Some(1.0));
        assert!(panose_distance("invalid", base).is_none());
    }

    #[test]
    fn matching_order_is_deterministic_for_ties() {
        let intent = FontIntent {
            scripts: vec!["latin".to_owned()],
            ..FontIntent::default()
        };
        let mut candidates = vec![
            synthetic_font("Zeta", "sans"),
            synthetic_font("alpha", "sans"),
            synthetic_font("Mu", "sans"),
        ];
        let first: Vec<String> = match_font(&intent, &candidates)
            .into_iter()
            .map(|matched| matched.font.asset.family)
            .collect();
        assert_eq!(first, vec!["alpha", "Mu", "Zeta"]);

        candidates.reverse();
        let second: Vec<String> = match_font(&intent, &candidates)
            .into_iter()
            .map(|matched| matched.font.asset.family)
            .collect();
        assert_eq!(first, second);
    }

    #[test]
    fn metric_compatibility_prefers_close_metrics() {
        let mut intent = FontIntent::new("body", "sans");
        intent.traits.units_per_em = Some(1_000);
        intent.traits.x_height = Some(500.0);
        intent.traits.cap_height = Some(700.0);
        intent.traits.ascent = Some(800);
        intent.traits.descent = Some(-200);
        intent.traits.average_advance = Some(500.0);

        let close = synthetic_font("Close", "sans");
        let mut far = synthetic_font("Far", "sans");
        far.x_height = Some(650);
        far.cap_height = Some(550);
        far.ascent = 950;
        far.descent = -50;
        far.average_advance = Some(700.0);

        assert!(score(&intent, &close).metrics > score(&intent, &far).metrics);
        assert!(score(&intent, &close).total > score(&intent, &far).total);
    }

    #[test]
    fn script_and_feature_coverage_are_scored_and_reported() {
        let mut intent = FontIntent::new("numeric", "sans");
        intent.scripts = vec!["latin".to_owned(), "arabic".to_owned()];
        intent.features = vec!["tabular_nums".to_owned(), "slashed_zero".to_owned()];

        let latin_only = synthetic_font("Latin Only", "sans");
        let mut broad = synthetic_font("Broad", "sans");
        broad.scripts.push("arabic".to_owned());
        broad.features.push("slashed_zero".to_owned());

        let latin_score = score(&intent, &latin_only);
        let broad_score = score(&intent, &broad);
        assert!(latin_score.coverage < 1.0);
        assert_eq!(broad_score.coverage, 1.0);
        assert!(broad_score.features > latin_score.features);

        let matches = match_font(&intent, &[latin_only, broad]);
        assert_eq!(matches[0].font.asset.family, "Broad");
        assert!(matches[1].missing_scripts.contains(&"arabic".to_owned()));
        assert!(matches[1]
            .missing_features
            .contains(&"slashed_zero".to_owned()));
    }

    #[test]
    fn classification_and_role_are_hard_filters() {
        let intent = FontIntent::new("code", "mono");
        let sans = synthetic_font("Sans", "sans");
        let mono = synthetic_font("Mono", "mono");
        let matches = match_font(&intent, &[sans, mono.clone()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].font.asset.family, "Mono");

        let icon_intent = FontIntent::new("icon", "icon");
        assert!(choose_font(&icon_intent, &[mono]).is_none());
    }

    #[test]
    fn target_format_compatibility_affects_score() {
        let mut intent = FontIntent::new("body", "sans");
        intent.target = Some(TargetFontProfile::new("browser", ["ttf"]));
        let ttf = synthetic_font("TTF Sans", "sans");
        let mut otf = synthetic_font("OTF Sans", "sans");
        otf.asset.format = "otf".to_owned();

        assert_eq!(score(&intent, &ttf).format, 1.0);
        assert_eq!(score(&intent, &otf).format, 0.0);
        assert!(score(&intent, &ttf).total > score(&intent, &otf).total);
    }

    #[test]
    fn plan_export_uses_original_exactly() {
        let original = synthetic_font("Source Sans", "sans");
        let intent = FontIntent::new("body", "sans");
        let target = TargetFontProfile::new("browser", ["ttf", "woff2"]);
        let report = plan_export(&intent, Some(&original), &[], &target);
        assert_eq!(report.action, FontTranslationAction::Exact);
        assert_eq!(report.source_font, "Source Sans");
        assert_eq!(report.target_font, "Source Sans");
        assert!(report.license_ok);
        assert!(report.shape_score.is_none());
        assert!(report.metrics.is_some());
        assert!(report.dropped_glyphs.is_empty());
    }

    #[test]
    fn plan_export_subsets_oversized_original() {
        let original = synthetic_font("Source Sans", "sans");
        let intent = FontIntent::new("body", "sans");
        let mut target = TargetFontProfile::new("lvgl", ["ttf"]);
        target.max_bytes = Some(500);
        let report = plan_export(&intent, Some(&original), &[], &target);
        assert_eq!(report.action, FontTranslationAction::Subset);
        assert_eq!(report.target_font, "Source Sans");
        assert!(report.license_ok);
    }

    #[test]
    fn plan_export_substitutes_non_embeddable_original() {
        let mut original = synthetic_font("Restricted Sans", "sans");
        original.asset.embedding_allowed = false;
        original.asset.subset_allowed = false;
        original.asset.fs_type = Some(0x0002);
        let candidate = synthetic_font("Open Sans", "sans");
        let intent = FontIntent::new("body", "sans");
        let target = TargetFontProfile::new("browser", ["ttf"]);

        let report = plan_export(&intent, Some(&original), &[candidate], &target);
        assert_eq!(report.action, FontTranslationAction::Substituted);
        assert_eq!(report.target_font, "Open Sans");
        assert_eq!(report.source_font, "Restricted Sans");
        assert!(report.shape_score.is_some());
        assert!(report.license_ok);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("does not permit embedding")));
    }

    #[test]
    fn plan_export_rasterizes_for_bitmap_targets() {
        let original = synthetic_font("Source Sans", "sans");
        let intent = FontIntent::new("label", "sans");
        let mut target = TargetFontProfile::new("eink", ["bdf"]);
        target.bpp = Some(1);
        let report = plan_export(&intent, Some(&original), &[], &target);
        assert_eq!(report.action, FontTranslationAction::Rasterized);
        assert_eq!(report.target_font, "Source Sans");
        assert!(report.license_ok);
    }

    #[test]
    fn plan_export_reports_unavailable_without_legal_candidate() {
        let original = synthetic_font("Source Sans", "sans");
        let mut intent = FontIntent::new("body", "sans");
        intent.scripts = vec!["latin".to_owned()];
        let target = TargetFontProfile::new("web", ["woff2"]);
        let report = plan_export(&intent, Some(&original), &[], &target);
        assert_eq!(report.action, FontTranslationAction::Unavailable);
        assert!(report.target_font.is_empty());
        assert!(!report.license_ok);
        assert_eq!(report.dropped_glyphs, vec!["script:latin".to_owned()]);
    }

    #[test]
    fn plan_export_rejects_disallowed_source_licence() {
        let mut original = synthetic_font("GPL Sans", "sans");
        original.asset.license = Some("GPL-2.0-only".to_owned());
        let intent = FontIntent::new("body", "sans");
        let target = TargetFontProfile::new("browser", ["ttf"]);
        let report = plan_export(&intent, Some(&original), &[], &target);
        assert_eq!(report.action, FontTranslationAction::Unavailable);
        assert!(!report.license_ok);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("licence")));
    }

    #[test]
    fn plan_export_can_substitute_for_rejected_licence() {
        let mut original = synthetic_font("GPL Sans", "sans");
        original.asset.license = Some("GPL-2.0-only".to_owned());
        let candidate = synthetic_font("OFL Sans", "sans");
        let intent = FontIntent::new("body", "sans");
        let target = TargetFontProfile::new("browser", ["ttf"]);
        let report = plan_export(&intent, Some(&original), &[candidate], &target);
        assert_eq!(report.action, FontTranslationAction::Substituted);
        assert_eq!(report.target_font, "OFL Sans");
        assert!(report.license_ok);
    }

    #[test]
    fn candidate_format_helper_matches_target_aliases() {
        let target = TargetFontProfile::new("browser", ["truetype"]);
        let font = synthetic_font("TTF Sans", "sans");
        assert!(candidate_supports_target(&target, &font));
    }
}
