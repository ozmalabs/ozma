// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Lossless font-asset metadata extraction.
//!
//! The importer keeps everything it can read from the font: naming-table
//! records, OS/2 embedding bits and metrics, PANOSE, variable axes, Unicode
//! coverage (as compact ranges), derived scripts, OpenType layout features, and
//! a content hash. Derived fields are marked as such in their documentation; the
//! original values are retained alongside them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use ttf_parser::{Face, Tag};

/// Length of a lower-case hexadecimal SHA-256 digest.
pub const SHA256_HEX_LEN: usize = 64;

/// Errors produced while reading or parsing a font asset.
#[derive(Debug, Error)]
pub enum FontError {
    /// The file could not be read.
    #[error("failed to read font file {path}: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The bytes are not a parseable SFNT face.
    #[error("failed to parse font: {0}")]
    Parse(#[from] ttf_parser::FaceParsingError),
    /// The container is understood but not handled by this parser.
    #[error("unsupported font container: {0}")]
    Unsupported(String),
}

/// Options controlling how a font asset is described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontParseOptions {
    /// Explicit asset id. Defaults to a content-derived id.
    pub id: Option<String>,
    /// Provenance: `embedded`, `device`, `system`, `web`, `generated`, or
    /// `imported`.
    pub source: String,
    /// SPDX-style licence identifier. When absent, the `name` table's licence
    /// description (name ID 13) is used verbatim.
    pub license: Option<String>,
    /// Whether the asset must stay private (the default for imports).
    pub private: bool,
}

impl Default for FontParseOptions {
    fn default() -> Self {
        Self {
            id: None,
            source: "imported".to_owned(),
            license: None,
            private: true,
        }
    }
}

/// A font asset and its provenance, as stored in the display IR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontAsset {
    /// Stable asset id, usually content-derived.
    pub id: String,
    /// Preferred family name.
    pub family: String,
    /// Preferred subfamily/style name.
    pub style: String,
    /// Font version string from name ID 5.
    pub version: Option<String>,
    /// Container/format: `ttf`, `otf`, `ttc`, `woff`, `woff2`, `bdf`, ...
    pub format: String,
    /// Lower-case hexadecimal SHA-256 of the complete asset bytes.
    pub sha256: String,
    /// Complete asset size in bytes.
    pub size_bytes: u64,
    /// Provenance: `embedded`, `device`, `system`, `web`, `generated`, or
    /// `imported`.
    pub source: String,
    /// Declared licence (SPDX identifier when known).
    pub license: Option<String>,
    /// Licence URL from name ID 14, when present.
    pub license_url: Option<String>,
    /// Copyright notice from name ID 0, when present.
    pub copyright: Option<String>,
    /// Whether the `fsType` bits permit outline embedding.
    pub embedding_allowed: bool,
    /// Whether this asset is itself already a subset.
    pub subset: bool,
    /// Whether the `fsType` bits permit subsetting.
    pub subset_allowed: bool,
    /// Raw OS/2 `fsType` value, retained for auditing.
    pub fs_type: Option<u16>,
    /// `fsType` declares bitmap-only embedding.
    pub bitmap_only: bool,
    /// Never publish this asset without explicit permission.
    pub private: bool,
}

impl Default for FontAsset {
    fn default() -> Self {
        Self {
            id: String::new(),
            family: String::new(),
            style: String::new(),
            version: None,
            format: String::new(),
            sha256: String::new(),
            size_bytes: 0,
            source: "imported".to_owned(),
            license: None,
            license_url: None,
            copyright: None,
            embedding_allowed: false,
            subset: false,
            subset_allowed: false,
            fs_type: None,
            bitmap_only: false,
            private: true,
        }
    }
}

impl FontAsset {
    /// Whether the declared licence is on the Ozma redistribution allowlist.
    ///
    /// Unknown or missing licences are rejected.
    pub fn license_allowed(&self) -> bool {
        license_is_allowlisted(self.license.as_deref())
    }

    /// Whether the asset may be embedded in an exported pack: both the licence
    /// and the font's own `fsType` bits must allow it.
    pub fn can_embed(&self) -> bool {
        self.embedding_allowed && self.license_allowed()
    }
}

/// A single `name` table record, retained verbatim for lossless capture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FontNameRecord {
    /// OpenType name ID.
    pub name_id: u16,
    /// Platform (`unicode`, `macintosh`, `windows`, ...).
    pub platform: String,
    /// Platform-specific encoding id.
    pub encoding_id: u16,
    /// Platform-specific language id.
    pub language_id: u16,
    /// Decoded UTF-8 value. Non-Unicode records are skipped by the parser.
    pub value: String,
}

/// One axis of a variable font, including its full range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableAxis {
    /// Four-character axis tag (`wght`, `wdth`, `opsz`, ...).
    pub tag: String,
    /// Human-readable name from the `name` table when available.
    pub name: Option<String>,
    /// Minimum axis value.
    pub min: f32,
    /// Default/nominal axis value.
    pub default: f32,
    /// Maximum axis value.
    pub max: f32,
    /// Whether the axis is intended to be hidden from users.
    pub hidden: bool,
}

/// OS/2 `fsType` embedding permissions.
///
/// The raw bits are retained because different targets have different notions
/// of "embedding" (editable outline, preview/print, bitmap, subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EmbeddingPermissions {
    /// Raw `fsType` value, if the OS/2 table exists.
    pub fs_type: Option<u16>,
    /// Outline embedding is permitted.
    pub embedding_allowed: bool,
    /// Subsetting is permitted.
    pub subset_allowed: bool,
    /// Only bitmap embedding is permitted (`0x0200`).
    pub bitmap_only: bool,
    /// Preview/print embedding only (`0x0004`).
    pub preview_print_only: bool,
    /// Subsetting is explicitly forbidden (`0x0100`).
    pub no_subsetting: bool,
    /// Restricted-licence embedding (`0x0002`).
    pub restricted: bool,
}

impl EmbeddingPermissions {
    /// Decode the OS/2 `fsType` embedding bits.
    ///
    /// The relevant bits are `0x0002` (restricted), `0x0004` (preview/print),
    /// `0x0100` (no subsetting), and `0x0200` (bitmap embedding only). Missing
    /// OS/2 data is treated conservatively: embedding is not allowed.
    pub fn from_fs_type(fs_type: Option<u16>) -> Self {
        let Some(value) = fs_type else {
            return Self::default();
        };
        let restricted = value & 0x0002 != 0;
        let preview_print_only = value & 0x0004 != 0;
        let no_subsetting = value & 0x0100 != 0;
        let bitmap_only = value & 0x0200 != 0;
        let embedding_allowed = !restricted && !preview_print_only && !bitmap_only;

        Self {
            fs_type: Some(value),
            embedding_allowed,
            subset_allowed: embedding_allowed && !no_subsetting,
            bitmap_only,
            preview_print_only,
            no_subsetting,
            restricted,
        }
    }
}

/// The ten PANOSE bytes, with helpers for the fields used by shape matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panose(pub [u8; 10]);

impl Panose {
    /// Parse a PANOSE string using spaces, `-`, `:`, `,`, or `;` as separators.
    pub fn parse(value: &str) -> Option<Self> {
        let mut bytes = [0u8; 10];
        let parts: Vec<&str> = value
            .split(|c: char| c.is_whitespace() || matches!(c, '-' | ':' | ',' | ';'))
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() != bytes.len() {
            return None;
        }
        for (slot, part) in bytes.iter_mut().zip(parts) {
            let trimmed = part
                .strip_prefix("0x")
                .or_else(|| part.strip_prefix("0X"))
                .unwrap_or(part);
            *slot = if trimmed.len() != part.len() {
                u8::from_str_radix(trimmed, 16).ok()?
            } else {
                trimmed.parse::<u8>().ok()?
            };
        }
        Some(Self(bytes))
    }

    /// PANOSE family kind (`0` any, `2` Latin text, `3` script, `4`
    /// decorative, `5` symbol, and higher values for non-Latin scripts).
    pub fn family_kind(self) -> u8 {
        self.0[0]
    }

    /// PANOSE serif style (index 1).
    pub fn serif_style(self) -> u8 {
        self.0[1]
    }

    /// PANOSE weight (index 2).
    pub fn weight(self) -> u8 {
        self.0[2]
    }

    /// PANOSE proportion (index 3).
    pub fn proportion(self) -> u8 {
        self.0[3]
    }

    /// PANOSE stroke variation (index 5), used as a contrast proxy.
    pub fn stroke_variation(self) -> u8 {
        self.0[5]
    }

    /// Whether every byte is zero (PANOSE "any").
    pub fn is_empty(self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }
}

impl fmt::Display for Panose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{byte}")?;
        }
        Ok(())
    }
}

/// Complete parsed metadata for one font face.
///
/// `asset` is flattened into the serialized form, so the JSON shape matches the
/// `FontAsset` contract plus the extended metadata fields.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FontMetadata {
    /// Provenance and asset fields.
    #[serde(flatten)]
    pub asset: FontAsset,
    /// Derived classification: `sans`, `serif`, `mono`, `display`, `script`,
    /// or `icon`.
    pub classification: String,
    /// OS/2 weight class (100..900).
    pub weight: u16,
    /// OS/2 width class (1..9).
    pub width: u16,
    /// Human-readable width class (`condensed`, `normal`, `expanded`, ...).
    pub width_name: String,
    /// Slant: `upright`, `italic`, or `oblique`.
    pub slant: String,
    /// Font declares `fvar` variation axes.
    pub is_variable: bool,
    /// Font is monospaced according to `post.isFixedPitch`/metrics.
    pub is_monospaced: bool,
    /// Font has colour glyph tables (`COLR`, `CBDT`, `sbix`, or `SVG`).
    pub has_color_glyphs: bool,
    /// Units per em from `head`.
    pub units_per_em: u16,
    /// Horizontal ascender in font units.
    pub ascent: i16,
    /// Horizontal descender in font units (usually negative).
    pub descent: i16,
    /// Line gap in font units.
    pub line_gap: i16,
    /// OS/2 x-height in font units.
    pub x_height: Option<i16>,
    /// OS/2 cap height in font units.
    pub cap_height: Option<i16>,
    /// Mean advance width in font units over a bounded sample of covered
    /// characters.
    pub average_advance: Option<f32>,
    /// Number of glyphs in the face.
    pub glyph_count: u16,
    /// Canonical PANOSE display string, when present.
    pub panose: Option<String>,
    /// OS/2 `sFamilyClass`, retained as a signed 16-bit value.
    pub os2_family_class: Option<i32>,
    /// Default value of each variable axis, keyed by tag.
    pub variable_axes: BTreeMap<String, f32>,
    /// Full variable-axis ranges.
    pub variable_axis_ranges: Vec<VariableAxis>,
    /// Compact Unicode coverage ranges (`U+0041-U+005A`).
    pub glyph_coverage: Vec<String>,
    /// Derived script names covered by the Unicode cmap.
    pub scripts: Vec<String>,
    /// Canonical OpenType layout feature names (for example `tabular_nums`).
    pub features: Vec<String>,
    /// Every decoded Unicode `name` table record.
    pub names: Vec<FontNameRecord>,
    /// Sorted SFNT table tags present in the face.
    pub tables: Vec<String>,
    /// Non-fatal issues encountered while capturing metadata.
    pub warnings: Vec<String>,
}

impl std::ops::Deref for FontMetadata {
    type Target = FontAsset;

    fn deref(&self) -> &Self::Target {
        &self.asset
    }
}

impl std::ops::DerefMut for FontMetadata {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.asset
    }
}

impl FontMetadata {
    /// Parse a font from bytes with default provenance options.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FontError> {
        Self::from_bytes_with(bytes, &FontParseOptions::default())
    }

    /// Parse a font from bytes with explicit provenance options.
    pub fn from_bytes_with(bytes: &[u8], options: &FontParseOptions) -> Result<Self, FontError> {
        let format = detect_format(bytes)?;
        let face = Face::parse(bytes, 0)?;
        let sha256 = sha256_hex(bytes);
        let names = collect_names(&face);
        let family = pick_name(&names, &[16, 1]).unwrap_or_else(|| "Unknown".to_owned());
        let style = pick_name(&names, &[17, 2]).unwrap_or_else(|| "Regular".to_owned());
        let version = pick_name(&names, &[5]);
        let copyright = pick_name(&names, &[0]);
        let license_description = pick_name(&names, &[13]);
        let license_url = pick_name(&names, &[14]);
        let os2 = os2_info(&face);
        let permissions = EmbeddingPermissions::from_fs_type(os2.fs_type);
        let panose = os2.panose.filter(|panose| !panose.is_empty());
        let weight = face.weight().to_number();
        let width = face.width().to_number();
        let width_name = width_name(face.width());
        let slant = match face.style() {
            ttf_parser::Style::Normal => "upright",
            ttf_parser::Style::Italic => "italic",
            ttf_parser::Style::Oblique => "oblique",
        }
        .to_owned();
        let codepoints = collect_codepoints(&face);
        let scripts = classify_scripts(&codepoints);
        let glyph_coverage = coverage_ranges(&codepoints);
        let features = collect_features(&face);
        let variable_axis_ranges = collect_variable_axes(&face, &names);
        let variable_axes = variable_axis_ranges
            .iter()
            .map(|axis| (axis.tag.clone(), axis.default))
            .collect();
        let tables = collect_tables(&face);
        let classification = classify_font(&family, face.is_monospaced(), panose);
        let size_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let id = options
            .id
            .clone()
            .unwrap_or_else(|| format!("font-{}", &sha256[..16]));

        let mut warnings = Vec::new();
        if os2.fs_type.is_none() {
            warnings.push(
                "font has no readable OS/2 fsType; embedding is not permitted by default"
                    .to_owned(),
            );
        } else if permissions.restricted {
            warnings.push("OS/2 fsType marks the font as restricted".to_owned());
        } else if permissions.preview_print_only {
            warnings.push("OS/2 fsType permits preview/print embedding only".to_owned());
        }
        if permissions.bitmap_only {
            warnings.push("OS/2 fsType permits bitmap embedding only".to_owned());
        }
        if permissions.no_subsetting {
            warnings.push("OS/2 fsType forbids subsetting".to_owned());
        }
        if options.license.is_none() && license_description.is_none() {
            warnings.push("font declares no licence in its name table".to_owned());
        }

        let asset = FontAsset {
            id,
            family,
            style,
            version,
            format: format.to_owned(),
            sha256,
            size_bytes,
            source: options.source.clone(),
            license: options.license.clone().or(license_description),
            license_url,
            copyright,
            embedding_allowed: permissions.embedding_allowed,
            subset: false,
            subset_allowed: permissions.subset_allowed,
            fs_type: os2.fs_type,
            bitmap_only: permissions.bitmap_only,
            private: options.private,
        };

        Ok(Self {
            asset,
            classification,
            weight,
            width,
            width_name,
            slant,
            is_variable: face.is_variable(),
            is_monospaced: face.is_monospaced(),
            has_color_glyphs: face.tables().colr.is_some()
                || face.tables().cbdt.is_some()
                || face.tables().sbix.is_some()
                || face.tables().svg.is_some(),
            units_per_em: face.units_per_em(),
            ascent: face.ascender(),
            descent: face.descender(),
            line_gap: face.line_gap(),
            x_height: face.x_height(),
            cap_height: face.capital_height(),
            average_advance: average_advance(&face, &codepoints),
            glyph_count: face.number_of_glyphs(),
            panose: panose.map(|panose| panose.to_string()),
            os2_family_class: os2.family_class,
            variable_axes,
            variable_axis_ranges,
            glyph_coverage,
            scripts,
            features,
            names,
            tables,
            warnings,
        })
    }

    /// Parse a font file from disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, FontError> {
        Self::from_path_with(path, &FontParseOptions::default())
    }

    /// Parse a font file from disk with explicit provenance options.
    pub fn from_path_with(
        path: impl AsRef<Path>,
        options: &FontParseOptions,
    ) -> Result<Self, FontError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| FontError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_bytes_with(&bytes, options)
    }

    /// Replace the declared licence (useful when the licence is known from a
    /// pack manifest rather than the font's name table).
    pub fn with_license(mut self, license: impl Into<String>) -> Self {
        self.asset.license = Some(license.into());
        self
    }

    /// Replace the provenance source.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.asset.source = source.into();
        self
    }

    /// Decode the stored OS/2 embedding bits.
    pub fn permissions(&self) -> EmbeddingPermissions {
        EmbeddingPermissions::from_fs_type(self.asset.fs_type)
    }

    /// Whether the asset is both licence- and fsType-embedding compatible.
    pub fn can_embed(&self) -> bool {
        self.asset.can_embed()
    }

    /// Parsed PANOSE value, when present.
    pub fn panose_value(&self) -> Option<Panose> {
        self.panose.as_deref().and_then(Panose::parse)
    }

    /// Normalize a metric to em units.
    pub fn em_ratio(&self, value: i16) -> Option<f32> {
        if self.units_per_em == 0 {
            None
        } else {
            Some(f32::from(value) / f32::from(self.units_per_em))
        }
    }

    /// x-height as a fraction of the em square.
    pub fn x_height_ratio(&self) -> Option<f32> {
        self.x_height.and_then(|value| self.em_ratio(value))
    }

    /// Cap height as a fraction of the em square.
    pub fn cap_height_ratio(&self) -> Option<f32> {
        self.cap_height.and_then(|value| self.em_ratio(value))
    }

    /// Ascender as a fraction of the em square.
    pub fn ascent_ratio(&self) -> Option<f32> {
        self.em_ratio(self.ascent)
    }

    /// Descender as a fraction of the em square.
    pub fn descent_ratio(&self) -> Option<f32> {
        self.em_ratio(self.descent)
    }

    /// Mean advance width as a fraction of the em square.
    pub fn average_advance_ratio(&self) -> Option<f32> {
        if self.units_per_em == 0 {
            return None;
        }
        self.average_advance
            .map(|value| value / f32::from(self.units_per_em))
    }

    /// Whether the declared scripts cover `script`.
    pub fn covers_script(&self, script: &str) -> bool {
        let script = normalize_script(script);
        self.scripts
            .iter()
            .any(|candidate| normalize_script(candidate) == script)
    }

    /// Whether a canonical OpenType feature name is available.
    pub fn supports_feature(&self, feature: &str) -> bool {
        let feature = normalize_feature(feature);
        self.features
            .iter()
            .any(|candidate| normalize_feature(candidate) == feature)
    }
}

/// Compute the lower-case hexadecimal SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(SHA256_HEX_LEN);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether a licence string is on the Ozma redistribution allowlist.
///
/// Allowed families are MIT, BSD, ISC, Apache-2.0, SIL OFL, CC0, public domain,
/// and Unlicense. GPL/AGPL/LGPL, non-commercial, personal-use-only, proprietary,
/// and unknown licences are rejected. This is intentionally conservative;
/// callers may add an explicit licence after attestation.
pub fn license_is_allowlisted(license: Option<&str>) -> bool {
    let Some(license) = license else {
        return false;
    };
    let normalized = license.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    const REJECT: &[&str] = &[
        "gpl",
        "agpl",
        "lgpl",
        "noncommercial",
        "non-commercial",
        "cc by-nc",
        "cc-by-nc",
        "personal use",
        "personal-use",
        "proprietary",
        "all rights reserved",
        "unknown",
    ];
    if REJECT.iter().any(|token| normalized.contains(token)) {
        return false;
    }
    const ALLOW: &[&str] = &["mit", "bsd", "isc", "apache", "ofl", "cc0", "unlicense"];
    let tokens: Vec<&str> = normalized
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    ALLOW.iter().any(|token| tokens.contains(token))
        || normalized.contains("open font license")
        || normalized.contains("public domain")
}

/// Normalize a script name for comparisons (`latin_ext` -> `latin-ext`).
pub fn normalize_script(script: &str) -> String {
    script.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}

/// Normalize an OpenType feature name and map raw tags to canonical names.
///
/// Both `tnum` and `tabular-nums` normalize to `tabular_nums`, so intents and
/// parsed font metadata compare consistently.
pub fn normalize_feature(feature: &str) -> String {
    canonical_feature_name(feature)
}

/// Map a raw or human-readable OpenType feature name to its canonical form.
pub fn canonical_feature_name(feature: &str) -> String {
    let normalized = feature.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "tnum" => "tabular_nums".to_owned(),
        "pnum" => "proportional_nums".to_owned(),
        "lnum" => "lining_nums".to_owned(),
        "onum" => "oldstyle_nums".to_owned(),
        "zero" => "slashed_zero".to_owned(),
        "liga" => "ligatures".to_owned(),
        "dlig" => "discretionary_ligatures".to_owned(),
        "clig" => "contextual_ligatures".to_owned(),
        "calt" => "contextual_alternates".to_owned(),
        "frac" => "fractions".to_owned(),
        "sups" => "superscript".to_owned(),
        "subs" => "subscript".to_owned(),
        "smcp" => "small_caps".to_owned(),
        "c2sc" => "caps_to_small_caps".to_owned(),
        "kern" => "kerning".to_owned(),
        "locl" => "localized_forms".to_owned(),
        "salt" => "stylistic_alternates".to_owned(),
        "case" => "case_sensitive".to_owned(),
        "ordn" => "ordinals".to_owned(),
        "numr" => "numerator".to_owned(),
        "dnom" => "denominator".to_owned(),
        _ => normalized,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Os2Info {
    fs_type: Option<u16>,
    family_class: Option<i32>,
    panose: Option<Panose>,
}

fn os2_info(face: &Face<'_>) -> Os2Info {
    let Some(data) = face.raw_face().table(Tag::from_bytes(b"OS/2")) else {
        return Os2Info::default();
    };
    let fs_type = read_u16(data, 8);
    let family_class = read_i16(data, 30).map(i32::from);
    let panose = data
        .get(32..42)
        .and_then(|slice| <[u8; 10]>::try_from(slice).ok())
        .map(Panose);
    Os2Info {
        fs_type,
        family_class,
        panose,
    }
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    read_u16(data, offset).map(|value| value as i16)
}

fn detect_format(bytes: &[u8]) -> Result<&'static str, FontError> {
    let magic = bytes.get(0..4).unwrap_or_default();
    match magic {
        [0x00, 0x01, 0x00, 0x00] | b"true" => Ok("ttf"),
        b"OTTO" => Ok("otf"),
        b"ttcf" => Ok("ttc"),
        b"wOFF" => Err(FontError::Unsupported(
            "WOFF must be decompressed to TTF/OTF before metadata capture".to_owned(),
        )),
        b"wOF2" => Err(FontError::Unsupported(
            "WOFF2 must be decompressed to TTF/OTF before metadata capture".to_owned(),
        )),
        _ if bytes.starts_with(b"STARTFONT ") => Err(FontError::Unsupported(
            "BDF fonts do not use SFNT metadata; provide a FontAsset directly".to_owned(),
        )),
        _ => Err(FontError::Unsupported(format!(
            "unrecognized font magic {magic:02x?}"
        ))),
    }
}

fn collect_names(face: &Face<'_>) -> Vec<FontNameRecord> {
    let mut records = Vec::new();
    for name in face.names() {
        let Some(value) = name.to_string() else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let platform = match name.platform_id {
            ttf_parser::PlatformId::Unicode => "unicode",
            ttf_parser::PlatformId::Macintosh => "macintosh",
            ttf_parser::PlatformId::Iso => "iso",
            ttf_parser::PlatformId::Windows => "windows",
            ttf_parser::PlatformId::Custom => "custom",
        };
        records.push(FontNameRecord {
            name_id: name.name_id,
            platform: platform.to_owned(),
            encoding_id: name.encoding_id,
            language_id: name.language_id,
            value,
        });
    }
    records.sort();
    records.dedup();
    records
}

fn pick_name(records: &[FontNameRecord], ids: &[u16]) -> Option<String> {
    for name_id in ids {
        let mut best: Option<(u8, &FontNameRecord)> = None;
        for record in records.iter().filter(|record| record.name_id == *name_id) {
            let score = match record.platform.as_str() {
                "windows" if record.language_id == 0x0409 => 4,
                "windows" => 3,
                "unicode" => 2,
                "macintosh" => 1,
                _ => 0,
            };
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, record));
            }
        }
        if let Some((_, record)) = best {
            return Some(record.value.clone());
        }
    }
    None
}

fn collect_tables(face: &Face<'_>) -> Vec<String> {
    let mut tables = BTreeSet::new();
    for record in face.raw_face().table_records {
        tables.insert(record.tag.to_chars().iter().collect::<String>());
    }
    tables.into_iter().collect()
}

fn collect_codepoints(face: &Face<'_>) -> BTreeSet<u32> {
    let mut codepoints = BTreeSet::new();
    let Some(cmap) = &face.tables().cmap else {
        return codepoints;
    };
    for index in 0..cmap.subtables.len() {
        let Some(subtable) = cmap.subtables.get(index) else {
            continue;
        };
        if !subtable.is_unicode() {
            continue;
        }
        subtable.codepoints(|codepoint| {
            codepoints.insert(codepoint);
        });
    }
    codepoints
}

fn average_advance(face: &Face<'_>, codepoints: &BTreeSet<u32>) -> Option<f32> {
    // Bound the sample so parsing a CJK font stays cheap and deterministic.
    const MAX_SAMPLE: usize = 8192;
    let mut sum = 0u64;
    let mut count = 0u64;
    for codepoint in codepoints.iter().take(MAX_SAMPLE) {
        let Some(character) = char::from_u32(*codepoint) else {
            continue;
        };
        let Some(glyph) = face.glyph_index(character) else {
            continue;
        };
        let Some(advance) = face.glyph_hor_advance(glyph) else {
            continue;
        };
        sum += u64::from(advance);
        count += 1;
    }
    if count == 0 {
        None
    } else {
        Some(sum as f32 / count as f32)
    }
}

fn coverage_ranges(codepoints: &BTreeSet<u32>) -> Vec<String> {
    let mut ranges = Vec::new();
    let mut iter = codepoints.iter().copied();
    let Some(mut start) = iter.next() else {
        return ranges;
    };
    let mut end = start;
    for codepoint in iter {
        if codepoint == end.saturating_add(1) {
            end = codepoint;
        } else {
            ranges.push(format_range(start, end));
            start = codepoint;
            end = codepoint;
        }
    }
    ranges.push(format_range(start, end));
    ranges
}

fn format_range(start: u32, end: u32) -> String {
    if start == end {
        format!("U+{start:04X}")
    } else {
        format!("U+{start:04X}-U+{end:04X}")
    }
}

fn classify_scripts(codepoints: &BTreeSet<u32>) -> Vec<String> {
    const SCRIPTS: &[(&str, &[(u32, u32)])] = &[
        (
            "latin",
            &[
                (0x0041, 0x005A),
                (0x0061, 0x007A),
                (0x00C0, 0x00FF),
                (0x2C60, 0x2C7F),
                (0xA720, 0xA7FF),
            ],
        ),
        (
            "latin-ext",
            &[
                (0x0100, 0x024F),
                (0x1E00, 0x1EFF),
                (0x2C60, 0x2C7F),
                (0xA720, 0xA7FF),
            ],
        ),
        ("greek", &[(0x0370, 0x03FF), (0x1F00, 0x1FFF)]),
        (
            "cyrillic",
            &[(0x0400, 0x052F), (0x2DE0, 0x2DFF), (0xA640, 0xA69F)],
        ),
        ("hebrew", &[(0x0590, 0x05FF), (0xFB1D, 0xFB4F)]),
        (
            "arabic",
            &[
                (0x0600, 0x06FF),
                (0x0750, 0x077F),
                (0x08A0, 0x08FF),
                (0xFB50, 0xFDFF),
                (0xFE70, 0xFEFF),
            ],
        ),
        ("devanagari", &[(0x0900, 0x097F), (0xA8E0, 0xA8FF)]),
        ("bengali", &[(0x0980, 0x09FF)]),
        ("gurmukhi", &[(0x0A00, 0x0A7F)]),
        ("gujarati", &[(0x0A80, 0x0AFF)]),
        ("oriya", &[(0x0B00, 0x0B7F)]),
        ("tamil", &[(0x0B80, 0x0BFF)]),
        ("telugu", &[(0x0C00, 0x0C7F)]),
        ("kannada", &[(0x0C80, 0x0CFF)]),
        ("malayalam", &[(0x0D00, 0x0D7F)]),
        ("sinhala", &[(0x0D80, 0x0DFF)]),
        ("thai", &[(0x0E00, 0x0E7F)]),
        ("lao", &[(0x0E80, 0x0EFF)]),
        ("tibetan", &[(0x0F00, 0x0FFF)]),
        (
            "myanmar",
            &[(0x1000, 0x109F), (0xA9E0, 0xA9FF), (0xAA60, 0xAA7F)],
        ),
        (
            "georgian",
            &[(0x10A0, 0x10FF), (0x1C90, 0x1CBF), (0x2D00, 0x2D2F)],
        ),
        (
            "hangul",
            &[
                (0x1100, 0x11FF),
                (0x3130, 0x318F),
                (0xA960, 0xA97F),
                (0xAC00, 0xD7AF),
            ],
        ),
        (
            "ethiopic",
            &[(0x1200, 0x137F), (0x1380, 0x139F), (0x2D80, 0x2DDF)],
        ),
        ("cherokee", &[(0x13A0, 0x13FF), (0xAB70, 0xABBF)]),
        ("canadian-aboriginal", &[(0x1400, 0x167F), (0x18B0, 0x18FF)]),
        ("khmer", &[(0x1780, 0x17FF), (0x19E0, 0x19FF)]),
        ("mongolian", &[(0x1800, 0x18AF)]),
        ("hiragana", &[(0x3040, 0x309F)]),
        (
            "katakana",
            &[(0x30A0, 0x30FF), (0x31F0, 0x31FF), (0xFF66, 0xFF9F)],
        ),
        (
            "cjk",
            &[
                (0x2E80, 0x2FFF),
                (0x3400, 0x4DBF),
                (0x4E00, 0x9FFF),
                (0xF900, 0xFAFF),
                (0x20000, 0x2FA1F),
            ],
        ),
        ("yi", &[(0xA000, 0xA4CF)]),
        ("emoji", &[(0x1F000, 0x1FAFF), (0x2600, 0x27BF)]),
        (
            "symbols",
            &[(0x2000, 0x25FF), (0x2B00, 0x2BFF), (0x1F000, 0x1FAFF)],
        ),
        (
            "private-use",
            &[(0xE000, 0xF8FF), (0xF0000, 0xFFFFD), (0x100000, 0x10FFFD)],
        ),
    ];

    let mut found = BTreeSet::new();
    for (name, ranges) in SCRIPTS {
        let present = codepoints.iter().any(|codepoint| {
            ranges
                .iter()
                .any(|(start, end)| *codepoint >= *start && *codepoint <= *end)
        });
        if present {
            found.insert((*name).to_owned());
        }
    }
    found.into_iter().collect()
}

fn collect_features(face: &Face<'_>) -> Vec<String> {
    let mut features = BTreeSet::new();
    for table in [face.tables().gsub.as_ref(), face.tables().gpos.as_ref()]
        .into_iter()
        .flatten()
    {
        for feature in table.features {
            features.insert(feature_name(feature.tag.to_chars()));
        }
    }
    if face.tables().kern.is_some() {
        features.insert("kerning".to_owned());
    }
    features.into_iter().collect()
}

fn feature_name(tag: [char; 4]) -> String {
    let tag: String = tag.into_iter().collect();
    canonical_feature_name(&tag)
}

fn collect_variable_axes(face: &Face<'_>, names: &[FontNameRecord]) -> Vec<VariableAxis> {
    let mut axes = Vec::new();
    for axis in face.variation_axes() {
        let tag: String = axis.tag.to_chars().iter().collect();
        axes.push(VariableAxis {
            name: pick_name(names, &[axis.name_id]),
            tag,
            min: axis.min_value,
            default: axis.def_value,
            max: axis.max_value,
            hidden: axis.hidden,
        });
    }
    axes.sort_by(|left, right| left.tag.cmp(&right.tag));
    axes
}

fn width_name(width: ttf_parser::Width) -> String {
    match width {
        ttf_parser::Width::UltraCondensed => "ultra-condensed",
        ttf_parser::Width::ExtraCondensed => "extra-condensed",
        ttf_parser::Width::Condensed => "condensed",
        ttf_parser::Width::SemiCondensed => "semi-condensed",
        ttf_parser::Width::Normal => "normal",
        ttf_parser::Width::SemiExpanded => "semi-expanded",
        ttf_parser::Width::Expanded => "expanded",
        ttf_parser::Width::ExtraExpanded => "extra-expanded",
        ttf_parser::Width::UltraExpanded => "ultra-expanded",
    }
    .to_owned()
}

fn classify_font(family: &str, monospaced: bool, panose: Option<Panose>) -> String {
    let family = family.to_ascii_lowercase();
    if monospaced
        || family.contains("mono")
        || family.contains("code")
        || family.contains("console")
    {
        return "mono".to_owned();
    }
    if family.contains("icon")
        || family.contains("symbol")
        || family.contains("awesome")
        || family.contains("emoji")
    {
        return "icon".to_owned();
    }
    if family.contains("script") || family.contains("hand") || family.contains("cursive") {
        return "script".to_owned();
    }
    if let Some(panose) = panose {
        match panose.family_kind() {
            3 => return "script".to_owned(),
            4 => return "display".to_owned(),
            5 => return "icon".to_owned(),
            _ => {}
        }
    }
    if family.contains("sans") {
        return "sans".to_owned();
    }
    if family.contains("serif") {
        return "serif".to_owned();
    }
    if family.contains("display") || family.contains("decorative") {
        return "display".to_owned();
    }
    if let Some(panose) = panose {
        match panose.serif_style() {
            2..=10 => return "serif".to_owned(),
            11..=15 => return "sans".to_owned(),
            _ => {}
        }
    }
    "sans".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SANS_FIXTURES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    ];
    const MONO_FIXTURES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];
    const TABULAR_FIXTURES: &[&str] = &[
        "/usr/share/fonts/Adwaita/AdwaitaSans-Italic.ttf",
        "/usr/share/fonts/adwaita/AdwaitaSans-Italic.ttf",
        "/usr/share/fonts/truetype/adwaita/AdwaitaSans-Italic.ttf",
    ];

    fn fixture(paths: &[&str]) -> Option<PathBuf> {
        paths.iter().map(PathBuf::from).find(|path| path.is_file())
    }

    fn sans_fixture() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("OZMA_FONT_TEST_FONT") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
        fixture(SANS_FIXTURES)
    }

    #[test]
    fn synthetic_metadata_round_trips_losslessly() {
        let metadata = FontMetadata {
            asset: FontAsset {
                id: "font-test".to_owned(),
                family: "Test Sans".to_owned(),
                style: "Regular".to_owned(),
                version: Some("1.234".to_owned()),
                format: "ttf".to_owned(),
                sha256: "a".repeat(SHA256_HEX_LEN),
                size_bytes: 42,
                source: "embedded".to_owned(),
                license: Some("OFL-1.1".to_owned()),
                license_url: Some("https://example.invalid/ofl".to_owned()),
                copyright: Some("Copyright Test".to_owned()),
                embedding_allowed: true,
                subset: true,
                subset_allowed: true,
                fs_type: Some(0),
                bitmap_only: false,
                private: true,
            },
            classification: "sans".to_owned(),
            weight: 400,
            width: 5,
            width_name: "normal".to_owned(),
            slant: "upright".to_owned(),
            is_variable: true,
            is_monospaced: false,
            has_color_glyphs: false,
            units_per_em: 1_000,
            ascent: 800,
            descent: -200,
            line_gap: 0,
            x_height: Some(500),
            cap_height: Some(700),
            average_advance: Some(500.0),
            glyph_count: 12,
            panose: Some("2 11 5 3 2 2 3 2 2 4".to_owned()),
            os2_family_class: Some(8),
            variable_axes: BTreeMap::from([("wght".to_owned(), 400.0)]),
            variable_axis_ranges: vec![VariableAxis {
                tag: "wght".to_owned(),
                name: Some("Weight".to_owned()),
                min: 100.0,
                default: 400.0,
                max: 900.0,
                hidden: false,
            }],
            glyph_coverage: vec!["U+0020-U+007E".to_owned()],
            scripts: vec!["latin".to_owned()],
            features: vec!["tabular_nums".to_owned()],
            names: vec![FontNameRecord {
                name_id: 1,
                platform: "windows".to_owned(),
                encoding_id: 1,
                language_id: 0x0409,
                value: "Test Sans".to_owned(),
            }],
            tables: vec!["OS/2".to_owned(), "cmap".to_owned(), "head".to_owned()],
            warnings: vec!["sample".to_owned()],
        };

        let value = serde_json::to_value(&metadata).expect("serialize metadata");
        assert_eq!(value["family"], "Test Sans");
        assert!(value.get("asset").is_none(), "asset must be flattened");
        let restored: FontMetadata = serde_json::from_value(value).expect("deserialize metadata");
        assert_eq!(metadata, restored);
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn fs_type_bits_are_decoded() {
        let installable = EmbeddingPermissions::from_fs_type(Some(0));
        assert!(installable.embedding_allowed);
        assert!(installable.subset_allowed);

        let restricted = EmbeddingPermissions::from_fs_type(Some(0x0002));
        assert!(!restricted.embedding_allowed);
        assert!(!restricted.subset_allowed);
        assert!(restricted.restricted);

        let preview = EmbeddingPermissions::from_fs_type(Some(0x0004));
        assert!(!preview.embedding_allowed);
        assert!(preview.preview_print_only);

        let no_subset = EmbeddingPermissions::from_fs_type(Some(0x0100));
        assert!(no_subset.embedding_allowed);
        assert!(!no_subset.subset_allowed);
        assert!(no_subset.no_subsetting);

        let bitmap = EmbeddingPermissions::from_fs_type(Some(0x0200));
        assert!(!bitmap.embedding_allowed);
        assert!(bitmap.bitmap_only);

        let unknown = EmbeddingPermissions::from_fs_type(None);
        assert!(!unknown.embedding_allowed);
        assert!(!unknown.subset_allowed);
    }

    #[test]
    fn licence_allowlist_is_conservative() {
        for allowed in [
            "MIT",
            "BSD-3-Clause",
            "ISC",
            "Apache-2.0",
            "OFL-1.1",
            "SIL Open Font License 1.1",
            "CC0-1.0",
            "public domain",
        ] {
            assert!(license_is_allowlisted(Some(allowed)), "{allowed}");
        }
        for rejected in [
            None,
            Some(""),
            Some("GPL-2.0-only"),
            Some("AGPL-3.0"),
            Some("CC BY-NC 4.0"),
            Some("personal-use-only"),
            Some("Proprietary"),
            Some("Unknown"),
            Some("submit-only license"),
        ] {
            assert!(!license_is_allowlisted(rejected), "{rejected:?}");
        }
    }

    #[test]
    fn panose_parses_and_formats() {
        let panose = Panose::parse("2 11 5 3 2 2 3 2 2 4").expect("valid panose");
        assert_eq!(panose.to_string(), "2 11 5 3 2 2 3 2 2 4");
        assert_eq!(panose.family_kind(), 2);
        assert_eq!(panose.serif_style(), 11);
        assert_eq!(Panose::parse("0:0:0:0:0:0:0:0:0:0"), Some(Panose([0; 10])));
        assert!(Panose::parse("not panose").is_none());
        assert!(Panose::parse("1 2 3").is_none());
    }

    #[test]
    fn coverage_ranges_are_compact_and_stable() {
        let points = BTreeSet::from([0x0041, 0x0042, 0x0044]);
        assert_eq!(
            coverage_ranges(&points),
            vec!["U+0041-U+0042".to_owned(), "U+0044".to_owned()]
        );
        assert!(coverage_ranges(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn scripts_are_classified_from_codepoints() {
        let points = BTreeSet::from([
            0x0041, // Latin A
            0x0100, // Latin Extended-A
            0x03B1, // Greek alpha
            0x0416, // Cyrillic Zhe
            0x0627, // Arabic alef
            0x4E2D, // CJK middle
        ]);
        let scripts = classify_scripts(&points);
        for expected in ["latin", "latin-ext", "greek", "cyrillic", "arabic", "cjk"] {
            assert!(
                scripts.iter().any(|script| script == expected),
                "{expected}"
            );
        }
    }

    #[test]
    fn feature_tags_are_canonicalized() {
        assert_eq!(feature_name(['t', 'n', 'u', 'm']), "tabular_nums");
        assert_eq!(feature_name(['z', 'e', 'r', 'o']), "slashed_zero");
        assert_eq!(feature_name(['l', 'i', 'g', 'a']), "ligatures");
        assert_eq!(feature_name(['s', 's', '0', '1']), "ss01");
        assert_eq!(normalize_feature("tnum"), "tabular_nums");
        assert_eq!(normalize_feature("tabular-nums"), "tabular_nums");
        assert_eq!(normalize_feature("slashed zero"), "slashed_zero");
    }

    #[test]
    fn rejects_woff_containers_with_a_clear_error() {
        let error = FontMetadata::from_bytes(b"wOFF\0\0\0\0").expect_err("woff is rejected");
        assert!(matches!(error, FontError::Unsupported(_)));
        assert!(error.to_string().contains("WOFF"));
    }

    #[test]
    fn system_sans_metadata_is_lossless_and_round_trips() {
        let Some(path) = sans_fixture() else {
            eprintln!(
                "skipping system_sans_metadata_is_lossless_and_round_trips: no DejaVu or \
                 Liberation font found; set OZMA_FONT_TEST_FONT to a TTF/OTF fixture"
            );
            return;
        };
        let bytes = fs::read(&path).expect("read fixture");
        let metadata = FontMetadata::from_path(&path).expect("parse fixture");

        assert_eq!(metadata.asset.sha256, sha256_hex(&bytes));
        assert_eq!(metadata.asset.sha256.len(), SHA256_HEX_LEN);
        assert_eq!(metadata.asset.size_bytes, bytes.len() as u64);
        assert!(!metadata.asset.family.is_empty());
        assert!(!metadata.asset.style.is_empty());
        assert!(["ttf", "otf", "ttc"].contains(&metadata.asset.format.as_str()));
        assert!(metadata.units_per_em > 0);
        assert!(metadata.glyph_count > 0);
        assert!(metadata.ascent > 0);
        assert!(metadata.ascent > metadata.descent);
        assert!(metadata.covers_script("latin"));
        assert!(!metadata.glyph_coverage.is_empty());
        assert!(metadata.tables.iter().any(|table| table == "head"));
        assert!(metadata
            .names
            .iter()
            .any(|name| name.name_id == 1 || name.name_id == 16));
        assert!(metadata
            .names
            .iter()
            .any(|name| name.name_id == 2 || name.name_id == 17));
        assert!(metadata.average_advance.is_some());

        // DejaVu and Liberation both declare OS/2 permissions and PANOSE.
        assert!(metadata.asset.fs_type.is_some());
        assert!(metadata.asset.embedding_allowed);
        if let Some(panose) = metadata.panose.as_deref() {
            assert!(Panose::parse(panose).is_some());
        }

        let json = serde_json::to_string(&metadata).expect("serialize metadata");
        let restored: FontMetadata = serde_json::from_str(&json).expect("deserialize metadata");
        assert_eq!(metadata, restored);
    }

    #[test]
    fn system_mono_font_is_classified_as_mono() {
        let Some(path) = fixture(MONO_FIXTURES) else {
            eprintln!(
                "skipping system_mono_font_is_classified_as_mono: no DejaVu or Liberation mono \
                 font found"
            );
            return;
        };
        let metadata = FontMetadata::from_path(&path).expect("parse mono fixture");
        assert!(
            metadata.is_monospaced
                || metadata.classification == "mono"
                || metadata.asset.family.to_ascii_lowercase().contains("mono"),
            "expected a monospaced face, got {metadata:?}"
        );
    }

    #[test]
    fn system_tabular_figures_feature_is_detected() {
        let Some(path) = fixture(TABULAR_FIXTURES) else {
            eprintln!(
                "skipping system_tabular_figures_feature_is_detected: no local font with a tnum \
                 feature found"
            );
            return;
        };
        let metadata = FontMetadata::from_path(&path).expect("parse tabular fixture");
        assert!(
            metadata
                .features
                .iter()
                .any(|feature| feature == "tabular_nums"),
            "expected tnum to map to tabular_nums, got {:?}",
            metadata.features
        );
        assert!(metadata.supports_feature("tabular-nums"));
    }

    #[test]
    fn variable_axes_are_captured_when_available() {
        let candidates = [
            "/usr/share/fonts/cantarell/Cantarell-VF.otf",
            "/usr/share/fonts/truetype/cantarell/Cantarell-VF.otf",
        ];
        let Some(path) = fixture(&candidates) else {
            eprintln!("skipping variable_axes_are_captured_when_available: no variable fixture");
            return;
        };
        let metadata = FontMetadata::from_path(&path).expect("parse variable fixture");
        assert!(metadata.is_variable);
        assert!(!metadata.variable_axis_ranges.is_empty());
        assert_eq!(
            metadata.variable_axes.len(),
            metadata.variable_axis_ranges.len()
        );
        for axis in &metadata.variable_axis_ranges {
            assert!(axis.min <= axis.default);
            assert!(axis.default <= axis.max);
            assert!(metadata.variable_axes.contains_key(&axis.tag));
        }
    }
}
